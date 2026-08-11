//! The audio pipeline: HTTPS -> bounded ring buffer -> symphonia -> rodio.
//!
//! The memory-critical invariant lives here. Audio is *never* buffered in full;
//! `stream-download`'s bounded storage is a fixed-size circular buffer that
//! overwrites its oldest bytes and back-pressures the downloader when the
//! decoder falls behind. A three-minute track and a three-hour livestream cost
//! exactly the same resident bytes.
//!
//! The known bottleneck is googlevideo's throttle. A single sequential GET is
//! held to roughly twice playback rate -- measured at a flat ~32 KB/s from the
//! first byte -- while the *same* bytes fetched as 512 KB `Range` chunks come
//! down at ~6 MB/s, two hundred times faster. That is why yt-dlp itself
//! downloads in chunks. [`PREFETCH_BYTES`] currently buys time-to-first-sound
//! back from the throttle rather than removing it; removing it means a custom
//! `SourceStream` that issues sequential range requests, which would also make
//! seeking cheap. The bounded-memory invariant above is unaffected either way.

use std::num::NonZeroUsize;
use std::time::Duration;

use anyhow::{Context, Result};
use stream_download::http::HttpStream;
use stream_download::storage::bounded::BoundedStorageProvider;
use stream_download::storage::memory::MemoryStorageProvider;
use stream_download::{Settings, StreamDownload};

use super::chunked::{ChunkedClient, StreamLink};

/// Ring buffer size. At itag 140's ~130 kbps this is roughly 30 seconds of
/// audio -- ample to ride out network jitter, small enough to stay negligible
/// against our total budget.
///
/// Raising this trades RAM for jitter tolerance one-for-one; it is the single
/// biggest tunable in the whole program.
pub const BUFFER_BYTES: usize = 512 * 1024;

/// How much must arrive before the decoder is handed the stream.
///
/// This is time-to-first-sound, and it is worth far more care than it looks,
/// because googlevideo throttles a single sequential connection to roughly
/// twice playback rate. Measured against itag 140: a flat ~32 KB/s from the
/// first byte, so the crate's 256 KB default costs **8.1 seconds of silence**
/// before a single sample is decoded. 32 KB costs ~1 second.
///
/// 32 KB is not arbitrary at either end. The `moov` atom sits at the front of
/// these files and ends within the first ~3 KB, so the decoder can initialise
/// almost immediately; the remaining ~29 KB is about two seconds of audio at
/// 130 kbps, which is the cushion playback starts with. Draining it is safe:
/// inflow beats consumption two to one, so the ring buffer only grows from
/// here.
///
/// Lowering this further trades that cushion for a few hundred milliseconds.
/// The real fix is chunked range requests, which sidestep the throttle
/// entirely -- see the module header.
const PREFETCH_BYTES: u64 = 32 * 1024;

/// How long `stream-download` waits on a chunk before reconnecting itself.
///
/// Raised from the crate's 5 s default so that it sits *outside* the chunked
/// client's own timeouts rather than inside them. A chunk is delivered as one
/// `bytes()` await, so with the default a chunk still legitimately arriving --
/// or one being retried after a transient fault -- was cut off from above and
/// reconnected underneath, re-requesting bytes that were already on their way.
/// The client below now owns that decision; this only has to be slower than it.
const RETRY_TIMEOUT: Duration = Duration::from_secs(30);

/// A network audio stream presented as `Read + Seek`, ready for a decoder.
pub type AudioStream = StreamDownload<BoundedStorageProvider<MemoryStorageProvider>>;

/// Opens `url` as a bounded, seekable stream.
///
/// Must be called from within a tokio runtime context: `stream-download` spawns
/// its downloader as a task. The returned handle is then read synchronously
/// from the player thread, off the runtime.
///
/// The [`StreamLink`] returned beside it is how the caller learns *why* the
/// stream stopped, and how it hands down a fresh URL when the old one is
/// refused. Nothing else survives the trip: symphonia turns a failed
/// read into "no more samples" and rodio ends that source exactly as it ends a
/// finished one, so without this a refused byte range and a song that reached
/// its end arrive identically.
pub async fn open(url: &str) -> Result<(AudioStream, StreamLink)> {
    let capacity = NonZeroUsize::new(BUFFER_BYTES).expect("BUFFER_BYTES is a non-zero constant");
    let storage = BoundedStorageProvider::new(MemoryStorageProvider, capacity);
    let link = StreamLink::default();

    // Built here and handed in, rather than through `StreamDownload::new` and
    // the `Client::create` it calls: that trait method takes no arguments, so
    // it is the one route by which the link can reach the client.
    //
    // A plain `reqwest` client is still not an option -- it reads the body as
    // one long response and gets throttled to roughly playback rate.
    let url = url
        .parse()
        .context("resolved stream URL is not a valid URL")?;
    let stream = HttpStream::new(ChunkedClient::with_link(link.clone()), url)
        .await
        .context("failed to open audio stream")?;

    let download = StreamDownload::from_stream(
        stream,
        storage,
        Settings::default()
            .prefetch_bytes(PREFETCH_BYTES)
            .retry_timeout(RETRY_TIMEOUT),
    )
    .await
    .context("failed to open audio stream")?;

    Ok((download, link))
}

/// Hands an open stream to symphonia.
///
/// `Decoder::new_mp4` rather than the probing `Decoder::new`: we always request
/// itag 140, so format sniffing would only waste a seek and a read.
///
/// It also leaves rodio's `is_seekable` at `false`, and that has to stay false
/// however much the decoder would like to seek, because the two costs of
/// telling symphonia otherwise are both ruinous here. Its mp4 reader parses
/// every atom in a seekable stream rather than stopping at the `mdat`, which
/// means a jump over the whole of the audio -- megabytes -- that bounded
/// storage refuses outright, so the decoder fails to open at all; and serving
/// that jump instead would mean downloading the track in full, which is the one
/// thing this module exists not to do.
///
/// What the stream is told it cannot do, it is then not asked to do. Symphonia
/// emulates a forward seek by reading ahead and discarding, which is exact. It
/// has no answer for a backward one -- see [`super::Command::Seek`], which winds
/// a fresh stream forward instead.
pub fn decoder(stream: AudioStream) -> Result<rodio::Decoder<AudioStream>> {
    rodio::Decoder::new_mp4(stream).context("could not decode audio stream")
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use rodio::Source;

    /// The stream these tests run against: a well-known video by default, or
    /// whatever `MTUI_VIDEO_ID` names -- which is how a track that misbehaved
    /// for a user gets put under the same measurements.
    fn stream_url() -> String {
        if let Ok(url) = std::env::var("MTUI_STREAM_URL") {
            return url;
        }
        let id = std::env::var("MTUI_VIDEO_ID").unwrap_or_else(|_| "dQw4w9WgXcQ".into());
        // The app's cascade, not just its fast path: a track the player API
        // will not serve whole is exactly the kind these tests exist for, and
        // going straight to InnerTube would measure a URL nothing ever plays.
        //
        // Shared with the worker rather than spelled out again here. Written
        // out, this drifted the moment the cap check moved: the local version
        // took the player API's answer whenever it resolved at all, capped or
        // not, and so measured exactly the URL the app declines to use.
        resolve(&id)
    }

    /// One trip through the app's resolve cascade, for the track under test.
    fn resolve(id: &str) -> String {
        let tube = crate::source::innertube::InnerTube::new().ok();
        let yt = crate::source::youtube::YouTube::default();
        crate::source::resolve_stream(&yt, tube.as_ref(), id)
            .expect("resolve failed")
            .0
            .url
    }

    /// A fresh stream can be wound to any point in the track.
    ///
    /// This is what recovery rests on. A stream that died at 2:00 cannot be
    /// picked up where it stopped: the ring buffer holds thirty seconds, and
    /// seeking a live stream past that is refused when it is not served wrong
    /// -- a backward seek beyond the buffer is answered without complaint and
    /// then fails the next read, which is precisely how a track ends up silent
    /// with no error. The player opens a new stream and winds it forward
    /// instead, which needs both halves checked here: that the container states
    /// a duration, without which a dead stream cannot be told from a finished
    /// one, and that winding forward really does yield audio, quickly.
    ///
    /// `cargo test --release winds_to -- --ignored --nocapture`
    #[test]
    #[ignore = "hits the network"]
    fn winds_to_the_middle_of_a_track() {
        let url = stream_url();

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .expect("could not build runtime");

        let (stream, _link) = runtime.block_on(super::open(&url)).expect("open failed");
        let decoder = super::decoder(stream).expect("decoder failed");
        let per_second = decoder.sample_rate().get() as usize * decoder.channels().get() as usize;

        let total = decoder
            .total_duration()
            .expect("the container should state a duration");
        assert!(total > Duration::from_secs(120), "test track is too short");

        let start = Instant::now();
        let mut wound = decoder.skip_duration(Duration::from_secs(120));
        let got = wound.by_ref().take(per_second).count();
        println!(
            "wound to 2:00 of {:.0}s in {:.2}s",
            total.as_secs_f64(),
            start.elapsed().as_secs_f64()
        );

        assert_eq!(got, per_second, "no audio after winding forward");
    }

    /// A wound stream is at the point it was wound to, to the sample.
    ///
    /// [`winds_to_the_middle_of_a_track`] proves winding forward yields audio,
    /// and quickly. This proves it yields the *right* audio, which is the half a
    /// rewind rests on: the player answers `←` by opening a fresh stream and
    /// winding it forward, because a decoder reading a ring buffer it has been
    /// told it cannot seek has no other way back. If winding landed anywhere but
    /// where it was sent, the clock would say one thing and the audio another --
    /// and the Lyrics panel, which marks the line being sung straight off that
    /// clock, would mark the wrong line for the rest of the song.
    ///
    /// The two paths to the same second of the track are compared: wound
    /// straight to it, and played to it without interruption. A relative measure
    /// rather than a sample-for-sample one, because it is seconds of drift this
    /// is looking for and not the last bit of a sample. The misaligned
    /// comparison is what stops the threshold passing vacuously -- against a
    /// window half a second out, the same measure has to fail.
    ///
    /// `cargo test --release winding_forward -- --ignored --nocapture`
    #[test]
    #[ignore = "hits the network"]
    fn winding_forward_lands_where_it_was_aimed() {
        /// Far enough in to be inside the music: a passage of near-silence
        /// would leave the two windows agreeing about nothing.
        const MARK: Duration = Duration::from_secs(20);

        let url = stream_url();

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .expect("could not build runtime");

        // Wound straight to the mark, which is the path a rewind takes.
        let (stream, _link) = runtime.block_on(super::open(&url)).expect("open failed");
        let decoder = super::decoder(stream).expect("decoder failed");
        let per_second = decoder.sample_rate().get() as usize * decoder.channels().get() as usize;
        let shift = per_second / 2;

        let start = Instant::now();
        let wound: Vec<_> = decoder.skip_duration(MARK).take(per_second).collect();
        println!(
            "wound to {}s in {:.2}s",
            MARK.as_secs(),
            start.elapsed().as_secs_f64()
        );
        assert_eq!(wound.len(), per_second, "no audio after winding forward");

        // Played to the same mark, which is the path an uninterrupted listen
        // takes and the one the wound stream has to agree with.
        let (stream, _link) = runtime.block_on(super::open(&url)).expect("open failed");
        let mut decoder = super::decoder(stream).expect("decoder failed");
        let to_mark = MARK.as_secs() as usize * per_second;
        assert_eq!(
            decoder.by_ref().take(to_mark).count(),
            to_mark,
            "the stream stopped before reaching {}s",
            MARK.as_secs()
        );
        // A second to compare against, and half a second more to compare
        // against wrongly.
        let played: Vec<_> = decoder.by_ref().take(per_second + shift).collect();
        assert_eq!(played.len(), per_second + shift, "the played stream ran short");

        // Mean absolute difference against the window's own mean level, so the
        // verdict does not depend on how loud this passage happens to be.
        let deviation = |from: &[f32]| -> f64 {
            let error: f64 = from
                .iter()
                .zip(&wound)
                .map(|(a, b)| (a - b).abs() as f64)
                .sum();
            let level: f64 = from.iter().map(|a| a.abs() as f64).sum();
            error / level.max(f64::EPSILON)
        };

        let aligned = deviation(&played[..per_second]);
        let misaligned = deviation(&played[shift..]);
        println!("deviation: aligned {aligned:.3}, half a second out {misaligned:.3}");

        assert!(
            aligned < 0.25,
            "winding to {}s did not land there: {aligned:.3} off the audio \
             an uninterrupted listen hears at that point",
            MARK.as_secs()
        );
        assert!(
            misaligned > 0.5,
            "a window half a second out scored {misaligned:.3}, so the \
             comparison above cannot tell where the wound stream landed"
        );
    }

    /// Decodes a whole track at playback pace and reports where it stopped.
    ///
    /// This is the only honest reproduction of the mid-track silence: the
    /// failure needs the downloader to be throttled by a reader consuming at
    /// ~16 KB/s over minutes, so decoding flat out never triggers it. No audio
    /// device is involved -- the samples are counted and dropped.
    ///
    /// `stream-download`'s tracing is turned on here because it is the only
    /// account of a reconnect, a re-requested gap or a download failure;
    /// rodio's decoder simply stops yielding when the read underneath it
    /// fails, which is exactly the symptom being chased.
    ///
    /// A URL refresh is answered here the way the player answers it, on a
    /// thread of its own, because otherwise the recovery path is not under test
    /// at all -- a stream whose URL stops being honoured mid-track waits for an
    /// answer nobody is there to give.
    ///
    /// The refresh count it prints is the measurement worth reading. It should
    /// be zero for a healthy track: a URL capped at a fixed offset is capped
    /// identically however often it is re-signed, so a run that needed
    /// refreshes to finish did not really recover -- it was handed a URL the
    /// resolve cascade should have rejected before playback began.
    ///
    /// `cargo test --release plays_a_whole_track -- --ignored --nocapture`
    #[test]
    #[ignore = "hits the network and runs for the length of the track"]
    fn plays_a_whole_track() {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| "stream_download=debug".into()),
            )
            .with_test_writer()
            .try_init();

        let id = std::env::var("MTUI_VIDEO_ID").unwrap_or_else(|_| "dQw4w9WgXcQ".into());
        let url = stream_url();
        // Stops the answerer below once the decode loop is done with it.
        let finished = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .expect("could not build runtime");

        let (stream, link) = runtime.block_on(super::open(&url)).expect("open failed");

        // Stands in for the player thread and the app behind it: watches for a
        // downloader that has been refused, resolves a fresh URL, and hands it
        // back. The counter is the measurement worth having -- it says how many
        // signatures a track of this length costs, which is the running price
        // of the fix.
        let refreshes = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let answerer = std::thread::spawn({
            let link = link.clone();
            let refreshes = std::sync::Arc::clone(&refreshes);
            let id = id.clone();
            let done = std::sync::Arc::clone(&finished);
            move || {
                let tube = crate::source::innertube::InnerTube::new().ok();
                let yt = crate::source::youtube::YouTube::default();
                while !done.load(std::sync::atomic::Ordering::Relaxed) {
                    if link.wants_url().is_some() {
                        let at = Instant::now();
                        // The same cascade the worker uses, cap check and
                        // all. Answering with anything cheaper hands back a URL
                        // capped exactly like the one being recovered from.
                        match crate::source::resolve_stream(&yt, tube.as_ref(), &id) {
                            Ok((fresh, _whole)) => {
                                refreshes.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                println!(
                                    "--- fresh URL in {:.2}s",
                                    at.elapsed().as_secs_f64()
                                );
                                link.supply_url(&fresh.url);
                            }
                            Err(e) => {
                                println!("--- could not resolve a fresh URL: {e:#}");
                                link.decline();
                            }
                        }
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
        });

        let mut decoder = super::decoder(stream).expect("decoder failed");
        let per_second =
            decoder.sample_rate().get() as u64 * decoder.channels().get() as u64;
        // What the player itself compares against to tell a track that ended
        // from a stream that died, so the verdict here is the one it would give.
        let total = decoder.total_duration();

        let start = Instant::now();
        let mut samples = 0u64;
        // The worst moment the decoder spent behind real time, which is the
        // only number here that says anything about what a listener hears.
        //
        // Read it in the right direction: `decoded` ahead of `wall` is the
        // decoder with cushion in hand and is what a healthy run looks like --
        // the loop below pulls a whole second and then sleeps off the
        // remainder, so it finishes a fraction ahead by construction. A gap in
        // the sound is the other sign: wall time that got *past* the audio
        // produced, meaning the ring buffer ran dry and the audio callback had
        // nothing to hand the speakers. That is what this measures.
        let mut worst_lag = Duration::ZERO;
        let mut worst_lag_at = Duration::ZERO;
        // Pull one second of audio, then wait until the wall clock has caught
        // up, so the ring buffer drains at the rate real playback drains it.
        loop {
            let got = decoder.by_ref().take(per_second as usize).count() as u64;
            samples += got;

            let decoded = Duration::from_secs_f64(samples as f64 / per_second as f64);
            // Sampled before the sleep below, which is what would hide it.
            if let Some(lag) = start.elapsed().checked_sub(decoded)
                && lag > worst_lag
            {
                worst_lag = lag;
                worst_lag_at = decoded;
            }
            if got < per_second {
                let verdict = match total {
                    Some(total) if decoded + Duration::from_secs(3) >= total => "reached the end",
                    Some(total) => &format!("STOPPED SHORT of {:.1}s", total.as_secs_f64()),
                    None => "no stated length to compare against",
                };
                println!(
                    "\n{verdict}: {:.1}s of audio in {:.1}s wall",
                    decoded.as_secs_f64(),
                    start.elapsed().as_secs_f64(),
                );
                // The whole reason this test exists. Before the fault log the
                // run ended here with the bare fact that the samples stopped,
                // which is the same thing the user sees and just as mute about
                // why -- the reason had already been discarded three layers
                // down. A refusal named here is the difference between "this
                // track is broken" and "this URL stops serving at 1.0 MiB".
                match link.fault() {
                    Some(fault) => println!("first stream fault: {fault}"),
                    None => println!("no stream fault recorded"),
                }
                println!(
                    "URL refreshes: {}",
                    refreshes.load(std::sync::atomic::Ordering::Relaxed)
                );
                // The verdict a listener would give. Anything up to a few tens
                // of milliseconds is the loop's own granularity; a real gap in
                // the sound shows up here as most of a second or more, at the
                // point in the track where it happened.
                println!(
                    "worst the decoder fell behind real time: {:.0}ms (at {:.0}s)",
                    worst_lag.as_secs_f64() * 1000.0,
                    worst_lag_at.as_secs_f64(),
                );
                break;
            }
            if samples.is_multiple_of(per_second * 10) {
                println!("decoded {:.0}s", decoded.as_secs_f64());
            }
            if let Some(wait) = decoded.checked_sub(start.elapsed()) {
                std::thread::sleep(wait);
            }
        }

        finished.store(true, std::sync::atomic::Ordering::Relaxed);
        answerer.join().expect("the URL answerer panicked");
    }

    /// Times the pipeline stage by stage against a live stream, so that
    /// "playback is slow" can be attributed to a stage instead of guessed at.
    ///
    /// Ignored by default: it needs the network. Resolves a well-known video
    /// through the normal fast path unless `MTUI_STREAM_URL` overrides it, so
    /// what it reports is the real end-to-end cost of pressing Enter.
    ///
    /// `cargo test --release probe -- --ignored --nocapture`
    #[test]
    #[ignore = "hits the network"]
    fn probe_pipeline_latency() {
        let url = std::env::var("MTUI_STREAM_URL").unwrap_or_else(|_| {
            let at_resolve = Instant::now();
            let tube = crate::source::innertube::InnerTube::new().expect("client should build");
            let resolved = tube.resolve("dQw4w9WgXcQ").expect("resolve failed");
            println!("resolve (InnerTube)   : {:.2}s", at_resolve.elapsed().as_secs_f64());
            resolved.url
        });

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .expect("could not build runtime");

        let start = Instant::now();
        let (stream, _link) = runtime.block_on(super::open(&url)).expect("open failed");
        println!(
            "open (prefetch {} KB)  : {:.2}s",
            super::PREFETCH_BYTES / 1024,
            start.elapsed().as_secs_f64()
        );

        let at_decoder = Instant::now();
        let mut decoder = super::decoder(stream).expect("decoder failed");
        println!("Decoder::new_mp4      : {:.2}s", at_decoder.elapsed().as_secs_f64());

        let at_first = Instant::now();
        let first = decoder.next();
        println!(
            "first sample          : {:.2}s (got one: {})",
            at_first.elapsed().as_secs_f64(),
            first.is_some()
        );

        // Wall time to pull one second of audio. Anything at or above 1.0s here
        // means the decoder is starved and playback cannot keep up.
        let wanted = decoder.sample_rate().get() as usize * decoder.channels().get() as usize;
        let at_second = Instant::now();
        let got = decoder.by_ref().take(wanted).count();
        println!(
            "1s of audio ({got}/{wanted}): {:.2}s",
            at_second.elapsed().as_secs_f64()
        );
        println!("TOTAL to first sound  : {:.2}s", start.elapsed().as_secs_f64());
    }
}
