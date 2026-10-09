//! One YouTube Music sign-in flow on every desktop.
//!
//! MTUI owns a small persistent webview profile and opens Google's real
//! `music.youtube.com` page in a separate helper process. On Unix that helper
//! is a companion executable, so the player never links WebKitGTK/WKWebView
//! and pays their memory cost only while the sign-in window exists. Windows
//! retains the single-file release: the main executable also handles the
//! private helper invocation there.

#[cfg(not(windows))]
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{
    Mutex,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use crate::config::{Cookies, Import};
use crate::source::sapisid;

pub mod renewal;
use crate::session_protocol as protocol;

static AUTHENTICATION_FAILED: AtomicBool = AtomicBool::new(false);

pub enum RenewalFailure {
    SignInRequired,
    Temporary(String),
}

pub fn classify_renewal_failure(message: String) -> RenewalFailure {
    if message.contains(protocol::SIGN_IN_REQUIRED) {
        RenewalFailure::SignInRequired
    } else {
        RenewalFailure::Temporary(message)
    }
}

/// Ignore failures from a session which has already been replaced or logged out.
pub fn authentication_failed(cookies: &Cookies) {
    let Ok(_lock) = SESSION_COMMIT.lock() else { return; };
    if Cookies::available().ok().flatten().is_some_and(|current| current.header() == cookies.header()) {
        AUTHENTICATION_FAILED.store(true, Ordering::SeqCst);
    }
}

pub fn take_authentication_failure() -> bool {
    AUTHENTICATION_FAILED.swap(false, Ordering::SeqCst)
}

// Import commits and logout share a boundary. A helper which finishes after
// logout, a newer sign-in, or application shutdown cannot restore credentials.
static SESSION_COMMIT: Mutex<()> = Mutex::new(());

#[cfg(windows)]
#[path = "session_helper.rs"]
mod helper;

const RECOVER_ARG: &str = "--recover-session";
const SILENT_ARG: &str = "--silent-session-renewal";
const HELPER_TIMEOUT: Duration = Duration::from_secs(60);
#[cfg(not(windows))]
const PROFILE_ARG: &str = "--profile";
#[cfg(windows)]
const HELPER_ARG: &str = "--mtui-music-sign-in-helper";
#[cfg(not(windows))]
const HELPER_NAME: &str = "mtui-sign-in";
const SESSION_NAME: &str = "MTUI sign-in window";

/// Recognises the private child-process invocation before terminal setup.
#[cfg(windows)]
pub fn helper_request() -> Option<(bool, bool)> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    args.iter()
        .any(|arg| arg == std::ffi::OsStr::new(HELPER_ARG))
        .then(|| {
            (
                args.iter()
                    .any(|arg| arg == std::ffi::OsStr::new(RECOVER_ARG)),
                args.iter()
                    .any(|arg| arg == std::ffi::OsStr::new(SILENT_ARG)),
            )
        })
}

/// Runs the embedded Windows helper, where the process main thread is free for
/// the native window event loop. Its stdout is a private pipe owned by the
/// parent MTUI process.
#[cfg(windows)]
pub fn run_helper(recover: bool, silent: bool) -> Result<()> {
    let profile = crate::config::dir()?.join("webview");
    let header = helper::run(profile, recover, silent)?;
    println!("{header}");
    Ok(())
}

/// Starts the cross-platform sign-in helper and waits for its session. Called
/// on a worker thread, so the terminal remains responsive.
pub fn sign_in(recover: bool, generation: &AtomicU64, expected: u64) -> Result<String> {
    let header = capture_session(recover, false)?;
    commit(&header, generation, expected, None)?;
    Ok(SESSION_NAME.to_string())
}

pub fn renew(generation: &AtomicU64, expected: u64) -> Result<()> {
    let previous = Import::load().context("No saved browser session to renew.")?;
    let header = capture_session(true, true)?;
    commit(&header, generation, expected, Some(&previous))
}

fn capture_session(recover: bool, silent: bool) -> Result<String> {
    let executable = std::env::current_exe().context("could not locate the MTUI executable")?;
    #[cfg(not(windows))]
    let profile = crate::config::dir()?.join("webview");
    #[cfg(windows)]
    let mut process = {
        let mut process = crate::source::command(&executable);
        process.arg(HELPER_ARG);
        process
    };
    #[cfg(not(windows))]
    let mut process = sign_in_command(&executable, &profile)?;
    if recover {
        process.arg(RECOVER_ARG);
    }
    if silent {
        process.arg(SILENT_ARG);
    }
    process
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let output = if silent {
        bounded_output(&mut process, HELPER_TIMEOUT)?
    } else {
        process
            .output()
            .context("could not start the YouTube Music sign-in window")?
    };
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let reason = stderr
            .lines()
            .find(|line| !line.trim().is_empty())
            .unwrap_or("the sign-in window closed without a session")
            .trim();
        bail!("{reason}");
    }

    let header = String::from_utf8(output.stdout)
        .context("the YouTube Music sign-in window returned an invalid session")?;
    let header = header.trim().to_string();
    if Cookies::from_header(&header).is_none() {
        bail!("YouTube Music did not provide a signing cookie");
    }
    Ok(header)
}

fn commit(
    header: &str,
    generation: &AtomicU64,
    expected: u64,
    previous: Option<&Import>,
) -> Result<()> {
    let _lock = SESSION_COMMIT
        .lock()
        .map_err(|_| anyhow::anyhow!("session storage is unavailable"))?;
    let current = previous.and_then(|_| Import::load());
    if generation.load(Ordering::SeqCst) != expected
        || previous.is_some_and(|previous| !unchanged_import(previous, current.as_ref()))
    {
        bail!("Session renewal was cancelled because the account changed.");
    }
    save(header)?;
    AUTHENTICATION_FAILED.store(false, Ordering::SeqCst);
    Ok(())
}

fn unchanged_import(previous: &Import, current: Option<&Import>) -> bool {
    current.is_some_and(|current| current.header == previous.header && current.at == previous.at)
}

/// Automatic renewal has a hard deadline even if browser startup hangs. Drain
/// both pipes concurrently with bounded storage so private cookie output never
/// fills a pipe or enters diagnostics.
fn bounded_output(
    process: &mut std::process::Command,
    timeout: Duration,
) -> Result<std::process::Output> {
    use std::io::Read;
    let mut child = process.spawn().context("could not start session renewal")?;
    let (tx, rx) = std::sync::mpsc::channel();
    let pipes: Vec<Box<dyn Read + Send>> = vec![
        Box::new(child.stdout.take().context("missing session output pipe")?),
        Box::new(child.stderr.take().context("missing session error pipe")?),
    ];
    for (index, mut pipe) in pipes.into_iter().enumerate() {
        let tx = tx.clone();
        std::thread::spawn(move || {
            let result = read_pipe(&mut pipe, if index == 0 { 128 * 1024 } else { 8 * 1024 });
            let _ = tx.send((index, result));
        });
    }
    drop(tx);
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(25)),
            result => {
                let _ = child.kill();
                let _ = child.wait();
                if let Err(error) = result {
                    return Err(error.into());
                }
                bail!(
                    "Automatic session renewal timed out. Use Menu → Account to sign in if needed."
                );
            }
        }
    };
    let mut data = [Vec::new(), Vec::new()];
    for _ in 0..2 {
        let (index, result) = rx
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .context("session renewal did not finish returning its result")?;
        data[index] = result?;
    }
    let [stdout, stderr] = data;
    Ok(std::process::Output {
        status,
        stdout,
        stderr,
    })
}

fn read_pipe(reader: &mut dyn std::io::Read, limit: usize) -> Result<Vec<u8>> {
    let mut data = Vec::new();
    let mut buffer = [0; 4096];
    let mut overflow = false;
    loop {
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        let keep = count.min(limit.saturating_sub(data.len()));
        data.extend_from_slice(&buffer[..keep]);
        overflow |= keep < count;
    }
    if overflow {
        bail!("session helper returned too much data");
    }
    Ok(data)
}

#[cfg(not(windows))]
fn sign_in_command(executable: &Path, profile: &Path) -> Result<std::process::Command> {
    let helper = helper_path(executable);
    if !helper.is_file() {
        bail!(
            "the YouTube Music sign-in helper is missing; install {} beside {}",
            helper.display(),
            executable.display()
        );
    }
    let mut process = std::process::Command::new(helper);
    process.arg(PROFILE_ARG).arg(profile);
    Ok(process)
}

#[cfg(not(windows))]
fn helper_path(executable: &Path) -> PathBuf {
    executable.with_file_name(format!("{HELPER_NAME}{}", std::env::consts::EXE_SUFFIX))
}

fn save(header: &str) -> Result<()> {
    if Cookies::from_header(header).is_none() {
        bail!("YouTube Music did not provide a signing cookie");
    }
    Import {
        browser: SESSION_NAME.to_string(),
        header: header.to_string(),
        at: sapisid::unix_now(),
    }
    .save()
}

/// Removes every local route back into the signed-in Music session.
///
/// The credential files are the logout boundary. A WebView profile that could
/// not be removed is reported as a warning rather than turning a completed
/// logout into a failure; no authenticated request can be made without the
/// files that were removed first.
pub fn sign_out() -> Result<Option<String>> {
    let _lock = SESSION_COMMIT
        .lock()
        .map_err(|_| anyhow::anyhow!("session storage is unavailable"))?;
    Cookies::forget()?;
    AUTHENTICATION_FAILED.store(false, Ordering::SeqCst);
    // Pending reports were collected under the account being left (or before
    // it signed in). A later account must not inherit that listening history.
    let pending_warning = crate::source::journal::forget_pending_reports()
        .err()
        .map(|error| format!("could not clear pending playback reports: {error}"));
    let profile = crate::config::dir()?.join("webview");
    match std::fs::remove_dir_all(&profile) {
        Ok(()) => Ok(pending_warning),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(pending_warning),
        Err(error) => {
            let profile_warning = format!(
                "could not clear the sign-in window data at {}: {error}",
                profile.display()
            );
            Ok(Some(match pending_warning {
                Some(pending) => format!("{pending}; {profile_warning}"),
                None => profile_warning,
            }))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn network_failures_cannot_open_an_interactive_sign_in_window() {
        for message in ["HTTP 500", "Automatic session renewal could not verify sign-in; retrying later.", "session renewal timed out"] {
            assert!(matches!(classify_renewal_failure(message.into()), RenewalFailure::Temporary(_)));
        }
        assert!(matches!(classify_renewal_failure(format!("Error: {}", protocol::SIGN_IN_REQUIRED)), RenewalFailure::SignInRequired));
    }

    #[test]
    fn logout_and_newer_sign_in_reject_late_helper_results() {
        let generation = AtomicU64::new(7);
        // The cancelled commit fails before it can write any credential file.
        let error = commit("SAPISID=fixture", &generation, 6, None).unwrap_err();
        assert!(error.to_string().contains("cancelled"));
    }

    #[test]
    fn renewal_cannot_overwrite_a_deleted_or_replaced_import() {
        let imported = Import {
            browser: "fixture".into(),
            header: "SAPISID=a".into(),
            at: 10,
        };
        assert!(unchanged_import(&imported, Some(&imported)));
        assert!(!unchanged_import(&imported, None));
        let replacement = Import {
            header: "SAPISID=b".into(),
            ..imported.clone()
        };
        assert!(!unchanged_import(&imported, Some(&replacement)));
        let refreshed = Import {
            at: 11,
            ..imported.clone()
        };
        assert!(!unchanged_import(&imported, Some(&refreshed)));
    }

    #[test]
    fn private_helper_output_is_bounded() {
        let bytes = vec![b'x'; 129 * 1024];
        assert!(read_pipe(&mut bytes.as_slice(), 128 * 1024).is_err());
        assert_eq!(
            read_pipe(&mut b"fixture".as_slice(), 8).unwrap(),
            b"fixture"
        );
    }

    #[test]
    fn silent_helpers_have_a_hard_deadline() {
        #[cfg(windows)]
        let mut command = {
            let mut command = crate::source::command("powershell.exe");
            command.args([
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "Start-Sleep -Seconds 5",
            ]);
            command
        };
        #[cfg(not(windows))]
        let mut command = {
            let mut command = std::process::Command::new("sh");
            command.args(["-c", "exec sleep 5"]);
            command
        };
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let started = Instant::now();
        let error = bounded_output(&mut command, Duration::from_millis(100)).unwrap_err();
        assert!(error.to_string().contains("timed out"));
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    #[cfg(windows)]
    #[test]
    fn helper_flag_is_private_and_unambiguous() {
        assert!(HELPER_ARG.starts_with("--mtui-"));
        assert_ne!(HELPER_ARG, RECOVER_ARG);
    }

    #[cfg(not(windows))]
    #[test]
    fn companion_lives_beside_the_player() {
        let player = Path::new("/opt/mtui/bin/mtui");
        assert_eq!(
            helper_path(player),
            Path::new("/opt/mtui/bin")
                .join(format!("{HELPER_NAME}{}", std::env::consts::EXE_SUFFIX))
        );
    }
}
