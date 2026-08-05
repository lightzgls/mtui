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
//! - **Cookie only.** OAuth cannot do this, for the reason
//!   [`crate::source::sapisid`] documents at length. No `cookies.txt`, no
//!   reporting -- and the caller checks that before it gets here.
//! - **Undocumented and unversioned.** The shape below is what the web client
//!   sent when this was written. Nothing here is load-bearing: every failure is
//!   swallowed by the caller, and a play that fails to report is still played,
//!   still journalled, and still on MTUI's own shelves.
//! - **Not verifiable from here.** A ping that is accepted and a ping that is
//!   quietly discarded both come back `204`. The only real check is whether
//!   YouTube Music's history actually fills up, which is what the ignored test
//!   at the bottom is for.
//!
//! This writes to the user's real account, so it is opt-in twice over: they have
//! to have saved a cookie, and they have to have left reporting on.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::Value;

use super::auth::Http;
use super::innertube::{MUSIC_CLIENT_NAME, MUSIC_CLIENT_VERSION};
use super::sapisid;
use crate::config::Cookies;

const ORIGIN: &str = "https://music.youtube.com";
const PLAYER_URL: &str = "https://music.youtube.com/youtubei/v1/player";

/// Below this, nothing is reported. The same threshold the journal calls a
/// play, so the two histories cannot disagree about what happened.
const MIN_REPORTABLE: Duration = Duration::from_secs(30);

/// The tracking protocol version the web client sends. Both pings carry it.
const TRACKING_VERSION: &str = "2";

/// Marks the one failure a caller can *fix* rather than merely report: YouTube
/// did not recognise the session behind the cookie.
///
/// Matched on as a string, which is not elegant, but the alternative is an
/// error enum threaded through a path where every other failure is equivalent
/// and swallowed. This one is different in kind -- an imported cookie that has
/// expired can be re-read from the browser without troubling the user, and this
/// is what tells the caller to go and do that.
pub const STALE: &str = "the cookie is no longer recognised";

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
pub fn report(
    http: &Http,
    cookies: &Cookies,
    video_id: &str,
    listened: Duration,
) -> Result<()> {
    if listened < MIN_REPORTABLE {
        return Ok(());
    }

    let cpn = nonce();
    let tracking = tracking_urls(http, cookies, video_id, &cpn)?;

    // Start first, then duration. The order is the protocol: a watchtime ping
    // for a playback that was never announced has nothing to attach itself to.
    ping(http, cookies, &tracking.playback, &cpn, None)?;
    ping(http, cookies, &tracking.watchtime, &cpn, Some(listened))?;
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
    let body = serde_json::json!({
        "videoId": video_id,
        "context": {
            "client": {
                "clientName": MUSIC_CLIENT_NAME,
                "clientVersion": MUSIC_CLIENT_VERSION,
                "hl": "en",
            }
        },
        // Announces the nonce the pings will carry, and asks for a response
        // that includes tracking at all -- a player response fetched without
        // this comes back with no `playbackTracking` to read.
        "playbackContext": {
            "contentPlaybackContext": {
                "signatureTimestamp": 0,
                "referer": ORIGIN,
            }
        },
        "cpn": cpn,
    });

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
    let json: Value = serde_json::from_slice(&raw)?;

    let base = |field: &str| -> Option<String> {
        json.pointer(&format!("/playbackTracking/{field}/baseUrl"))
            .and_then(Value::as_str)
            .map(str::to_string)
    };

    // Absent rather than malformed is the ordinary failure here: it is what a
    // cookie YouTube no longer honours looks like, since an anonymous player
    // response carries no tracking to report against.
    let (Some(playback), Some(watchtime)) =
        (base("videostatsPlaybackUrl"), base("videostatsWatchtimeUrl"))
    else {
        bail!("{STALE} -- the player response carried no playback tracking");
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
    // The base URL already carries its own query string -- `docid`, `ei` and a
    // signature among them -- so ours is appended to it rather than started.
    let separator = if base.contains('?') { '&' } else { '?' };
    let mut url = format!(
        "{base}{separator}ver={TRACKING_VERSION}&cpn={cpn}&cver={MUSIC_CLIENT_VERSION}"
    );

    if let Some(listened) = listened {
        let secs = listened.as_secs();
        // Start time and end time, in seconds. Reported as one continuous span
        // from zero: MTUI plays a track through rather than seeking around it,
        // so a single span is not a simplification of what happened.
        url.push_str(&format!("&st=0&et={secs}&state=playing"));
    }

    let now = sapisid::unix_now();
    let request = http
        .client()
        .get(&url)
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

/// A client playback nonce: 16 characters identifying this one play.
///
/// Hand-rolled rather than pulled in, on the same trade the SHA-1 in
/// [`crate::source::sapisid`] makes -- `rand` costs several transitive crates
/// for sixteen characters that do not need to be unpredictable. They need to be
/// *distinct*: the nonce ties two pings to one play, and reusing one across
/// plays is what would make two songs look like one.
///
/// Seeded from the clock in nanoseconds and stirred with xorshift64. Two plays
/// cannot start in the same nanosecond, so they cannot collide.
fn nonce() -> String {
    let mut state = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_nanos() as u64)
        .unwrap_or(0x2545_F491_4F6C_DD1D)
        // A zero seed is a fixed point of xorshift and would yield the same
        // nonce forever. Only reachable from a clock at the epoch, but free.
        | 1;

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
