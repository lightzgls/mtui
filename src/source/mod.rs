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
