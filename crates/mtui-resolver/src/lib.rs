//! Validated YouTube audio stream resolution.
//!
//! The fast path uses YouTube's player API over one pooled connection. yt-dlp
//! is retained as a short-lived compatibility fallback for videos that need
//! signature, age-gate, or alternate-client handling. Every candidate is
//! probed near its end before it can be cached or returned.

use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use reqwest::StatusCode;

fn command(program: impl AsRef<std::ffi::OsStr>) -> Command {
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

const PLAYER_TIMEOUT: Duration = Duration::from_secs(5);
const FALLBACK_BUDGET: Duration = Duration::from_secs(30);
const ATTEMPT_TIMEOUT: Duration = Duration::from_secs(10);
const PROCESS_POLL: Duration = Duration::from_millis(50);
const SOCKET_TIMEOUT: &str = "10";
const AUDIO_FORMAT: &str = "141/140/bestaudio[ext=m4a]";
const MUSIC_FORMAT: &str = "141/140/bestaudio[ext=m4a]/18";
const RESTRICTED: &str = "This video is restricted";
const RESOLVE_FLAGS: &[&str] = &["--print", "urls", "--no-warnings", "--ignore-config"];
const MUSIC_CLIENT_FLAGS: &[&str] = &[
    "--extractor-args",
    "youtube:player_client=web_music;player_skip=webpage,configs",
];
const PLAYER_URL: &str = "https://music.youtube.com/youtubei/v1/player";
const MUSIC_ORIGIN: &str = "https://music.youtube.com";
/// Highest-quality formats the compiled AAC/MP4 decoder can consume.
const AUDIO_ITAGS: &[u32] = &[141, 140, 139];

/// Where capped googlevideo URLs observed in the wild stop serving bytes.
pub const CAP_BYTES: u64 = 1024 * 1024;

/// Input to [`Resolver::resolve`].
#[derive(Debug, Clone)]
pub struct ResolveRequest<'a> {
    pub video_id: &'a str,
    pub bypass_cache: bool,
}

impl<'a> ResolveRequest<'a> {
    pub fn new(video_id: &'a str) -> Self {
        Self {
            video_id,
            bypass_cache: false,
        }
    }
}

/// A YouTube web session used only for authenticated player API calls.
///
/// Deliberately has no `Debug` implementation: both fields are account
/// credentials and must never appear in diagnostics. The session stays in
/// memory; yt-dlp fallbacks remain anonymous rather than receiving secrets in
/// process arguments or environment variables.
#[derive(Clone, PartialEq, Eq)]
pub struct PlaybackSession {
    cookie_header: String,
    sapisid: String,
}

impl PlaybackSession {
    pub fn new(cookie_header: impl Into<String>, sapisid: impl Into<String>) -> Self {
        Self {
            cookie_header: cookie_header.into(),
            sapisid: sapisid.into(),
        }
    }
}

/// Which tier produced a stream URL.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ResolveSource {
    Cache,
    InnerTubeWebMusic,
    InnerTubeIos,
    InnerTubeAndroidVr,
    YtDlp,
    YtDlpMusic,
}

/// Audio format information known at resolution time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AudioFormat {
    pub itag: Option<u32>,
}

/// A complete, directly streamable URL.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ResolvedStream {
    pub url: String,
    pub expires_at: Option<SystemTime>,
    pub content_length: Option<u64>,
    pub format: AudioFormat,
    pub source: ResolveSource,
}

/// Compatibility name used by MTUI's player and worker messages.
pub type StreamUrl = ResolvedStream;

impl ResolvedStream {
    /// Whether enough signed lifetime remains to safely reuse this URL.
    pub fn is_valid(&self) -> bool {
        const SAFETY_MARGIN: Duration = Duration::from_secs(300);
        match self.expires_at {
            Some(exp) => exp
                .duration_since(SystemTime::now())
                .is_ok_and(|left| left > SAFETY_MARGIN),
            None => true,
        }
    }
}

/// Stable failure categories callers can act on without parsing text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolveErrorKind {
    Unavailable,
    Private,
    AuthenticationRequired,
    GeoRestricted,
    RestrictedMode,
    RateLimited,
    Capped,
    TimedOut,
    ExtractorOutdated,
    Network,
    Other,
}

/// A classified resolver failure with the provider's useful diagnostic.
#[derive(Debug, Clone)]
pub struct ResolveError {
    kind: ResolveErrorKind,
    message: String,
}

impl ResolveError {
    pub fn kind(&self) -> ResolveErrorKind {
        self.kind
    }

    pub fn message(&self) -> &str {
        &self.message
    }

    fn from_message(message: impl Into<String>) -> Self {
        let message = message.into();
        let lower = message.to_ascii_lowercase();
        let kind = if lower.contains("private video") {
            ResolveErrorKind::Private
        } else if lower.contains("sign in") || lower.contains("login_required") {
            ResolveErrorKind::AuthenticationRequired
        } else if lower.contains("not available in your country")
            || lower.contains("geo-restricted")
        {
            ResolveErrorKind::GeoRestricted
        } else if message.contains(RESTRICTED) || lower.contains("restricted mode") {
            ResolveErrorKind::RestrictedMode
        } else if lower.contains("429") || lower.contains("too many requests") {
            ResolveErrorKind::RateLimited
        } else if lower.contains("timed out") || lower.contains("timeout") {
            ResolveErrorKind::TimedOut
        } else if lower.contains("update yt-dlp") || lower.contains("extractor is broken") {
            ResolveErrorKind::ExtractorOutdated
        } else if lower.contains("network") || lower.contains("connection") || lower.contains("dns")
        {
            ResolveErrorKind::Network
        } else if lower.contains("unavailable") || lower.contains("removed") {
            ResolveErrorKind::Unavailable
        } else {
            ResolveErrorKind::Other
        };
        Self { kind, message }
    }

    fn capped(video_id: &str) -> Self {
        Self {
            kind: ResolveErrorKind::Capped,
            message: format!("every stream URL resolved for {video_id} stopped after one mebibyte"),
        }
    }
}

impl fmt::Display for ResolveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ResolveError {}

/// Stateful resolver with a pooled native client and bounded URL cache.
pub struct Resolver {
    bin: String,
    js_runtime: Option<String>,
    client: reqwest::Client,
    runtime: tokio::runtime::Runtime,
    cache: UrlCache,
    session: Option<PlaybackSession>,
}

impl Resolver {
    pub fn new(bin: impl Into<String>) -> std::result::Result<Self, ResolveError> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| ResolveError::from_message(format!("could not start resolver: {e}")))?;
        let client = reqwest::Client::builder()
            .timeout(PLAYER_TIMEOUT)
            .build()
            .map_err(|e| ResolveError::from_message(format!("could not build resolver: {e}")))?;
        Ok(Self {
            bin: bin.into(),
            js_runtime: None,
            client,
            runtime,
            cache: UrlCache::new(),
            session: None,
        })
    }

    /// Selects the JavaScript runtime used by yt-dlp fallbacks.
    pub fn set_js_runtime(&mut self, runtime: Option<String>) {
        self.js_runtime = runtime;
    }

    /// Replaces the account used for Premium-capable player requests.
    /// Cached URLs are account-dependent, so a changed session invalidates all
    /// of them before another resolve can reuse the wrong quality or access.
    pub fn set_session(&mut self, session: Option<PlaybackSession>) {
        if self.session != session {
            self.session = session;
            self.cache.clear();
        }
    }

    /// Resolves and verifies a complete stream, consulting the URL cache first.
    pub fn resolve(
        &mut self,
        request: ResolveRequest<'_>,
    ) -> std::result::Result<ResolvedStream, ResolveError> {
        if request.bypass_cache {
            self.invalidate(request.video_id);
        } else if let Some(mut hit) = self.cache.get(request.video_id) {
            hit.source = ResolveSource::Cache;
            return Ok(hit);
        }

        let stream = self.resolve_uncached(request.video_id)?;
        self.cache
            .insert(request.video_id.to_string(), stream.clone());
        Ok(stream)
    }

    /// Warms only the cheap native fast path. Never starts yt-dlp.
    pub fn prefetch_fast(&mut self, video_id: &str) -> bool {
        if self.cache.get(video_id).is_some() {
            return true;
        }
        let Ok(stream) =
            resolve_player(&self.runtime, &self.client, self.session.as_ref(), video_id)
        else {
            return false;
        };
        if !serves_whole_file(&self.runtime, &self.client, &stream.url) {
            return false;
        }
        self.cache.insert(video_id.to_string(), stream);
        true
    }

    /// Whether a reusable URL is already cached, without doing network work.
    pub fn is_cached(&mut self, video_id: &str) -> bool {
        self.cache.get(video_id).is_some()
    }

    pub fn invalidate(&mut self, video_id: &str) {
        self.cache.invalidate(video_id);
    }

    fn resolve_uncached(
        &self,
        video_id: &str,
    ) -> std::result::Result<ResolvedStream, ResolveError> {
        if let Ok(fast) =
            resolve_player(&self.runtime, &self.client, self.session.as_ref(), video_id)
            && serves_whole_file(&self.runtime, &self.client, &fast.url)
        {
            return Ok(fast);
        }

        let mut first_error = None;
        // The native path above already gave complete Premium AAC its quick
        // chance. Music's yt-dlp client is the most reliable complete fallback
        // for art tracks, so try it before spending another process on standard
        // clients whose signed URLs are commonly capped. Each process gets only
        // a slice of the overall budget, so a stalled authenticated attempt
        // cannot starve anonymous playback, or vice versa.
        let mut attempts = Vec::new();
        if let Some(session) = self.session.as_ref() {
            attempts.push((
                Some(session),
                "https://music.youtube.com/watch?v=",
                MUSIC_FORMAT,
                MUSIC_CLIENT_FLAGS,
                ResolveSource::YtDlpMusic,
            ));
        }
        attempts.push((
            None,
            "https://music.youtube.com/watch?v=",
            MUSIC_FORMAT,
            MUSIC_CLIENT_FLAGS,
            ResolveSource::YtDlpMusic,
        ));
        if let Some(session) = self.session.as_ref() {
            attempts.push((
                Some(session),
                "https://www.youtube.com/watch?v=",
                AUDIO_FORMAT,
                &[] as &[&str],
                ResolveSource::YtDlp,
            ));
        }
        attempts.push((
            None,
            "https://www.youtube.com/watch?v=",
            AUDIO_FORMAT,
            &[] as &[&str],
            ResolveSource::YtDlp,
        ));
        let fallback_started = Instant::now();
        for (session, watch_url, format, flags, source) in attempts {
            let remaining = FALLBACK_BUDGET.saturating_sub(fallback_started.elapsed());
            if remaining.is_zero() {
                break;
            }
            let timeout = remaining.min(ATTEMPT_TIMEOUT);
            let candidate = resolve_yt_dlp_as(
                (&self.bin, self.js_runtime.as_deref(), timeout),
                session,
                video_id,
                watch_url,
                format,
                flags,
                source,
            );
            match candidate {
                Ok(stream) if serves_whole_file(&self.runtime, &self.client, &stream.url) => {
                    return Ok(stream);
                }
                Ok(_) => {}
                Err(error) => {
                    first_error.get_or_insert(error);
                }
            }
        }
        if fallback_started.elapsed() >= FALLBACK_BUDGET {
            return Err(ResolveError::from_message(
                "stream resolution timed out after 30 seconds",
            ));
        }
        if let Some(error) = first_error {
            let message = format!("{error:#}");
            if message.contains(RESTRICTED) {
                return Err(ResolveError::from_message(restricted_mode(video_id)));
            }
            return Err(ResolveError::from_message(message));
        }

        Err(ResolveError::capped(video_id))
    }
}

#[derive(Debug, Clone, Copy)]
struct PlayerClient {
    name: &'static str,
    version: &'static str,
    user_agent: &'static str,
    source: ResolveSource,
    device_model: Option<&'static str>,
    android_sdk: Option<u32>,
}

const CLIENTS: &[PlayerClient] = &[
    PlayerClient {
        name: "IOS",
        version: "20.10.4",
        user_agent: "com.google.ios.youtube/20.10.4 (iPhone16,2; U; CPU iOS 18_3_2 like Mac OS X;)",
        source: ResolveSource::InnerTubeIos,
        device_model: Some("iPhone16,2"),
        android_sdk: None,
    },
    PlayerClient {
        name: "ANDROID_VR",
        version: "1.62.27",
        user_agent: "com.google.android.apps.youtube.vr.oculus/1.62.27 (Linux; U; Android 12L; eureka-user Build/SQ3A.220605.009.A1) gzip",
        source: ResolveSource::InnerTubeAndroidVr,
        device_model: None,
        android_sdk: Some(32),
    },
];

const AUTHENTICATED_CLIENT: PlayerClient = PlayerClient {
    name: "WEB_REMIX",
    version: "1.20241127.01.00",
    user_agent: "Mozilla/5.0",
    source: ResolveSource::InnerTubeWebMusic,
    device_model: None,
    android_sdk: None,
};

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct PlayerResponse {
    #[serde(default)]
    playability_status: PlayabilityStatus,
    #[serde(default)]
    streaming_data: StreamingData,
}

#[derive(serde::Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct PlayabilityStatus {
    #[serde(default)]
    status: String,
    #[serde(default)]
    reason: Option<String>,
}

#[derive(serde::Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct StreamingData {
    #[serde(default)]
    adaptive_formats: Vec<Format>,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct Format {
    itag: u32,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    mime_type: Option<String>,
}

/// Resolves through YouTube's player API using a caller-owned pooled client.
pub fn resolve_player(
    runtime: &tokio::runtime::Runtime,
    client: &reqwest::Client,
    session: Option<&PlaybackSession>,
    video_id: &str,
) -> Result<ResolvedStream> {
    let mut last = None;
    if let Some(session) = session {
        match resolve_player_as(
            runtime,
            client,
            &AUTHENTICATED_CLIENT,
            Some(session),
            video_id,
        ) {
            Ok(stream) => return Ok(stream),
            Err(e) => last = Some(e),
        }
    }
    for identity in CLIENTS {
        match resolve_player_as(runtime, client, identity, None, video_id) {
            Ok(stream) => return Ok(stream),
            Err(e) => last = Some(e),
        }
    }
    Err(last.unwrap_or_else(|| anyhow::anyhow!("no player clients are configured")))
}

fn resolve_player_as(
    runtime: &tokio::runtime::Runtime,
    http: &reqwest::Client,
    identity: &PlayerClient,
    session: Option<&PlaybackSession>,
    video_id: &str,
) -> Result<ResolvedStream> {
    let mut client = serde_json::json!({
        "clientName": identity.name,
        "clientVersion": identity.version,
        "hl": "en",
    });
    if let Some(model) = identity.device_model {
        client["deviceModel"] = model.into();
    }
    if let Some(sdk) = identity.android_sdk {
        client["androidSdkVersion"] = sdk.into();
    }
    let body = serde_json::to_vec(&serde_json::json!({
        "videoId": video_id,
        "context": { "client": client },
    }))
    .context("could not encode the player API request")?;

    let raw = runtime.block_on(async {
        let mut request = http
            .post(PLAYER_URL)
            .header(reqwest::header::USER_AGENT, identity.user_agent)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body);
        if let Some(session) = session {
            request = request
                .header(reqwest::header::COOKIE, &session.cookie_header)
                .header(reqwest::header::ORIGIN, MUSIC_ORIGIN)
                .header(
                    reqwest::header::AUTHORIZATION,
                    sapisid_authorization(&session.sapisid, MUSIC_ORIGIN, unix_now()),
                );
        }
        request.send().await?.error_for_status()?.bytes().await
    })?;
    let response: PlayerResponse =
        serde_json::from_slice(&raw).context("player API returned unexpected JSON")?;
    if response.playability_status.status != "OK" {
        bail!(
            "{} refused {video_id}: {} ({})",
            identity.name,
            response.playability_status.status,
            response
                .playability_status
                .reason
                .as_deref()
                .unwrap_or("no reason given")
        );
    }

    let (url, itag) = pick_audio(&response.streaming_data.adaptive_formats)
        .with_context(|| format!("player API returned no usable audio for {video_id}"))?;
    Ok(stream(url.to_string(), Some(itag), identity.source))
}

fn pick_audio(formats: &[Format]) -> Option<(&str, u32)> {
    let usable = |f: &&Format| {
        f.url.is_some()
            && f.mime_type
                .as_deref()
                .is_some_and(|mime| mime.starts_with("audio/mp4"))
    };
    let preferred = AUDIO_ITAGS.iter().find_map(|itag| {
        formats.iter().find(|format| {
            format.itag == *itag
                && format.url.is_some()
                && format
                    .mime_type
                    .as_deref()
                    .is_some_and(|mime| mime.starts_with("audio/mp4"))
        })
    });
    preferred
        .or_else(|| formats.iter().find(usable))
        .and_then(|format| Some((format.url.as_deref()?, format.itag)))
}

fn resolve_yt_dlp_as(
    tool: (&str, Option<&str>, Duration),
    session: Option<&PlaybackSession>,
    video_id: &str,
    watch_url: &str,
    format: &str,
    extra: &[&str],
    source: ResolveSource,
) -> Result<ResolvedStream> {
    let (bin, js_runtime, timeout) = tool;
    let cookie_jar = session.map(SessionJar::create).transpose()?;
    let mut command = command(bin);
    if let Some(runtime) = js_runtime {
        command.args(["--no-js-runtimes", "--js-runtimes", runtime]);
    }
    command
        .arg("--format")
        .arg(format)
        .args(RESOLVE_FLAGS)
        .arg("--socket-timeout")
        .arg(SOCKET_TIMEOUT)
        .args(extra);
    if let Some(jar) = &cookie_jar {
        command.arg("--cookies").arg(&jar.path);
    }
    let mut child = command
        .arg(format!("{watch_url}{video_id}"))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("failed to run `{bin}` to resolve {video_id}"))?;
    let started = Instant::now();
    loop {
        if child
            .try_wait()
            .with_context(|| format!("could not wait for `{bin}` while resolving {video_id}"))?
            .is_some()
        {
            break;
        }
        if started.elapsed() >= timeout {
            let _ = child.kill();
            let _ = child.wait();
            bail!(
                "could not resolve {video_id}: yt-dlp timed out after {} seconds",
                timeout.as_secs().max(1)
            );
        }
        thread::sleep(PROCESS_POLL);
    }
    let out = child
        .wait_with_output()
        .with_context(|| format!("could not read `{bin}` output while resolving {video_id}"))?;
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
    let itag = query_value(&url, "itag").and_then(|value| value.parse().ok());
    Ok(stream(url, itag, source))
}

/// A short-lived Netscape jar for yt-dlp. The value never enters arguments;
/// only this random path does, and dropping the guard removes it on every exit.
struct SessionJar {
    path: PathBuf,
}

impl SessionJar {
    fn create(session: &PlaybackSession) -> Result<Self> {
        let mut jar = String::from("# Netscape HTTP Cookie File\n");
        for pair in session.cookie_header.split(';') {
            let Some((name, value)) = pair.trim().split_once('=') else {
                continue;
            };
            if name.is_empty()
                || name.contains(['\t', '\r', '\n'])
                || value.contains(['\t', '\r', '\n'])
            {
                continue;
            }
            jar.push_str(&format!(
                ".youtube.com\tTRUE\t/\tTRUE\t0\t{name}\t{value}\n"
            ));
        }

        for attempt in 0..16u8 {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "mtui-playback-{}-{nonce}-{attempt}.txt",
                std::process::id()
            ));
            match create_private(&path) {
                Ok(mut file) => {
                    file.write_all(jar.as_bytes())
                        .context("could not write temporary playback cookies")?;
                    return Ok(Self { path });
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => {
                    return Err(error).context("could not create temporary playback cookies");
                }
            }
        }
        bail!("could not allocate temporary playback cookies")
    }
}

impl Drop for SessionJar {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

#[cfg(unix)]
fn create_private(path: &std::path::Path) -> std::io::Result<fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
}

#[cfg(not(unix))]
fn create_private(path: &std::path::Path) -> std::io::Result<fs::File> {
    // The per-user temp directory carries the user's Windows ACL. `create_new`
    // also prevents a pre-existing path from redirecting the credential write.
    OpenOptions::new().write(true).create_new(true).open(path)
}

fn stream(url: String, itag: Option<u32>, source: ResolveSource) -> ResolvedStream {
    ResolvedStream {
        expires_at: parse_expiry(&url),
        content_length: content_length(&url),
        format: AudioFormat { itag },
        source,
        url,
    }
}

/// Whether a signed URL proves it serves its complete file.
pub fn serves_whole_file(
    runtime: &tokio::runtime::Runtime,
    client: &reqwest::Client,
    url: &str,
) -> bool {
    let length = content_length(url);
    let last = match length {
        Some(size) => size.saturating_sub(1),
        None => CAP_BYTES,
    };
    runtime.block_on(serves_byte(client, url, last, length.is_none()))
}

async fn serves_byte(client: &reqwest::Client, url: &str, at: u64, unknown_length: bool) -> bool {
    for _ in 0..2 {
        let sent = client
            .get(url)
            .header(reqwest::header::RANGE, format!("bytes={at}-{at}"))
            .send()
            .await;
        if let Ok(response) = sent {
            return served_probe(response.status(), unknown_length);
        }
    }
    false
}

fn served_probe(status: StatusCode, unknown_length: bool) -> bool {
    status.is_success() || (unknown_length && status == StatusCode::RANGE_NOT_SATISFIABLE)
}

/// File size recorded in a signed googlevideo URL.
pub fn content_length(url: &str) -> Option<u64> {
    query_value(url, "clen")?.parse().ok()
}

/// Expiration recorded in a signed googlevideo URL.
pub fn parse_expiry(url: &str) -> Option<SystemTime> {
    let secs: u64 = query_value(url, "expire")?.parse().ok()?;
    Some(UNIX_EPOCH + Duration::from_secs(secs))
}

fn query_value<'a>(url: &'a str, key: &str) -> Option<&'a str> {
    url.split(['?', '&'])
        .find_map(|field| field.strip_prefix(key)?.strip_prefix('='))
}

fn first_error_line(stderr: &[u8]) -> String {
    let text = String::from_utf8_lossy(stderr);
    let line = text
        .lines()
        .find(|line| line.contains("ERROR"))
        .or_else(|| text.lines().rev().find(|line| !line.trim().is_empty()))
        .unwrap_or("no error output")
        .trim();
    strip_preamble(line).to_string()
}

fn strip_preamble(line: &str) -> &str {
    let line = line.strip_prefix("ERROR: ").unwrap_or(line);
    let line = match line.strip_prefix('[') {
        Some(rest) => rest.split_once("] ").map_or(line, |(_, tail)| tail),
        None => line,
    };
    match line.split_once(": ") {
        Some((head, tail)) if is_video_id(head) => tail,
        _ => line,
    }
}

fn is_video_id(value: &str) -> bool {
    value.len() == 11
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}

fn restricted_mode(video_id: &str) -> String {
    format!(
        "{video_id} is blocked by YouTube's Restricted Mode, and the retry \
         through YouTube Music could not reach it either.\n\n\
         Restricted Mode is applied by the network, not by this program. It \
         points www.youtube.com at restrictmoderate.youtube.com \
         (216.239.38.119) or restrict.youtube.com (216.239.38.120), and that \
         front end refuses whatever it treats as mature.\n\n\
         `nslookup www.youtube.com` tells you whether this is it: an answer in \
         216.239.38.x is Restricted Mode. Turning on DNS-over-HTTPS, or using \
         a network without the policy, lifts it for every track at once."
    )
}

struct UrlCache {
    entries: Vec<(String, ResolvedStream)>,
}

impl UrlCache {
    const CAPACITY: usize = 32;

    fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    fn get(&mut self, id: &str) -> Option<ResolvedStream> {
        let pos = self.entries.iter().position(|(key, _)| key == id)?;
        if !self.entries[pos].1.is_valid() {
            self.entries.remove(pos);
            return None;
        }
        let entry = self.entries.remove(pos);
        let stream = entry.1.clone();
        self.entries.push(entry);
        Some(stream)
    }

    fn invalidate(&mut self, id: &str) {
        if let Some(pos) = self.entries.iter().position(|(key, _)| key == id) {
            self.entries.remove(pos);
        }
    }

    fn insert(&mut self, id: String, stream: ResolvedStream) {
        if let Some(pos) = self.entries.iter().position(|(key, _)| *key == id) {
            self.entries.remove(pos);
        } else if self.entries.len() >= Self::CAPACITY {
            self.entries.remove(0);
        }
        self.entries.push((id, stream));
    }

    fn clear(&mut self) {
        self.entries.clear();
    }
}

/// Google's `SAPISIDHASH` authorization value for one origin and moment.
fn sapisid_authorization(sapisid: &str, origin: &str, now: u64) -> String {
    let digest = sha1_hex(format!("{now} {sapisid} {origin}").as_bytes());
    format!("SAPISIDHASH {now}_{digest}")
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_secs())
        .unwrap_or(0)
}

/// SHA-1, used solely by Google's cookie-signing protocol. RFC 3174.
#[allow(unknown_lints)]
#[allow(clippy::chunks_exact_to_as_chunks)]
fn sha1_hex(data: &[u8]) -> String {
    let mut state: [u32; 5] = [
        0x6745_2301,
        0xEFCD_AB89,
        0x98BA_DCFE,
        0x1032_5476,
        0xC3D2_E1F0,
    ];
    let bits = (data.len() as u64).wrapping_mul(8);
    let mut message = data.to_vec();
    message.push(0x80);
    while message.len() % 64 != 56 {
        message.push(0);
    }
    message.extend_from_slice(&bits.to_be_bytes());

    for block in message.chunks_exact(64) {
        let mut words = [0u32; 80];
        for (index, word) in block.chunks_exact(4).enumerate() {
            words[index] = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
        }
        for index in 16..80 {
            words[index] =
                (words[index - 3] ^ words[index - 8] ^ words[index - 14] ^ words[index - 16])
                    .rotate_left(1);
        }

        let [mut a, mut b, mut c, mut d, mut e] = state;
        for (round, word) in words.iter().enumerate() {
            let (mix, constant) = match round {
                0..=19 => ((b & c) | (!b & d), 0x5A82_7999),
                20..=39 => (b ^ c ^ d, 0x6ED9_EBA1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1B_BCDC),
                _ => (b ^ c ^ d, 0xCA62_C1D6),
            };
            let next = a
                .rotate_left(5)
                .wrapping_add(mix)
                .wrapping_add(e)
                .wrapping_add(constant)
                .wrapping_add(*word);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = next;
        }
        for (word, added) in state.iter_mut().zip([a, b, c, d, e]) {
            *word = word.wrapping_add(added);
        }
    }

    state.iter().map(|word| format!("{word:08x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(tag: &str) -> ResolvedStream {
        stream(
            format!("https://example.com/{tag}.m4a"),
            Some(140),
            ResolveSource::YtDlp,
        )
    }

    fn format(itag: u32, url: Option<&str>, mime: &str) -> Format {
        Format {
            itag,
            url: url.map(str::to_string),
            mime_type: Some(mime.to_string()),
        }
    }

    #[test]
    fn ranks_decodable_audio_formats() {
        let formats = vec![
            format(251, Some("opus"), "audio/webm; codecs=\"opus\""),
            format(141, Some("fat"), "audio/mp4; codecs=\"mp4a.40.2\""),
            format(140, Some("aac"), "audio/mp4; codecs=\"mp4a.40.2\""),
        ];
        assert_eq!(pick_audio(&formats), Some(("fat", 141)));
    }

    #[test]
    fn rejects_ciphered_and_undecodable_formats() {
        let formats = vec![
            format(140, None, "audio/mp4"),
            format(251, Some("opus"), "audio/webm"),
        ];
        assert_eq!(pick_audio(&formats), None);
    }

    #[test]
    fn parses_signed_url_metadata() {
        let url = "https://x.googlevideo.com/videoplayback?expire=1785600392&clen=3474230&itag=140";
        assert_eq!(content_length(url), Some(3_474_230));
        assert_eq!(
            parse_expiry(url),
            Some(UNIX_EPOCH + Duration::from_secs(1_785_600_392))
        );
    }

    #[test]
    fn probe_requires_a_served_range() {
        assert!(served_probe(StatusCode::PARTIAL_CONTENT, false));
        assert!(served_probe(StatusCode::OK, false));
        assert!(!served_probe(StatusCode::UNAUTHORIZED, false));
        assert!(!served_probe(StatusCode::FORBIDDEN, false));
        assert!(!served_probe(StatusCode::TOO_MANY_REQUESTS, false));
        assert!(!served_probe(StatusCode::BAD_GATEWAY, false));
        assert!(served_probe(StatusCode::RANGE_NOT_SATISFIABLE, true));
        assert!(!served_probe(StatusCode::RANGE_NOT_SATISFIABLE, false));
    }

    #[test]
    fn cache_is_bounded_lru_and_invalidatable() {
        let mut cache = UrlCache::new();
        for i in 0..UrlCache::CAPACITY {
            cache.insert(i.to_string(), candidate(&i.to_string()));
        }
        assert!(cache.get("0").is_some());
        cache.insert("new".into(), candidate("new"));
        assert!(cache.get("0").is_some());
        assert!(cache.get("1").is_none());
        cache.invalidate("0");
        assert!(cache.get("0").is_none());
    }

    #[test]
    fn changing_the_playback_session_clears_cached_urls() {
        let mut resolver = Resolver::new("yt-dlp").expect("resolver should build");
        resolver
            .cache
            .insert("track".into(), candidate("anonymous"));

        resolver.set_session(Some(PlaybackSession::new("SAPISID=x", "x")));
        assert!(resolver.cache.get("track").is_none());

        resolver
            .cache
            .insert("track".into(), candidate("signed-in"));
        resolver.set_session(Some(PlaybackSession::new("SAPISID=x", "x")));
        assert!(
            resolver.cache.get("track").is_some(),
            "unchanged session cleared the cache"
        );
    }

    #[test]
    fn sapisid_signatures_match_the_known_shape() {
        assert_eq!(sha1_hex(b"abc"), "a9993e364706816aba3e25717850c26c9cd0d89d");
        let header = sapisid_authorization("SECRET", MUSIC_ORIGIN, 1_700_000_000);
        assert!(header.starts_with("SAPISIDHASH 1700000000_"));
        assert_eq!(header.len(), "SAPISIDHASH 1700000000_".len() + 40);
    }

    #[test]
    fn temporary_cookie_jars_are_scoped_and_removed() {
        let path = {
            let session = PlaybackSession::new("SAPISID=secret; PREF=value", "secret");
            let jar = SessionJar::create(&session).expect("jar should be created");
            let body = fs::read_to_string(&jar.path).expect("jar should be readable");
            assert!(body.contains(".youtube.com\tTRUE\t/\tTRUE\t0\tSAPISID\tsecret"));
            assert!(body.contains(".youtube.com\tTRUE\t/\tTRUE\t0\tPREF\tvalue"));
            jar.path.clone()
        };
        assert!(!path.exists(), "temporary cookie jar survived its guard");
    }

    #[test]
    fn cached_results_are_identified_as_cache_hits() {
        let mut cache = UrlCache::new();
        cache.insert("track".into(), candidate("track"));
        let mut hit = cache.get("track").expect("entry should be cached");
        hit.source = ResolveSource::Cache;
        assert_eq!(hit.source, ResolveSource::Cache);
        assert_eq!(hit.format.itag, Some(140));
    }

    #[test]
    fn errors_are_structured() {
        assert_eq!(
            ResolveError::from_message("HTTP 429 Too Many Requests").kind(),
            ResolveErrorKind::RateLimited
        );
        assert_eq!(
            ResolveError::from_message("This is a private video").kind(),
            ResolveErrorKind::Private
        );
        assert_eq!(
            ResolveError::capped("abcdefghijk").kind(),
            ResolveErrorKind::Capped
        );
        assert!(
            ResolveError::capped("abcdefghijk")
                .message()
                .contains("abcdefghijk")
        );
    }

    #[test]
    fn strips_yt_dlp_error_preamble() {
        let stderr = b"WARNING: noise\nERROR: [youtube] dQw4w9WgXcQ: Video unavailable\n";
        assert_eq!(first_error_line(stderr), "Video unavailable");
    }

    #[test]
    fn yt_dlp_flags_exist_when_installed() {
        let Ok(out) = command("yt-dlp").arg("--help").output() else {
            return;
        };
        let help = String::from_utf8_lossy(&out.stdout);
        for flag in RESOLVE_FLAGS
            .iter()
            .chain(MUSIC_CLIENT_FLAGS)
            .chain(["--format", "--socket-timeout"].iter())
            .filter(|flag| flag.starts_with("--"))
        {
            assert!(help.contains(flag), "yt-dlp does not document {flag}");
        }
    }

    #[test]
    #[ignore = "hits YouTube and requires yt-dlp"]
    fn resolves_a_complete_live_stream() {
        let bin = std::env::var("MTUI_YT_DLP").unwrap_or_else(|_| "yt-dlp".into());
        let id = std::env::var("MTUI_VIDEO_ID").unwrap_or_else(|_| "nNN88hijp-o".into());
        let mut resolver = Resolver::new(bin).expect("resolver should build");
        let stream = resolver
            .resolve(ResolveRequest::new(&id))
            .expect("track should resolve");
        println!(
            "resolved {id} through {:?}, itag {:?}",
            stream.source, stream.format.itag
        );
        assert_ne!(stream.source, ResolveSource::Cache);
        assert!(serves_whole_file(
            &resolver.runtime,
            &resolver.client,
            &stream.url
        ));
    }
}
