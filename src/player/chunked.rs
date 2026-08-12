//! An HTTP client for `stream-download` that fetches in ranged chunks.
//!
//! googlevideo throttles a single long-lived connection to roughly twice
//! playback rate. Measured on itag 140: a flat ~32 KB/s from the first byte, so
//! the ~45 KB the decoder needs before it can emit a sample takes ~1.4 s, and
//! the ring buffer never builds much cushion. The *same* bytes requested as
//! `Range` chunks arrive at ~6 MB/s -- two hundred times faster. yt-dlp does
//! exactly this, and calls it `--http-chunk-size`.
//!
//! So the body is never read as one long response. Each chunk is its own short
//! request, which never lives long enough to be throttled, and they are issued
//! back to back to look like one continuous stream to the layer above. The
//! bounded ring buffer is untouched by this: chunks are handed over as they
//! arrive and the same fixed capacity still bounds what is resident.
//!
//! A server that ignores `Range` and answers 200 is handled by falling back to
//! reading that single response, which is exactly the old behaviour.

use std::error::Error;
use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use futures_util::{Stream, StreamExt, stream};
use reqwest::StatusCode;
use reqwest::header::{CONTENT_RANGE, HeaderMap, RANGE};
use stream_download::http::{Client, ClientResponse};
use stream_download::source::DecodeError;

/// Bytes per range request.
///
/// A quarter of the ring buffer, so a chunk lands quickly even at the larger
/// high-quality buffer size. Large enough that per-request overhead is irrelevant at the
/// speeds this unlocks, small enough that the first one lands quickly -- which
/// is the number that decides time-to-first-sound.
const CHUNK_BYTES: u64 = 256 * 1024;

/// How long a chunk request may take to connect, and to finish altogether.
///
/// Every other client in this program sets these; this one did not, and it is
/// the one that matters most. The decoder reads from the ring buffer on the
/// audio callback thread, so a connect that hangs does not fail slowly -- it
/// stops the sound and holds the thread that produces it.
///
/// Generous against what these requests really cost: a ranged 256 KB chunk
/// arrives at ~6 MB/s, so about forty milliseconds. One that has taken ten
/// seconds is not slow, it is gone. `timeout` rather than `read_timeout`
/// because it bounds the whole request -- a body trickling in just fast enough
/// to reset a per-read timer would otherwise never trip either.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Attempts at one chunk before its offset is reported as a failure.
///
/// Only transient faults are retried -- see [`is_transient`]. A refusal is not
/// made less of a refusal by asking again, and spending three round trips
/// discovering that is three round trips the ring buffer is draining through.
const CHUNK_ATTEMPTS: u32 = 3;

/// Backoff before re-asking for a chunk. Doubles per attempt.
///
/// Short on purpose: the ring buffer holds about thirty seconds, and every
/// millisecond spent waiting is one it drains. Two retries at 200 ms and 400 ms
/// cost well under a second in total.
const RETRY_BACKOFF: Duration = Duration::from_millis(200);

/// Why a stream stopped delivering bytes, recorded where it happened.
///
/// This exists because nothing else survives the trip up. `stream-download`
/// reports a failed chunk through `tracing`, and `tracing-subscriber` is a
/// dev-dependency, so in the shipped binary the reason is written to nowhere.
/// Symphonia then turns the short read into `None`, and rodio ends a source
/// whose read failed exactly as it ends a finished one. By the time the player
/// notices silence, the only evidence left is how far the clock got -- which is
/// why a capped stream and a finished song were indistinguishable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamFault {
    /// The status that refused us, when the server gave one. `None` for a
    /// transport failure that never reached a response.
    pub status: Option<StatusCode>,
    /// Byte offset the refused chunk would have started at.
    pub offset: u64,
}

impl fmt::Display for StreamFault {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let at = self.offset as f64 / (1024.0 * 1024.0);
        match self.status {
            Some(status) => write!(f, "HTTP {} at {at:.1} MiB", status.as_u16()),
            None => write!(f, "connection lost at {at:.1} MiB"),
        }
    }
}

/// How long the downloader waits for a fresh URL before giving up on one.
///
/// Bounded by the ring buffer, not by patience: it holds about thirty seconds
/// of audio and this wait is spent draining it, so the whole of that is the
/// budget and anything left over is cushion for the new URL to refill from.
///
/// Close to the whole of it, because waiting costs nothing that giving up
/// saves. The audio keeps playing out of the buffer either way; ending the wait
/// early does not rescue a second of it, it only reaches the silence sooner and
/// throws away an answer that might still have arrived in time. Measured
/// recovery resolves ranged from 4 s to 28 s -- against ten, and then twenty,
/// the slow ones lost a track that a longer wait would have saved.
const REFRESH_WAIT: Duration = Duration::from_secs(30);

/// How often the waiting downloader looks for the URL it asked for.
const REFRESH_POLL: Duration = Duration::from_millis(100);

/// The link between a running download and the player that owns it.
///
/// Carries two things in one place because they are two halves of the same
/// conversation: what went wrong, and what to do about it.
///
/// The reason it exists at all is that nothing else survives the trip up.
/// `stream-download` reports a failed chunk through `tracing`, and
/// `tracing-subscriber` is a dev-dependency, so in the shipped binary the
/// reason is written to nowhere. Symphonia then turns the short read into
/// `None`, and rodio ends a source whose read failed exactly as it ends a
/// finished one. By the time the player noticed silence, the only evidence left
/// was how far the clock got -- which is why a refused stream and a finished
/// song were indistinguishable.
///
/// The refresh half covers a signature that stopped being honoured while the
/// stream was still using it -- most plainly an `expire` that lapsed mid-track,
/// which a three-hour livestream reaches by simply playing, and any refusal
/// from one CDN node that another will serve. A fresh URL for the same file
/// answers the very same byte range, so nothing above this notices: no gap and
/// no re-decode.
///
/// It is the second line of defence against a capped URL rather than the first.
/// Measured on one: every freshly signed URL *from the same client* refuses
/// `bytes=1048576-` just the same, so this only helps because whoever answers
/// runs the full resolve cascade and can come back from a different client
/// entirely. The first line is `mtui_resolver::serves_whole_file`, which
/// rejects a capped URL before a note is played; this is what stops a track
/// dying silently when that probe is wrong.
#[derive(Debug, Clone, Default)]
pub struct StreamLink(Arc<Mutex<LinkState>>);

#[derive(Debug, Default)]
struct LinkState {
    /// First fault seen. First rather than last: the one that stopped playback
    /// is the one that explains it, and anything after it is a consequence.
    fault: Option<StreamFault>,
    /// Offset the downloader is stuck at, once it has asked for a new URL.
    wanted: Option<u64>,
    /// A fresh URL from the player, not yet picked up.
    supplied: Option<reqwest::Url>,
    /// Set when the player has nothing to offer, so the wait can end early
    /// rather than run out the clock on an answer that is not coming.
    declined: bool,
}

impl StreamLink {
    fn record(&self, fault: StreamFault) {
        if let Ok(mut state) = self.0.lock() {
            state.fault.get_or_insert(fault);
        }
    }

    /// Stands in for a downloader that hit something, so the player's
    /// end-of-track decisions can be tested without a network.
    #[cfg(test)]
    pub fn record_for_test(&self, fault: StreamFault) {
        self.record(fault);
    }

    /// The fault this stream died of, if it died of one.
    pub fn fault(&self) -> Option<StreamFault> {
        self.0.lock().ok().and_then(|state| state.fault)
    }

    /// The offset a stalled downloader is waiting on a fresh URL for.
    ///
    /// Read by the player on its tick. `None` means nothing is waiting.
    pub fn wants_url(&self) -> Option<u64> {
        self.0.lock().ok().and_then(|state| state.wanted)
    }

    /// Hands a freshly resolved URL to the downloader.
    ///
    /// Does not require anything to be waiting. A replacement can be armed
    /// ahead of the refusal it answers -- which is the point when the resolve
    /// path already knows the URL in use is capped, because then the swap costs
    /// nothing at all: the downloader finds its answer waiting the instant it
    /// asks, instead of spending seconds of the buffer's cushion on a resolve.
    pub fn supply_url(&self, url: &str) -> bool {
        let Ok(parsed) = url.parse::<reqwest::Url>() else {
            self.decline();
            return false;
        };
        if let Ok(mut state) = self.0.lock() {
            state.supplied = Some(parsed);
            state.declined = false;
            return true;
        }
        false
    }

    /// Tells the waiting downloader that no new URL is coming.
    pub fn decline(&self) {
        if let Ok(mut state) = self.0.lock() {
            state.declined = true;
        }
    }

    /// Registers that the downloader is stuck at `offset` and needs a URL.
    ///
    /// A URL armed in advance is deliberately left in place: clearing it here
    /// would throw away the replacement that was resolved precisely so this
    /// moment would cost nothing.
    fn request_url(&self, offset: u64) {
        if let Ok(mut state) = self.0.lock() {
            state.wanted = Some(offset);
            state.declined = false;
        }
    }

    /// Picks up a supplied URL, if one has arrived, and ends the request.
    fn take_url(&self) -> Option<reqwest::Url> {
        let mut state = self.0.lock().ok()?;
        let url = state.supplied.take()?;
        state.wanted = None;
        Some(url)
    }

    fn declined(&self) -> bool {
        self.0.lock().is_ok_and(|state| state.declined)
    }

    /// Ends a request that nothing answered.
    fn give_up(&self) {
        if let Ok(mut state) = self.0.lock() {
            state.wanted = None;
        }
    }

    /// Asks the player for a fresh URL and waits for it.
    ///
    /// Runs on the downloader task, so the wait is spent draining the ring
    /// buffer rather than blocking anything the user can hear -- there are
    /// about thirty seconds of cushion and this gives up after ten.
    async fn refresh(&self, offset: u64) -> Option<reqwest::Url> {
        // A replacement armed before the refusal, which is the case the resolve
        // path sets up when it already knows this URL is capped. Answered here
        // without a wait and without troubling the player at all.
        if let Some(ready) = self.take_url() {
            return Some(ready);
        }

        self.request_url(offset);

        let deadline = tokio::time::Instant::now() + REFRESH_WAIT;
        while tokio::time::Instant::now() < deadline {
            if let Some(url) = self.take_url() {
                return Some(url);
            }
            if self.declined() {
                break;
            }
            tokio::time::sleep(REFRESH_POLL).await;
        }

        self.give_up();
        None
    }
}

/// Whether a failed chunk is worth asking for again.
///
/// A timeout, a dropped connection or a 5xx is the network having a bad moment
/// and is retried. A 403 is googlevideo declining to serve this byte range to
/// this URL, which is a fact about the URL and not about the moment -- asking
/// again gets the same answer, and the URL has to be replaced instead.
fn is_transient(status: Option<StatusCode>, error: &reqwest::Error) -> bool {
    match status {
        Some(status) => transient_status(status),
        // No response at all: a connect failure, a timeout, or a body that
        // stopped part way. All worth one more try.
        None => error.is_timeout() || error.is_connect() || error.is_request(),
    }
}

/// The half of [`is_transient`] that depends only on what the server said.
///
/// Split out because it is the half worth testing: a `reqwest::Error` cannot be
/// constructed to order, and the status is where the interesting distinction
/// lives anyway.
fn transient_status(status: StatusCode) -> bool {
    status.is_server_error() || status == StatusCode::REQUEST_TIMEOUT
}

/// Failure to start reading a chunked response.
#[derive(Debug)]
pub struct ChunkedError(String);

impl fmt::Display for ChunkedError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Error for ChunkedError {}
impl DecodeError for ChunkedError {}

#[derive(Clone)]
pub struct ChunkedClient {
    inner: reqwest::Client,
    /// Where a refusal is written down. Shared with the caller, which is the
    /// only way the reason survives: everything between here and the player
    /// turns a failed read into an ordinary end of stream.
    link: StreamLink,
}

impl ChunkedClient {
    /// Builds a client that talks to the player through `link`.
    ///
    /// Used instead of [`Client::create`] -- which the trait gives no way to
    /// pass anything to -- by opening the stream through `HttpStream::new`.
    pub fn with_link(link: StreamLink) -> Self {
        Self {
            inner: build_client(),
            link,
        }
    }
}

/// The one HTTP client on the audio path.
///
/// Timeouts are not optional here, whatever they are elsewhere. The decoder
/// reads this buffer from the audio callback thread, so a socket that hangs
/// does not degrade playback, it holds the thread playback comes out of. The
/// values are generous against the ~6 MB/s these requests really run at:
/// anything slower than this has already failed, it just has not said so.
fn build_client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(REQUEST_TIMEOUT)
        .build()
        // Only fails if the TLS backend cannot be initialised, and by this
        // point several other clients in the process have already built one.
        .unwrap_or_else(|_| reqwest::Client::new())
}

pub struct ChunkedResponse {
    client: reqwest::Client,
    url: reqwest::Url,
    /// Shared with the client that produced this, and through it with the
    /// player. See [`StreamLink`].
    link: StreamLink,
    /// The first chunk's response. Its body is the head of the stream, so the
    /// request that discovered the resource size is not wasted.
    first: reqwest::Response,
    /// Total size of the whole resource, not of this chunk.
    total: Option<u64>,
    /// Offset `first`'s body begins at.
    start: u64,
    /// Whether the server honoured our `Range`. False means `first` is already
    /// the entire remainder and no further chunks are issued.
    partial: bool,
    /// Exclusive end, when the caller asked for a bounded range.
    limit: Option<u64>,
}

impl ChunkedClient {
    async fn fetch(
        &self,
        url: &reqwest::Url,
        start: u64,
        limit: Option<u64>,
    ) -> Result<ChunkedResponse, reqwest::Error> {
        let end = chunk_end(start, limit);
        let first = self
            .inner
            .get(url.clone())
            .header(RANGE, format!("bytes={start}-{end}"))
            .send()
            .await?;

        // 206 means the range was honoured and more chunks follow. Anything
        // else (typically 200) means this one response is the whole thing.
        let partial = first.status() == StatusCode::PARTIAL_CONTENT;
        let total = if partial {
            parse_total(first.headers())
        } else {
            first.content_length()
        };

        Ok(ChunkedResponse {
            client: self.inner.clone(),
            url: url.clone(),
            link: self.link.clone(),
            total,
            start,
            partial,
            limit,
            first,
        })
    }
}

impl Client for ChunkedClient {
    type Url = reqwest::Url;
    type Response = ChunkedResponse;
    type Error = reqwest::Error;
    type Headers = HeaderMap;

    fn create() -> Self {
        Self {
            inner: build_client(),
            link: StreamLink::default(),
        }
    }

    async fn get(&self, url: &Self::Url) -> Result<Self::Response, Self::Error> {
        self.fetch(url, 0, None).await
    }

    async fn get_range(
        &self,
        url: &Self::Url,
        start: u64,
        end: Option<u64>,
    ) -> Result<Self::Response, Self::Error> {
        // The trait's `end` is inclusive; ours is exclusive.
        self.fetch(url, start, end.map(|e| e + 1)).await
    }
}

impl ClientResponse for ChunkedResponse {
    type ResponseError = ChunkedError;
    type StreamError = reqwest::Error;
    type Headers = HeaderMap;

    fn content_length(&self) -> Option<u64> {
        self.total
    }

    fn content_type(&self) -> Option<&str> {
        self.first
            .headers()
            .get(reqwest::header::CONTENT_TYPE)?
            .to_str()
            .ok()
    }

    fn headers(&self) -> Self::Headers {
        self.first.headers().clone()
    }

    fn into_result(self) -> Result<Self, Self::ResponseError> {
        let status = self.first.status();
        if status.is_success() {
            Ok(self)
        } else {
            Err(ChunkedError(format!("chunk request failed: HTTP {status}")))
        }
    }

    fn stream(
        self,
    ) -> Box<dyn Stream<Item = Result<Bytes, Self::StreamError>> + Unpin + Send + Sync> {
        let Self {
            client,
            url,
            link,
            first,
            start,
            partial,
            limit,
            ..
        } = self;

        let head = first.bytes_stream();
        if !partial {
            // Server ignored the range; this one body is everything.
            return Box::new(Box::pin(head));
        }

        // The next chunk resumes from what the head *delivered*, never from the
        // end of the range that was asked for. A server may answer a 256 KB
        // range with fewer bytes, and a body can drop part way through. Either
        // way, resuming at the requested end does not leave a gap -- it splices
        // the missing bytes out of the file, because everything that arrives is
        // written contiguously. Nothing above this layer can see that: the AAC
        // bitstream simply stops decoding a few frames later, and the track ends
        // mid-song with no error anywhere.
        let delivered = Arc::new(AtomicU64::new(start));
        let broken = Arc::new(AtomicBool::new(false));

        let head = head.inspect({
            let delivered = Arc::clone(&delivered);
            let broken = Arc::clone(&broken);
            move |item| match item {
                Ok(bytes) => {
                    delivered.fetch_add(bytes.len() as u64, Ordering::Relaxed);
                }
                Err(_) => broken.store(true, Ordering::Relaxed),
            }
        });

        let rest = stream::unfold(
            Cursor {
                client,
                url,
                link,
                next: None,
                resume: Some(delivered),
                broken,
                limit,
            },
            |mut cursor| async move {
                // First time through, pick up wherever the head stopped. It has
                // finished by then: `chain` does not poll us any earlier.
                if let Some(resume) = cursor.resume.take() {
                    if cursor.broken.load(Ordering::Relaxed) {
                        // The head's body failed part way through. Ending here
                        // leaves the shortfall as a real gap, which the layer
                        // above re-requests from the position it actually wrote
                        // to. Carrying on would hide it instead.
                        return None;
                    }
                    cursor.next = Some(resume.load(Ordering::Relaxed));
                }

                let start = cursor.next?;
                if cursor.limit.is_some_and(|end| start >= end) {
                    return None;
                }

                let end = chunk_end(start, cursor.limit);
                let mut outcome = fetch_chunk(&cursor.client, &cursor.url, start, end).await;

                // A refusal is usually about the URL rather than the bytes: a
                // signature that has lapsed, or a node that will not serve this
                // one. So the answer is a new signature for the same file and
                // the *same* range asked again -- which costs no re-decode and
                // leaves no gap, unlike recovering higher up by winding a fresh
                // stream forward from byte zero.
                //
                // Asked once per refusal, not in a loop. Whoever answers has
                // already run the whole resolve cascade, so a second ask gets
                // the same answer from the same tiers and spends another slice
                // of the ring buffer's cushion arriving at it.
                if let Chunk::Failed { status, .. } = &outcome {
                    let status = *status;
                    cursor.link.record(StreamFault {
                        status,
                        offset: start,
                    });
                    if let Some(fresh) = cursor.link.refresh(start).await {
                        cursor.url = fresh;
                        outcome = fetch_chunk(&cursor.client, &cursor.url, start, end).await;
                    }
                }

                match outcome {
                    Chunk::End => None,
                    Chunk::Data(bytes) => {
                        cursor.next = Some(start + bytes.len() as u64);
                        Some((Ok(bytes), cursor))
                    }
                    // Nothing left to try. The fault is already recorded above,
                    // which is what lets the player tell this from a song that
                    // simply ended.
                    Chunk::Failed { error, .. } => {
                        cursor.next = None;
                        Some((Err(error), cursor))
                    }
                }
            },
        );

        Box::new(Box::pin(head.chain(rest)))
    }
}

/// What one chunk request settled on.
enum Chunk {
    /// Bytes to hand up.
    Data(Bytes),
    /// The resource ends here. Not a fault.
    End,
    /// Every attempt failed. `status` is what the server said, when it said
    /// anything.
    Failed {
        error: reqwest::Error,
        status: Option<StatusCode>,
    },
}

/// Fetches one chunk, retrying only what is worth retrying.
///
/// The distinction is the point of this function. Before it, any failure at all
/// ended the download outright: a connection reset four minutes into a track
/// was as fatal as a 403, and neither was ever asked again. A reset is the
/// network having a bad moment and costs one round trip to recover from; a 403
/// is googlevideo declining to serve this range to this URL, and no number of
/// retries changes that -- it needs a different URL, which is a decision made
/// well above here.
async fn fetch_chunk(client: &reqwest::Client, url: &reqwest::Url, start: u64, end: u64) -> Chunk {
    let mut backoff = RETRY_BACKOFF;

    for attempt in 0..CHUNK_ATTEMPTS {
        let sent = client
            .get(url.clone())
            .header(RANGE, format!("bytes={start}-{end}"))
            .send()
            .await;

        let outcome = match sent {
            Err(e) => Err((None, e)),
            Ok(response) => {
                // Asking past the end is how we discover the end when the
                // length was never advertised. It is a clean stop, not a fault.
                if response.status() == StatusCode::RANGE_NOT_SATISFIABLE {
                    return Chunk::End;
                }

                let status = response.status();
                // Any other refusal is a failure, and is neither data nor an
                // end. What comes back is an error page: handing its body on
                // would splice it into the AAC bitstream, and taking an empty
                // one for the end of the stream truncates the track without a
                // word -- which is how a 403 four chunks in became a song that
                // stopped at 1:05 with nothing reported anywhere.
                match response.error_for_status() {
                    Err(e) => Err((Some(status), e)),
                    Ok(response) => match response.bytes().await {
                        // A body that stopped part way through. The status was
                        // fine, so the fault is in transport, not in the URL.
                        Err(e) => Err((None, e)),
                        Ok(bytes) if bytes.is_empty() => return Chunk::End,
                        Ok(bytes) => Ok(bytes),
                    },
                }
            }
        };

        match outcome {
            Ok(bytes) => return Chunk::Data(bytes),
            Err((status, error)) => {
                let last = attempt + 1 == CHUNK_ATTEMPTS;
                if last || !is_transient(status, &error) {
                    return Chunk::Failed { error, status };
                }
                // Cheap against a ring buffer that holds ~30s, and the whole
                // budget here is under a second.
                tokio::time::sleep(backoff).await;
                backoff *= 2;
            }
        }
    }

    unreachable!("the loop returns on its final attempt")
}

/// Walking state for the chunk sequence.
struct Cursor {
    client: reqwest::Client,
    url: reqwest::Url,
    /// Where a refusal is written down, for the player to read after the
    /// decoder has gone quiet. See [`StreamLink`].
    link: StreamLink,
    /// Offset the next chunk starts at. `None` ends the sequence.
    next: Option<u64>,
    /// How far the head got, taken on the first iteration. It cannot be read
    /// any earlier -- the head has not finished arriving until then.
    resume: Option<Arc<AtomicU64>>,
    /// Set when the head's body failed part way through.
    broken: Arc<AtomicBool>,
    limit: Option<u64>,
}

/// Inclusive end offset of the chunk beginning at `start`.
fn chunk_end(start: u64, limit: Option<u64>) -> u64 {
    let end = start.saturating_add(CHUNK_BYTES - 1);
    match limit {
        Some(limit) => end.min(limit.saturating_sub(1)),
        None => end,
    }
}

/// Pulls the total resource size out of `Content-Range: bytes 0-255/3449447`.
///
/// The part after the slash is the whole resource, which is what the layer
/// above means by content length -- this chunk's own length is not useful to it
/// and reporting it would make the stream look complete after one chunk.
fn parse_total(headers: &HeaderMap) -> Option<u64> {
    headers
        .get(CONTENT_RANGE)?
        .to_str()
        .ok()?
        .rsplit('/')
        .next()?
        .trim()
        .parse()
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers_with(value: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_RANGE, value.parse().expect("valid header value"));
        headers
    }

    #[test]
    fn reads_total_size_from_content_range() {
        assert_eq!(
            parse_total(&headers_with("bytes 0-262143/3449447")),
            Some(3_449_447)
        );
    }

    #[test]
    fn unknown_total_is_none() {
        // A live stream has no final size, and the header says so.
        assert_eq!(parse_total(&headers_with("bytes 0-262143/*")), None);
        assert_eq!(parse_total(&HeaderMap::new()), None);
    }

    #[test]
    fn chunks_are_capped_at_the_chunk_size() {
        assert_eq!(chunk_end(0, None), CHUNK_BYTES - 1);
        assert_eq!(chunk_end(CHUNK_BYTES, None), CHUNK_BYTES * 2 - 1);
    }

    /// The first fault is the one that explains the silence; the ones after it
    /// are consequences of it, and reporting the last would name the wrong
    /// offset in the message the user is shown.
    #[test]
    fn the_first_fault_is_the_one_kept() {
        let link = StreamLink::default();
        assert_eq!(link.fault(), None);

        link.record(StreamFault {
            status: Some(StatusCode::FORBIDDEN),
            offset: 1024 * 1024,
        });
        link.record(StreamFault {
            status: None,
            offset: 2 * 1024 * 1024,
        });

        let fault = link.fault().expect("a fault was recorded");
        assert_eq!(fault.offset, 1024 * 1024);
        assert_eq!(fault.status, Some(StatusCode::FORBIDDEN));
    }

    /// The handshake the recovery rests on: the downloader says where it is
    /// stuck, the player answers with a freshly signed URL, and the same byte
    /// range is asked again.
    #[test]
    fn a_supplied_url_answers_the_request() {
        let link = StreamLink::default();
        assert_eq!(link.wants_url(), None, "nothing is stuck yet");

        link.request_url(1024 * 1024);
        assert_eq!(link.wants_url(), Some(1024 * 1024));
        assert!(link.take_url().is_none(), "nothing supplied yet");

        assert!(link.supply_url("https://example.com/next.m4a"));
        let url = link.take_url().expect("the supplied URL should arrive");
        assert_eq!(url.as_str(), "https://example.com/next.m4a");
        // Taking it ends the request, so the player's next tick does not ask
        // the app to resolve the same thing all over again.
        assert_eq!(link.wants_url(), None);
    }

    /// A replacement can be armed before the refusal it answers, which is what
    /// the resolve path does when it already knows the URL in use is capped.
    /// Registering the request must not throw that answer away -- doing so
    /// would spend seconds of the buffer's cushion resolving again something
    /// already in hand.
    #[test]
    fn a_url_armed_in_advance_survives_the_request_it_answers() {
        let link = StreamLink::default();
        assert!(link.supply_url("https://example.com/ready.m4a"));

        // The downloader hits its refusal only now, after the answer landed.
        link.request_url(1024 * 1024);

        let url = link
            .take_url()
            .expect("the armed URL should still be there");
        assert_eq!(url.as_str(), "https://example.com/ready.m4a");
        assert_eq!(link.wants_url(), None);
    }

    /// A URL that will not parse is no answer at all, and must not be reported
    /// as one -- the downloader would wait out its whole timeout for a second
    /// answer nobody is going to send.
    #[test]
    fn an_unusable_url_declines_rather_than_answering() {
        let link = StreamLink::default();
        link.request_url(0);

        assert!(!link.supply_url("not a url"));
        assert!(link.declined());
        assert!(link.take_url().is_none());
    }

    /// Nothing left to try. The wait ends early rather than running out the
    /// clock on an answer that has already come back empty.
    #[test]
    fn a_declined_request_is_visible_to_the_waiter() {
        let link = StreamLink::default();
        link.request_url(512);
        assert!(!link.declined());

        link.decline();
        assert!(link.declined());
    }

    #[test]
    fn a_fault_names_the_status_and_the_offset() {
        assert_eq!(
            StreamFault {
                status: Some(StatusCode::FORBIDDEN),
                offset: 1024 * 1024,
            }
            .to_string(),
            "HTTP 403 at 1.0 MiB"
        );
        // No response ever arrived, so there is no status to name.
        assert_eq!(
            StreamFault {
                status: None,
                offset: 3 * 1024 * 1024 / 2,
            }
            .to_string(),
            "connection lost at 1.5 MiB"
        );
    }

    /// A refusal is a fact about the URL and asking again does not change it;
    /// a reset or a timeout is the network having a bad moment and costs one
    /// round trip to recover from. Before this distinction both ended the
    /// download outright.
    #[test]
    fn only_transient_faults_are_worth_retrying() {
        for status in [
            StatusCode::INTERNAL_SERVER_ERROR,
            StatusCode::BAD_GATEWAY,
            StatusCode::SERVICE_UNAVAILABLE,
            StatusCode::REQUEST_TIMEOUT,
        ] {
            assert!(transient_status(status), "{status} is worth asking again");
        }

        // 403 is the one that matters: it is what a lapsed signature and a
        // capped URL both answer with, and retrying it just spends the ring
        // buffer's cushion on two more refusals.
        for status in [
            StatusCode::FORBIDDEN,
            StatusCode::NOT_FOUND,
            StatusCode::UNAUTHORIZED,
            StatusCode::GONE,
        ] {
            assert!(
                !transient_status(status),
                "{status} will say the same thing next time"
            );
        }
    }

    #[test]
    fn chunks_stop_at_a_requested_limit() {
        // A bounded range must not be overshot: the limit is exclusive, so the
        // last byte fetched is limit - 1.
        assert_eq!(chunk_end(0, Some(1000)), 999);
        // A limit past the chunk size does not widen the chunk.
        assert_eq!(chunk_end(0, Some(CHUNK_BYTES * 4)), CHUNK_BYTES - 1);
    }
}

#[cfg(test)]
mod probe {
    use super::*;

    /// Prints the response headers googlevideo answers a range request with.
    ///
    /// `cargo test --release probe_range_headers -- --ignored --nocapture`
    #[test]
    #[ignore = "hits the network"]
    fn probe_range_headers() {
        let url = std::env::var("MTUI_STREAM_URL").unwrap_or_else(|_| {
            let tube = crate::source::innertube::InnerTube::new().ok();
            let yt = crate::source::youtube::YouTube::default();
            crate::source::resolve_stream(&yt, tube.as_ref(), "dQw4w9WgXcQ")
                .expect("resolve failed")
                .0
                .url
        });

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");

        rt.block_on(async {
            let client = reqwest::Client::new();
            for (label, range) in [("ranged", Some("bytes=0-262143")), ("plain", None)] {
                let mut req = client.get(&url);
                if let Some(r) = range {
                    req = req.header(RANGE, r);
                }
                let resp = req.send().await.expect("request failed");
                println!("--- {label}: HTTP {}", resp.status());
                for (k, v) in resp.headers() {
                    println!("    {k}: {}", v.to_str().unwrap_or("<binary>"));
                }
            }
        });
    }
}
