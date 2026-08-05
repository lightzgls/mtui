//! Reading the YouTube session out of whichever browser has one.
//!
//! A cookie is the only credential InnerTube accepts -- OAuth is refused
//! outright, and not merely the user's own client: a token issued to Google's
//! living-room TV client is refused identically, which
//! [`crate::source::home`]'s `tv_client_oauth_against_innertube` pins. So the
//! question is not how to avoid needing a cookie. It is how to stop the user
//! having to fetch one by hand every few weeks.
//!
//! They do not have to, because yt-dlp is already on disk for entirely
//! unrelated reasons -- see [`crate::source::bootstrap`] -- and it already
//! knows how to read a cookie jar out of every major browser, including the
//! platform-specific decryption that makes doing so awkward: DPAPI on Windows,
//! Keychain on macOS, kwallet or gnome-keyring on Linux. None of that has to be
//! reimplemented here. This spawns the binary that already does it and parses
//! what comes back.
//!
//! ## Which browsers actually work
//!
//! Every browser below is *tried*. Not all of them can succeed, and the reason
//! is not something this program controls:
//!
//! - **Firefox** works everywhere. Its jar is a plain SQLite file.
//! - **Chrome, Edge and most Chromium forks** stopped working on Windows at
//!   Chrome 127, which introduced App-Bound Encryption -- cookies are sealed
//!   with a key tied to the browser executable, specifically so that other
//!   programs cannot read them. That is a deliberate anti-exfiltration measure
//!   and it works; yt-dlp cannot get past it either.
//! - **macOS and Linux** are fine across the board, though a keychain may
//!   prompt the user the first time.
//!
//! So the list is tried in full and the first browser that yields a signing
//! cookie wins. A browser that is not installed fails in milliseconds, which is
//! what keeps probing the whole list affordable.

use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

use super::youtube::YouTube;
use crate::config::Cookies;

/// Everything yt-dlp can read, most-likely-to-work first.
///
/// Firefox leads because it is the one that works on every platform. The rest
/// follow in rough order of how many people use them; on Windows the Chromium
/// family will mostly fail, and fails fast enough that having tried costs
/// little.
pub const BROWSERS: [&str; 9] = [
    "firefox",
    "chrome",
    "edge",
    "brave",
    "chromium",
    "vivaldi",
    "opera",
    "safari",
    "whale",
];

/// yt-dlp insists on a URL even when the only thing wanted is the cookie jar.
/// Which one hardly matters -- the run is simulated, so nothing is downloaded
/// and no format is chosen. This is a stable, uncontroversial video that has
/// been up for over a decade.
const ANCHOR_URL: &str = "https://music.youtube.com/watch?v=dQw4w9WgXcQ";

/// Bounds the network half of the probe. The whole point of this path is that
/// it runs rarely and unattended; what it must never do is hang the library
/// thread behind a browser that will not answer.
const SOCKET_TIMEOUT: &str = "10";

/// Reads a session out of the first browser that has one.
///
/// Returns the browser's name alongside the cookies, so the caller can record
/// which one worked and go straight to it next time -- probing the whole list
/// costs a process spawn per browser, and [`crate::source::bootstrap`] is not
/// the only place where yt-dlp's ~1.3 s start-up is the dominant cost.
///
/// The error names every browser tried and why each declined, because "no
/// cookies found" on its own is the least actionable message this program could
/// produce: the user's next move is entirely different depending on whether
/// their browser is unsupported, not signed in, or sealed by App-Bound
/// Encryption.
pub fn detect(yt: &YouTube) -> Result<(String, Cookies)> {
    let mut refused = Vec::new();

    for browser in BROWSERS {
        match extract(yt, browser) {
            Ok(cookies) => return Ok((browser.to_string(), cookies)),
            Err(why) => refused.push(format!("  {browser:<9} {why}")),
        }
    }

    bail!(
        "no browser on this machine has a YouTube session that could be read:\n{}\n\
         Sign in to YouTube in Firefox, or paste a cookie header into cookies.txt \
         by hand -- see the README.",
        refused.join("\n")
    )
}

/// Reads the session out of one named browser.
///
/// Every failure here is ordinary. A browser that is not installed, one with no
/// YouTube cookies, and one whose jar cannot be decrypted are all the same kind
/// of answer: not this one, try the next.
pub fn extract(yt: &YouTube, browser: &str) -> Result<Cookies> {
    let jar = tempfile(browser)?;
    // Removed before the run, not just after: a stale jar from an earlier
    // attempt would otherwise be parsed as though this attempt had produced it.
    let _ = std::fs::remove_file(&jar);

    let out = Command::new(yt.bin())
        .arg("--cookies-from-browser")
        .arg(browser)
        // yt-dlp reads cookies from this path *and dumps the jar back into it*,
        // which is the whole mechanism -- there is no "export cookies" flag.
        .arg("--cookies")
        .arg(&jar)
        .args([
            "--simulate",
            "--skip-download",
            "--socket-timeout",
            SOCKET_TIMEOUT,
            "--no-warnings",
            // The user's own yt-dlp config must not be able to change what this
            // does; it is being run as a library call, not on their behalf.
            "--ignore-config",
            ANCHOR_URL,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .with_context(|| format!("could not run {} to read {browser}", yt.bin()))?;

    let read = std::fs::read_to_string(&jar);
    // Whatever happened, the jar does not stay on disk: it is a full account
    // credential in a world-readable temporary directory, and it has already
    // been read into memory by the line above.
    let _ = std::fs::remove_file(&jar);

    let Ok(text) = read else {
        // yt-dlp wrote no jar at all, so its own complaint is the useful one.
        bail!("{}", explain(&out.stderr));
    };

    let header = header_from(&text)
        .context("the jar held no youtube.com cookies -- not signed in to YouTube there")?;
    Cookies::from_header(&header)
        .context("the jar has youtube.com cookies but no SAPISID -- signed out, or a partial session")
}

/// Where the jar is written while it is being read.
///
/// The system temporary directory, because it is short-lived and deleted either
/// way; named for the browser so two probes cannot collide, and for the process
/// so two copies of MTUI cannot either.
fn tempfile(browser: &str) -> Result<std::path::PathBuf> {
    Ok(std::env::temp_dir().join(format!("mtui-cookies-{browser}-{}.txt", std::process::id())))
}

/// Turns a Netscape cookie jar into the `Cookie:` header a browser would send
/// to music.youtube.com.
///
/// The format is tab-separated and older than most of what uses it: domain, a
/// subdomain flag, path, a secure flag, an expiry, then the name and value.
/// Comment lines start with `#`, except that `#HttpOnly_` is a prefix on the
/// domain rather than a comment -- and those cookies are exactly the ones that
/// matter here, since the session cookies are all HttpOnly.
fn header_from(jar: &str) -> Option<String> {
    let pairs: Vec<String> = jar
        .lines()
        .filter_map(|line| {
            let line = line.strip_prefix("#HttpOnly_").unwrap_or(line);
            if line.starts_with('#') || line.trim().is_empty() {
                return None;
            }

            let mut fields = line.split('\t');
            let domain = fields.next()?;
            // Skip the subdomain flag, path, secure flag and expiry to reach
            // the pair. Positional because the format has no header row.
            let name = fields.nth(4)?;
            let value = fields.next()?;

            // Only what would travel to music.youtube.com. A jar holds every
            // site the user has visited, and sending Google's cookies for
            // unrelated hosts would be both wrong and a needless disclosure.
            sends_to_youtube(domain).then(|| format!("{name}={value}"))
        })
        .collect();

    (!pairs.is_empty()).then(|| pairs.join("; "))
}

/// Whether a cookie scoped to `domain` would be sent to music.youtube.com.
///
/// Deliberately not a general cookie-domain implementation: the only host this
/// program signs requests for is music.youtube.com, and matching more broadly
/// would mean copying cookies for other Google properties into a header that
/// has no business carrying them.
fn sends_to_youtube(domain: &str) -> bool {
    let domain = domain.trim_start_matches('.');
    domain == "youtube.com" || domain.ends_with(".youtube.com")
}

/// The first line of yt-dlp's complaint, tidied for a one-line report.
///
/// yt-dlp prefixes its errors with `ERROR:` and often follows them with a
/// paragraph of advice aimed at someone running it from a shell, which is not
/// where this is being run from.
fn explain(stderr: &[u8]) -> String {
    String::from_utf8_lossy(stderr)
        .lines()
        .find(|line| line.contains("ERROR") || line.contains("error"))
        .map(|line| {
            line.trim()
                .trim_start_matches("ERROR:")
                .trim()
                .to_string()
        })
        .unwrap_or_else(|| "gave nothing back".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A jar in the shape yt-dlp writes them.
    const JAR: &str = "\
# Netscape HTTP Cookie File
# This file is generated by yt-dlp.

#HttpOnly_.youtube.com\tTRUE\t/\tTRUE\t1799999999\tSAPISID\tsecret-value
#HttpOnly_.youtube.com\tTRUE\t/\tTRUE\t1799999999\tSID\tsid-value
.youtube.com\tTRUE\t/\tTRUE\t1799999999\tPREF\tf6=40000000
.google.com\tTRUE\t/\tTRUE\t1799999999\tNID\tunrelated
.github.com\tTRUE\t/\tTRUE\t1799999999\tsession\tnothing-to-do-with-us
";

    #[test]
    fn reads_the_session_out_of_a_jar() {
        let header = header_from(JAR).expect("the jar holds youtube cookies");
        assert!(header.contains("SAPISID=secret-value"));
        assert!(header.contains("SID=sid-value"));
        assert!(header.contains("PREF=f6=40000000"), "a value containing = must survive");

        // And it is a header a signing path can actually use.
        let cookies = Cookies::from_header(&header).expect("SAPISID should be found");
        assert_eq!(cookies.sapisid(), "secret-value");
    }

    #[test]
    fn httponly_cookies_are_not_mistaken_for_comments() {
        // The session cookies are all HttpOnly, so reading `#HttpOnly_` as a
        // comment would silently drop every cookie that matters and leave a
        // header of nothing but preferences.
        let header = header_from(JAR).unwrap();
        assert!(header.contains("SAPISID"), "HttpOnly cookies were dropped");
    }

    #[test]
    fn only_youtube_cookies_travel() {
        let header = header_from(JAR).unwrap();
        assert!(!header.contains("NID"), "a google.com cookie leaked into the header");
        assert!(!header.contains("nothing-to-do-with-us"), "an unrelated site leaked");
    }

    #[test]
    fn domain_matching_does_not_overreach() {
        assert!(sends_to_youtube(".youtube.com"));
        assert!(sends_to_youtube("youtube.com"));
        assert!(sends_to_youtube("music.youtube.com"));
        assert!(sends_to_youtube(".www.youtube.com"));

        assert!(!sends_to_youtube(".google.com"));
        // The one that a naive `contains` or `ends_with` on the bare string
        // would get wrong, and which is exactly how a session leaks somewhere
        // it was never scoped for.
        assert!(!sends_to_youtube("notyoutube.com"));
        assert!(!sends_to_youtube("youtube.com.evil.test"));
    }

    #[test]
    fn a_jar_with_nothing_relevant_is_no_jar_at_all() {
        assert!(header_from("").is_none());
        assert!(header_from("# just a comment\n\n").is_none());
        assert!(
            header_from(".github.com\tTRUE\t/\tTRUE\t1799999999\tsession\tx").is_none(),
            "a jar with no youtube cookies should not produce a header"
        );
    }

    #[test]
    fn a_truncated_line_is_skipped_rather_than_panicking() {
        // A jar half-written by an interrupted run, which is a file this code
        // can genuinely be handed.
        let jar = format!("{JAR}.youtube.com\tTRUE\t/\tTRUE");
        let header = header_from(&jar).expect("the intact lines should still parse");
        assert!(header.contains("SAPISID"));
    }

    #[test]
    fn yt_dlp_errors_are_reduced_to_one_line() {
        let stderr = b"WARNING: something\nERROR: could not find firefox cookies database\n  hint: blah";
        assert_eq!(explain(stderr), "could not find firefox cookies database");
        assert_eq!(explain(b""), "gave nothing back");
    }

    /// The whole chain: read a browser, save the import, and check that what
    /// came out actually buys the personalised feed.
    ///
    /// Extracting a cookie and having YouTube *honour* it are different claims,
    /// and only the second one is worth anything. A jar with a SAPISID in it
    /// still fails if the session is signed out, scoped to the wrong account,
    /// or missing a cookie the signing path needs.
    ///
    /// Writes the import to the config directory, which is exactly what the
    /// program does on first launch -- so running this leaves the machine in
    /// the state a successful launch would.
    ///
    /// `cargo test import_end_to_end -- --ignored --nocapture`
    #[test]
    #[ignore = "reads the real browser profiles and saves the session it finds"]
    fn import_end_to_end() {
        use crate::config::Import;
        use crate::source::auth::Http;
        use crate::source::home;

        let yt = crate::source::bootstrap::locate().expect("yt-dlp should be available");
        let (browser, cookies) = match detect(&yt) {
            Ok(found) => found,
            Err(why) => {
                println!("{why}");
                return;
            }
        };
        println!("  read the session from {browser}");

        Import {
            browser: browser.clone(),
            header: cookies.header().to_string(),
            at: crate::source::sapisid::unix_now(),
        }
        .save()
        .expect("the import should save");
        println!("  saved");

        let http = Http::new().expect("client should build");
        let shelves = home::fetch(&http, Some(&cookies)).expect("the feed should come back");
        println!("\n  {} shelves:", shelves.len());
        for shelf in &shelves {
            println!("    {}", shelf.title);
        }

        // The signed-out feed is two shelves of playlists. Anything YouTube
        // built from this account's own listening is the proof that the cookie
        // was not merely well-formed but recognised.
        let personal = shelves.iter().any(|shelf| {
            ["Listen again", "Quick picks", "Heard in Shorts", "Mixed for you"]
                .iter()
                .any(|name| shelf.title.starts_with(name))
        });
        assert!(
            personal,
            "the cookie was read and accepted but the feed is not personalised -- \
             the browser is probably signed out of YouTube"
        );
        println!("\n  PERSONALISED -- the browser session works end to end");
    }

    /// Whether a session can actually be read out of a browser on this machine.
    ///
    /// Ignored because it reads the user's real browser profile, which is not
    /// something a plain `cargo test` should do. Prints which browsers answered
    /// and which declined, and never prints a cookie value.
    ///
    /// `cargo test browser_cookies -- --ignored --nocapture`
    #[test]
    #[ignore = "reads the real browser profiles on this machine"]
    fn browser_cookies_on_this_machine() {
        let yt = crate::source::bootstrap::locate().expect("yt-dlp should be available");

        for browser in BROWSERS {
            match extract(&yt, browser) {
                // Deliberately reports only the length. The value is a full
                // account credential and this prints to a terminal.
                Ok(cookies) => println!(
                    "  {browser:<9} OK -- {} cookies, SAPISID present",
                    cookies.header().split(';').count()
                ),
                Err(why) => println!("  {browser:<9} -- {why}"),
            }
        }
    }
}
