//! Interactive YouTube Music session setup.
//!
//! OAuth cannot authenticate the private Home endpoint. On Windows, MTUI opens
//! the real YouTube Music site in WebView2 and saves its session cookies after
//! Google has completed sign-in. MTUI never receives a password or form value.

#[cfg(windows)]
mod imp {
    use std::cell::RefCell;
    use std::rc::Rc;
    use std::time::{Duration, Instant};

    use anyhow::{Context, Result, bail};
    use tao::dpi::LogicalSize;
    use tao::event::{Event, StartCause, WindowEvent};
    use tao::event_loop::{ControlFlow, EventLoopBuilder};
    use tao::platform::run_return::EventLoopExtRunReturn;
    use tao::platform::windows::EventLoopBuilderExtWindows;
    use tao::window::WindowBuilder;
    use wry::{WebContext, WebViewBuilder};

    use crate::config::{Cookies, Import};
    use crate::source::sapisid;

    const MUSIC: &str = "https://music.youtube.com/";
    const POLL: Duration = Duration::from_secs(1);

    pub fn sign_in(force: bool) -> Result<()> {
        let profile = crate::config::dir()?.join("webview2");
        std::fs::create_dir_all(&profile)
            .with_context(|| format!("could not create {}", profile.display()))?;

        let mut builder = EventLoopBuilder::new();
        builder.with_any_thread(true);
        let mut event_loop = builder.build();
        let window = WindowBuilder::new()
            .with_title("MTUI - Sign in to YouTube Music")
            .with_inner_size(LogicalSize::new(980.0, 720.0))
            .with_min_inner_size(LogicalSize::new(640.0, 520.0))
            .build(&event_loop)
            .context("could not create the YouTube Music sign-in window")?;
        let mut context = WebContext::new(Some(profile));
        let webview = WebViewBuilder::new_with_web_context(&mut context)
            .with_devtools(false)
            .with_hotkeys_zoom(false)
            .build(&window)
            .context("could not start Microsoft Edge WebView2")?;
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
                    Err(err) => {
                        *result.borrow_mut() = Some(Err(err));
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
            browser: "MTUI WebView2".to_string(),
            header: header.to_string(),
            at: sapisid::unix_now(),
        }
        .save()
    }
}

#[cfg(not(windows))]
mod imp {
    use anyhow::{Result, bail};

    pub fn sign_in(_force: bool) -> Result<()> {
        bail!("embedded YouTube Music sign-in is currently available on Windows only")
    }
}

pub use imp::sign_in;
