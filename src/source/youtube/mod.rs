//! YouTube source, backed by short-lived `yt-dlp` invocations.
//!
//! yt-dlp is Python and, since v2025.11.12, additionally needs a JavaScript
//! runtime (Deno/Node/Bun) to solve YouTube's nsig/PO-token challenges. That
//! costs ~80 MB while it runs, which is why it is never kept alive: it converts
//! a video id into a signed `googlevideo.com` URL, prints it, and exits. All
//! subsequent streaming and decoding happens in-process.

pub(crate) mod account;
pub(crate) mod bootstrap;
pub(crate) mod innertube;
pub(crate) mod watch;

use std::process::Stdio;

use anyhow::{Context, Result, bail};

use crate::source::command;

/// Hard ceiling on search results. Bounded by construction so a long session
/// cannot grow the heap.
pub const MAX_RESULTS: usize = 200;

/// Cloneable because it is only a path: the library thread runs the same binary
/// to read browser cookies, and locating a second copy for that would be absurd.
#[derive(Clone)]
pub struct YouTube {
    /// Path to the binary, so a user with a non-PATH install can point at it.
    bin: String,
    /// yt-dlp spelling, for example `deno:C:\path\deno.exe`.
    js_runtime: Option<String>,
    /// Directory containing the provider plugin zip, for `--plugin-dirs`.
    pot_plugin_dir: Option<String>,
    /// Provider checkout's `server` directory, for script mode.
    pot_server_home: Option<String>,
}

impl Default for YouTube {
    fn default() -> Self {
        Self::new("yt-dlp")
    }
}

impl YouTube {
    pub fn new(bin: impl Into<String>) -> Self {
        Self {
            bin: bin.into(),
            js_runtime: None,
            pot_plugin_dir: None,
            pot_server_home: None,
        }
    }

    pub fn with_js_runtime(mut self, runtime: Option<String>) -> Self {
        self.js_runtime = runtime;
        self
    }

    pub(super) fn with_pot_provider(mut self, plugin_dir: String, server_home: String) -> Self {
        self.pot_plugin_dir = Some(plugin_dir);
        self.pot_server_home = Some(server_home);
        self
    }

    /// The configured yt-dlp executable.
    pub(super) fn bin(&self) -> &str {
        &self.bin
    }

    pub(super) fn js_runtime(&self) -> Option<&str> {
        self.js_runtime.as_deref()
    }

    pub(super) fn pot_plugin_dir(&self) -> Option<&str> {
        self.pot_plugin_dir.as_deref()
    }

    pub(super) fn pot_server_home(&self) -> Option<&str> {
        self.pot_server_home.as_deref()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_runtime_is_carried_to_every_yt_dlp_call() {
        let yt = YouTube::new("yt-dlp").with_js_runtime(Some("deno:/tmp/deno".to_string()));
        assert_eq!(yt.js_runtime(), Some("deno:/tmp/deno"));
    }

    #[test]
    fn provider_paths_are_carried_with_clones() {
        let yt = YouTube::new("yt-dlp")
            .with_pot_provider("/tmp/plugins".to_string(), "/tmp/server".to_string());
        let clone = yt.clone();
        assert_eq!(clone.pot_plugin_dir(), Some("/tmp/plugins"));
        assert_eq!(clone.pot_server_home(), Some("/tmp/server"));
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
        use crate::source::Track;
        use std::time::Duration;

        let t = |secs: Option<u64>| Track {
            id: "x".into(),
            title: "t".into(),
            uploader: "u".into(),
            duration: secs.map(Duration::from_secs),
            album: None,
            artist_ref: None,
        };
        assert_eq!(t(Some(65)).duration_str(), "1:05");
        assert_eq!(t(Some(3725)).duration_str(), "1:02:05");
        assert_eq!(t(None).duration_str(), "LIVE");
    }
}
