//! On-disk configuration and the YouTube Music web session.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// Written by hand, and optional. Holds one `Cookie:` header copied out of a
/// browser signed in to YouTube Music.
const COOKIE_FILE: &str = "cookies.txt";

/// Ours: written by interactive Music-session setup.
///
/// Kept apart from [`COOKIE_FILE`] so a file the user wrote by hand is never
/// silently overwritten by something this program regenerated.
const IMPORT_FILE: &str = "browser-cookies.json";

/// Written by hand, and optional. Points the Discord presence at a different
/// application than the one built in, which is a thing only a fork needs.
const DISCORD_FILE: &str = "discord.json";

/// Ours: the Discord presence switch, written when the user flips it.
///
/// A separate file from [`DISCORD_FILE`] under the same rule as the pairs
/// above. The two are written by different hands and at wildly different
/// rates -- one is typed once by somebody running their own build, the other is
/// rewritten by a keypress -- and folding them together would mean a toggle
/// serialising over an id the user typed.
const PRESENCE_FILE: &str = "presence.json";

/// Ours: general application preferences changed from the settings panel.
const SETTINGS_FILE: &str = "settings.json";

/// Where our files live: `%APPDATA%\mtui` on Windows, `$XDG_CONFIG_HOME/mtui`
/// (or `~/.config/mtui`) elsewhere.
pub fn dir() -> Result<PathBuf> {
    let base = if cfg!(windows) {
        std::env::var_os("APPDATA").map(PathBuf::from)
    } else {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            // The spec says a relative XDG path is to be ignored, not resolved.
            .filter(|p| p.is_absolute())
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
    };
    let base = base.context("could not locate a configuration directory")?;
    Ok(base.join("mtui"))
}

/// A YouTube Music web session, optionally copied by hand.
///
/// The credential used for a personalised YouTube Music feed. The shelves
/// built from a user's own listening -- "Listen again", "Quick
/// picks", "Heard in Shorts" -- are reachable only the way Google's own web
/// player reaches them, by signing requests over a cookie.
///
/// Entirely optional, and read on the same terms as everything else here: a
/// user who never creates this file never touches it.
#[derive(Debug, Clone)]
pub struct Cookies {
    /// The whole header, sent back verbatim. Which cookies YouTube wants is
    /// YouTube's business, and trimming it to the ones that look relevant is
    /// how a session stops working for reasons nobody can see.
    header: String,
    /// The one value that is also signed over. Extracted here so a file that
    /// cannot work says so when it is read, rather than on the first request.
    sapisid: String,
}

impl Cookies {
    /// Loads the cookie header, or `None` when the user has not saved one.
    ///
    /// A file that exists but carries no `SAPISID` is an error rather than a
    /// `None`: the user did something deliberate and it will not work, which is
    /// worth saying now instead of silently failing personalized Home.
    pub fn load() -> Result<Option<Self>> {
        let path = dir()?.join(COOKIE_FILE);
        let raw = match fs::read_to_string(&path) {
            Ok(raw) => raw,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e).with_context(|| format!("could not read {}", path.display())),
        };

        let header = raw.trim().to_string();
        if header.is_empty() {
            return Ok(None);
        }

        Self::from_header(&header)
            .with_context(|| {
                format!(
                    "{} has no SAPISID cookie in it. Copy the whole `cookie:` request \
                     header from a music.youtube.com request in your browser's network tab",
                    path.display()
                )
            })
            .map(Some)
    }

    /// Builds from a raw `Cookie:` header, or `None` when it carries nothing to
    /// sign with. Split out from [`Self::load`] so the parse can be exercised
    /// without a file behind it.
    pub fn from_header(raw: &str) -> Option<Self> {
        let header = raw.trim().to_string();
        Some(Self {
            sapisid: sapisid(&header)?,
            header,
        })
    }

    /// The cookies to actually use, from wherever they came from.
    ///
    /// The app-owned webview session is primary: pressing `M` must replace a
    /// stale manual cookie rather than successfully importing a fresh session
    /// that the app then ignores. `cookies.txt` remains a compatibility
    /// fallback.
    pub fn available() -> Result<Option<Self>> {
        if let Some(imported) = Import::load().and_then(|import| Self::from_header(&import.header))
        {
            return Ok(Some(imported));
        }
        Self::load()
    }

    pub fn header(&self) -> &str {
        &self.header
    }

    pub fn sapisid(&self) -> &str {
        &self.sapisid
    }

    /// Removes the hand-written compatibility session as part of an explicit
    /// logout. Unlike an automatic stale-session cleanup, logout means every
    /// credential MTUI can fall back to must go or the next request would sign
    /// straight back in from `cookies.txt`.
    pub fn forget() -> Result<()> {
        let path = dir()?.join(COOKIE_FILE);
        let manual =
            remove_optional(&path).with_context(|| format!("could not remove {}", path.display()));
        let imported = Import::forget();
        manual.and(imported)
    }
}

/// A session established by interactive setup, and which surface created it.
///
/// `browser` is retained as the serialized field name for existing installs;
/// current Windows setup stores `MTUI WebView2` there.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Import {
    pub browser: String,
    pub header: String,
    /// Unix seconds. Not used to decide staleness -- only YouTube knows when a
    /// cookie died, and it says so by refusing -- but it is what makes a stale
    /// import legible when someone looks at the file.
    pub at: u64,
}

impl Import {
    /// Reads the last import, or `None` if there has never been one.
    ///
    /// Infallible by design: this is a cache of something re-derivable, so a
    /// file that will not parse is worth exactly one re-import and no error.
    pub fn load() -> Option<Self> {
        let path = dir().ok()?.join(IMPORT_FILE);
        serde_json::from_slice(&fs::read(path).ok()?).ok()
    }

    #[cfg_attr(not(any(windows, test)), allow(dead_code))]
    pub fn save(&self) -> Result<()> {
        let dir = dir()?;
        fs::create_dir_all(&dir).with_context(|| format!("could not create {}", dir.display()))?;
        let path = dir.join(IMPORT_FILE);

        let body = serde_json::to_vec_pretty(self).context("could not encode the import")?;
        write_atomic(&path, &body)
    }

    /// Forgets the session, so the next launch asks the user to sign in again.
    pub fn forget() -> Result<()> {
        let path = dir()?.join(IMPORT_FILE);
        remove_optional(&path).with_context(|| format!("could not remove {}", path.display()))
    }
}

fn remove_optional(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

/// The Discord application the presence should be published under, when the
/// user has named one.
///
/// Only a fork or a private build needs this: the shipped binary carries an id
/// of its own, and there is nothing secret about an application id -- it
/// travels in every presence payload on the wire. What this exists for is
/// being able to point a build at a
/// differently-named application without rebuilding it.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct Discord {
    pub application_id: String,
}

impl Discord {
    /// The configured id, or `None` if there is no file, it will not parse, or
    /// it names nothing.
    ///
    /// Infallible on purpose. This is an override of a working default, so
    /// every way of getting it wrong should land on that default rather than
    /// on an error in front of a user who never asked for the feature.
    pub fn application_id() -> Option<String> {
        let path = dir().ok()?.join(DISCORD_FILE);
        let discord: Self = serde_json::from_slice(&fs::read(path).ok()?).ok()?;
        let id = discord.application_id.trim();
        (!id.is_empty()).then(|| id.to_string())
    }
}

/// Whether what MTUI is playing may be broadcast to Discord.
///
/// Persisted rather than kept for the session, because it is a privacy setting
/// and a user who switched it off meant it. Off by default -- see
/// [`Presence::load`] -- so playback metadata does not leave the machine until
/// the user has explicitly enabled it.
#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
pub struct Presence {
    pub enabled: bool,
}

impl Presence {
    /// Reads the switch, defaulting to off.
    ///
    /// A missing or unparseable preference fails closed. The switch is one
    /// keypress away and the status bar reports where it landed.
    pub fn load() -> bool {
        let Ok(path) = dir().map(|dir| dir.join(PRESENCE_FILE)) else {
            return false;
        };
        fs::read(path)
            .ok()
            .and_then(|raw| serde_json::from_slice::<Self>(&raw).ok())
            .is_some_and(|presence| presence.enabled)
    }

    /// Records where the switch was left.
    ///
    /// Best effort at the call site: a read-only config directory should cost
    /// the user the setting surviving a restart, not the keypress working.
    pub fn save(enabled: bool) -> Result<()> {
        let dir = dir()?;
        fs::create_dir_all(&dir).with_context(|| format!("could not create {}", dir.display()))?;
        let path = dir.join(PRESENCE_FILE);

        let body = serde_json::to_vec_pretty(&Self { enabled })
            .context("could not encode the presence setting")?;
        write_atomic(&path, &body)
    }
}

/// How the large cover for the current song is drawn.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CoverStyle {
    #[default]
    Pixel,
    ColoredAscii,
}

/// How bitmap artwork is sent to the terminal.
///
/// Automatic keeps the safe capability probe as the default. Kitty is an
/// explicit escape hatch for multiplexers and terminals whose `$TERM` does not
/// advertise the protocol, while PixelArt never emits graphics escapes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ImageRenderer {
    #[default]
    Automatic,
    Kitty,
    PixelArt,
}

impl ImageRenderer {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Automatic => "Automatic",
            Self::Kitty => "Kitty",
            Self::PixelArt => "Pixel art",
        }
    }

    pub const fn next(self) -> Self {
        match self {
            Self::Automatic => Self::Kitty,
            Self::Kitty => Self::PixelArt,
            Self::PixelArt => Self::Automatic,
        }
    }

    pub const fn previous(self) -> Self {
        match self {
            Self::Automatic => Self::PixelArt,
            Self::Kitty => Self::Automatic,
            Self::PixelArt => Self::Kitty,
        }
    }
}

impl CoverStyle {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Pixel => "Pixel art",
            Self::ColoredAscii => "Colored ASCII",
        }
    }

    pub const fn next(self) -> Self {
        match self {
            Self::Pixel => Self::ColoredAscii,
            Self::ColoredAscii => Self::Pixel,
        }
    }

    pub const fn previous(self) -> Self {
        self.next()
    }
}

/// Artwork used for MTUI's runtime-owned icons.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum IconTheme {
    #[default]
    Signal,
    Wave,
    Orbit,
    Mono,
}

impl IconTheme {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Signal => "Signal",
            Self::Wave => "Wave",
            Self::Orbit => "Orbit",
            Self::Mono => "Mono",
        }
    }

    pub const fn next(self) -> Self {
        match self {
            Self::Signal => Self::Wave,
            Self::Wave => Self::Orbit,
            Self::Orbit => Self::Mono,
            Self::Mono => Self::Signal,
        }
    }

    pub const fn previous(self) -> Self {
        match self {
            Self::Signal => Self::Mono,
            Self::Wave => Self::Signal,
            Self::Orbit => Self::Wave,
            Self::Mono => Self::Orbit,
        }
    }
}

/// Preferences changed from MTUI's Settings panel or the playback controls.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Settings {
    pub start_in_tray: bool,
    pub icon_theme: IconTheme,
    pub cover_style: CoverStyle,
    pub image_renderer: ImageRenderer,
    /// Player gain, where `1.0` is 100% and `2.0` is the supported maximum.
    pub volume: f32,
    /// Stable CPAL device id. `None` follows the operating-system default.
    pub output_device: Option<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            start_in_tray: false,
            icon_theme: IconTheme::default(),
            cover_style: CoverStyle::default(),
            image_renderer: ImageRenderer::default(),
            volume: 1.0,
            output_device: None,
        }
    }
}

impl Settings {
    pub fn load() -> Self {
        let Some(path) = dir().ok().map(|dir| dir.join(SETTINGS_FILE)) else {
            return Self::default();
        };
        let mut settings: Self = fs::read(path)
            .ok()
            .and_then(|raw| serde_json::from_slice(&raw).ok())
            .unwrap_or_default();
        settings.volume = normalize_volume(settings.volume);
        settings
    }

    pub fn save(mut self) -> Result<()> {
        self.volume = normalize_volume(self.volume);
        let dir = dir()?;
        fs::create_dir_all(&dir).with_context(|| format!("could not create {}", dir.display()))?;
        let path = dir.join(SETTINGS_FILE);
        let body = serde_json::to_vec_pretty(&self).context("could not encode settings")?;
        write_atomic(&path, &body)
    }
}

fn normalize_volume(volume: f32) -> f32 {
    if volume.is_finite() {
        volume.clamp(0.0, 2.0)
    } else {
        1.0
    }
}

/// Publish a complete file in one rename, keeping the previous file on failure.
pub(crate) fn write_atomic(path: &Path, body: &[u8]) -> Result<()> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let mut name = path.file_name().context("configuration file has no name")?.to_os_string();
    name.push(format!(".tmp-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
    let temporary = path.with_file_name(name);
    struct Cleanup(PathBuf);
    impl Drop for Cleanup { fn drop(&mut self) { let _ = fs::remove_file(&self.0); } }
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)] {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temporary).with_context(||format!("could not prepare {}",path.display()))?;
    let _cleanup = Cleanup(temporary.clone());
    restrict(&temporary)?;
    file.write_all(body)?;
    file.sync_all()?;
    drop(file);
    fs::rename(&temporary, path).with_context(||format!("could not save {}",path.display()))
}

/// Pulls the value Google signs requests over out of a cookie header.
///
/// `__Secure-3PAPISID` is accepted as well as `SAPISID`: a browser in a
/// third-party-cookie-restricted context is issued the former and not always
/// the latter, and they carry the same value for this purpose.
fn sapisid(header: &str) -> Option<String> {
    let named = |name: &str| {
        header.split(';').find_map(|pair| {
            let (key, value) = pair.split_once('=')?;
            (key.trim() == name && !value.trim().is_empty()).then(|| value.trim().to_string())
        })
    };
    named("SAPISID").or_else(|| named("__Secure-3PAPISID"))
}

/// Narrows a file to the owner. A refresh token is a long-lived credential for
/// the user's YouTube account, so it should not inherit the directory's mode.
#[cfg(unix)]
fn restrict(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
}

/// Windows has no mode bits; `%APPDATA%` is already per-user by construction.
#[cfg(not(unix))]
fn restrict(_path: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_the_signing_cookie_in_a_real_header() {
        // A browser sends a dozen of these in one line, in no fixed order.
        let header = "VISITOR_INFO1_LIVE=abc; YSC=def; SAPISID=ThE0nEtHaTmAtTeRs; \
                      __Secure-1PSID=xyz; PREF=tz=Asia.Ho_Chi_Minh";
        assert_eq!(sapisid(header).as_deref(), Some("ThE0nEtHaTmAtTeRs"));
    }

    #[test]
    fn falls_back_to_the_third_party_cookie() {
        // A browser restricting third-party cookies issues `__Secure-3PAPISID`
        // and not always `SAPISID`; they carry the same value for signing.
        let header = "YSC=def; __Secure-3PAPISID=SameValue; PREF=x";
        assert_eq!(sapisid(header).as_deref(), Some("SameValue"));
    }

    #[test]
    fn blank_signing_cookies_are_skipped_and_blank_only_sessions_are_rejected() {
        assert!(Cookies::from_header("SAPISID= ; __Secure-3PAPISID=").is_none());
        assert_eq!(sapisid("SAPISID=; __Secure-3PAPISID=valid").as_deref(), Some("valid"));
        assert_eq!(sapisid("SAPISID= ; SAPISID=valid").as_deref(), Some("valid"));
    }

    #[test]
    fn atomic_settings_replacement_never_exposes_partial_json() {
        use std::sync::{Arc, atomic::{AtomicBool, Ordering}};
        let folder = std::env::temp_dir().join(format!("mtui-atomic-{}", std::process::id()));
        fs::create_dir_all(&folder).unwrap();
        let path = folder.join("settings.json");
        write_atomic(&path, br#"{"volume":1}"#).unwrap();
        let done = Arc::new(AtomicBool::new(false));
        let reader_path = path.clone();
        let reader_done = done.clone();
        let reader = std::thread::spawn(move || {
            while !reader_done.load(Ordering::Relaxed) {
                let raw = fs::read(&reader_path).unwrap();
                let value: serde_json::Value = serde_json::from_slice(&raw).unwrap();
                assert!(value["volume"].is_number());
            }
        });
        for volume in 0..20 { write_atomic(&path, format!("{{\"volume\":{volume}}}").as_bytes()).unwrap(); }
        done.store(true, Ordering::Relaxed);
        reader.join().unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "{\"volume\":19}");
        assert_eq!(fs::read_dir(&folder).unwrap().count(), 1);
        fs::remove_file(path).unwrap();
        fs::remove_dir(folder).unwrap();
    }

    #[test]
    fn a_cookie_name_that_merely_ends_in_sapisid_is_not_it() {
        // `__Secure-1PAPISID` is a different cookie and signing with it fails
        // in a way that looks exactly like an expired session.
        let header = "__Secure-1PAPISID=wrong; HSID=x";
        assert_eq!(sapisid(header), None);
    }

    #[test]
    fn a_header_with_nothing_to_sign_with_is_rejected() {
        assert_eq!(sapisid("YSC=def; PREF=x"), None);
        assert_eq!(sapisid(""), None);
        // A value containing `=` (base64 padding) survives the split.
        assert_eq!(sapisid("SAPISID=pad==").as_deref(), Some("pad=="));
    }

    #[test]
    fn config_dir_is_under_the_platform_base() {
        // Only meaningful when the environment actually names one; CI images
        // sometimes have neither.
        if let Ok(path) = dir() {
            assert!(path.ends_with("mtui"));
        }
    }

    #[test]
    fn old_or_empty_settings_default_to_the_terminal() {
        assert!(!Settings::default().start_in_tray);
        let settings: Settings = serde_json::from_str("{}").unwrap();
        assert!(!settings.start_in_tray);
        assert_eq!(settings.icon_theme, IconTheme::Signal);
        assert_eq!(settings.cover_style, CoverStyle::Pixel);
        assert_eq!(settings.image_renderer, ImageRenderer::Automatic);
        assert_eq!(settings.volume, 1.0);
        assert_eq!(settings.output_device, None);

        let legacy: Settings = serde_json::from_str(r#"{"start_in_tray":true}"#).unwrap();
        assert!(legacy.start_in_tray);
        assert_eq!(legacy.icon_theme, IconTheme::Signal);
        assert_eq!(legacy.cover_style, CoverStyle::Pixel);
        assert_eq!(legacy.image_renderer, ImageRenderer::Automatic);
        assert_eq!(legacy.volume, 1.0);
        assert_eq!(legacy.output_device, None);
    }

    #[test]
    fn the_tray_preference_round_trips() {
        let body = serde_json::to_string(&Settings {
            start_in_tray: true,
            icon_theme: IconTheme::Wave,
            cover_style: CoverStyle::ColoredAscii,
            image_renderer: ImageRenderer::Kitty,
            volume: 0.65,
            output_device: Some("Wasapi:test-output".to_string()),
        })
        .unwrap();
        let settings: Settings = serde_json::from_str(&body).unwrap();
        assert!(settings.start_in_tray);
        assert_eq!(settings.icon_theme, IconTheme::Wave);
        assert_eq!(settings.cover_style, CoverStyle::ColoredAscii);
        assert_eq!(settings.image_renderer, ImageRenderer::Kitty);
        assert_eq!(settings.volume, 0.65);
        assert_eq!(
            settings.output_device.as_deref(),
            Some("Wasapi:test-output")
        );
    }

    #[test]
    fn volume_is_kept_inside_the_players_supported_range() {
        assert_eq!(normalize_volume(-1.0), 0.0);
        assert_eq!(normalize_volume(0.75), 0.75);
        assert_eq!(normalize_volume(3.0), 2.0);
        assert_eq!(normalize_volume(f32::NAN), 1.0);
    }

    #[test]
    fn cover_styles_cycle_and_use_stable_config_names() {
        assert_eq!(CoverStyle::Pixel.next(), CoverStyle::ColoredAscii);
        assert_eq!(CoverStyle::ColoredAscii.next(), CoverStyle::Pixel);
        assert_eq!(CoverStyle::Pixel.previous(), CoverStyle::ColoredAscii);
        assert_eq!(
            serde_json::to_string(&CoverStyle::ColoredAscii).unwrap(),
            "\"colored-ascii\""
        );
    }

    #[test]
    fn image_renderers_cycle_and_use_stable_config_names() {
        assert_eq!(ImageRenderer::Automatic.next(), ImageRenderer::Kitty);
        assert_eq!(ImageRenderer::Kitty.next(), ImageRenderer::PixelArt);
        assert_eq!(ImageRenderer::PixelArt.next(), ImageRenderer::Automatic);
        assert_eq!(ImageRenderer::Automatic.previous(), ImageRenderer::PixelArt);
        assert_eq!(
            serde_json::to_string(&ImageRenderer::PixelArt).unwrap(),
            "\"pixel-art\""
        );
    }

    #[test]
    fn icon_themes_cycle_and_use_stable_config_names() {
        assert_eq!(IconTheme::Signal.next(), IconTheme::Wave);
        assert_eq!(IconTheme::Signal.previous(), IconTheme::Mono);
        assert_eq!(IconTheme::Wave.next(), IconTheme::Orbit);
        assert_eq!(IconTheme::Orbit.next(), IconTheme::Mono);
        assert_eq!(IconTheme::Mono.next(), IconTheme::Signal);
        assert_eq!(serde_json::to_string(&IconTheme::Wave).unwrap(), "\"wave\"");
    }
}
