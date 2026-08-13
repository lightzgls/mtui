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
mod discord;
mod graphics;
mod player;
mod sixel;
mod source;
mod tray;
mod ui;

use std::io::{self, Write};
use std::time::Duration;

use anyhow::{Context, Result};
use crossterm::cursor::{MoveTo, RestorePosition, SavePosition};
use crossterm::event::{self, Event};
use crossterm::queue;

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
    let mut app = App::new(player, source, graphics);

    // The two faces, alternating until something asks to quit. The terminal one
    // runs first because that is how the program is started; after that, each
    // returns when the user has asked for the other.
    loop {
        run_foreground(&mut app)?;
        if app.should_quit {
            break;
        }
        run_background(&mut app)?;
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
fn run_foreground(app: &mut App) -> Result<()> {
    app.wants_foreground = false;

    // `ratatui::run` enables raw mode and the alternate screen, and installs a
    // panic hook that restores the terminal before unwinding. Restoring on the
    // way out is what makes it safe to call again: the console this leaves
    // behind is one a shell can carry on using, whether it belongs to the
    // window MTUI started in or to one `console::attach` opened later.
    ratatui::run(|terminal| -> Result<()> {
        while !app.should_quit && !app.wants_background {
            app.poll_source();
            // Before the prefetch: a track that just ended starts the next one
            // here, and the prefetch wants to be warming the one after it.
            app.tick_playback();
            app.tick_page();
            app.tick_prefetch();
            app.tick_presence();
            terminal.draw(|frame| ui::render(frame, app))?;

            // Sixel pixels live outside ratatui's buffer, so nothing it draws
            // can erase them. When they no longer belong where they are, the
            // screen has to be cleared and rebuilt in the same breath -- a bare
            // clear would leave the user looking at nothing until the next tick.
            if app.image_needs_clearing() {
                terminal.clear()?;
                app.invalidate_image();
                terminal.draw(|frame| ui::render(frame, app))?;
            }
            paint_cover(app)?;

            // Block for input, but wake on TICK so the position clock advances
            // and worker responses are picked up without a keypress.
            let tick = if app.awaiting() { BUSY_TICK } else { TICK };
            if event::poll(tick)? {
                match event::read()? {
                    // Windows reports both press and release; acting on both
                    // would double every keystroke.
                    Event::Key(key) if key.is_press() => app.handle_key(key)?,
                    // A resize can drop the pixels without moving the pane,
                    // so the image is repainted rather than assumed intact.
                    Event::Resize(_, _) => app.invalidate_image(),
                    _ => {}
                }
            }
        }
        Ok(())
    })
}

/// Detached: no console, no drawing, an icon in the notification area and the
/// queue playing on behind it. Returns when the user asks for the interface
/// back or quits from the icon's menu.
///
/// Every failure on the way in leaves the program exactly where it was -- still
/// in the foreground, with the reason in the status bar. Backgrounding is a
/// convenience, and half of one, with the icon missing or the console gone, is
/// a player the user cannot reach at all.
fn run_background(app: &mut App) -> Result<()> {
    app.wants_background = false;

    let mut tray = match Tray::spawn() {
        Ok(tray) => tray,
        Err(err) => {
            app.status = format!("{err:#}");
            return Ok(());
        }
    };
    if let Err(err) = tray.show(&app.tray_tip()) {
        app.status = format!("{err:#}");
        return Ok(());
    }

    // Said on the terminal that is about to be given up, since it is the last
    // thing this window will ever show and the icon is small.
    let mut out = io::stdout();
    let _ = writeln!(
        out,
        "MTUI is playing in the background. Its icon is in the notification area, \
         by the clock -- click it to bring this back, or right-click for controls. \
         This window can be closed."
    );
    let _ = out.flush();

    if let Err(err) = console::detach() {
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
        let _ = tray.show(&app.tray_tip());

        let tick = if app.awaiting() { BUSY_TICK } else { IDLE_TICK };
        if let Some(command) = tray.next_command(tick) {
            app.handle_tray(command);
        }
    }

    // Order matters on the way out: the icon goes first, so that a failure to
    // get a console back does not leave the user with neither.
    tray.hide();
    if app.wants_foreground {
        console::attach().context("could not reopen a terminal window")?;
    }
    Ok(())
}

/// Paints the planned cover as sixel pixels, if it is not already on screen.
///
/// Encoding runs here rather than in the renderer: it costs tens of
/// milliseconds and happens once per track, which is fine between frames and
/// would not be fine inside one.
fn paint_cover(app: &mut App) -> Result<()> {
    let Some((cover, plan)) = app.image_to_paint() else {
        return Ok(());
    };
    let (width, height) = (u32::from(plan.width), u32::from(plan.height));
    let payload = sixel::encode(&cover.resample(width, height), width, height);

    let mut out = io::stdout();
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
