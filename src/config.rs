//! On-disk configuration: the OAuth client the user registered, and the tokens
//! obtained with it.
//!
//! The client id and secret are *not* baked into the binary, and that is not an
//! oversight. Google treats the YouTube scopes as sensitive, so an application
//! that ships them publicly needs verification before anyone but its own test
//! users can log in. Registering a client takes a few minutes and is done once;
//! shipping one would mean either a verification review or a login that stops
//! working for the 101st user.
//!
//! Nothing here is loaded at startup. A user who never signs in never touches
//! the disk, and a malformed config surfaces when they ask for the library
//! rather than as a failure to launch.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};

/// Written by hand, never rewritten by us.
const CLIENT_FILE: &str = "client.json";

/// Ours: replaced on every token refresh. Kept in a separate file from
/// [`CLIENT_FILE`] precisely so that a refresh cannot serialise over whatever
/// the user typed -- including comments and formatting serde would not preserve.
const TOKEN_FILE: &str = "tokens.json";

/// Shown when there is no client to read. Long because it is the entire setup
/// procedure, and a user hitting this has no other documentation to hand.
const SETUP_HELP: &str = "\
no OAuth client configured. To sign in:
  1. console.cloud.google.com -> new project -> enable \"YouTube Data API v3\"
  2. OAuth consent screen -> add yourself as a user, then set publishing status
     to \"In production\" (in \"Testing\" Google expires the login after 7 days)
  3. Credentials -> Create credentials -> OAuth client ID
     -> application type \"TV and Limited Input devices\"
  4. save the id and secret as {path}:
     {{ \"client_id\": \"...\", \"client_secret\": \"...\" }}";

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

/// The OAuth client the user registered. Read-only as far as this program is
/// concerned.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct Credentials {
    pub client_id: String,
    pub client_secret: String,
}

impl Credentials {
    /// Loads the client, or explains how to create one.
    pub fn load() -> Result<Self> {
        let path = dir()?.join(CLIENT_FILE);
        let raw = match fs::read(&path) {
            Ok(raw) => raw,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                bail!(SETUP_HELP.replace("{path}", &path.display().to_string()));
            }
            Err(e) => return Err(e).with_context(|| format!("could not read {}", path.display())),
        };

        let creds: Self = serde_json::from_slice(&raw)
            .with_context(|| format!("{} is not valid JSON with the expected keys", path.display()))?;
        if creds.client_id.trim().is_empty() || creds.client_secret.trim().is_empty() {
            bail!("{} has an empty client_id or client_secret", path.display());
        }
        Ok(creds)
    }
}

/// What Google gave us in exchange for the user's consent.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Tokens {
    pub access_token: String,
    /// The long-lived half. Google only sends this on the *first* exchange, so
    /// a refresh response that omits it must not be allowed to erase it -- see
    /// [`Tokens::refreshed_with`].
    pub refresh_token: String,
    /// Unix seconds. Stored as an absolute instant rather than the `expires_in`
    /// Google actually sends: a token read back from disk an hour later has to
    /// be seen as expired, and a relative lifetime would call it fresh.
    pub expires_at: u64,
}

impl Tokens {
    /// Builds from Google's relative `expires_in`, anchored to now.
    pub fn new(access_token: String, refresh_token: String, expires_in: u64) -> Self {
        Self {
            access_token,
            refresh_token,
            expires_at: unix_now() + expires_in,
        }
    }

    /// True when the access token has less than a minute left.
    ///
    /// The margin matters: a token that passes this check is about to be used
    /// for a request that itself takes time, and expiring mid-flight would
    /// surface as an opaque 401 rather than a refresh.
    pub fn is_expired(&self) -> bool {
        self.expires_at <= unix_now() + 60
    }

    /// Applies a refresh response, keeping the existing refresh token when the
    /// response did not carry a new one -- which is the normal case.
    pub fn refreshed_with(&self, access_token: String, refresh: Option<String>, expires_in: u64) -> Self {
        Self {
            access_token,
            refresh_token: refresh.unwrap_or_else(|| self.refresh_token.clone()),
            expires_at: unix_now() + expires_in,
        }
    }

    pub fn load() -> Result<Option<Self>> {
        let path = dir()?.join(TOKEN_FILE);
        match fs::read(&path) {
            Ok(raw) => Ok(serde_json::from_slice(&raw).ok()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).with_context(|| format!("could not read {}", path.display())),
        }
    }

    /// Writes the tokens, creating the directory if needed.
    ///
    /// Written to a temporary file and renamed, so an interrupted write leaves
    /// the previous tokens intact rather than a truncated file that reads as
    /// "signed out" and forces the whole consent flow again.
    pub fn save(&self) -> Result<()> {
        let dir = dir()?;
        fs::create_dir_all(&dir)
            .with_context(|| format!("could not create {}", dir.display()))?;

        let path = dir.join(TOKEN_FILE);
        let tmp = dir.join(format!("{TOKEN_FILE}.new"));
        let body = serde_json::to_vec_pretty(self).context("could not encode tokens")?;

        fs::write(&tmp, &body).with_context(|| format!("could not write {}", tmp.display()))?;
        // Best effort, and before the rename so the file is never briefly
        // world-readable under its real name. A failure here is not worth
        // refusing the sign-in over.
        let _ = restrict(&tmp);
        fs::rename(&tmp, &path).with_context(|| format!("could not write {}", path.display()))?;
        Ok(())
    }

    /// Removes the stored tokens. Signing out is local: it forgets the grant
    /// rather than revoking it, so the user should also remove the app at
    /// myaccount.google.com if that is what they meant.
    pub fn forget() -> Result<()> {
        let path = dir()?.join(TOKEN_FILE);
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e).with_context(|| format!("could not remove {}", path.display())),
        }
    }
}

/// Seconds since the epoch. A clock before 1970 is not a case worth carrying an
/// error type for; treating it as the epoch makes every token look expired,
/// which fails safe.
fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs()
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

    fn tokens(expires_in: u64) -> Tokens {
        Tokens::new("access".into(), "refresh".into(), expires_in)
    }

    #[test]
    fn a_fresh_token_is_not_expired() {
        assert!(!tokens(3600).is_expired());
    }

    #[test]
    fn a_token_inside_the_margin_is_expired() {
        // Thirty seconds left: still technically valid, but not for long enough
        // to start a request with.
        assert!(tokens(30).is_expired());
        assert!(tokens(0).is_expired());
    }

    #[test]
    fn a_refresh_without_a_new_refresh_token_keeps_the_old_one() {
        // This is the normal case, and getting it wrong means the next refresh
        // has nothing to present and the user is silently signed out.
        let refreshed = tokens(0).refreshed_with("new-access".into(), None, 3600);
        assert_eq!(refreshed.access_token, "new-access");
        assert_eq!(refreshed.refresh_token, "refresh");
        assert!(!refreshed.is_expired());
    }

    #[test]
    fn a_rotated_refresh_token_replaces_the_old_one() {
        let refreshed = tokens(0).refreshed_with("a".into(), Some("rotated".into()), 3600);
        assert_eq!(refreshed.refresh_token, "rotated");
    }

    #[test]
    fn credentials_reject_blank_fields() {
        // A user who created the file but has not filled it in yet should be
        // told that, not handed an empty client id to fail an HTTP call with.
        let creds: Credentials =
            serde_json::from_str(r#"{"client_id":"  ","client_secret":"x"}"#).unwrap();
        assert!(creds.client_id.trim().is_empty());
    }

    #[test]
    fn the_setup_help_names_the_file_it_wants() {
        let rendered = SETUP_HELP.replace("{path}", "C:/somewhere/client.json");
        assert!(rendered.contains("C:/somewhere/client.json"));
        assert!(!rendered.contains("{path}"), "placeholder was left unfilled");
        // `App::report` routes on this: a multi-line message goes to an overlay
        // rather than the one-line status bar, which would hide all but the
        // first instruction.
        assert!(rendered.lines().count() > 1);
        // The two steps a user cannot guess and that silently break the login
        // if missed.
        assert!(rendered.contains("TV and Limited Input devices"));
        assert!(rendered.contains("In production"));
    }

    #[test]
    fn config_dir_is_under_the_platform_base() {
        // Only meaningful when the environment actually names one; CI images
        // sometimes have neither.
        if let Ok(path) = dir() {
            assert!(path.ends_with("mtui"));
        }
    }
}
