//! Native YouTube Music sign-in window.
//!
//! This module is compiled into the Unix companion executable and into the
//! Windows player. Keep it independent of MTUI's other modules: that boundary
//! is what lets the ordinary Unix player avoid linking the webview runtime.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tao::dpi::LogicalSize;
use tao::event::{Event, StartCause, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoopBuilder};
use tao::platform::run_return::EventLoopExtRunReturn;
use tao::window::WindowBuilder;
use wry::{WebContext, WebViewBuilder};

const MUSIC: &str = "https://music.youtube.com/";
const POLL: Duration = Duration::from_secs(1);

pub fn run(profile: PathBuf, force: bool) -> Result<String> {
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

    let outcome: Rc<RefCell<Option<Result<String>>>> = Rc::new(RefCell::new(None));
    let result = Rc::clone(&outcome);
    event_loop.run_return(move |event, _, control_flow| {
        *control_flow = ControlFlow::WaitUntil(Instant::now() + POLL);
        match event {
            Event::NewEvents(StartCause::ResumeTimeReached { .. }) => match capture(&webview) {
                Ok(Some(header)) => {
                    *result.borrow_mut() = Some(Ok(header));
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
    Ok(has_signing_cookie(&header).then_some(header))
}

fn has_signing_cookie(header: &str) -> bool {
    header.split(';').any(|pair| {
        let Some((name, value)) = pair.split_once('=') else {
            return false;
        };
        matches!(name.trim(), "SAPISID" | "__Secure-3PAPISID") && !value.trim().is_empty()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn waits_for_a_cookie_that_can_sign_music_requests() {
        assert!(!has_signing_cookie("YSC=x; PREF=y"));
        assert!(has_signing_cookie("YSC=x; SAPISID=secret"));
        assert!(has_signing_cookie("__Secure-3PAPISID=secret; PREF=y"));
        assert!(!has_signing_cookie("SAPISID=  "));
    }
}
