//! YouTube source, backed by short-lived `yt-dlp` invocations.
//!
//! yt-dlp is Python and, since v2025.11.12, additionally needs a JavaScript
//! runtime (Deno/Node/Bun) to solve YouTube's nsig/PO-token challenges. That
//! costs ~80 MB while it runs, which is why it is never kept alive: it converts
//! a video id into a signed `googlevideo.com` URL, prints it, and exits. All
//! subsequent streaming and decoding happens in-process.

use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, bail};

use super::{Track, UNKNOWN_ARTIST, command};

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

/// Cloneable because it is only a path: the library thread runs the same binary
/// to read browser cookies, and locating a second copy for that would be absurd.
#[derive(Clone)]
pub struct YouTube {
    /// Path to the binary, so a user with a non-PATH install can point at it.
    bin: String,
    /// yt-dlp spelling, for example `deno:C:\path\deno.exe`.
    js_runtime: Option<String>,
}

impl Default for YouTube {
    fn default() -> Self {
        Self {
            bin: "yt-dlp".to_string(),
            js_runtime: None,
        }
    }
}

impl YouTube {
    pub fn new(bin: impl Into<String>) -> Self {
        Self {
            bin: bin.into(),
            js_runtime: None,
        }
    }

    pub fn with_js_runtime(mut self, runtime: Option<String>) -> Self {
        self.js_runtime = runtime;
        self
    }

    /// The binary, lent to [`crate::source::browser`].
    ///
    /// Reading cookies out of a browser is not a yt-dlp call in the sense the
    /// rest of this file means -- nothing is searched for or resolved -- but it
    /// is the same binary, and locating a second copy of it to run one more
    /// flag against would be absurd.
    pub(super) fn bin(&self) -> &str {
        &self.bin
    }

    pub(super) fn js_runtime(&self) -> Option<&str> {
        self.js_runtime.as_deref()
    }

    /// Verifies yt-dlp is present and returns its version.
    ///
    /// Called once at startup so a missing dependency surfaces as a clear
    /// message instead of a confusing failure on the first play.
    pub fn version(&self) -> Result<String> {
        let out = command(&self.bin)
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

        let mut command = command(&self.bin);
        if let Some(runtime) = &self.js_runtime {
            command.args(["--no-js-runtimes", "--js-runtimes", runtime]);
        }
        let out = command
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
                uploader: e.uploader.unwrap_or_else(|| UNKNOWN_ARTIST.to_string()),
                duration: e.duration.filter(|d| *d > 0.0).map(Duration::from_secs_f64),
                // A flat search listing carries no album; only the music
                // corpus knows one.
                album: None,
                artist_ref: None,
                // Search results are not in a playlist.
                playlist_item_id: None,
                id: e.id,
            })
            .take(limit)
            .collect();

        Ok(tracks)
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

/// yt-dlp writes multi-line diagnostics; the first ERROR line is the useful
/// one. Falls back to the last non-empty line so we never report nothing.
fn first_error_line(stderr: &[u8]) -> String {
    let text = String::from_utf8_lossy(stderr);
    let line = text
        .lines()
        .find(|l| l.contains("ERROR"))
        .or_else(|| text.lines().rev().find(|l| !l.trim().is_empty()))
        .unwrap_or("no error output")
        .trim();
    strip_preamble(line).to_string()
}

/// Drops yt-dlp's `ERROR: [youtube] <id>: ` preamble, leaving the sentence.
///
/// Every part of it is already known here: the caller names the id it asked
/// about, and the extractor is always the same one. What it costs is the front
/// half of a line that has to fit a status bar beside the key hints -- so the
/// preamble survives and the half that says what happened is what gets cut.
fn strip_preamble(line: &str) -> &str {
    let line = line.strip_prefix("ERROR: ").unwrap_or(line);
    // `[youtube]`, or whichever extractor answered.
    let line = match line.strip_prefix('[') {
        Some(rest) => rest.split_once("] ").map_or(line, |(_, tail)| tail),
        None => line,
    };
    // Then the id, which is this call's own argument coming back. Checked
    // rather than assumed: the messages themselves contain colons, and a
    // failure that names no id must not lose its first clause to this.
    match line.split_once(": ") {
        Some((head, tail)) if is_video_id(head) => tail,
        _ => line,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_runtime_is_carried_to_every_yt_dlp_call() {
        let yt = YouTube::new("yt-dlp").with_js_runtime(Some("deno:/tmp/deno".to_string()));
        assert_eq!(yt.js_runtime(), Some("deno:/tmp/deno"));
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
        let Ok(out) = command("yt-dlp").arg("--help").output() else {
            eprintln!("skipping: yt-dlp not installed");
            return;
        };
        let help = String::from_utf8_lossy(&out.stdout);

        // Non-flag arguments (values like "urls", or the extractor-args
        // string itself) are skipped.
        let flags = SEARCH_FLAGS.iter().filter(|a| a.starts_with("--"));

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
    fn an_error_line_keeps_only_what_the_caller_does_not_already_know() {
        let stderr = b"WARNING: something noisy\n\
                       ERROR: [youtube] GwSSrwryxN0: Video unavailable. This video is restricted.\n"
            as &[u8];
        assert_eq!(
            first_error_line(stderr),
            "Video unavailable. This video is restricted."
        );
    }

    #[test]
    fn a_message_that_names_no_id_keeps_its_first_clause() {
        // "Unable to download webpage" reads as a sentence with a colon in it,
        // not as a preamble. Cutting at the first colon would throw away the
        // half that says what failed.
        let stderr = b"ERROR: unable to download webpage: timed out\n" as &[u8];
        assert_eq!(
            first_error_line(stderr),
            "unable to download webpage: timed out"
        );
    }

    #[test]
    fn nothing_on_stderr_still_reports_something() {
        assert_eq!(first_error_line(b""), "no error output");
    }

    #[test]
    fn formats_durations() {
        let t = |secs: Option<u64>| Track {
            id: "x".into(),
            title: "t".into(),
            uploader: "u".into(),
            duration: secs.map(Duration::from_secs),
            album: None,
            artist_ref: None,
            playlist_item_id: None,
        };
        assert_eq!(t(Some(65)).duration_str(), "1:05");
        assert_eq!(t(Some(3725)).duration_str(), "1:02:05");
        assert_eq!(t(None).duration_str(), "LIVE");
    }
}
