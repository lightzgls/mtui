//! Music sources.
//!
//! A source turns a user's intent ("play this") into a plain HTTPS URL that the
//! player can stream. Nothing here stays resident: resolvers spawn a child
//! process, read its stdout, and let it exit. That containment is the whole
//! reason the steady-state footprint stays in single-digit megabytes.

pub mod auth;
pub mod bootstrap;
pub mod browser;
pub mod cover;
pub mod home;
pub mod innertube;
pub mod journal;
pub mod library;
pub mod lrclib;
pub mod sapisid;
pub mod stats;
pub mod watch;
pub mod worker;
pub mod youtube;

#[cfg(feature = "spotify")]
pub mod spotify;

use std::time::{Duration, SystemTime};

/// Stand-in when a source cannot name the artist. Shared so that the places
/// which write it and the places which suppress it cannot drift apart.
pub const UNKNOWN_ARTIST: &str = "unknown";

/// A track as shown in search results. Deliberately small and owned -- these
/// are held in bounded `Vec`s and dropped as soon as the user moves on.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Track {
    /// Source-scoped id. For YouTube this is the 11-character video id.
    pub id: String,
    pub title: String,
    /// The performing artist when the source knows one, otherwise whoever
    /// uploaded it -- which for a plain YouTube search is often not the artist.
    pub uploader: String,
    /// `None` for livestreams.
    pub duration: Option<Duration>,
    /// Album, when the source knows one. YouTube Music supplies it; a plain
    /// YouTube search cannot, and singles genuinely have none.
    pub album: Option<String>,
    /// Identifies this row *within one playlist*, which is not the video id --
    /// the same video sitting in two playlists has two of these, and a playlist
    /// may legitimately hold the same video twice. Only a playlist listing
    /// knows one, and removing a row is the only thing that needs it.
    #[serde(default)]
    pub playlist_item_id: Option<String>,
}

impl Track {
    /// One-line description, as the player shows it while the track runs.
    ///
    /// The artist is dropped rather than shown as "unknown" when the source
    /// could not name one -- a bare title reads better than a wrong one.
    pub fn label(&self) -> String {
        if self.uploader.is_empty() || self.uploader == UNKNOWN_ARTIST {
            self.title.clone()
        } else {
            format!("{} — {}", self.title, self.uploader)
        }
    }

    /// `H:MM:SS` / `M:SS`, or `LIVE` when the duration is unknown.
    pub fn duration_str(&self) -> String {
        let Some(d) = self.duration else {
            return "LIVE".to_string();
        };
        let total = d.as_secs();
        let (h, m, s) = (total / 3600, (total % 3600) / 60, total % 60);
        if h > 0 {
            format!("{h}:{m:02}:{s:02}")
        } else {
            format!("{m}:{s:02}")
        }
    }
}

/// A resolved, directly streamable URL.
///
/// These are short-lived: YouTube signs them with an `expire` query parameter
/// roughly six hours out. `expires_at` is what lets the cache avoid handing a
/// dead URL to the player.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct StreamUrl {
    pub url: String,
    pub expires_at: Option<SystemTime>,
}

impl StreamUrl {
    /// Treats a URL within 5 minutes of expiry as already dead, so a track
    /// never dies mid-buffer. A URL with no known expiry is always usable.
    pub fn is_valid(&self) -> bool {
        const SAFETY_MARGIN: Duration = Duration::from_secs(300);
        match self.expires_at {
            Some(exp) => exp
                .duration_since(SystemTime::now())
                .is_ok_and(|left| left > SAFETY_MARGIN),
            None => true,
        }
    }
}

/// Bounded LRU of resolved URLs, keyed by track id.
///
/// Resolving spawns yt-dlp and costs ~3 s, while the URL it returns stays good
/// for hours. Caching turns a replay -- or a play the user was prefetched into
/// -- from a process spawn into a channel round trip. That is the single
/// largest latency win available here.
///
/// Capacity is fixed, so a long session cannot grow the heap. A signed
/// googlevideo URL runs ~2 KB, putting a full cache around 64 KB: an eighth of
/// the audio ring buffer, which is the budget this program is really spending.
pub struct UrlCache {
    /// Least-recent first. A linear scan over 32 entries is far cheaper than
    /// the allocation a map would need, and keeps the LRU order explicit.
    entries: Vec<(String, StreamUrl)>,
}

impl UrlCache {
    const CAPACITY: usize = 32;

    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    /// Returns a still-valid URL for `id`, promoting it to most-recent.
    ///
    /// Expired entries are dropped as they are found rather than swept on a
    /// timer -- an expired entry costs nothing until it is looked up.
    pub fn get(&mut self, id: &str) -> Option<StreamUrl> {
        let pos = self.entries.iter().position(|(k, _)| k == id)?;
        if !self.entries[pos].1.is_valid() {
            self.entries.remove(pos);
            return None;
        }
        let entry = self.entries.remove(pos);
        let url = entry.1.clone();
        self.entries.push(entry);
        Some(url)
    }

    /// Drops the entry for `id`, so the next lookup has to resolve again.
    ///
    /// Expiry alone is not enough to keep a bad URL out. A URL that googlevideo
    /// has stopped serving past some byte carries an `expire` six hours out and
    /// looks perfectly valid to [`StreamUrl::is_valid`], so a track that died
    /// mid-song was handed the very same URL on every replay for the rest of
    /// the session -- and then started working on its own once the signature
    /// lapsed, which is as confusing a bug as this program has had.
    pub fn invalidate(&mut self, id: &str) {
        if let Some(pos) = self.entries.iter().position(|(k, _)| k == id) {
            self.entries.remove(pos);
        }
    }

    /// Inserts as most-recent, evicting the least-recent entry when full.
    pub fn insert(&mut self, id: String, url: StreamUrl) {
        if let Some(pos) = self.entries.iter().position(|(k, _)| *k == id) {
            self.entries.remove(pos);
        } else if self.entries.len() >= Self::CAPACITY {
            self.entries.remove(0);
        }
        self.entries.push((id, url));
    }
}

impl Default for UrlCache {
    fn default() -> Self {
        Self::new()
    }
}

/// Where a capped URL stops being served. Measured, not documented: every
/// capped URL seen answers ranges below one mebibyte and 403s from there on.
pub const CAP_BYTES: u64 = 1024 * 1024;

/// Whether YouTube will serve all of `url`, or only the start of it.
///
/// A resolve that succeeds is not the same thing as a URL that plays. For some
/// tracks -- licensed music, in every case seen here -- the answer is a URL
/// that serves its first mebibyte and then refuses every range past it with a
/// 403. Nothing in the response says so, and the first mebibyte plays: about
/// sixty-five seconds at itag 140, and proportionally less on anything with a
/// higher bitrate -- roughly ten on itag 18, whose video track shares the byte
/// budget. By then the resolve is a minute in the past and what the user sees
/// is a track that stopped for no reason.
///
/// This lives here, rather than beside the player API that first needed it,
/// because the cap is a property of a signed googlevideo URL and not of the
/// route that produced one. Guarding a single tier let the others hand out
/// exactly the URLs it was written to catch.
///
/// One request for one byte on a pooled connection, against resolves that cost
/// a round trip at best and three seconds at worst.
pub fn serves_whole_file(
    runtime: &tokio::runtime::Runtime,
    client: &reqwest::Client,
    url: &str,
) -> bool {
    // The file's own last byte, which is the one question that settles it: a
    // URL that will serve that will serve everything before it.
    let last = match content_length(url) {
        // Under the cap there is nothing past it to be refused, so this costs
        // nothing and is skipped. Measured 2026-08-11: the ceiling sits exactly
        // at CAP_BYTES -- ranges below it are served and ranges above it are
        // refused -- so a file that ends first cannot be truncated by it.
        Some(clen) if clen <= CAP_BYTES => return true,
        Some(clen) => clen - 1,
        // No length to aim at, so ask at the cap itself. A file that ends
        // before it answers 416, which is not a refusal.
        None => CAP_BYTES,
    };

    runtime.block_on(serves_byte(client, url, last))
}

/// Whether the server will hand over the single byte at `at`.
///
/// A network failure is retried once before being taken for an answer.
/// Previously any transport error at all was read as "not capped", on the
/// reasoning that the stream would find out for itself -- but the stream finds
/// out by going silent thirty seconds into a song, which is the failure this
/// check exists to prevent. One retry keeps a passing hiccup from sending every
/// resolve down the slow path, without letting a capped URL through on one.
async fn serves_byte(client: &reqwest::Client, url: &str, at: u64) -> bool {
    for _ in 0..2 {
        let sent = client
            .get(url)
            .header(reqwest::header::RANGE, format!("bytes={at}-{at}"))
            .send()
            .await;
        if let Ok(response) = sent {
            return response.status() != reqwest::StatusCode::FORBIDDEN;
        }
    }
    // Never reached the server at all. That is not evidence of a cap, and
    // refusing here would send every resolve to yt-dlp on a passing hiccup.
    true
}

/// Resolves `id` to a URL that will actually play, by the cheapest route that
/// works. The cascade itself, with no cache in front of it.
///
/// Two tiers: YouTube's player API (~0.2 s), then yt-dlp (~4 s). A player API
/// failure is swallowed rather than reported -- it is expected for age-gated
/// and region-locked videos, and yt-dlp is about to answer the same question
/// properly. If yt-dlp also fails, *its* error is the one worth showing.
///
/// This lives here, rather than inside the worker that used to own it, because
/// it is the definition of "the URL this program would play" and more than one
/// caller needs exactly that. When the cap check lived inside
/// [`InnerTube::resolve`], every direct caller got it for free; moving the
/// check out to cover yt-dlp as well would silently have taken it away from
/// them. Sharing the cascade is what keeps the tests measuring the URL the app
/// really streams instead of one nothing ever plays.
///
/// Returns the URL and whether it is known to serve the whole file. A capped
/// URL is still returned: it plays its first minute or so, which beats
/// refusing the track outright, and the caller is told so it can decline to
/// cache what it must be able to ask for again.
pub fn resolve_stream(
    yt: &youtube::YouTube,
    tube: Option<&innertube::InnerTube>,
    id: &str,
) -> anyhow::Result<(StreamUrl, bool)> {
    // With no player API there is no pooled client to probe with either, and
    // building one for a single check would cost a TLS handshake per resolve.
    // Unchecked is what this has always been in that case.
    let serves_whole = |url: &str| tube.is_none_or(|t| t.serves_whole_file(url));

    // The fast path, taken only when what it returns will play to the end.
    if let Some(fast) = tube.and_then(|t| t.resolve(id).ok())
        && serves_whole(&fast.url)
    {
        return Ok((fast, true));
    }

    let slow = yt.resolve(id)?;
    let whole = serves_whole(&slow.url);
    Ok((slow, whole))
}

/// The file's size, which googlevideo states in the URL it signs as `clen`.
pub fn content_length(url: &str) -> Option<u64> {
    url.split(['?', '&'])
        .find_map(|kv| kv.strip_prefix("clen="))?
        .parse()
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(tag: &str) -> StreamUrl {
        StreamUrl {
            url: format!("https://example.com/{tag}.m4a"),
            expires_at: None,
        }
    }

    #[test]
    fn returns_what_was_inserted() {
        let mut cache = UrlCache::new();
        cache.insert("a".into(), url("a"));
        assert_eq!(cache.get("a").unwrap().url, "https://example.com/a.m4a");
        assert!(cache.get("b").is_none());
    }

    /// A URL that failed has to be forgettable before it expires.
    ///
    /// Expiry alone does not cover it: a URL googlevideo has stopped serving
    /// past some byte still carries an `expire` six hours out, so it stays
    /// perfectly valid by every test the cache applies and gets handed back on
    /// every replay. That is how one track stopped at the same second all
    /// session and then started working by itself the next morning.
    #[test]
    fn an_invalidated_entry_is_gone_before_it_expires() {
        let mut cache = UrlCache::new();
        cache.insert("a".into(), url("a"));
        cache.insert("b".into(), url("b"));

        cache.invalidate("a");
        assert!(cache.get("a").is_none(), "the failed URL must not come back");
        // Only the one named: a track that failed says nothing about the rest.
        assert!(cache.get("b").is_some());

        // Invalidating something absent is not an error -- a track can fail
        // before it was ever cached, which is exactly the capped-URL case.
        cache.invalidate("nothing");
    }

    #[test]
    fn expired_entries_are_dropped_on_lookup() {
        let mut cache = UrlCache::new();
        cache.insert(
            "a".into(),
            StreamUrl {
                url: "https://example.com/a.m4a".into(),
                // Inside the 5-minute safety margin, so already unusable.
                expires_at: Some(SystemTime::now() + Duration::from_secs(60)),
            },
        );
        assert!(cache.get("a").is_none());
        assert!(cache.entries.is_empty(), "expired entry should be evicted");
    }

    #[test]
    fn evicts_least_recently_used_when_full() {
        let mut cache = UrlCache::new();
        for i in 0..UrlCache::CAPACITY {
            cache.insert(i.to_string(), url(&i.to_string()));
        }
        // Touch the oldest so it is no longer the eviction candidate.
        assert!(cache.get("0").is_some());
        cache.insert("new".into(), url("new"));

        assert_eq!(cache.entries.len(), UrlCache::CAPACITY);
        assert!(cache.get("0").is_some(), "promoted entry should survive");
        assert!(cache.get("1").is_none(), "next-oldest should be evicted");
        assert!(cache.get("new").is_some());
    }

    #[test]
    fn reinserting_updates_without_growing() {
        let mut cache = UrlCache::new();
        cache.insert("a".into(), url("old"));
        cache.insert("a".into(), url("new"));
        assert_eq!(cache.entries.len(), 1);
        assert_eq!(cache.get("a").unwrap().url, "https://example.com/new.m4a");
    }
}
