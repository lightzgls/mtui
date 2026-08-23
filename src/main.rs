#![cfg_attr(all(windows, not(test)), windows_subsystem = "windows")]

//! MTUI -- a terminal music player built to stay small in memory.
//!
//! This thread renders and reads input; a player thread owns audio, and the
//! source workers own everything that touches the network or spawns yt-dlp.
//! Nothing that can block for more than a frame happens here.
//!
//! The program has two faces and spends its life in one or the other: the
//! terminal interface, and -- once `B` has handed the terminal back -- an icon
//! in the notification area with the queue still playing behind it. Both are
//! driven by the same [`App`], which is what makes the switch cost nothing:
//! going into the background stops drawing, not playing.

mod app;
mod art;
mod config;
mod console;
mod diagnostics;
mod discord;
mod graphics;
mod player;
mod session;
mod sixel;
mod source;
mod tray;
mod ui;

#[cfg(windows)]
use std::io;
use std::io::Write;
use std::time::Duration;

use anyhow::{Context, Result};
use crossterm::cursor::{MoveTo, RestorePosition, SavePosition};
use crossterm::event::Event;
use crossterm::terminal::{EnterAlternateScreen, LeaveAlternateScreen};
#[cfg(not(windows))]
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use crossterm::{execute, queue};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
#[cfg(windows)]
use ratatui::backend::{Backend, ClearType, WindowSize};
#[cfg(windows)]
use ratatui::buffer::Cell;
#[cfg(windows)]
use ratatui::layout::{Position, Rect, Size};
#[cfg(windows)]
use ratatui::{TerminalOptions, Viewport};
#[cfg(windows)]
use unicode_width::UnicodeWidthStr;

use app::App;
use player::Player;
use source::worker::SourceWorker;
use tray::Tray;

/// Redraw cadence when no input arrives. Fast enough for a smooth position
/// clock, slow enough that an idle player is not spinning the CPU.
const TICK: Duration = Duration::from_millis(200);

/// Cadence while a request is outstanding. A prefetched track resolves from
/// cache almost instantly, and waiting out a full [`TICK`] to notice would be
/// most of what latency it has left.
const BUSY_TICK: Duration = Duration::from_millis(50);

/// Cadence with no window up.
///
/// Nothing is being drawn, so the only work left is advancing the queue and
/// warming the track after it -- neither of which anyone is watching a clock
/// for. A fifth of a second of slack costs nothing audible and keeps a
/// backgrounded player off the CPU.
const IDLE_TICK: Duration = Duration::from_millis(500);

fn main() -> Result<()> {
    #[cfg(windows)]
    if console::is_host() {
        diagnostics::init();
        diagnostics::info("console", "helper started");
        let result = console::run_host();
        match &result {
            Ok(()) => diagnostics::info("console", "helper stopped"),
            Err(error) => diagnostics::error("console", &format!("helper failed: {error:#}")),
        }
        return result;
    }
    #[cfg(windows)]
    console::prepare_parent();

    diagnostics::init();
    diagnostics::info(
        "app",
        &format!("MTUI {} started", env!("CARGO_PKG_VERSION")),
    );

    if let Err(err) = start() {
        diagnostics::error("app", &format!("fatal error: {err:#}"));
        console::report_error(&format!("{err:#}"));
        return Err(err);
    }
    diagnostics::info("app", "clean shutdown");
    Ok(())
}

fn start() -> Result<()> {
    let settings = config::Settings::load();
    let keep_tray = settings.start_in_tray;
    // The Windows-subsystem binary starts without a console. Opening one here
    // makes the UI the normal startup state; the tray icon is an additional
    // entry point, and `B` is still the explicit action that hides the UI.
    #[cfg(windows)]
    console::attach().context("could not open the startup terminal")?;

    let startup_tray = if keep_tray {
        Tray::spawn().ok().and_then(|mut tray| {
            tray.show("MTUI is starting ...", settings.icon_theme)
                .ok()?;
            Some(tray)
        })
    } else {
        None
    };

    run(settings, startup_tray)
}

fn run(settings: config::Settings, mut tray: Option<Tray>) -> Result<()> {
    // Fail before touching the terminal, so a missing dependency prints a plain
    // message instead of appearing as a blank alternate screen. On a first run
    // this is also where yt-dlp gets fetched, which prints progress -- another
    // reason it has to happen before the alternate screen is up.
    let yt = source::bootstrap::locate().context("yt-dlp is required but could not be obtained")?;
    let yt = source::bootstrap::with_js_runtime(yt)
        .context("the optional JavaScript runtime could not be prepared")?;

    // Asked before the alternate screen is up, so the replies cannot land in
    // the middle of a drawn frame.
    let graphics = graphics::detect();

    let player = Player::spawn()?;
    let source = SourceWorker::spawn(yt)?;
    let mut app = App::new(player, source, graphics, settings);

    // The two faces, alternating until something asks to quit. The terminal one
    // runs first because that is how the program is started; after that, each
    // returns when the user has asked for the other.
    loop {
        run_foreground(&mut app, &mut tray)?;
        if app.should_quit {
            break;
        }
        run_background(&mut app, &mut tray)?;
        if app.should_quit {
            break;
        }
    }

    // The track that was playing when the user quit. Written here rather than
    // left to the library worker, which is not joined on the way out -- see
    // `App::flush_listening`.
    app.flush_listening();
    Ok(())
}

/// The terminal interface. Returns when the user quits or asks to background.
fn run_foreground(app: &mut App, tray: &mut Option<Tray>) -> Result<()> {
    app.wants_foreground = false;

    if console::closed() {
        app.wants_background = true;
        return Ok(());
    }

    // Each foreground owner gets duplicate helper pipes. The graphics probe
    // has already released its copies before normal input begins.
    let mut foreground = match Foreground::open() {
        Ok(foreground) => foreground,
        Err(_) if console::closed() => {
            app.wants_background = true;
            return Ok(());
        }
        Err(err) => return Err(err),
    };
    let mut input = match console::Input::open().context("could not open foreground input") {
        Ok(input) => input,
        Err(_) if console::closed() => {
            app.wants_background = true;
            return Ok(());
        }
        Err(err) => return Err(err),
    };

    (|| -> Result<()> {
        while !app.should_quit && !app.wants_background {
            app.poll_source();
            // Before the prefetch: a track that just ended starts the next one
            // here, and the prefetch warms the one after it.
            app.tick_playback();
            app.tick_page();
            app.tick_prefetch();
            app.tick_presence();
            sync_foreground_tray(app, tray);
            foreground
                .terminal
                .draw(|frame| ui::render(frame, app))
                .context("could not draw the foreground")?;

            // Sixel pixels live outside ratatui's buffer, so nothing it draws
            // can erase them. Clear and rebuild in the same breath.
            if app.image_needs_clearing() {
                foreground
                    .terminal
                    .clear()
                    .context("could not clear the foreground")?;
                app.invalidate_image();
                foreground
                    .terminal
                    .draw(|frame| ui::render(frame, app))
                    .context("could not redraw the foreground")?;
            }
            paint_cover(app, foreground.terminal.backend_mut())
                .context("could not draw the cover")?;
            if console::closed() {
                // Unlike explicit B, X always backgrounds, including idle.
                app.wants_background = true;
                break;
            }

            // Block for input, but wake so clocks and workers advance.
            let tick = if app.awaiting() { BUSY_TICK } else { TICK };
            if let Some(event) = input
                .next(tick)
                .context("could not poll foreground input")?
            {
                match event {
                    Event::Key(key) if key.is_press() => app.handle_key(key)?,
                    Event::Resize(width, height) => {
                        #[cfg(not(windows))]
                        let _ = (width, height);
                        #[cfg(windows)]
                        {
                            foreground
                                .terminal
                                .backend_mut()
                                .set_size(Size { width, height });
                            foreground
                                .terminal
                                .resize(Rect::new(0, 0, width, height))
                                .context("could not resize the foreground")?;
                        }
                        app.invalidate_image();
                    }
                    _ => {}
                }
            }
            if console::closed() {
                app.wants_background = true;
            }
        }
        Ok(())
    })()
}

#[cfg(windows)]
type ForegroundBackend = WindowsBackend;
#[cfg(not(windows))]
type ForegroundBackend = CrosstermBackend<console::Output>;

struct Foreground {
    terminal: Terminal<ForegroundBackend>,
}

impl Foreground {
    fn open() -> Result<Self> {
        #[cfg(windows)]
        let (width, height) = console::size().context("could not read the foreground size")?;
        let mut output = console::output().context("could not open the foreground output")?;
        enable_foreground_input().context("could not enable foreground input")?;
        if let Err(err) = execute!(output, EnterAlternateScreen) {
            disable_foreground_input();
            return Err(err).context("could not enter the alternate screen");
        }
        #[cfg(windows)]
        let terminal = Terminal::with_options(
            WindowsBackend::new(output, Size { width, height }),
            TerminalOptions {
                viewport: Viewport::Fixed(Rect::new(0, 0, width, height)),
            },
        );
        #[cfg(not(windows))]
        let terminal = Terminal::new(CrosstermBackend::new(output));
        let terminal = match terminal {
            Ok(terminal) => terminal,
            Err(err) => {
                if let Ok(mut output) = console::output() {
                    let _ = execute!(output, LeaveAlternateScreen);
                }
                disable_foreground_input();
                return Err(err).context("could not create the terminal renderer");
            }
        };
        Ok(Self { terminal })
    }
}

#[cfg(windows)]
struct WindowsBackend {
    inner: CrosstermBackend<console::Output>,
    size: Size,
    cursor: Position,
}

#[cfg(windows)]
impl WindowsBackend {
    fn new(output: console::Output, size: Size) -> Self {
        Self {
            inner: CrosstermBackend::new(output),
            size,
            cursor: Position::ORIGIN,
        }
    }

    fn set_size(&mut self, size: Size) {
        self.size = size;
        self.cursor.x = self.cursor.x.min(size.width.saturating_sub(1));
        self.cursor.y = self.cursor.y.min(size.height.saturating_sub(1));
    }
}

#[cfg(windows)]
impl Write for WindowsBackend {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.inner.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        Write::flush(&mut self.inner)
    }
}

#[cfg(windows)]
impl Backend for WindowsBackend {
    type Error = io::Error;

    fn draw<'a, I>(&mut self, content: I) -> io::Result<()>
    where
        I: Iterator<Item = (u16, u16, &'a Cell)>,
    {
        let mut cursor = self.cursor;
        let size = self.size;
        let tracked = content.inspect(|(x, y, cell)| {
            let width = u16::try_from(cell.symbol().width())
                .unwrap_or(u16::MAX)
                .max(1);
            let next = x.saturating_add(width);
            cursor = if size.width != 0 && next >= size.width {
                Position {
                    x: 0,
                    y: y.saturating_add(1).min(size.height.saturating_sub(1)),
                }
            } else {
                Position { x: next, y: *y }
            };
        });
        Backend::draw(&mut self.inner, tracked)?;
        self.cursor = cursor;
        Ok(())
    }

    fn append_lines(&mut self, n: u16) -> io::Result<()> {
        Backend::append_lines(&mut self.inner, n)?;
        self.cursor.x = 0;
        self.cursor.y = self
            .cursor
            .y
            .saturating_add(n)
            .min(self.size.height.saturating_sub(1));
        Ok(())
    }

    fn hide_cursor(&mut self) -> io::Result<()> {
        Backend::hide_cursor(&mut self.inner)
    }

    fn show_cursor(&mut self) -> io::Result<()> {
        Backend::show_cursor(&mut self.inner)
    }

    fn get_cursor_position(&mut self) -> io::Result<Position> {
        Ok(self.cursor)
    }

    fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> io::Result<()> {
        let position = position.into();
        Backend::set_cursor_position(&mut self.inner, position)?;
        self.cursor = position;
        Ok(())
    }

    fn clear(&mut self) -> io::Result<()> {
        Backend::clear(&mut self.inner)
    }

    fn clear_region(&mut self, clear_type: ClearType) -> io::Result<()> {
        Backend::clear_region(&mut self.inner, clear_type)
    }

    fn size(&self) -> io::Result<Size> {
        Ok(self.size)
    }

    fn window_size(&mut self) -> io::Result<WindowSize> {
        Ok(WindowSize {
            columns_rows: self.size,
            pixels: Size {
                width: 0,
                height: 0,
            },
        })
    }

    fn flush(&mut self) -> io::Result<()> {
        Backend::flush(&mut self.inner)
    }
}

impl Drop for Foreground {
    fn drop(&mut self) {
        let _ = execute!(self.terminal.backend_mut(), LeaveAlternateScreen);
        disable_foreground_input();
    }
}

#[cfg(windows)]
fn enable_foreground_input() -> Result<()> {
    // Raw mode belongs to the helper's CONIN$, configured before its handshake.
    Ok(())
}

#[cfg(not(windows))]
fn enable_foreground_input() -> Result<()> {
    enable_raw_mode()?;
    Ok(())
}

#[cfg(windows)]
fn disable_foreground_input() {}

#[cfg(not(windows))]
fn disable_foreground_input() {
    let _ = disable_raw_mode();
}

fn sync_foreground_tray(app: &mut App, tray: &mut Option<Tray>) {
    if app.start_in_tray && tray.is_none() {
        match Tray::spawn() {
            Ok(icon) => *tray = Some(icon),
            Err(err) => {
                diagnostics::error("tray", &format!("could not create icon: {err:#}"));
                app.start_in_tray = false;
                app.status = format!("could not create the tray icon: {err:#}");
                if let Err(save) = (config::Settings {
                    start_in_tray: false,
                    icon_theme: app.icon_theme(),
                    cover_style: app.cover_style(),
                })
                .save()
                {
                    app.status
                        .push_str(&format!("; could not save settings: {save:#}"));
                }
                return;
            }
        }
    } else if !app.start_in_tray
        && let Some(mut icon) = tray.take()
    {
        icon.hide();
    }

    let Some(icon) = tray.as_mut() else {
        return;
    };
    if let Err(err) = icon.show(&app.tray_tip(), app.icon_theme()) {
        diagnostics::error("tray", &format!("could not show icon: {err:#}"));
        app.start_in_tray = false;
        app.status = format!("could not show the tray icon: {err:#}");
        icon.hide();
        *tray = None;
        if let Err(save) = (config::Settings {
            start_in_tray: false,
            icon_theme: app.icon_theme(),
            cover_style: app.cover_style(),
        })
        .save()
        {
            app.status
                .push_str(&format!("; could not save settings: {save:#}"));
        }
        return;
    }
    if let Some(command) = icon.next_command(Duration::ZERO)
        && command != tray::TrayCommand::Show
    {
        app.handle_tray(command);
    }
}

/// Detached: no console, no drawing, an icon in the notification area and the
/// queue playing on behind it. Returns when the user asks for the interface
/// back or quits from the icon's menu.
///
/// Every failure on the way in leaves the program exactly where it was -- still
/// in the foreground, with the reason in the status bar. Backgrounding is a
/// convenience, and half of one, with the icon missing or the console gone, is
/// a player the user cannot reach at all.
fn run_background(app: &mut App, tray: &mut Option<Tray>) -> Result<()> {
    app.wants_background = false;

    if tray.is_none() {
        *tray = match Tray::spawn() {
            Ok(tray) => Some(tray),
            Err(err) => {
                diagnostics::error(
                    "tray",
                    &format!("could not create background icon: {err:#}"),
                );
                app.status = format!("{err:#}");
                if console::closed() {
                    console::detach()?;
                    console::attach()
                        .context("could not restore the terminal after tray failure")?;
                }
                return Ok(());
            }
        };
    }
    let icon = tray.as_mut().expect("the tray was created above");
    if let Err(err) = icon.show(&app.tray_tip(), app.icon_theme()) {
        diagnostics::error("tray", &format!("could not enter background: {err:#}"));
        app.status = format!("{err:#}");
        if console::closed() {
            console::detach()?;
            console::attach().context("could not restore the terminal after tray failure")?;
        }
        return Ok(());
    }

    // Said on the terminal that is about to be given up, since it is the last
    // thing this window will ever show and the icon is small.
    if let Ok(mut out) = console::output() {
        let _ = writeln!(
            out,
            "MTUI is running in the background. Its icon is in the notification area, \
             by the clock -- click it to bring this back, or right-click for controls. \
             This window can be closed."
        );
        let _ = out.flush();
    }

    if let Err(err) = console::detach() {
        diagnostics::error("app", &format!("could not detach console: {err:#}"));
        app.status = format!("{err:#}");
        return Ok(());
    }

    while !app.should_quit && !app.wants_foreground {
        app.poll_source();
        app.tick_playback();
        app.tick_prefetch();
        // Unlike `tick_page` below, this one *is* kept: the Discord card is the
        // one part of a backgrounded player other people can still see, so
        // letting go of the terminal must not freeze it on whatever was playing
        // at the time.
        app.tick_presence();
        // Not `tick_page`: lyrics and comments are panels of a page nobody can
        // see, and fetching them here would spend a request per track on
        // something that is thrown away when the track changes.
        if let Err(err) = icon.show(&app.tray_tip(), app.icon_theme()) {
            diagnostics::error("tray", &format!("background icon lost: {err:#}"));
            app.status = format!("the tray icon was lost: {err:#}");
            app.wants_foreground = true;
            continue;
        }

        let tick = if app.awaiting() { BUSY_TICK } else { IDLE_TICK };
        if let Some(command) = icon.next_command(tick) {
            app.handle_tray(command);
        }
    }

    // Order matters on the way out: the icon goes first, so that a failure to
    // get a console back does not leave the user with neither.
    if !app.start_in_tray {
        icon.hide();
    }
    if app.wants_foreground {
        console::attach().context("could not reopen a terminal window")?;
    }
    if !app.start_in_tray {
        *tray = None;
    }
    Ok(())
}

/// Paints the planned cover as sixel pixels, if it is not already on screen.
///
/// Encoding runs here rather than in the renderer: it costs tens of
/// milliseconds and happens once per track, which is fine between frames and
/// would not be fine inside one.
fn paint_cover(app: &mut App, out: &mut impl Write) -> Result<()> {
    let Some((cover, plan)) = app.image_to_paint() else {
        return Ok(());
    };
    let (width, height) = (u32::from(plan.width), u32::from(plan.height));
    let payload = sixel::encode(&cover.resample(width, height), width, height);

    // Save and restore around the write: drawing an image leaves the cursor
    // after it, and ratatui has already put the cursor where the search box
    // wants it.
    queue!(out, SavePosition, MoveTo(plan.col, plan.row))?;
    out.write_all(&payload)?;
    queue!(out, RestorePosition)?;
    out.flush()?;

    app.mark_painted();
    Ok(())
}
