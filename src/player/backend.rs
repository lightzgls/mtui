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

use anyhow::{Context, Result};
use stream_download::http::HttpStream;
use stream_download::storage::bounded::BoundedStorageProvider;
use stream_download::storage::memory::MemoryStorageProvider;
use stream_download::{Settings, StreamDownload};

use super::chunked::ChunkedClient;

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

/// A network audio stream presented as `Read + Seek`, ready for a decoder.
pub type AudioStream = StreamDownload<BoundedStorageProvider<MemoryStorageProvider>>;

/// Opens `url` as a bounded, seekable stream.
///
/// Must be called from within a tokio runtime context: `stream-download` spawns
/// its downloader as a task. The returned handle is then read synchronously
/// from the player thread, off the runtime.
pub async fn open(url: &str) -> Result<AudioStream> {
    let capacity = NonZeroUsize::new(BUFFER_BYTES).expect("BUFFER_BYTES is a non-zero constant");
    let storage = BoundedStorageProvider::new(MemoryStorageProvider, capacity);

    // `new` rather than `new_http`: the latter hard-codes a plain `reqwest`
    // client, which reads the body as one long response and gets throttled.
    StreamDownload::new::<HttpStream<ChunkedClient>>(
        url.parse()
            .context("resolved stream URL is not a valid URL")?,
        storage,
        Settings::default().prefetch_bytes(PREFETCH_BYTES),
    )
    .await
    .context("failed to open audio stream")
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
        let tube = crate::source::innertube::InnerTube::new().expect("client should build");
        tube.resolve(&id)
            .or_else(|_| crate::source::youtube::YouTube::default().resolve(&id))
            .expect("resolve failed")
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

        let stream = runtime.block_on(super::open(&url)).expect("open failed");
        let decoder = rodio::Decoder::new_mp4(stream).expect("decoder failed");
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

        let url = stream_url();

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .expect("could not build runtime");

        let stream = runtime.block_on(super::open(&url)).expect("open failed");
        let mut decoder = rodio::Decoder::new_mp4(stream).expect("decoder failed");
        let per_second =
            decoder.sample_rate().get() as u64 * decoder.channels().get() as u64;
        // What the player itself compares against to tell a track that ended
        // from a stream that died, so the verdict here is the one it would give.
        let total = decoder.total_duration();

        let start = Instant::now();
        let mut samples = 0u64;
        // Pull one second of audio, then wait until the wall clock has caught
        // up, so the ring buffer drains at the rate real playback drains it.
        loop {
            let got = decoder.by_ref().take(per_second as usize).count() as u64;
            samples += got;

            let decoded = Duration::from_secs_f64(samples as f64 / per_second as f64);
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
                break;
            }
            if samples.is_multiple_of(per_second * 10) {
                println!("decoded {:.0}s", decoded.as_secs_f64());
            }
            if let Some(wait) = decoded.checked_sub(start.elapsed()) {
                std::thread::sleep(wait);
            }
        }
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
        let stream = runtime.block_on(super::open(&url)).expect("open failed");
        println!(
            "open (prefetch {} KB)  : {:.2}s",
            super::PREFETCH_BYTES / 1024,
            start.elapsed().as_secs_f64()
        );

        let at_decoder = Instant::now();
        let mut decoder = rodio::Decoder::new_mp4(stream).expect("decoder failed");
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
