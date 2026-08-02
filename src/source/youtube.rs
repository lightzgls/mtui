//! YouTube source, backed by short-lived `yt-dlp` invocations.
//!
//! yt-dlp is Python and, since v2025.11.12, additionally needs a JavaScript
//! runtime (Deno/Node/Bun) to solve YouTube's nsig/PO-token challenges. That
//! costs ~80 MB while it runs, which is why it is never kept alive: it converts
//! a video id into a signed `googlevideo.com` URL, prints it, and exits. All
//! subsequent streaming and decoding happens in-process.

use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};

use super::{StreamUrl, Track};

/// itag 140: AAC-LC in an MP4 container, ~130 kbps.
///
/// Chosen over itag 251 (Opus/WebM) because symphonia has no Opus decoder --
/// `symphonia-codec-opus` is an unimplemented stub. Opus would mean linking
/// libopus through `audiopus`, reintroducing a C toolchain dependency on both
/// platforms. AAC-LC decodes with pure Rust and is rated "Great" upstream.
const AUDIO_FORMAT: &str = "140/bestaudio[ext=m4a]";

/// Hard ceiling on search results. Bounded by construction so a long session
/// cannot grow the heap.
pub const MAX_RESULTS: usize = 200;

/// Flags for a search: list the results without visiting each video page.
const SEARCH_FLAGS: &[&str] = &[
    "--flat-playlist",
    "--dump-json",
    "--no-warnings",
    "--ignore-config",
];

/// Flags for resolving one video to a stream URL.
///
/// `--print urls` rather than the shorter `-g`: `-g` is an undocumented
/// youtube-dl legacy alias that does not appear in `yt-dlp --help` at all, so
/// it could be dropped without notice. `--print` is documented and produces an
/// identical URL. `--format` is passed separately since it takes an argument.
const RESOLVE_FLAGS: &[&str] = &["--print", "urls", "--no-warnings", "--ignore-config"];

/// Shape of the `--dump-json` fields we consume. yt-dlp emits far more; serde
/// drops the rest rather than allocating it.
#[derive(serde::Deserialize)]
struct YtDlpEntry {
    id: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    uploader: Option<String>,
    #[serde(default)]
    duration: Option<f64>,
}

pub struct YouTube {
    /// Path to the binary, so a user with a non-PATH install can point at it.
    bin: String,
}

impl Default for YouTube {
    fn default() -> Self {
        Self {
            bin: "yt-dlp".to_string(),
        }
    }
}

impl YouTube {
    pub fn new(bin: impl Into<String>) -> Self {
        Self { bin: bin.into() }
    }

    /// Verifies yt-dlp is present and returns its version.
    ///
    /// Called once at startup so a missing dependency surfaces as a clear
    /// message instead of a confusing failure on the first play.
    pub fn version(&self) -> Result<String> {
        let out = Command::new(&self.bin)
            .arg("--version")
            .stdin(Stdio::null())
            .output()
            .with_context(|| {
                format!(
                    "could not run `{}`. Install yt-dlp and make sure it is on PATH \
                     (winget install yt-dlp.yt-dlp)",
                    self.bin
                )
            })?;
        if !out.status.success() {
            bail!("`{} --version` failed", self.bin);
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    }

    /// Searches YouTube, returning at most `limit` (capped at [`MAX_RESULTS`]).
    ///
    /// Uses `--flat-playlist` so yt-dlp only reads the search listing and never
    /// visits each video page -- one process, not N, and no JS challenge solve.
    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<Track>> {
        let limit = limit.min(MAX_RESULTS);
        if query.trim().is_empty() {
            return Ok(Vec::new());
        }

        let out = Command::new(&self.bin)
            .args(SEARCH_FLAGS)
            .arg(format!("ytsearch{limit}:{query}"))
            .stdin(Stdio::null())
            .stderr(Stdio::piped())
            .output()
            .with_context(|| format!("failed to run `{}` for search", self.bin))?;

        if !out.status.success() {
            bail!("search failed: {}", first_error_line(&out.stderr));
        }

        // One JSON object per line. A malformed line is skipped rather than
        // failing the whole search -- yt-dlp occasionally emits entries for
        // deleted videos that lack the fields we need.
        let stdout = String::from_utf8_lossy(&out.stdout);
        let tracks = stdout
            .lines()
            .filter(|l| !l.trim().is_empty())
            .filter_map(|line| serde_json::from_str::<YtDlpEntry>(line).ok())
            .map(|e| Track {
                title: e.title.unwrap_or_else(|| "(untitled)".to_string()),
                uploader: e.uploader.unwrap_or_else(|| "unknown".to_string()),
                duration: e.duration.filter(|d| *d > 0.0).map(Duration::from_secs_f64),
                // A flat search listing carries no album; only the music
                // corpus knows one.
                album: None,
                // Search results are not in a playlist.
                playlist_item_id: None,
                id: e.id,
            })
            .take(limit)
            .collect();

        Ok(tracks)
    }

    /// Resolves a video id to a signed, directly streamable audio URL.
    ///
    /// This is the expensive call (~2-4 s, ~80 MB transient). Callers should
    /// consult the URL cache first and prefetch the next queue item during
    /// playback so this cost lands off the critical path.
    pub fn resolve(&self, video_id: &str) -> Result<StreamUrl> {
        let out = Command::new(&self.bin)
            .arg("--format")
            .arg(AUDIO_FORMAT)
            .args(RESOLVE_FLAGS)
            .arg(format!("https://www.youtube.com/watch?v={video_id}"))
            .stdin(Stdio::null())
            .stderr(Stdio::piped())
            .output()
            .with_context(|| format!("failed to run `{}` to resolve {video_id}", self.bin))?;

        if !out.status.success() {
            bail!(
                "could not resolve {video_id}: {}",
                first_error_line(&out.stderr)
            );
        }

        let url = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if url.is_empty() {
            bail!("yt-dlp returned no stream URL for {video_id}");
        }

        Ok(StreamUrl {
            expires_at: parse_expiry(&url),
            url,
        })
    }
}

/// Pulls an 11-character video id out of a YouTube URL, or accepts a bare id.
///
/// Lets the search box double as an address bar: a pasted link plays directly
/// instead of being searched for as literal text. `None` means "treat this as
/// search terms".
pub fn extract_video_id(input: &str) -> Option<String> {
    let input = input.trim();

    if is_video_id(input) {
        return Some(input.to_string());
    }
    if !input.starts_with("http://") && !input.starts_with("https://") {
        return None;
    }

    // watch?v=<id>, plus the youtu.be / shorts / embed path forms.
    let candidate = input
        .split(['?', '&'])
        .find_map(|kv| kv.strip_prefix("v="))
        .or_else(|| {
            input
                .rsplit('/')
                .find(|seg| !seg.is_empty())
                .map(|seg| seg.split('?').next().unwrap_or(seg))
        })?;

    is_video_id(candidate).then(|| candidate.to_string())
}

fn is_video_id(s: &str) -> bool {
    s.len() == 11
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Extracts the `expire=<unix_ts>` query parameter YouTube signs into playback
/// URLs. Absent or unparseable means "unknown", which the cache treats as
/// always-valid; a stale URL then surfaces as an ordinary playback error.
pub(super) fn parse_expiry(url: &str) -> Option<SystemTime> {
    let secs: u64 = url
        .split(['?', '&'])
        .find_map(|kv| kv.strip_prefix("expire="))?
        .parse()
        .ok()?;
    Some(UNIX_EPOCH + Duration::from_secs(secs))
}

/// yt-dlp writes multi-line diagnostics; the first ERROR line is the useful
/// one. Falls back to the last non-empty line so we never report nothing.
fn first_error_line(stderr: &[u8]) -> String {
    let text = String::from_utf8_lossy(stderr);
    text.lines()
        .find(|l| l.contains("ERROR"))
        .or_else(|| text.lines().rev().find(|l| !l.trim().is_empty()))
        .unwrap_or("no error output")
        .trim()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_expiry_from_signed_url() {
        let url = "https://rr11.googlevideo.com/videoplayback?expire=1785600392&ei=abc";
        let ts = parse_expiry(url).expect("expire= should parse");
        assert_eq!(ts, UNIX_EPOCH + Duration::from_secs(1785600392));
    }

    #[test]
    fn missing_expiry_is_none() {
        assert!(parse_expiry("https://example.com/a.m4a").is_none());
        // `expire` must be the whole key, not a suffix of another one.
        assert!(parse_expiry("https://example.com/a?noexpire=123").is_none());
    }

    #[test]
    fn url_without_expiry_is_valid() {
        let s = StreamUrl {
            url: "https://example.com/a.m4a".into(),
            expires_at: None,
        };
        assert!(s.is_valid());
    }

    #[test]
    fn url_inside_safety_margin_is_invalid() {
        // Expires in one minute: inside the 5-minute margin, so unusable.
        let soon = SystemTime::now() + Duration::from_secs(60);
        let s = StreamUrl {
            url: "https://example.com/a.m4a".into(),
            expires_at: Some(soon),
        };
        assert!(!s.is_valid());

        let later = SystemTime::now() + Duration::from_secs(3600);
        let s = StreamUrl {
            url: "https://example.com/a.m4a".into(),
            expires_at: Some(later),
        };
        assert!(s.is_valid());
    }

    /// Guards against passing a flag yt-dlp does not have.
    ///
    /// This exact class of bug shipped once already (`--no-playlist-metafiles`,
    /// which does not exist -- the real option is `--no-write-playlist-metafiles`),
    /// and it is invisible until a user triggers a search. `--help` needs no
    /// network. Skips rather than fails when yt-dlp is absent, so the suite
    /// still runs on a machine without it.
    #[test]
    fn flags_are_accepted_by_yt_dlp() {
        let Ok(out) = Command::new("yt-dlp").arg("--help").output() else {
            eprintln!("skipping: yt-dlp not installed");
            return;
        };
        let help = String::from_utf8_lossy(&out.stdout);

        // Non-flag arguments (values like "urls") are skipped.
        let flags = SEARCH_FLAGS
            .iter()
            .chain(RESOLVE_FLAGS)
            .chain(["--format"].iter())
            .filter(|a| a.starts_with("--"));

        for flag in flags {
            assert!(
                help.contains(flag),
                "yt-dlp does not document {flag}; check `yt-dlp --help` for the real name"
            );
        }
    }

    #[test]
    fn extracts_id_from_url_forms() {
        let expected = Some("dQw4w9WgXcQ".to_string());
        assert_eq!(
            extract_video_id("https://www.youtube.com/watch?v=dQw4w9WgXcQ"),
            expected
        );
        assert_eq!(extract_video_id("https://youtu.be/dQw4w9WgXcQ"), expected);
        assert_eq!(
            extract_video_id("https://www.youtube.com/shorts/dQw4w9WgXcQ"),
            expected
        );
        assert_eq!(
            extract_video_id("https://youtu.be/dQw4w9WgXcQ?t=42"),
            expected
        );
        // A bare id is accepted as-is.
        assert_eq!(extract_video_id("dQw4w9WgXcQ"), expected);
    }

    #[test]
    fn treats_non_urls_as_search_terms() {
        assert_eq!(extract_video_id("daft punk around the world"), None);
        assert_eq!(extract_video_id("short"), None);
        assert_eq!(extract_video_id("hello world"), None);
    }

    #[test]
    fn formats_durations() {
        let t = |secs: Option<u64>| Track {
            id: "x".into(),
            title: "t".into(),
            uploader: "u".into(),
            duration: secs.map(Duration::from_secs),
            album: None,
            playlist_item_id: None,
        };
        assert_eq!(t(Some(65)).duration_str(), "1:05");
        assert_eq!(t(Some(3725)).duration_str(), "1:02:05");
        assert_eq!(t(None).duration_str(), "LIVE");
    }
}
