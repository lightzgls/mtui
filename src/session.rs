//! One YouTube Music sign-in flow on every desktop.
//!
//! MTUI owns a small persistent webview profile and opens Google's real
//! `music.youtube.com` page in a separate helper process. Wry supplies the
//! native engine on each platform: WebView2 on Windows, WebKitGTK on Linux and
//! WKWebView on macOS. The user sees the same window and MTUI never receives a
//! password or form value; it reads the resulting YouTube cookies only after
//! Google has completed sign-in.
//!
//! The helper process is important rather than decorative. GTK and AppKit both
//! require their window event loop on the process's main thread, while session
//! setup is requested from MTUI's source worker. Spawning the same executable
//! in helper mode satisfies that rule without moving the terminal UI or audio
//! player onto a GUI event loop.

use std::cell::RefCell;
use std::process::Stdio;
use std::rc::Rc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use tao::dpi::LogicalSize;
use tao::event::{Event, StartCause, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoopBuilder};
use tao::platform::run_return::EventLoopExtRunReturn;
use tao::window::WindowBuilder;
use wry::{WebContext, WebViewBuilder};

use crate::config::{Cookies, Import};
use crate::source::sapisid;

const MUSIC: &str = "https://music.youtube.com/";
const POLL: Duration = Duration::from_secs(1);
const HELPER_ARG: &str = "--mtui-music-sign-in-helper";
const FORCE_ARG: &str = "--clear-session";
const SESSION_NAME: &str = "MTUI sign-in window";

/// Recognises the private child-process invocation before terminal setup.
pub fn helper_request() -> Option<bool> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    args.iter()
        .any(|arg| arg == HELPER_ARG)
        .then(|| args.iter().any(|arg| arg == FORCE_ARG))
}

/// Starts the cross-platform sign-in helper and waits for its persisted
/// session. Called on a worker thread, so the terminal remains responsive.
pub fn sign_in(force: bool) -> Result<String> {
    let executable = std::env::current_exe().context("could not locate the MTUI executable")?;
    let mut process = std::process::Command::new(executable);
    process.arg(HELPER_ARG);
    if force {
        process.arg(FORCE_ARG);
    }
    let output = process
        .stdin(Stdio::null())
        .stdout(Stdio::null())
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
    Cookies::available()
        .context("could not read the imported YouTube Music session")?
        .context("the sign-in window closed without a YouTube Music session")?;
    Ok(SESSION_NAME.to_string())
}

/// Runs inside the private child process, where this is the main thread.
pub fn run_helper(force: bool) -> Result<()> {
    let profile = crate::config::dir()?.join("webview");
    std::fs::create_dir_all(&profile)
        .with_context(|| format!("could not create {}", profile.display()))?;

    let mut event_loop = EventLoopBuilder::new().build();
    let window = WindowBuilder::new()
        .with_title("MTUI - Sign in to YouTube Music")
        .with_inner_size(LogicalSize::new(980.0, 720.0))
        .with_min_inner_size(LogicalSize::new(640.0, 520.0))
        .build(&event_loop)
        .context("could not create the YouTube Music sign-in window")?;
    let mut context = WebContext::new(Some(profile));
    let builder = WebViewBuilder::new_with_web_context(&mut context)
        .with_devtools(false)
        .with_hotkeys_zoom(false);
    #[cfg(target_os = "linux")]
    let webview = {
        use tao::platform::unix::WindowExtUnix;
        use wry::WebViewBuilderExtUnix;

        let container = window
            .default_vbox()
            .context("the Linux sign-in window has no GTK container")?;
        builder
            .build_gtk(container)
            .context("could not start the system webview")?
    };
    #[cfg(not(target_os = "linux"))]
    let webview = builder
        .build(&window)
        .context("could not start the system webview")?;
    if force {
        webview
            .clear_all_browsing_data()
            .context("could not clear the expired YouTube Music session")?;
    }
    webview
        .load_url(MUSIC)
        .context("could not open YouTube Music")?;

    let outcome: Rc<RefCell<Option<Result<()>>>> = Rc::new(RefCell::new(None));
    let result = Rc::clone(&outcome);
    event_loop.run_return(move |event, _, control_flow| {
        *control_flow = ControlFlow::WaitUntil(Instant::now() + POLL);
        match event {
            Event::NewEvents(StartCause::ResumeTimeReached { .. }) => match capture(&webview) {
                Ok(Some(header)) => {
                    *result.borrow_mut() = Some(save(&header));
                    *control_flow = ControlFlow::Exit;
                }
                Ok(None) => {}
                Err(error) => {
                    *result.borrow_mut() = Some(Err(error));
                    *control_flow = ControlFlow::Exit;
                }
            },
            Event::WindowEvent {
                event: WindowEvent::CloseRequested,
                ..
            } => {
                *result.borrow_mut() =
                    Some(Err(anyhow::anyhow!("YouTube Music sign-in was cancelled")));
                *control_flow = ControlFlow::Exit;
            }
            _ => {}
        }
    });

    outcome
        .borrow_mut()
        .take()
        .unwrap_or_else(|| Err(anyhow::anyhow!("YouTube Music sign-in ended unexpectedly")))
}

fn capture(webview: &wry::WebView) -> Result<Option<String>> {
    let cookies = webview
        .cookies_for_url(MUSIC)
        .context("could not read the YouTube Music session")?;
    let header = cookies
        .iter()
        .map(|cookie| format!("{}={}", cookie.name(), cookie.value()))
        .collect::<Vec<_>>()
        .join("; ");
    Ok(Cookies::from_header(&header).map(|_| header))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helper_flag_is_private_and_unambiguous() {
        assert!(HELPER_ARG.starts_with("--mtui-"));
        assert_ne!(HELPER_ARG, FORCE_ARG);
    }
}
