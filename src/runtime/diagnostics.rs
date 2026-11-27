//! Small, persistent diagnostics for failures that cannot stay on screen.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, Once, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

const LOG_FILE: &str = "mtui.log";
const OLD_LOG_FILE: &str = "mtui.log.1";
const MAX_BYTES: u64 = 1024 * 1024;

static LOG: OnceLock<Mutex<PathBuf>> = OnceLock::new();
static PANIC_HOOK: Once = Once::new();
static YTDLP_VERSION: OnceLock<String> = OnceLock::new();
type Version = (u64, u64, u64);
static JS_RUNTIME: OnceLock<(&'static str, Version)> = OnceLock::new();
static PROVIDER_VERSION: OnceLock<&'static str> = OnceLock::new();
static LAST_PLAYBACK_FAILURE: Mutex<Option<&'static str>> = Mutex::new(None);

pub(crate) fn note_ytdlp_version(version: String) {
    // Accept only a version token. Locally supplied executables can print arbitrary text.
    if version.len() <= 48 && version.as_bytes().first().is_some_and(u8::is_ascii_digit)
        && version.bytes().all(|b| b.is_ascii_alphanumeric() || b".-+_".contains(&b))
    {
        let _ = YTDLP_VERSION.set(version);
    }
}

pub(crate) fn note_js_runtime(name: &'static str, version: Version) {
    let _ = JS_RUNTIME.set((name, version));
}

pub(crate) fn note_provider_version(version: &'static str) {
    let _ = PROVIDER_VERSION.set(version);
}

// Callers pass a fixed summary, never raw error text.
pub(crate) fn note_playback_failure(failure: &'static str) {
    if let Ok(mut latest) = LAST_PLAYBACK_FAILURE.lock() {
        *latest = Some(failure);
    }
}

pub(crate) fn report() -> String {
    let runtime = JS_RUNTIME.get().map_or_else(|| "unavailable".into(), |(name, (major, minor, patch))| {
        format!("{name} {major}.{minor}.{patch}")
    });
    let latest = LAST_PLAYBACK_FAILURE.lock().ok().and_then(|latest| *latest);
    format!(
        "MTUI {}\nOS: {} ({})\nyt-dlp: {}\nJavaScript runtime: {}\nPO token provider: {}\nLatest playback error: {}",
        env!("CARGO_PKG_VERSION"), std::env::consts::OS, std::env::consts::ARCH,
        YTDLP_VERSION.get().map_or("unknown", String::as_str), runtime,
        PROVIDER_VERSION.get().copied().unwrap_or("unavailable"),
        latest.unwrap_or("None in this session"),
    )
}

/// Prepares the log and installs a process-wide hook for panics on every thread.
pub fn init() {
    if let Ok(dir) = crate::runtime::config::dir()
        && fs::create_dir_all(&dir).is_ok()
    {
        let path = dir.join(LOG_FILE);
        let _ = rotate(&path);
        let _ = LOG.set(Mutex::new(path));
    }

    PANIC_HOOK.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |panic| {
            let message = panic
                .payload()
                .downcast_ref::<&str>()
                .copied()
                .or_else(|| panic.payload().downcast_ref::<String>().map(String::as_str))
                .unwrap_or("non-string panic");
            let location = panic
                .location()
                .map(|at| format!("{}:{}", at.file(), at.line()))
                .unwrap_or_else(|| "unknown location".to_string());
            error("panic", &format!("{location}: {message}"));
            previous(panic);
        }));
    });
}

pub fn info(subsystem: &str, message: &str) {
    write("INFO", subsystem, message);
}

pub fn warn(subsystem: &str, message: &str) {
    write("WARN", subsystem, message);
}

pub fn error(subsystem: &str, message: &str) {
    write("ERROR", subsystem, message);
}

fn write(level: &str, subsystem: &str, message: &str) {
    let Some(log) = LOG.get() else {
        return;
    };
    let Ok(path) = log.lock() else {
        return;
    };
    if path
        .metadata()
        .is_ok_and(|metadata| metadata.len() >= MAX_BYTES)
    {
        let _ = rotate(&path);
    }
    let Ok(mut file) = open_log(&path) else {
        return;
    };
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let subsystem = clean_label(subsystem);
    let message = safe_message(message);
    let _ = writeln!(file, "{timestamp} [{level}] [{subsystem}] {message}");
    let _ = file.flush();
}

fn rotate(path: &Path) -> std::io::Result<()> {
    if !path
        .metadata()
        .is_ok_and(|metadata| metadata.len() >= MAX_BYTES)
    {
        return Ok(());
    }
    let old = path.with_file_name(OLD_LOG_FILE);
    match fs::remove_file(&old) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    fs::rename(path, old)
}

fn open_log(path: &Path) -> std::io::Result<std::fs::File> {
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        options.mode(0o600);
        let file = options.open(path)?;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
        Ok(file)
    }
    #[cfg(not(unix))]
    options.open(path)
}

fn clean_label(label: &str) -> String {
    label
        .chars()
        .filter(|character| character.is_ascii_alphanumeric() || *character == '-')
        .take(16)
        .collect()
}

pub(crate) fn safe_message(message: &str) -> String {
    let lower = message.to_ascii_lowercase();
    if [
        "authorization:",
        "authorization=",
        "bearer ",
        "basic ",
        "cookie:",
        "cookie=",
        "password",
        "api_key",
        "api-key",
        "token=",
        "secret=",
        "set-cookie",
        "access_token",
        "refresh_token",
        "client_secret",
        "sapisid",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
    {
        return "[sensitive details omitted]".to_string();
    }

    message
        .split_whitespace()
        .map(|word| if word.contains("://") { "[url]" } else { word })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diagnostics_flatten_lines_and_redact_urls() {
        assert_eq!(
            safe_message("request failed\nhttps://example.test/private?id=1"),
            "request failed [url]"
        );
    }

    #[test]
    fn diagnostics_drop_credential_bearing_messages() {
        assert_eq!(
            safe_message("refresh_token=do-not-write-this"),
            "[sensitive details omitted]"
        );
        assert_eq!(
            safe_message("request failed with Authorization=Bearer abc123"),
            "[sensitive details omitted]"
        );
        assert_eq!(
            safe_message("cookie SAPISID=do-not-write-this"),
            "[sensitive details omitted]"
        );
    }

    #[test]
    fn subsystem_labels_cannot_break_the_log_shape() {
        assert_eq!(clean_label("player]\nforged"), "playerforged");
    }

    #[test]
    fn a_full_log_is_rotated_to_one_backup() {
        let dir = std::env::temp_dir().join(format!("mtui-log-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(LOG_FILE);
        fs::write(&path, vec![b'x'; MAX_BYTES as usize]).unwrap();

        rotate(&path).unwrap();

        assert!(!path.exists());
        assert_eq!(dir.join(OLD_LOG_FILE).metadata().unwrap().len(), MAX_BYTES);
        fs::remove_dir_all(dir).unwrap();
    }
}
