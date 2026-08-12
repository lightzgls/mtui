//! Google OAuth 2.0, limited-input device flow.
//!
//! A terminal cannot host a redirect URI, so the browser-redirect flow is not
//! available here. The device flow is the one Google publishes for exactly this
//! situation: we ask for a short code, the user types it into a browser on any
//! machine, and we poll until they have approved it. Nothing is ever typed back
//! into the terminal, so no password or token passes through this program.
//!
//! The tokens land in [`crate::config`]. From then on the only cost is a
//! refresh every hour, which is one HTTPS POST and happens underneath whatever
//! library call triggered it.

use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use crate::config::{Credentials, Tokens};

const DEVICE_URL: &str = "https://oauth2.googleapis.com/device/code";
const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";

/// Read *and* write on the user's YouTube account.
///
/// `youtube.readonly` would cover browsing, but not adding to a playlist or
/// liking a track, and Google will not widen a scope on an existing grant --
/// the user would have to go through consent a second time. Asking once, up
/// front, for what the feature set actually needs is the honest trade.
const SCOPE: &str = "https://www.googleapis.com/auth/youtube";

const GRANT_DEVICE: &str = "urn:ietf:params:oauth:grant-type:device_code";
const GRANT_REFRESH: &str = "refresh_token";

/// Generous compared to the player API's 5 s. There is no fallback path here
/// and no latency budget to defend -- the user is watching a code on screen.
const TIMEOUT: Duration = Duration::from_secs(20);

/// Floor on the poll interval, whatever Google asks for. Polling faster than
/// this earns a `slow_down` and gets us nowhere sooner.
const MIN_INTERVAL: Duration = Duration::from_secs(5);

/// The HTTP machinery the Google calls share.
///
/// Held together rather than built per call so the connection pool and TLS
/// session survive between requests, for the same reason
/// [`crate::source::innertube::InnerTube`] holds its own.
pub struct Http {
    client: reqwest::Client,
    runtime: tokio::runtime::Runtime,
}

impl Http {
    pub fn new() -> Result<Self> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("could not start a runtime for Google API calls")?;
        let client = reqwest::Client::builder()
            .timeout(TIMEOUT)
            .build()
            .context("could not build the Google API client")?;
        Ok(Self { client, runtime })
    }

    /// Posts a form and returns the status alongside the body.
    ///
    /// The body is returned even on a failure status, which is not incidental:
    /// the device flow signals "not yet approved" as a 428 whose body is the
    /// only thing distinguishing it from a real error, so `error_for_status`
    /// would throw away the one field that matters.
    fn post_form(&self, url: &str, fields: &[(&str, &str)]) -> Result<(u16, Vec<u8>)> {
        let body = form_urlencoded::Serializer::new(String::new())
            .extend_pairs(fields.iter())
            .finish();

        self.runtime.block_on(async {
            let response = self
                .client
                .post(url)
                .header(
                    reqwest::header::CONTENT_TYPE,
                    "application/x-www-form-urlencoded",
                )
                .body(body)
                .send()
                .await?;
            let status = response.status().as_u16();
            let bytes = response.bytes().await?;
            Ok((status, bytes.to_vec()))
        })
    }

    /// Sends an authorised request and returns the status alongside the body,
    /// for the same reason [`Self::post_form`] does: Google's error bodies are
    /// far more useful than its status codes.
    pub(super) fn send(&self, request: reqwest::RequestBuilder) -> Result<(u16, Vec<u8>)> {
        self.runtime.block_on(async {
            let response = request.send().await?;
            let status = response.status().as_u16();
            let bytes = response.bytes().await?;
            Ok((status, bytes.to_vec()))
        })
    }

    pub(super) fn client(&self) -> &reqwest::Client {
        &self.client
    }
}

/// What the user has to act on, and what we need to keep polling with.
#[derive(Debug, Clone)]
pub struct DeviceCode {
    /// Never shown to the user; this is our half of the handshake.
    device_code: String,
    /// The short code the user types into the browser.
    pub user_code: String,
    pub verification_url: String,
    interval: Duration,
    deadline: Instant,
}

impl DeviceCode {
    /// How much longer Google will accept this code.
    ///
    /// Saturating rather than a panic on an elapsed deadline: this is read to
    /// draw a countdown, and a frame that lands a moment after expiry is an
    /// ordinary race, not a bug worth taking the terminal down for.
    pub fn expires_in(&self) -> Duration {
        self.deadline.saturating_duration_since(Instant::now())
    }
}

/// Asks Google for a code for the user to approve.
pub fn start(http: &Http, creds: &Credentials) -> Result<DeviceCode> {
    #[derive(serde::Deserialize)]
    struct Response {
        device_code: String,
        user_code: String,
        /// Google's own field name is `verification_url`; the RFC calls it
        /// `verification_uri`. Accepting both means a rename on their side
        /// does not present as "sign-in is broken".
        #[serde(alias = "verification_uri")]
        verification_url: String,
        expires_in: u64,
        #[serde(default)]
        interval: u64,
    }

    let (status, body) = http.post_form(
        DEVICE_URL,
        &[("client_id", creds.client_id.as_str()), ("scope", SCOPE)],
    )?;
    if status != 200 {
        bail!(
            "Google refused the sign-in request: {}",
            describe(&body, status)
        );
    }

    let response: Response =
        serde_json::from_slice(&body).context("Google returned an unexpected sign-in response")?;

    Ok(DeviceCode {
        device_code: response.device_code,
        user_code: response.user_code,
        verification_url: response.verification_url,
        interval: Duration::from_secs(response.interval).max(MIN_INTERVAL),
        deadline: Instant::now() + Duration::from_secs(response.expires_in),
    })
}

/// Outcome of one poll of the token endpoint.
enum Poll {
    /// The user has not finished approving yet.
    Pending,
    /// Google asked us to back off. Carries the amount to add to the interval.
    SlowDown,
    Ready(Box<Tokens>),
}

/// Blocks until the user approves the code, refuses it, or lets it expire.
///
/// Runs on a thread of its own: this sits for as long as it takes the user to
/// walk to another device, and the serial source worker cannot be occupied for
/// minutes while searches and plays queue up behind it.
pub fn wait_for_approval(http: &Http, creds: &Credentials, code: &DeviceCode) -> Result<Tokens> {
    let mut interval = code.interval;

    loop {
        if Instant::now() >= code.deadline {
            bail!("the sign-in code expired before it was approved");
        }
        std::thread::sleep(interval);

        match poll_once(http, creds, code)? {
            Poll::Ready(tokens) => return Ok(*tokens),
            Poll::Pending => {}
            // Additive rather than a reset: Google may ask more than once, and
            // each request is a fresh strike against the same rate limit.
            Poll::SlowDown => interval += MIN_INTERVAL,
        }
    }
}

fn poll_once(http: &Http, creds: &Credentials, code: &DeviceCode) -> Result<Poll> {
    let (status, body) = http.post_form(
        TOKEN_URL,
        &[
            ("client_id", creds.client_id.as_str()),
            ("client_secret", creds.client_secret.as_str()),
            ("device_code", code.device_code.as_str()),
            ("grant_type", GRANT_DEVICE),
        ],
    )?;

    if status == 200 {
        let granted: Granted = serde_json::from_slice(&body)
            .context("Google returned an unexpected token response")?;
        let refresh = granted.refresh_token.context(
            "Google granted access but no refresh token. Remove this app at \
             myaccount.google.com/permissions and sign in again",
        )?;
        return Ok(Poll::Ready(Box::new(Tokens::new(
            granted.access_token,
            refresh,
            granted.expires_in,
        ))));
    }

    // Everything below is a non-200 whose body says which kind it is. Only two
    // of them mean "keep waiting"; the rest are terminal and worth reporting,
    // because the user is sitting in front of a code that will never work.
    match error_code(&body).as_deref() {
        Some("authorization_pending") => Ok(Poll::Pending),
        Some("slow_down") => Ok(Poll::SlowDown),
        Some("access_denied") => bail!("sign-in was declined"),
        Some("expired_token") => bail!("the sign-in code expired before it was approved"),
        _ => bail!("sign-in failed: {}", describe(&body, status)),
    }
}

/// Fields of a successful token response. Google sends more; serde drops it.
#[derive(serde::Deserialize)]
struct Granted {
    access_token: String,
    expires_in: u64,
    /// Absent on a refresh, and on a re-grant to a client the user has already
    /// approved. Only the very first exchange is guaranteed to carry one.
    #[serde(default)]
    refresh_token: Option<String>,
}

/// A signed-in account, and the only way to get a usable access token.
///
/// Refreshing is deliberately not something a caller can forget to do: there is
/// no accessor for the raw token, so every use goes through
/// [`Session::access_token`] and picks up the expiry check.
pub struct Session {
    creds: Credentials,
    tokens: Tokens,
}

impl Session {
    /// Loads the stored session, or `None` if the user has never signed in.
    ///
    /// Missing tokens are `None` rather than an error -- being signed out is an
    /// ordinary state. A missing *client*, on the other hand, is an error: the
    /// user has asked for something that cannot work until they configure it.
    pub fn load() -> Result<Option<Self>> {
        let Some(tokens) = Tokens::load()? else {
            return Ok(None);
        };
        Ok(Some(Self {
            creds: Credentials::load()?,
            tokens,
        }))
    }

    /// Returns a token good for at least a minute, refreshing first if needed.
    pub fn access_token(&mut self, http: &Http) -> Result<&str> {
        if self.tokens.is_expired() {
            self.refresh(http)?;
        }
        Ok(&self.tokens.access_token)
    }

    fn refresh(&mut self, http: &Http) -> Result<()> {
        let (status, body) = http.post_form(
            TOKEN_URL,
            &[
                ("client_id", self.creds.client_id.as_str()),
                ("client_secret", self.creds.client_secret.as_str()),
                ("refresh_token", self.tokens.refresh_token.as_str()),
                ("grant_type", GRANT_REFRESH),
            ],
        )?;

        if status != 200 {
            // The overwhelmingly common cause, and one the error body states
            // far too tersely for a user to act on. Google expires refresh
            // tokens after 7 days while the consent screen is in "Testing".
            if error_code(&body).as_deref() == Some("invalid_grant") {
                bail!(
                    "the saved sign-in is no longer valid -- press A to sign in again. \
                     If this happens every week, set the OAuth consent screen's publishing \
                     status to \"In production\" in the Google Cloud console"
                );
            }
            bail!("could not refresh the sign-in: {}", describe(&body, status));
        }

        let granted: Granted = serde_json::from_slice(&body)
            .context("Google returned an unexpected refresh response")?;
        self.tokens = self.tokens.refreshed_with(
            granted.access_token,
            granted.refresh_token,
            granted.expires_in,
        );

        // Persisted so the next launch starts from the refreshed token rather
        // than a stale one. A write failure is not fatal -- the session in
        // memory is still good -- but it does mean the next launch re-refreshes.
        let _ = self.tokens.save();
        Ok(())
    }
}

/// Pulls the `error` field out of an OAuth error body.
fn error_code(body: &[u8]) -> Option<String> {
    serde_json::from_slice::<serde_json::Value>(body)
        .ok()?
        .get("error")
        .and_then(|e| {
            // Plain OAuth errors are a string; the Google API errors that
            // `library` runs into are an object with a `message`. Reading both
            // here keeps one formatting path for every Google failure.
            e.as_str()
                .map(str::to_string)
                .or_else(|| e.get("status").and_then(|s| s.as_str()).map(str::to_string))
        })
}

/// Best available human-readable description of a Google error body.
pub(super) fn describe(body: &[u8], status: u16) -> String {
    let json: Option<serde_json::Value> = serde_json::from_slice(body).ok();
    let described = json.as_ref().and_then(|j| {
        j.pointer("/error/message")
            .or_else(|| j.get("error_description"))
            .or_else(|| j.get("error"))
            .and_then(|v| v.as_str())
            .map(str::to_string)
    });
    described.unwrap_or_else(|| format!("HTTP {status}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_a_plain_oauth_error() {
        let body = br#"{"error":"authorization_pending","error_description":"Precondition"}"#;
        assert_eq!(error_code(body).as_deref(), Some("authorization_pending"));
        assert_eq!(describe(body, 428), "Precondition");
    }

    #[test]
    fn reads_a_google_api_error() {
        // A different shape entirely to the OAuth one above, and the shape the
        // Data API returns. Both have to format into something readable.
        let body =
            br#"{"error":{"code":403,"message":"Quota exceeded","status":"RESOURCE_EXHAUSTED"}}"#;
        assert_eq!(describe(body, 403), "Quota exceeded");
        assert_eq!(error_code(body).as_deref(), Some("RESOURCE_EXHAUSTED"));
    }

    #[test]
    fn falls_back_to_the_status_when_the_body_says_nothing() {
        // An HTML error page from a proxy, which is not JSON at all.
        assert_eq!(describe(b"<html>502</html>", 502), "HTTP 502");
        assert_eq!(describe(b"", 500), "HTTP 500");
        assert!(error_code(b"not json").is_none());
    }

    #[test]
    fn a_grant_without_a_refresh_token_parses_but_is_caught() {
        // Google omits `refresh_token` when re-granting to a client the user
        // already approved. Parsing has to succeed so the caller can give the
        // specific "revoke and retry" advice rather than a serde error.
        let granted: Granted =
            serde_json::from_slice(br#"{"access_token":"a","expires_in":3599}"#).unwrap();
        assert!(granted.refresh_token.is_none());
        assert_eq!(granted.expires_in, 3599);
    }

    #[test]
    fn device_response_accepts_either_verification_field_name() {
        #[derive(serde::Deserialize)]
        struct Response {
            #[serde(alias = "verification_uri")]
            verification_url: String,
        }
        let google: Response =
            serde_json::from_str(r#"{"verification_url":"https://google.com/device"}"#).unwrap();
        let rfc: Response =
            serde_json::from_str(r#"{"verification_uri":"https://google.com/device"}"#).unwrap();
        assert_eq!(google.verification_url, rfc.verification_url);
    }

    #[test]
    fn the_poll_interval_never_drops_below_the_floor() {
        // Google currently sends 5, but a smaller value would only earn a
        // `slow_down` and delay the sign-in it was meant to speed up.
        assert_eq!(Duration::from_secs(1).max(MIN_INTERVAL), MIN_INTERVAL);
        assert_eq!(
            Duration::from_secs(10).max(MIN_INTERVAL),
            Duration::from_secs(10)
        );
    }
}
