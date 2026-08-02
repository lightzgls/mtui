//! MTUI -- a terminal music player built to stay small in memory.
//!
//! Three threads: this one renders and reads input, a player thread owns audio,
//! and a source worker runs yt-dlp. Nothing that can block for more than a
//! frame happens here.

mod app;
mod config;
mod graphics;
mod player;
mod sixel;
mod source;
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

/// Redraw cadence when no input arrives. Fast enough for a smooth position
/// clock, slow enough that an idle player is not spinning the CPU.
const TICK: Duration = Duration::from_millis(200);

/// Cadence while a request is outstanding. A prefetched track resolves from
/// cache almost instantly, and waiting out a full [`TICK`] to notice would be
/// most of what latency it has left.
const BUSY_TICK: Duration = Duration::from_millis(50);

fn main() -> Result<()> {
    // Fail before touching the terminal, so a missing dependency prints a plain
    // message instead of appearing as a blank alternate screen. On a first run
    // this is also where yt-dlp gets fetched, which prints progress -- another
    // reason it has to happen before the alternate screen is up.
    let yt = source::bootstrap::locate().context("yt-dlp is required but could not be obtained")?;

    // Asked before the alternate screen is up, so the replies cannot land in
    // the middle of a drawn frame.
    let graphics = graphics::detect();

    let player = Player::spawn()?;
    let source = SourceWorker::spawn(yt)?;
    let mut app = App::new(player, source, graphics);

    // `ratatui::run` enables raw mode and the alternate screen, and installs a
    // panic hook that restores the terminal before unwinding.
    ratatui::run(|terminal| -> Result<()> {
        while !app.should_quit {
            app.poll_source();
            app.tick_prefetch();
            terminal.draw(|frame| ui::render(frame, &mut app))?;

            // Sixel pixels live outside ratatui's buffer, so nothing it draws
            // can erase them. When they no longer belong where they are, the
            // screen has to be cleared and rebuilt in the same breath -- a bare
            // clear would leave the user looking at nothing until the next tick.
            if app.image_needs_clearing() {
                terminal.clear()?;
                app.invalidate_image();
                terminal.draw(|frame| ui::render(frame, &mut app))?;
            }
            paint_cover(&mut app)?;

            // Block for input, but wake on TICK so the position clock advances
            // and worker responses are picked up without a keypress.
            let tick = if app.busy { BUSY_TICK } else { TICK };
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
