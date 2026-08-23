//! Music sources.
//!
//! Search and library modules turn provider responses into tracks. Playback URL
//! resolution lives in `mtui-resolver`, while the worker here decides when that
//! potentially blocking work may run.

pub mod artist;
pub mod auth;
pub mod bootstrap;
#[cfg(test)]
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

use std::ffi::OsStr;
use std::process::Command;
use std::time::Duration;

pub use mtui_resolver::StreamUrl;

/// A command-line child that must never acquire a console of its own.
pub(super) fn command(program: impl AsRef<OsStr>) -> Command {
    #[allow(unused_mut)]
    let mut command = Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;

        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    command
}

/// Compatibility entry point for network diagnostics and ignored player tests.
/// Production playback keeps one [`mtui_resolver::Resolver`] in the source
/// worker so its connection pool and cache survive between tracks.
#[cfg(test)]
pub fn resolve_stream(
    yt: &youtube::YouTube,
    _tube: Option<&innertube::InnerTube>,
    video_id: &str,
) -> anyhow::Result<(StreamUrl, bool)> {
    let mut resolver = mtui_resolver::Resolver::new(yt.bin())?;
    resolver.set_js_runtime(yt.js_runtime().map(str::to_string));
    if let Some(cookies) = crate::config::Cookies::available()? {
        resolver.set_session(Some(mtui_resolver::PlaybackSession::new(
            cookies.header().to_string(),
            cookies.sapisid().to_string(),
        )));
    }
    let stream = resolver.resolve(mtui_resolver::ResolveRequest::new(video_id))?;
    Ok((stream, true))
}

/// Stand-in when a source cannot name the artist. Shared so that the places
/// which write it and the places which suppress it cannot drift apart.
pub const UNKNOWN_ARTIST: &str = "unknown";

/// A YouTube Music browse route, including the opaque parameters some pages
/// require in addition to their id.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct BrowseEndpoint {
    pub browse_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<String>,
}

impl BrowseEndpoint {
    pub fn new(browse_id: impl Into<String>) -> Self {
        Self {
            browse_id: browse_id.into(),
            params: None,
        }
    }
}

/// A named, stable route to an artist page.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ArtistRef {
    pub name: String,
    pub endpoint: BrowseEndpoint,
}

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
    /// Canonical Music artist route when the source exposed one. A display
    /// name alone is never promoted into a route because channel names are not
    /// stable artist identities.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artist_ref: Option<ArtistRef>,
    /// Identifies this row within one playlist. Only playlist listings have it.
    #[serde(default)]
    pub playlist_item_id: Option<String>,
}

impl Track {
    /// One-line description, as the player shows it while the track runs.
    pub fn label(&self) -> String {
        if self.uploader.is_empty() || self.uploader == UNKNOWN_ARTIST {
            self.title.clone()
        } else {
            format!("{} — {}", self.title, self.uploader)
        }
    }

    /// `H:MM:SS` / `M:SS`, or `LIVE` when the duration is unknown.
    pub fn duration_str(&self) -> String {
        let Some(duration) = self.duration else {
            return "LIVE".to_string();
        };
        let total = duration.as_secs();
        let (hours, minutes, seconds) = (total / 3600, (total % 3600) / 60, total % 60);
        if hours > 0 {
            format!("{hours}:{minutes:02}:{seconds:02}")
        } else {
            format!("{minutes}:{seconds:02}")
        }
    }
}

#[cfg(test)]
mod live_quality {
    #[test]
    #[ignore = "uses the saved YouTube Music browser session"]
    fn saved_session_resolves_a_complete_stream_quickly() {
        use std::time::{Duration, Instant};

        let yt = super::bootstrap::locate().expect("yt-dlp should be available");
        let cookies = crate::config::Cookies::available()
            .expect("saved cookies should parse")
            .expect("no saved browser session is available");
        let mut resolver = mtui_resolver::Resolver::new(yt.bin()).expect("resolver should build");
        resolver.set_js_runtime(yt.js_runtime().map(str::to_string));
        resolver.set_session(Some(mtui_resolver::PlaybackSession::new(
            cookies.header().to_string(),
            cookies.sapisid().to_string(),
        )));

        let started = Instant::now();
        let stream = resolver
            .resolve(mtui_resolver::ResolveRequest::new("nNN88hijp-o"))
            .expect("the Music track should resolve");
        let elapsed = started.elapsed();
        println!(
            "authenticated source {:?}, itag {:?}, {:.2}s",
            stream.source,
            stream.format.itag,
            elapsed.as_secs_f64()
        );
        assert!(
            elapsed < Duration::from_secs(15),
            "resolving held playback for {elapsed:.2?}"
        );
        assert!(matches!(stream.format.itag, Some(141 | 140 | 18)));
    }
}
