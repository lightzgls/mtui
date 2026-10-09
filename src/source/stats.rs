//! Authenticated listening reports, confirmed against YouTube Music history.
use super::{http::Http, journal::PendingReport, sapisid};
use crate::config::Cookies;
use anyhow::{Context, Result, bail};
use serde_json::Value;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

const ORIGIN: &str = "https://music.youtube.com";
const PLAYER_URL: &str = "https://music.youtube.com/youtubei/v1/player";
const BROWSE_URL: &str = "https://music.youtube.com/youtubei/v1/browse";
const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/134.0.0.0 Safari/537.36";
pub(super) const MIN_REPORTABLE: Duration = Duration::from_secs(30);

struct Identity {
    version: String,
    visitor: String,
    loaded: Instant,
    cookie: String,
}
pub(super) struct Reporter {
    http: Http,
    identity: Option<Identity>,
}

impl Reporter {
    pub fn clear_session(&mut self) {
        self.identity = None;
    }
    pub fn new() -> Result<Self> {
        Ok(Self {
            http: Http::new()?,
            identity: None,
        })
    }

    /// Match ytmusicapi's browser flow: obtain an anonymous visitor identity,
    /// then sign player, stats and history requests with the account cookie.
    fn identity(&mut self, cookies: &Cookies) -> Result<&Identity> {
        if self.identity.as_ref().is_none_or(|identity| {
            identity.loaded.elapsed() > Duration::from_secs(3600)
                || identity.cookie != cookies.header()
        }) {
            let (status, raw) = send(
                &self.http,
                self.http
                    .client()
                    .get(ORIGIN)
                    .header(reqwest::header::USER_AGENT, USER_AGENT),
            )?;
            if status != 200 {
                bail!("could not refresh Music reporting session: HTTP {status}");
            }
            let html = std::str::from_utf8(&raw).context("invalid Music session response")?;
            self.identity = Some(Identity {
                version: daily_client_version(sapisid::unix_now()),
                visitor: config_string(html, "VISITOR_DATA")
                    .context("Music reporting visitor identity is missing")?,
                loaded: Instant::now(),
                cookie: cookies.header().to_string(),
            });
        }
        Ok(self.identity.as_ref().expect("identity was refreshed"))
    }

    /// A 204 alone is insufficient: retain the event until its song appears
    /// in account history. Use add_history_item's minimal playback beacon;
    /// listening duration is measured locally, without extra analytics pings.
    pub fn report(&mut self, cookies: &Cookies, report: &PendingReport) -> Result<()> {
        if report.listened < MIN_REPORTABLE.as_secs() {
            return Ok(());
        }
        // Legacy outbox entries have no measured-progress revision. Their
        // nonce may have been sent with the old invalid payload; recover them
        // with add_history_item's fresh nonce while keeping their durable key.
        let recovered = (report.revision == 0).then(|| PendingReport {
            cpn: nonce(),
            ..report.clone()
        });
        let report = recovered.as_ref().unwrap_or(report);
        self.identity(cookies)?;
        if report.revision == 0 && self.recent_history(cookies)?.contains(&report.video_id) {
            // Recover legacy HTTP-only acknowledgements without replaying songs
            // which were already recorded. History can coalesce repeat listens.
            return Ok(());
        }
        let identity = self.identity.as_ref().expect("identity was refreshed");
        let request = authenticated(self.http.client().post(PLAYER_URL), cookies, identity).body(
            serde_json::to_vec(&tracking_body(&report.video_id, identity))?,
        );
        let (status, raw) = send(&self.http, request)?;
        if status == 401 { crate::session::authentication_failed(cookies); }
        if status != 200 {
            bail!("Music refused the reporting player request: HTTP {status}");
        }
        let json: Value = serde_json::from_slice(&raw)?;
        if !authenticated_player(&json) {
            if super::account::logged_in(&json) == Some(false) { crate::session::authentication_failed(cookies); }
            bail!("Music reporting session has expired; sign in again");
        }
        let tracking = parse_tracking(&json)?;
        self.ping(cookies, &tracking.playback, report)?;
        for attempt in 0..3 {
            if self
                .recent_history(cookies)?
                .iter()
                .any(|id| id == &report.video_id)
            {
                return Ok(());
            }
            if attempt < 2 {
                std::thread::sleep(Duration::from_secs(2));
            }
        }
        bail!(
            "Music accepted telemetry but has not confirmed the history entry; retained for retry"
        )
    }

    fn ping(&self, cookies: &Cookies, base: &str, report: &PendingReport) -> Result<()> {
        let identity = self.identity.as_ref().expect("identity was refreshed");
        let url = ping_url(base, report)?;
        let (status, _) = send(
            &self.http,
            authenticated(self.http.client().get(url), cookies, identity),
        )?;
        if !(200..300).contains(&status) {
            if status == 401 { crate::session::authentication_failed(cookies); }
            bail!("Music refused listening telemetry: HTTP {status}");
        }
        Ok(())
    }

    fn recent_history(&self, cookies: &Cookies) -> Result<Vec<String>> {
        let identity = self.identity.as_ref().expect("identity was refreshed");
        let body =
            serde_json::json!({ "context": context(identity), "browseId": "FEmusic_history" });
        let (status, raw) = send(
            &self.http,
            authenticated(self.http.client().post(BROWSE_URL), cookies, identity)
                .body(serde_json::to_vec(&body)?),
        )?;
        if status != 200 {
            if status == 401 { crate::session::authentication_failed(cookies); }
            bail!("Music history verification failed: HTTP {status}");
        }
        let json: Value = serde_json::from_slice(&raw)?;
        if super::account::logged_in(&json) == Some(false) {
            crate::session::authentication_failed(cookies);
        }
        if json.get("contents").is_none() {
            bail!("Music did not return account history; sign in again");
        }
        Ok(history_ids(&json["contents"]))
    }
}

fn config_string(html: &str, key: &str) -> Option<String> {
    let key = format!("\"{key}\"");
    let after = html
        .split_once(&key)?
        .1
        .trim_start()
        .strip_prefix(':')?
        .trim_start();
    serde_json::Deserializer::from_str(after)
        .into_iter::<String>()
        .next()?
        .ok()
        .filter(|value| !value.is_empty())
}
fn context(identity: &Identity) -> Value {
    serde_json::json!({ "client": { "clientName": "WEB_REMIX", "clientVersion": identity.version }, "user": {} })
}

/// UTC Gregorian date, without adding a date/time runtime dependency. This is
/// the daily WEB_REMIX version convention used by ytmusicapi's context builder.
fn daily_client_version(unix: u64) -> String {
    let days = unix / 86_400 + 719_468;
    let era = days / 146_097;
    let day_of_era = days - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let march_month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * march_month + 2) / 5 + 1;
    let month = if march_month < 10 {
        march_month + 3
    } else {
        march_month - 9
    };
    year += u64::from(month <= 2);
    format!("1.{year:04}{month:02}{day:02}.01.00")
}

fn reporting_sapisid(cookies: &Cookies) -> &str {
    cookies
        .header()
        .split(';')
        .find_map(|cookie| {
            cookie
                .trim()
                .strip_prefix("__Secure-3PAPISID=")
                .filter(|value| !value.is_empty())
        })
        .unwrap_or_else(|| cookies.sapisid())
}
fn tracking_body(video_id: &str, identity: &Identity) -> Value {
    // ytmusicapi's day-stamp fallback is sufficient for reporting-only player
    // responses. Audio resolution remains independent of these requests.
    // The playback nonce belongs on the stats requests, as in add_history_item.
    serde_json::json!({ "video_id": video_id, "context": context(identity),
        "playbackContext": { "contentPlaybackContext": {
            "signatureTimestamp": (sapisid::unix_now() / 86_400).saturating_sub(1) } } })
}
fn signed(request: reqwest::RequestBuilder, cookies: &Cookies) -> reqwest::RequestBuilder {
    request
        .header(reqwest::header::USER_AGENT, USER_AGENT)
        .header(reqwest::header::ORIGIN, ORIGIN)
        .header("X-Origin", ORIGIN)
        .header(reqwest::header::REFERER, format!("{ORIGIN}/"))
        .header(reqwest::header::COOKIE, cookies.header())
        .header("X-Goog-AuthUser", "0")
        .header(
            reqwest::header::AUTHORIZATION,
            sapisid::authorization(reporting_sapisid(cookies), ORIGIN, sapisid::unix_now()),
        )
}
fn authenticated(
    request: reqwest::RequestBuilder,
    cookies: &Cookies,
    identity: &Identity,
) -> reqwest::RequestBuilder {
    signed(request, cookies)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .header("X-Goog-Visitor-Id", &identity.visitor)
}
/// Never include signed URLs in diagnostics.
fn send(http: &Http, request: reqwest::RequestBuilder) -> Result<(u16, Vec<u8>)> {
    http.send(request)
        .map_err(|error| match error.downcast::<reqwest::Error>() {
            Ok(error) => error.without_url().into(),
            Err(error) => error,
        })
}
fn authenticated_player(json: &Value) -> bool {
    json.pointer("/responseContext/serviceTrackingParams")
        .and_then(Value::as_array)
        .is_some_and(|services| {
            services.iter().any(|service| {
                service["params"].as_array().is_some_and(|params| {
                    params
                        .iter()
                        .any(|p| p["key"] == "logged_in" && p["value"] == "1")
                })
            })
        })
}
struct Tracking {
    playback: String,
}
fn parse_tracking(json: &Value) -> Result<Tracking> {
    let base = |field: &str| {
        json.pointer(&format!("/playbackTracking/{field}/baseUrl"))
            .and_then(Value::as_str)
            .map(str::to_string)
    };
    let Some(playback) = base("videostatsPlaybackUrl") else {
        let status = json
            .pointer("/playabilityStatus/status")
            .and_then(Value::as_str)
            .unwrap_or("unknown status");
        let reason = json
            .pointer("/playabilityStatus/reason")
            .and_then(Value::as_str)
            .unwrap_or("no reason given");
        bail!("no playback tracking ({status}: {reason})");
    };
    Ok(Tracking { playback })
}
fn ping_url(base: &str, report: &PendingReport) -> Result<reqwest::Url> {
    let mut url = reqwest::Url::parse(base).context("invalid Music tracking URL")?;
    if url.scheme() != "https"
        || !matches!(url.host_str(), Some("s.youtube.com" | "music.youtube.com"))
    {
        bail!("Music returned an unexpected tracking origin");
    }
    let overwritten = ["ver", "c", "cpn"];
    let preserved: Vec<_> = url
        .query_pairs()
        .filter(|(key, _)| !overwritten.contains(&key.as_ref()))
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect();
    url.set_query(None);
    let mut query = url.query_pairs_mut();
    query.extend_pairs(preserved);
    query
        .append_pair("ver", "2")
        .append_pair("c", "WEB_REMIX")
        .append_pair("cpn", &report.cpn);
    drop(query);
    Ok(url)
}
fn history_ids(value: &Value) -> Vec<String> {
    fn walk(value: &Value, ids: &mut Vec<String>) {
        match value {
            Value::Object(object) => {
                if let Some(item) = object.get("musicResponsiveListItemRenderer")
                    && let Some(id) = item
                        .pointer("/playlistItemData/videoId")
                        .or_else(|| item.pointer("/navigationEndpoint/watchEndpoint/videoId"))
                        .and_then(Value::as_str)
                {
                    ids.push(id.to_string());
                }
                for child in object.values() {
                    walk(child, ids);
                }
            }
            Value::Array(array) => {
                for child in array {
                    walk(child, ids);
                }
            }
            _ => {}
        }
    }
    let mut ids = Vec::new();
    walk(value, &mut ids);
    ids
}
pub(super) fn nonce() -> String {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut state = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_nanos() as u64)
        .unwrap_or(0x2545_F491_4F6C_DD1D)
        ^ SEQ
            .fetch_add(1, Ordering::Relaxed)
            .wrapping_mul(0x9E37_79B9_7F4A_7C15);
    state |= 1;
    (0..16)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            ALPHABET[(state % ALPHABET.len() as u64) as usize] as char
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::super::listening::Interval;
    use super::*;

    fn play() -> PendingReport {
        PendingReport {
            video_id: "video".into(),
            cpn: "abcdefghijklmnop".into(),
            at: 0,
            listened: 45,
            position_ms: Some(115_250),
            revision: 45_250,
            intervals: vec![
                Interval {
                    start_ms: 0,
                    end_ms: 30_000,
                },
                Interval {
                    start_ms: 100_000,
                    end_ms: 115_250,
                },
            ],
        }
    }
    fn query(url: &reqwest::Url) -> std::collections::HashMap<String, String> {
        url.query_pairs()
            .map(|(key, value)| (key.into_owned(), value.into_owned()))
            .collect()
    }

    #[test]
    fn playback_overrides_client_identity_without_duplicate_parameters() {
        let url = ping_url(
            "https://s.youtube.com/api/stats/playback?docid=video&c=old&ver=1&cpn=old&cver=server",
            &play(),
        )
        .unwrap();
        let params = query(&url);
        assert_eq!(url.host_str(), Some("s.youtube.com"));
        assert_eq!(params["cpn"], "abcdefghijklmnop");
        assert_eq!(params["cver"], "server");
        assert_eq!(params["c"], "WEB_REMIX");
        assert_eq!(params["ver"], "2");
        assert!(!params.contains_key("fmt"));
        assert_eq!(url.query_pairs().filter(|(key, _)| key == "c").count(), 1);
    }

    #[test]
    fn playback_keeps_server_tokens_without_inventing_a_stream_format() {
        let url = ping_url(
            "https://s.youtube.com/api/stats/playback?docid=video&vm=token",
            &play(),
        )
        .unwrap();
        let params = query(&url);
        assert_eq!(params["vm"], "token");
        assert_eq!(params["cpn"], "abcdefghijklmnop");
        assert!(!params.contains_key("cver"));
        assert!(!params.contains_key("fmt"));
        assert!(!params.contains_key("st"));
    }

    #[test]
    fn unknown_origins_cannot_receive_account_credentials() {
        assert!(ping_url("https://example.test/api/stats/playback", &play(),).is_err());
        assert!(ping_url("http://s.youtube.com/api/stats/playback", &play(),).is_err());
    }

    #[test]
    fn anonymous_player_response_is_rejected_even_when_playable() {
        let make = |flag| {
            serde_json::json!({ "responseContext": { "serviceTrackingParams": [{ "params": [
            { "key": "logged_in", "value": flag }] }] }, "playabilityStatus": { "status": "OK" } })
        };
        assert!(!authenticated_player(&make("0")));
        assert!(authenticated_player(&make("1")));
        assert!(!authenticated_player(&serde_json::json!({})));
    }

    #[test]
    fn session_config_handles_json_escaping_and_missing_values() {
        let html =
            r#"ytcfg.set({"INNERTUBE_CLIENT_VERSION":"new","VISITOR_DATA":"visitor\u003d"});"#;
        assert_eq!(
            config_string(html, "INNERTUBE_CLIENT_VERSION").as_deref(),
            Some("new")
        );
        assert_eq!(
            config_string(html, "VISITOR_DATA").as_deref(),
            Some("visitor=")
        );
        assert!(config_string(html, "absent").is_none());
    }

    #[test]
    fn reporting_context_matches_ytmusicapi_and_keeps_visitor_in_headers() {
        let identity = Identity {
            version: "1.20261007.01.00".into(),
            visitor: "visitor".into(),
            loaded: Instant::now(),
            cookie: String::new(),
        };
        let body = tracking_body("video", &identity);
        assert_eq!(body["video_id"], "video");
        assert_eq!(body["context"]["client"]["clientName"], "WEB_REMIX");
        assert_eq!(body["context"]["user"], serde_json::json!({}));
        assert!(body.get("cpn").is_none());
        assert!(body["context"]["client"].get("visitorData").is_none());
        assert_eq!(daily_client_version(0), "1.19700101.01.00");
        assert_eq!(daily_client_version(1_709_164_800), "1.20240229.01.00");
        let cookies = Cookies::from_header("SAPISID=first; __Secure-3PAPISID=third").unwrap();
        assert_eq!(reporting_sapisid(&cookies), "third");
    }

    #[test]
    fn history_verification_reads_playable_rows_in_server_order() {
        let rows = serde_json::json!([{ "musicResponsiveListItemRenderer": { "playlistItemData": { "videoId": "newest" } } },
            { "musicResponsiveListItemRenderer": { "navigationEndpoint": { "watchEndpoint": { "videoId": "older" } } } }]);
        assert_eq!(history_ids(&rows), vec!["newest", "older"]);
    }

    #[test]
    fn nonces_are_distinct_and_url_safe() {
        let nonces: std::collections::HashSet<_> = (0..64).map(|_| nonce()).collect();
        assert_eq!(nonces.len(), 64);
        assert!(nonces.iter().all(|cpn| {
            cpn.len() == 16
                && cpn
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        }));
    }

    #[test]
    fn glances_never_touch_the_network() {
        let cookies = Cookies::from_header("SAPISID=x; SID=y").unwrap();
        let mut report = play();
        report.listened = 29;
        let mut reporter = Reporter::new().unwrap();
        assert!(reporter.report(&cookies, &report).is_ok());
        assert!(reporter.identity.is_none());
    }

    #[test]
    #[ignore = "reads the saved Music session and account history"]
    fn current_music_session_can_read_history() {
        let cookies = Cookies::available()
            .unwrap()
            .expect("Music sign-in required");
        let mut reporter = Reporter::new().unwrap();
        reporter.identity(&cookies).unwrap();
        println!(
            "authenticated Music history rows: {}",
            reporter.recent_history(&cookies).unwrap().len()
        );
    }

    /// No synthetic test song: opt in to recovering an actual persisted listen.
    #[test]
    #[ignore = "resends an existing listen; requires MTUI_HISTORY_REPAIR_CPN"]
    fn recover_actual_listen_and_verify_server_history() {
        let wanted =
            std::env::var("MTUI_HISTORY_REPAIR_CPN").expect("select an existing listen explicitly");
        let path = crate::config::dir().unwrap().join("pending-plays.jsonl");
        let report: PendingReport = std::fs::read_to_string(path)
            .unwrap()
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .find(|event| event["event"] == "pending" && event["cpn"] == wanted)
            .map(|event| serde_json::from_value(event).unwrap())
            .expect("existing listen required");
        let cookies = Cookies::available()
            .unwrap()
            .expect("Music sign-in required");
        let mut reporter = Reporter::new().unwrap();
        reporter.identity(&cookies).unwrap();
        let before = reporter.recent_history(&cookies).unwrap();
        reporter.report(&cookies, &report).unwrap();
        let after = reporter.recent_history(&cookies).unwrap();
        println!(
            "actual listen confirmed; was_recent={}, now_recent={}",
            before.iter().take(5).any(|id| id == &report.video_id),
            after.iter().take(5).any(|id| id == &report.video_id)
        );
    }

    #[test]
    #[ignore = "streams an existing listened song silently and verifies a real history write; requires MTUI_HISTORY_REPAIR_CPN"]
    fn real_playback_writes_server_history() {
        use super::super::listening::Listening;
        use crate::player::{Command, PlayState, Player};
        let wanted =
            std::env::var("MTUI_HISTORY_REPAIR_CPN").expect("select an existing listened song");
        let path = crate::config::dir().unwrap().join("pending-plays.jsonl");
        let previous: PendingReport = std::fs::read_to_string(path)
            .unwrap()
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .find(|event| event["event"] == "pending" && event["cpn"] == wanted)
            .map(|event| serde_json::from_value(event).unwrap())
            .expect("existing listen required");
        let cookies = Cookies::available()
            .unwrap()
            .expect("Music sign-in required");
        let mut reporter = Reporter::new().unwrap();
        reporter.identity(&cookies).unwrap();
        let before = reporter.recent_history(&cookies).unwrap();
        assert!(
            !before.contains(&previous.video_id),
            "choose a saved listen which is absent from account history"
        );
        let yt =
            super::super::bootstrap::with_js_runtime(super::super::bootstrap::locate().unwrap())
                .unwrap();
        let mut resolver = mtui_resolver::Resolver::new(yt.bin()).unwrap();
        resolver.set_js_runtime(yt.js_runtime().map(str::to_string));
        resolver.set_pot_provider(
            yt.pot_plugin_dir().map(str::to_string),
            yt.pot_server_home().map(str::to_string),
        );
        resolver.set_session(Some(mtui_resolver::PlaybackSession::new(
            cookies.header(),
            cookies.sapisid(),
        )));
        let stream = resolver
            .resolve(mtui_resolver::ResolveRequest::new(&previous.video_id))
            .unwrap();
        let player = Player::spawn(0.0, None).unwrap();
        let mut listening = Listening::new();
        player
            .send(Command::Play {
                url: stream.url,
                format: stream.format,
                title: "history verification".into(),
                id: previous.video_id.clone(),
            })
            .unwrap();
        let started = Instant::now();
        loop {
            let snap = player.snapshot();
            listening.observe(
                snap.position,
                snap.state == PlayState::Playing,
                Instant::now(),
            );
            assert!(
                snap.error.is_none(),
                "test stream failed before actual playback completed"
            );
            if listening.heard >= Duration::from_secs(32) {
                break;
            }
            assert!(
                started.elapsed() < Duration::from_secs(55),
                "playback did not reach the listening threshold: state={:?}, position={:?}, measured={:?}",
                snap.state,
                snap.position,
                listening.heard
            );
            std::thread::sleep(Duration::from_millis(100));
        }
        player.send(Command::Stop).unwrap();
        let report = listening.report(&previous.video_id);
        std::fs::write(
            "target/history-measured.json",
            serde_json::to_vec(&report).unwrap(),
        )
        .unwrap();
        println!(
            "observed actual playback: {} seconds; measured intervals: {}",
            report.listened,
            report.intervals.len()
        );
        reporter.report(&cookies, &report).unwrap();
        let after = reporter.recent_history(&cookies).unwrap();
        assert_eq!(
            after.first(),
            Some(&report.video_id),
            "real playback must move this song to the newest history position"
        );
        println!(
            "real playback confirmed; prior_position={:?}, newest_position={:?}",
            before.iter().position(|id| id == &report.video_id),
            after.iter().position(|id| id == &report.video_id)
        );
    }

    #[test]
    #[ignore = "resends measured playback from the real-playback diagnostic; requires MTUI_HISTORY_REPAIR_CPN"]
    fn measured_saved_listen_writes_server_history() {
        assert!(
            std::env::var("MTUI_HISTORY_REPAIR_CPN").is_ok(),
            "explicit opt-in required"
        );
        let mut report: PendingReport =
            serde_json::from_slice(&std::fs::read("target/history-measured.json").unwrap())
                .unwrap();
        report.cpn = nonce();
        let cookies = Cookies::available()
            .unwrap()
            .expect("Music sign-in required");
        let mut reporter = Reporter::new().unwrap();
        reporter.identity(&cookies).unwrap();
        let before = reporter.recent_history(&cookies).unwrap();
        assert!(
            !before.contains(&report.video_id),
            "measured song must still be absent before this write"
        );
        reporter.report(&cookies, &report).unwrap();
        let after = reporter.recent_history(&cookies).unwrap();
        assert!(after.contains(&report.video_id));
        println!(
            "measured listen added to account history; prior_position=absent, position={:?}",
            after.iter().position(|id| id == &report.video_id)
        );
    }
}
