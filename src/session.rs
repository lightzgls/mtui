//! One YouTube Music sign-in flow on every desktop.
//!
//! MTUI owns a small persistent webview profile and opens Google's real
//! `music.youtube.com` page in a separate helper process. On Unix that helper
//! is a companion executable, so the player never links WebKitGTK/WKWebView
//! and pays their memory cost only while the sign-in window exists. Windows
//! retains the single-file release: the main executable also handles the
//! private helper invocation there.

use std::path::Path;
#[cfg(not(windows))]
use std::path::PathBuf;
use std::process::Stdio;

use anyhow::{Context, Result, bail};

use crate::config::{Cookies, Import};
use crate::source::sapisid;

#[cfg(windows)]
#[path = "session_helper.rs"]
mod helper;

const FORCE_ARG: &str = "--clear-session";
#[cfg(not(windows))]
const PROFILE_ARG: &str = "--profile";
#[cfg(windows)]
const HELPER_ARG: &str = "--mtui-music-sign-in-helper";
#[cfg(not(windows))]
const HELPER_NAME: &str = "mtui-sign-in";
const SESSION_NAME: &str = "MTUI sign-in window";

/// Recognises the private child-process invocation before terminal setup.
#[cfg(windows)]
pub fn helper_request() -> Option<bool> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    args.iter()
        .any(|arg| arg == HELPER_ARG)
        .then(|| args.iter().any(|arg| arg == FORCE_ARG))
}

/// Runs the embedded Windows helper, where the process main thread is free for
/// the native window event loop. Its stdout is a private pipe owned by the
/// parent MTUI process.
#[cfg(windows)]
pub fn run_helper(force: bool) -> Result<()> {
    let profile = crate::config::dir()?.join("webview");
    let header = helper::run(profile, force)?;
    println!("{header}");
    Ok(())
}

/// Starts the cross-platform sign-in helper and waits for its session. Called
/// on a worker thread, so the terminal remains responsive.
pub fn sign_in(force: bool) -> Result<String> {
    let executable = std::env::current_exe().context("could not locate the MTUI executable")?;
    #[cfg(not(windows))]
    let profile = crate::config::dir()?.join("webview");
    #[cfg(windows)]
    let mut process = {
        let mut process = std::process::Command::new(&executable);
        process.arg(HELPER_ARG);
        process
    };
    #[cfg(not(windows))]
    let mut process = sign_in_command(&executable, &profile)?;
    if force {
        process.arg(FORCE_ARG);
    }
    let output = process
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .context("could not start the YouTube Music sign-in window")?;
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
    save(header.trim())?;
    Cookies::available()
        .context("could not read the imported YouTube Music session")?
        .context("the sign-in window closed without a YouTube Music session")?;
    Ok(SESSION_NAME.to_string())
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
    Cookies::forget()?;
    let profile = crate::config::dir()?.join("webview");
    match std::fs::remove_dir_all(&profile) {
        Ok(()) => Ok(None),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Ok(Some(format!(
            "could not clear the sign-in window data at {}: {error}",
            profile.display()
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    #[test]
    fn helper_flag_is_private_and_unambiguous() {
        assert!(HELPER_ARG.starts_with("--mtui-"));
        assert_ne!(HELPER_ARG, FORCE_ARG);
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
