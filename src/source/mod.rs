//! Music sources.
//!
//! Search and library modules turn provider responses into tracks. Playback URL
//! resolution lives in `mtui-resolver`, while the worker here decides when that
//! potentially blocking work may run.

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

use std::time::Duration;

pub use mtui_resolver::StreamUrl;

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
    fn saved_session_exposes_its_music_quality() {
        let yt = super::bootstrap::locate().expect("yt-dlp should be available");
        let cookies = crate::config::Cookies::available()
            .expect("saved cookies should parse")
            .expect("no saved browser session is available");
        let mut resolver = mtui_resolver::Resolver::new(yt.bin()).expect("resolver should build");
        resolver.set_session(Some(mtui_resolver::PlaybackSession::new(
            cookies.header().to_string(),
            cookies.sapisid().to_string(),
        )));

        let stream = resolver
            .resolve(mtui_resolver::ResolveRequest::new("nNN88hijp-o"))
            .expect("the Music track should resolve");
        println!(
            "authenticated source {:?}, itag {:?}",
            stream.source, stream.format.itag
        );
        assert_eq!(
            stream.format.itag,
            Some(141),
            "the saved account did not expose 256 kbps AAC"
        );
    }
}
