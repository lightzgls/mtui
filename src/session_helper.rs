//! Native YouTube Music sign-in window.
//!
//! This module is compiled into the Unix companion executable and into the
//! Windows player. Keep it independent of MTUI's other modules: that boundary
//! is what lets the ordinary Unix player avoid linking the webview runtime.

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tao::dpi::LogicalSize;
use tao::event::{Event, StartCause, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoopBuilder};
use tao::platform::run_return::EventLoopExtRunReturn;
use tao::window::WindowBuilder;
use wry::{PageLoadEvent, WebContext, WebViewBuilder};

const MUSIC: &str = "https://music.youtube.com/";
const POLL: Duration = Duration::from_secs(1);
/// Give response cookies a moment to land after the main Music document has
/// finished. Reading the profile immediately can return the snapshot that made
/// recovery necessary instead of the cookies from the page that just loaded.
const COOKIE_SETTLE: Duration = Duration::from_millis(500);
/// A recovery starts out of sight because a healthy persistent profile can
/// renew itself without asking anything of the user. If navigation stalls or
/// lands outside Music, reveal it so an actual Google reauthentication is not
/// hidden behind the player.
const RECOVERY_REVEAL: Duration = Duration::from_secs(8);

pub fn run(profile: PathBuf, recover: bool) -> Result<String> {
    std::fs::create_dir_all(&profile)
        .with_context(|| format!("could not create {}", profile.display()))?;

    let mut event_loop = EventLoopBuilder::new().build();
    let window = WindowBuilder::new()
        .with_title("MTUI - Sign in to YouTube Music")
        .with_inner_size(LogicalSize::new(980.0, 720.0))
        .with_min_inner_size(LogicalSize::new(640.0, 520.0))
        .with_visible(!recover)
        .build(&event_loop)
        .context("could not create the YouTube Music sign-in window")?;
    let mut context = WebContext::new(Some(profile));
    let loaded_at = Rc::new(Cell::new(None));
    let page_loaded_at = Rc::clone(&loaded_at);
    let builder = WebViewBuilder::new_with_web_context(&mut context)
        .with_devtools(false)
        .with_hotkeys_zoom(false)
        .with_on_page_load_handler(move |event, url| {
            if matches!(event, PageLoadEvent::Finished) && is_music_url(&url) {
                page_loaded_at.set(Some(Instant::now()));
            }
        });
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
    webview
        .load_url(MUSIC)
        .context("could not open YouTube Music")?;

    let outcome: Rc<RefCell<Option<Result<String>>>> = Rc::new(RefCell::new(None));
    let result = Rc::clone(&outcome);
    let started = Instant::now();
    let mut visible = !recover;
    event_loop.run_return(move |event, _, control_flow| {
        *control_flow = ControlFlow::WaitUntil(Instant::now() + POLL);
        match event {
            Event::NewEvents(StartCause::ResumeTimeReached { .. }) => {
                let now = Instant::now();
                if ready_to_capture(loaded_at.get(), now) {
                    match capture(&webview) {
                        Ok(Some(header)) => {
                            *result.borrow_mut() = Some(Ok(header));
                            *control_flow = ControlFlow::Exit;
                        }
                        Ok(None) => reveal(&window, &mut visible),
                        Err(error) => {
                            *result.borrow_mut() = Some(Err(error));
                            *control_flow = ControlFlow::Exit;
                        }
                    }
                } else if recover && !visible && now.duration_since(started) >= RECOVERY_REVEAL {
                    reveal(&window, &mut visible);
                }
            }
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

fn is_music_url(url: &str) -> bool {
    url == MUSIC.trim_end_matches('/') || url.starts_with(MUSIC)
}

fn ready_to_capture(loaded_at: Option<Instant>, now: Instant) -> bool {
    loaded_at.is_some_and(|loaded| now.duration_since(loaded) >= COOKIE_SETTLE)
}

fn reveal(window: &tao::window::Window, visible: &mut bool) {
    if !*visible {
        window.set_visible(true);
        window.set_focus();
        *visible = true;
    }
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

    #[test]
    fn only_a_completed_music_page_can_release_cookies() {
        let now = Instant::now();
        assert!(!ready_to_capture(None, now));
        assert!(!ready_to_capture(Some(now), now));
        assert!(ready_to_capture(Some(now - COOKIE_SETTLE), now));
        assert!(is_music_url(MUSIC));
        assert!(is_music_url("https://music.youtube.com/library"));
        assert!(!is_music_url("https://accounts.google.com/"));
        assert!(!is_music_url("https://music.youtube.com.evil.example/"));
    }
}
