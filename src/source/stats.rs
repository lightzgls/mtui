//! Telling YouTube that a track was played, so it lands in the real history.
//!
//! This is the other half of the sync. [`crate::source::journal`] records plays
//! locally, which is what MTUI's own shelves rank against; this reports them
//! upstream, which is what makes YouTube Music's "Listen again" -- on the phone,
//! on the web, everywhere else -- reflect what was played here.
//!
//! It works the only way it can: by doing what Google's own web player does.
//! There is no documented endpoint for "I played this". The web client fetches a
//! player response, reads two tracking URLs out of it, and pings them -- once
//! when playback starts, once with how long it ran. Both pings must carry the
//! same client playback nonce, and the whole exchange has to be signed with the
//! user's cookie or it is attributed to nobody.
//!
//! Consequences worth stating plainly:
//!
//! - **Cookie only.** No saved web session or `cookies.txt`, no immediate
//!   reporting. The caller keeps the play in a durable outbox until sign-in.
//! - **Undocumented and unversioned.** The shape below is what the web client
//!   sent when this was written. A failure never costs the local play and stays
//!   queued for a later authenticated attempt.
//! - **Not verifiable from here.** A ping that is accepted and a ping that is
//!   quietly discarded both come back `204`. The only real check is whether
//!   YouTube Music's history actually fills up, which is what the ignored test
//!   at the bottom is for.
//!
//! This writes to the user's real account, so it requires an explicit Music
//! sign-in and a YouTube account whose watch history is enabled.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::Value;

use super::http::Http;
use super::innertube::MUSIC_CLIENT_NAME;
use super::sapisid;
use crate::config::Cookies;

const ORIGIN: &str = "https://music.youtube.com";
const PLAYER_URL: &str = "https://music.youtube.com/youtubei/v1/player";

/// Below this, nothing is reported. The same threshold the journal calls a
/// play, so the two histories cannot disagree about what happened.
pub(super) const MIN_REPORTABLE: Duration = Duration::from_secs(30);

/// The tracking protocol version the web client sends. Both pings carry it.
const TRACKING_VERSION: &str = "2";
const TRACKING_CLIENT_PARAM: &str = "web_remix";
const TRACKING_FORMAT: &str = "251";

/// Identity and player-script timestamp currently published by YouTube Music's
/// web client. Unlike browsing, a player request is rejected as `UNPLAYABLE`
/// when this timestamp is zero, and that response contains no tracking URLs.
/// Kept local to reporting so a protocol repair cannot perturb search or Home.
const TRACKING_CLIENT_VERSION: &str = "1.20260830.16.00";
const SIGNATURE_TIMESTAMP: u64 = 20_684;

/// Length of a client playback nonce, and the alphabet it is drawn from. Both
/// are what the web player uses; a nonce of the wrong shape is the kind of
/// thing that gets a ping accepted and then ignored.
const CPN_LENGTH: usize = 16;
const CPN_ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

/// Reports one finished play.
///
/// `listened` is how much of the track was actually heard, not its length --
/// YouTube is told the truth about a song that was skipped halfway, the same
/// truth the journal records.
///
/// Two round trips on top of the play itself, which is why this is called from
/// the worker after the fact rather than anywhere near the audio path.
#[cfg(test)]
pub fn report(http: &Http, cookies: &Cookies, video_id: &str, listened: Duration) -> Result<()> {
    if listened < MIN_REPORTABLE {
        return Ok(());
    }

    let cpn = nonce();
    report_with_cpn(http, cookies, video_id, listened, &cpn)
}

/// Reports a queued play with the nonce assigned before its first attempt.
/// Reusing it makes a retry describe the same playback instead of inventing a
/// second one after a crash between YouTube accepting the ping and our local
/// acknowledgement reaching disk.
pub(super) fn report_with_cpn(
    http: &Http,
    cookies: &Cookies,
    video_id: &str,
    listened: Duration,
    cpn: &str,
) -> Result<()> {
    if listened < MIN_REPORTABLE {
        return Ok(());
    }

    let tracking = tracking_urls(http, cookies, video_id, cpn)?;

    // Start first, then duration. The order is the protocol: a watchtime ping
    // for a playback that was never announced has nothing to attach itself to.
    ping(http, cookies, &tracking.playback, cpn, None)?;
    ping(http, cookies, &tracking.watchtime, cpn, Some(listened))?;
    Ok(())
}

/// The two URLs a player response carries for reporting against this video.
struct Tracking {
    playback: String,
    watchtime: String,
}

/// Fetches a player response over the user's cookie and reads the tracking URLs
/// out of it.
///
/// Deliberately a second player call rather than a reuse of the one
/// [`crate::source::innertube`] makes to resolve the stream. That one is
/// anonymous and tries several client identities to get the best audio URL; a
/// play reported against an anonymous session is attributed to nobody, which
/// would be all of the cost of this file and none of the benefit.
fn tracking_urls(http: &Http, cookies: &Cookies, video_id: &str, cpn: &str) -> Result<Tracking> {
    let body = tracking_body(video_id, cpn);

    let now = sapisid::unix_now();
    let request = http
        .client()
        .post(PLAYER_URL)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .header(reqwest::header::ORIGIN, ORIGIN)
        .header(reqwest::header::COOKIE, cookies.header())
        .header(
            reqwest::header::AUTHORIZATION,
            sapisid::authorization(cookies.sapisid(), ORIGIN, now),
        )
        .body(serde_json::to_vec(&body).context("could not encode the player request")?);

    let (status, raw) = http.send(request)?;
    if !(200..300).contains(&status) {
        bail!("YouTube refused the player request: HTTP {status}");
    }
    parse_tracking(&serde_json::from_slice(&raw)?)
}

fn tracking_body(video_id: &str, cpn: &str) -> Value {
    serde_json::json!({
        "videoId": video_id,
        "context": {
            "client": {
                "clientName": MUSIC_CLIENT_NAME,
                "clientVersion": TRACKING_CLIENT_VERSION,
                "hl": "en",
            }
        },
        // Announces the nonce the pings will carry, and asks for a response
        // that includes tracking at all -- a player response fetched without
        // this comes back with no `playbackTracking` to read.
        "playbackContext": {
            "contentPlaybackContext": {
                "signatureTimestamp": SIGNATURE_TIMESTAMP,
                "referer": ORIGIN,
            }
        },
        "cpn": cpn,
    })
}

fn parse_tracking(json: &Value) -> Result<Tracking> {
    let base = |field: &str| -> Option<String> {
        json.pointer(&format!("/playbackTracking/{field}/baseUrl"))
            .and_then(Value::as_str)
            .map(str::to_string)
    };

    let (Some(playback), Some(watchtime)) = (
        base("videostatsPlaybackUrl"),
        base("videostatsWatchtimeUrl"),
    ) else {
        let status = json
            .pointer("/playabilityStatus/status")
            .and_then(Value::as_str)
            .unwrap_or("unknown status");
        let reason = json
            .pointer("/playabilityStatus/reason")
            .and_then(Value::as_str)
            .unwrap_or("no reason given");
        bail!("the player response carried no playback tracking ({status}: {reason})");
    };

    Ok(Tracking {
        playback,
        watchtime,
    })
}

/// Sends one tracking ping.
///
/// `listened` present makes this a watchtime ping, which carries how far the
/// track got; absent makes it the playback ping, which only announces a start.
fn ping(
    http: &Http,
    cookies: &Cookies,
    base: &str,
    cpn: &str,
    listened: Option<Duration>,
) -> Result<()> {
    let url = ping_url(base, cpn, listened)?;

    let now = sapisid::unix_now();
    let request = http
        .client()
        .get(url)
        .header(reqwest::header::ORIGIN, ORIGIN)
        .header(reqwest::header::REFERER, ORIGIN)
        .header(reqwest::header::COOKIE, cookies.header())
        .header(
            reqwest::header::AUTHORIZATION,
            sapisid::authorization(cookies.sapisid(), ORIGIN, now),
        );

    let (status, _) = http.send(request)?;
    // 204 is the success everyone sees; 200 is accepted too. Anything else is
    // worth naming, even though the caller only logs it.
    if !(200..300).contains(&status) {
        bail!("YouTube refused a playback ping: HTTP {status}");
    }
    Ok(())
}

/// Builds the same stats URL as the Music web client.
///
/// A successful response does not mean YouTube accepted the event: these
/// endpoints also return 204 for incomplete telemetry and silently discard it.
/// The client identity, playback format and timing fields are therefore part of
/// the protocol rather than optional analytics decoration.
fn ping_url(base: &str, cpn: &str, listened: Option<Duration>) -> Result<reqwest::Url> {
    // YouTube returns `s.youtube.com`; its Music client deliberately sends the
    // same signed path through the Music origin instead.
    let base = base.replacen("https://s.", "https://music.", 1);
    let mut url = reqwest::Url::parse(&base).context("YouTube returned an invalid tracking URL")?;

    {
        let mut query = url.query_pairs_mut();
        query
            .append_pair("ver", TRACKING_VERSION)
            .append_pair("c", TRACKING_CLIENT_PARAM)
            .append_pair("cbrver", TRACKING_CLIENT_VERSION)
            .append_pair("cver", TRACKING_CLIENT_VERSION)
            .append_pair("cpn", cpn);

        match listened {
            None => {
                query
                    .append_pair("fmt", TRACKING_FORMAT)
                    .append_pair("rtn", "0")
                    .append_pair("rt", "0");
            }
            Some(listened) => {
                let position = format!("{}.000", listened.as_secs());
                query
                    .append_pair("st", &position)
                    .append_pair("et", &position)
                    .append_pair("cmt", &position)
                    .append_pair("final", "1");
            }
        }
    }

    Ok(url)
}

/// A client playback nonce: 16 characters identifying this one play.
///
/// Hand-rolled rather than pulled in, on the same trade the SHA-1 in
/// [`crate::source::sapisid`] makes -- `rand` costs several transitive crates
/// for sixteen characters that do not need to be unpredictable. They need to be
/// *distinct*: the nonce ties two pings to one play, and reusing one across
/// plays is what would make two songs look like one.
///
/// Seeded from the clock in nanoseconds, mixed with a monotonic per-process
/// counter, and stirred with xorshift64. The clock alone is not enough: its
/// granularity is far coarser than a call on most platforms, so two nonces made
/// in the same tick would seed identically. The counter is what guarantees a
/// distinct seed -- and the odd Weyl multiplier spreads its low bits across the
/// whole word so even the first few nonces of a run differ in every position.
pub(super) fn nonce() -> String {
    static SEQ: AtomicU64 = AtomicU64::new(0);

    let mut state = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_nanos() as u64)
        .unwrap_or(0x2545_F491_4F6C_DD1D)
        ^ SEQ
            .fetch_add(1, Ordering::Relaxed)
            .wrapping_mul(0x9E37_79B9_7F4A_7C15);
    // A zero seed is a fixed point of xorshift and would yield the same nonce
    // forever. Only reachable from a clock at the epoch, but free.
    state |= 1;

    (0..CPN_LENGTH)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            CPN_ALPHABET[(state % CPN_ALPHABET.len() as u64) as usize] as char
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn query(url: &reqwest::Url) -> std::collections::HashMap<String, String> {
        url.query_pairs()
            .map(|(key, value)| (key.into_owned(), value.into_owned()))
            .collect()
    }

    #[test]
    fn tracking_request_carries_a_real_player_timestamp() {
        let body = tracking_body("video", "abcdefghijklmnop");

        assert_eq!(
            body.pointer("/context/client/clientVersion")
                .and_then(Value::as_str),
            Some(TRACKING_CLIENT_VERSION)
        );
        assert_eq!(
            body.pointer("/playbackContext/contentPlaybackContext/signatureTimestamp")
                .and_then(Value::as_u64),
            Some(SIGNATURE_TIMESTAMP)
        );
        assert_ne!(SIGNATURE_TIMESTAMP, 0);
    }

    #[test]
    fn missing_tracking_reports_the_player_failure_without_blaming_the_session() {
        let error = parse_tracking(&serde_json::json!({
            "playabilityStatus": {
                "status": "UNPLAYABLE",
                "reason": "Video unavailable",
            }
        }))
        .err()
        .expect("tracking should be required")
        .to_string();

        assert!(error.contains("UNPLAYABLE: Video unavailable"));
        assert!(!error.contains("cookie"));
        assert!(!error.contains("session"));
    }

    #[test]
    fn tracking_urls_are_read_from_a_playable_response() {
        let tracking = parse_tracking(&serde_json::json!({
            "playbackTracking": {
                "videostatsPlaybackUrl": { "baseUrl": "https://example.test/start" },
                "videostatsWatchtimeUrl": { "baseUrl": "https://example.test/time" },
            }
        }))
        .expect("both tracking URLs should parse");

        assert_eq!(tracking.playback, "https://example.test/start");
        assert_eq!(tracking.watchtime, "https://example.test/time");
    }

    #[test]
    fn playback_ping_matches_the_music_web_client() {
        let url = ping_url(
            "https://s.youtube.com/api/stats/playback?docid=video&ei=event",
            "abcdefghijklmnop",
            None,
        )
        .expect("tracking URL should parse");
        let query = query(&url);

        assert_eq!(url.host_str(), Some("music.youtube.com"));
        assert_eq!(url.path(), "/api/stats/playback");
        assert_eq!(query.get("docid").map(String::as_str), Some("video"));
        assert_eq!(query.get("c").map(String::as_str), Some("web_remix"));
        assert_eq!(query.get("fmt").map(String::as_str), Some("251"));
        assert_eq!(query.get("rt").map(String::as_str), Some("0"));
        assert_eq!(query.get("rtn").map(String::as_str), Some("0"));
        assert_eq!(
            query.get("cpn").map(String::as_str),
            Some("abcdefghijklmnop")
        );
    }

    #[test]
    fn final_watchtime_ping_carries_the_playback_position() {
        let url = ping_url(
            "https://s.youtube.com/api/stats/watchtime?docid=video",
            "abcdefghijklmnop",
            Some(Duration::from_secs(187)),
        )
        .expect("tracking URL should parse");
        let query = query(&url);

        assert_eq!(url.host_str(), Some("music.youtube.com"));
        assert_eq!(query.get("st").map(String::as_str), Some("187.000"));
        assert_eq!(query.get("et").map(String::as_str), Some("187.000"));
        assert_eq!(query.get("cmt").map(String::as_str), Some("187.000"));
        assert_eq!(query.get("final").map(String::as_str), Some("1"));
        assert!(!query.contains_key("fmt"));
    }

    #[test]
    #[ignore = "hits the live YouTube Music player API without reporting a play"]
    fn current_player_request_returns_tracking_urls() {
        let http = Http::new().expect("client should build");
        // Deliberately invalid credentials: this check is about the player
        // request shape and never follows either returned tracking URL.
        let cookies = Cookies::from_header("SAPISID=x; SID=y").expect("header should parse");

        let tracking = tracking_urls(&http, &cookies, "dQw4w9WgXcQ", "abcdefghijklmnop")
            .expect("the current player request should carry tracking URLs");

        assert!(tracking.playback.starts_with("https://"));
        assert!(tracking.watchtime.starts_with("https://"));
    }

    #[test]
    fn a_nonce_is_the_shape_youtube_expects() {
        let cpn = nonce();
        assert_eq!(cpn.len(), CPN_LENGTH);
        assert!(
            cpn.bytes().all(|b| CPN_ALPHABET.contains(&b)),
            "a nonce outside the alphabet gets the ping quietly dropped"
        );
    }

    #[test]
    fn nonces_do_not_repeat() {
        // The one property that actually matters: a reused nonce makes two
        // plays look like one to YouTube.
        let nonces: std::collections::HashSet<String> = (0..64).map(|_| nonce()).collect();
        assert_eq!(nonces.len(), 64, "a nonce repeated within one run");
    }

    #[test]
    fn a_glance_is_never_reported() {
        // Cheap to assert and worth asserting: this is a write to somebody's
        // real account, and the threshold is the whole of what keeps arrowing
        // through a list out of their listening history. `report` returns
        // before touching the network, so the arguments below are never used.
        let http = Http::new().expect("client should build");
        let cookies = Cookies::from_header("SAPISID=x; SID=y").expect("header should parse");
        assert!(report(&http, &cookies, "dQw4w9WgXcQ", Duration::from_secs(5)).is_ok());
        assert!(report(&http, &cookies, "dQw4w9WgXcQ", Duration::from_secs(29)).is_ok());
    }

    /// Whether a play reported here actually lands in the account's history.
    ///
    /// There is no way to assert this from inside the program: an accepted ping
    /// and a discarded one are both `204`. So this reports a play and tells the
    /// user where to go and look, which is the honest shape for a check on an
    /// undocumented endpoint.
    ///
    /// `cargo test reports_a_play -- --ignored --nocapture`
    #[test]
    #[ignore = "writes a play to the real account behind the saved cookie"]
    fn reports_a_play_to_the_live_account() {
        let Some(cookies) = Cookies::load().expect("cookies.txt should parse") else {
            println!("no cookies.txt saved -- nothing to report with");
            return;
        };

        let http = Http::new().expect("client should build");
        // Something unmistakable in a history listing, so it is easy to find
        // and easy to remove again.
        let video_id = "dQw4w9WgXcQ";

        match report(&http, &cookies, video_id, Duration::from_secs(120)) {
            Ok(()) => println!(
                "reported 120s of {video_id}.\n\
                 check history.google.com/history/youtube -- if it is not there \
                 within a minute, the pings are being accepted and discarded."
            ),
            Err(e) => panic!("reporting failed: {e:#}"),
        }
    }
}
