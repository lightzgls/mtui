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
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use bytes::Bytes;
use futures_util::{Stream, StreamExt, stream};
use reqwest::StatusCode;
use reqwest::header::{CONTENT_RANGE, HeaderMap, RANGE};
use stream_download::http::{Client, ClientResponse};
use stream_download::source::DecodeError;

/// Bytes per range request.
///
/// Half the ring buffer, so a chunk can never arrive larger than the space
/// waiting for it. Large enough that per-request overhead is irrelevant at the
/// speeds this unlocks, small enough that the first one lands quickly -- which
/// is the number that decides time-to-first-sound.
const CHUNK_BYTES: u64 = 256 * 1024;

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
}

pub struct ChunkedResponse {
    client: reqwest::Client,
    url: reqwest::Url,
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
            inner: reqwest::Client::new(),
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
                let sent = cursor
                    .client
                    .get(cursor.url.clone())
                    .header(RANGE, format!("bytes={start}-{end}"))
                    .send()
                    .await;

                let response = match sent {
                    Ok(response) => response,
                    Err(e) => {
                        cursor.next = None;
                        return Some((Err(e), cursor));
                    }
                };

                // Asking past the end is how we discover the end when the
                // length was never advertised. It is a clean stop, not a fault.
                if response.status() == StatusCode::RANGE_NOT_SATISFIABLE {
                    return None;
                }

                // Any other refusal is a failure, and is neither data nor an
                // end. What comes back is an error page: handing its body on
                // would splice it into the AAC bitstream, and taking an empty
                // one for the end of the stream truncates the track without a
                // word -- which is how a 403 four chunks in became a song that
                // stopped at 1:05 with nothing reported anywhere. Reporting it
                // instead leaves a gap the layer above re-requests, and a
                // failure it can surface if that does not work either.
                let response = match response.error_for_status() {
                    Ok(response) => response,
                    Err(e) => {
                        cursor.next = None;
                        return Some((Err(e), cursor));
                    }
                };

                match response.bytes().await {
                    Err(e) => {
                        cursor.next = None;
                        Some((Err(e), cursor))
                    }
                    Ok(bytes) if bytes.is_empty() => None,
                    Ok(bytes) => {
                        cursor.next = Some(start + bytes.len() as u64);
                        Some((Ok(bytes), cursor))
                    }
                }
            },
        );

        Box::new(Box::pin(head.chain(rest)))
    }
}

/// Walking state for the chunk sequence.
struct Cursor {
    client: reqwest::Client,
    url: reqwest::Url,
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
            let tube = crate::source::innertube::InnerTube::new().expect("client should build");
            tube.resolve("dQw4w9WgXcQ").expect("resolve failed").url
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

