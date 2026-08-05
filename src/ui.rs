//! Rendering. Pure: reads [`App`] and draws, never mutates anything except
//! the scroll offset (which depends on the viewport height, known only here).
//!
//! The results list is genuinely virtualized -- only the rows that fit on
//! screen are turned into widget items, so a full result set costs the same to
//! draw as a single screenful.

use std::time::{Duration, Instant};

use ratatui::Frame;
use ratatui::buffer::CellDiffOption;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, List, ListItem, Paragraph};

use crate::app::{
    App, CoverSize, ImagePlan, Mode, NowPlaying, Overlay, Panel, RelatedRow, SignIn, Tab, View,
};
use crate::graphics::Graphics;
use crate::player::{PlayState, Snapshot};
use crate::source::Track;
use crate::source::cover::Cover;
use crate::source::home::{Card, Shelf};
use crate::source::library::Playlist;

/// Width reserved for the right-aligned duration column, including padding.
const DURATION_WIDTH: usize = 8;

/// Width of the artist column, when the row is wide enough to carry one.
const ARTIST_WIDTH: usize = 22;

/// Width of the album column, when the row is wide enough to carry one.
const ALBUM_WIDTH: usize = 20;

/// Title width defended before any metadata column is granted. Below this a
/// title truncates to the point of not identifying a song, which costs more
/// than the artist and album are worth.
const MIN_TITLE_WIDTH: usize = 24;

/// Bounds on the columns given to the cover pane, borders included. The pane is
/// a fraction of the row between these, so a wide terminal spends its extra
/// columns on the picture instead of on whitespace after the titles.
const COVER_MIN_WIDTH: u16 = 34;
const COVER_MAX_WIDTH: u16 = 56;

/// Narrower than this and the cover would squeeze the results into a column too
/// thin to read titles in, so it is dropped instead.
const MIN_WIDTH_WITH_COVER: u16 = 64;

/// Bounds on the player page's panel, borders included.
///
/// The floor is what the tab row measures: four names and their padding come to
/// thirty-two columns, and a panel that cannot show its own tabs cannot be
/// navigated. The ceiling exists so a wide terminal spends its columns on the
/// cover, which is the thing that gets better with more of them.
const PANEL_MIN_WIDTH: u16 = 34;
const PANEL_MAX_WIDTH: u16 = 52;

/// Rows the player page reserves under the cover: the title, the artist, a
/// blank, the progress bar, and the volume bar.
const INFO_HEIGHT: u16 = 5;

/// Widest the volume bar is drawn, however much room the panel has. It is a
/// setting rather than a position, so it is deliberately shorter than the
/// progress bar above it -- two bars of the same length read as two of the
/// same thing.
const VOLUME_BAR_WIDTH: usize = 12;

/// Columns a queue row spends on the `▶` marker and the space either side.
const QUEUE_MARKER_WIDTH: usize = 3;

/// Rows the Up next panel spends on naming the queue before listing it. These
/// do not scroll, so they come off the viewport rather than out of the list.
const UP_NEXT_HEADER: usize = 3;

/// Width of the artist column in a queue row, and the title width defended
/// before one is granted. Same principle as [`MIN_TITLE_WIDTH`] in the results
/// list, at the narrower scale a side panel can afford.
const QUEUE_ARTIST_WIDTH: usize = 14;
const MIN_QUEUE_TITLE: usize = 16;

/// Name width defended in a playlist row before its track count is granted.
/// Same principle as [`MIN_TITLE_WIDTH`]: the label outranks the metadata.
const MIN_PLAYLIST_NAME: usize = 8;

/// Columns the status bar reserves for the key hints. Every hint below has to
/// fit, since the column truncates rather than wraps.
///
/// Six wider than it was, bought from the track name beside it, so that the two
/// keys a playing track adds -- back to its page, and away to the notification
/// area -- can be named without any of the existing hints being dropped for
/// them. The name gives up less than it looks: it is the one thing on this bar
/// that is also written across the pane above in full.
const HINTS_WIDTH: u16 = 52;

/// Named rather than inlined into [`hint_line`] so the test that checks they
/// fit [`HINTS_WIDTH`] measures the strings that are actually drawn.
const HINTS_EDITING: &str = "Enter run   Esc browse   ^L your library";
const HINTS_PLAYLISTS: &str = "Enter open  r reload  x sign out  Esc back";
const HINTS_TRACKS: &str = "/ search  L library  a add  f like  q quit";
/// The landing page. `Esc` and `H` also return here from the track list, and
/// neither fits beside what that line already has to offer -- so this one names
/// what moves the cursor instead, which is the part a grid needs said.
/// `r` is named here and not on the playing variant below, where there is no
/// room for it: it rebuilds the page from different seeds rather than merely
/// re-fetching it, which is not something a user would think to try unprompted.
const HINTS_HOME: &str = "Enter play  hjkl move  r refresh  / search  q quit";
/// The player page. `n`, `p` and the tab keys are what it is for; `Esc` is
/// named because the view is entered without being asked for and the way out of
/// it is the first thing a user looks for. `+-` is named beside the volume bar
/// this page draws, since a bar with no key named for it invites hunting.
const HINTS_PLAYING: &str = "hl tabs  n next  jk scroll  +- vol  Esc back  B tray";
/// Offered in either browse view with no session. `a`, `f`, `x` and `Enter` on
/// a playlist all need one, so naming them to a signed-out user advertises keys
/// that can only answer by starting a sign-in they did not ask for. Every key
/// here works in both views, which is what lets one line serve both.
const HINTS_SIGNED_OUT: &str = "A sign in   / search   q quit";

/// The same four lines for when something is playing and its page is not on
/// screen, each naming the two keys that only mean anything then: `P` back to
/// the page, and `B` away from the terminal altogether.
///
/// A separate set rather than a line appended to the others, because the column
/// is full: every one of these gives up a hint to make the room. What they give
/// up is the least of what they offered -- `q` has `Ctrl-C` behind it, and
/// reload and sign-out are session-long errands -- and what they gain is the
/// only thing on screen that says a playing track can be returned to at all,
/// or left running without a window.
const HINTS_PLAYLISTS_PLAYING: &str = "Enter open  r reload  Esc back  P player  B tray";
const HINTS_TRACKS_PLAYING: &str = "/ search  L library  a add  f like  P player  B tray";
const HINTS_HOME_PLAYING: &str = "Enter play  hjkl move  / search  P player  B tray";
const HINTS_SIGNED_OUT_PLAYING: &str = "A sign in   / search   P player   B tray   q quit";

/// Full height of the sign-in panel, borders included. Below this the compact
/// layout runs instead.
const SIGN_IN_FULL_HEIGHT: u16 = 13;

/// Floor on the sign-in panel's width. Wide enough that the footer hint reads
/// as one line, whatever the URL beside it happens to measure.
const SIGN_IN_MIN_WIDTH: u16 = 48;

/// Ceiling on the width of wrapped error text. Only the URL is allowed to push
/// the panel past this, and only because it cannot be wrapped.
const SIGN_IN_MAX_WIDTH: u16 = 64;

/// Indent shared by the URL and the code box, aligning both under the text of
/// the numbered step above them rather than under its number.
const INDENT: &str = "     ";

/// Columns the code box adds around the code: two borders and two spaces
/// either side.
const CODE_BOX_PADDING: u16 = 6;

/// Width reserved for the "expires in m:ss" column.
const EXPIRY_WIDTH: u16 = 16;

/// Card geometry on the landing page, borders included.
///
/// Two lines of text inside: a title and the subtitle YouTube Music writes
/// under it. A third would fit more of a long title and cost a shelf off the
/// bottom of the page, which is the wrong trade -- the shelves are what the
/// page is for.
const CARD_WIDTH: u16 = 26;
const CARD_HEIGHT: u16 = 4;

/// One shelf: its heading, its cards, and a blank row under them so two shelves
/// do not read as one block of boxes.
const SHELF_HEIGHT: u16 = CARD_HEIGHT + 2;

/// Below this a card holds nothing but borders and an ellipsis, so the page
/// stands down and says so rather than drawing a column of empty boxes.
const MIN_HOME_WIDTH: u16 = 24;

/// Columns the `▌` before a shelf heading occupies, space included.
const MARKER_WIDTH: usize = 2;

/// Heading width defended before the position counter is granted. Same
/// principle as [`MIN_PLAYLIST_NAME`].
const MIN_SHELF_TITLE: usize = 8;

const HINT_HIDE: &str = "  Esc hides this";
const HINT_HIDE_RUNNING: &str = "  Esc hides this -- the sign-in keeps running";

pub fn render(frame: &mut Frame, app: &mut App) {
    let [search_area, main_area, status_area] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(1),
        Constraint::Length(3),
    ])
    .areas(frame.area());

    render_search(frame, app, search_area);

    // The player page lays itself out around the cover rather than beside it,
    // so it reserves its own rect instead of going through `split_cover`.
    let cover_area = if app.view == View::Playing {
        render_player(frame, app, main_area)
    } else {
        let (list_area, cover_area) = split_cover(app, main_area);
        if let Some(area) = list_area {
            match app.view {
                View::Home => render_home(frame, app, area),
                View::Tracks => render_results(frame, app, area),
                View::Playlists => render_playlists(frame, app, area),
                // Handled above.
                View::Playing => {}
            }
        }
        cover_area
    };
    render_status(frame, app, status_area, page_draws_progress(app, main_area));

    // Planned last so it can reach into the finished buffer and mark the cells
    // the image covers.
    //
    // The player page always draws the cover full-size: `Side` exists to frame
    // a picture sitting next to a list, and here it is the subject of the view
    // rather than an annotation on one. The rect it is centred in has already
    // been carved out by `render_player`.
    let size = if app.view == View::Playing {
        CoverSize::Full
    } else {
        app.cover_size
    };
    app.image = match (cover_area, app.cover.as_ref()) {
        // Sixel pixels are not in ratatui's buffer, so an overlay drawn over
        // them would not cover them -- the modal would appear with the picture
        // showing through. Dropping the plan is what puts the existing
        // clear-and-redraw path to work erasing it.
        _ if app.overlay.is_open() => None,
        (Some(area), Some(cover)) if app.graphics.sixel => {
            plan_image(frame, cover, area, app.graphics, size)
        }
        (Some(area), Some(cover)) => {
            render_cover(frame, cover, area, size);
            None
        }
        _ => None,
    };

    // Over everything, including the cover pane's cells.
    render_overlay(frame, app);
}

/// Draws whichever modal is up, centred over the whole window.
fn render_overlay(frame: &mut Frame, app: &App) {
    match &app.overlay {
        Overlay::None => {}
        Overlay::SignIn(phase) => render_sign_in(frame, phase),
        Overlay::AddTo {
            title, selected, ..
        } => render_add_to(frame, app, title, *selected),
        Overlay::Message { body } => render_message(frame, body),
    }
}

/// A multi-line message, sized to its own content.
///
/// Laid out from the text rather than to a fixed box: this carries the OAuth
/// setup procedure, whose lines are long and must not wrap -- a wrapped console
/// URL is one the user cannot retype.
fn render_message(frame: &mut Frame, body: &str) {
    let widest = body.lines().map(display_width).max().unwrap_or(0);
    let area = centred(
        frame.area(),
        (widest as u16).saturating_add(4),
        (body.lines().count() as u16).saturating_add(3),
    );

    let block = Block::bordered()
        .border_style(Style::default().fg(Color::Yellow))
        .title(" mtui ");
    let inner = block.inner(area);
    frame.render_widget(Clear, area);
    frame.render_widget(block, area);

    let mut lines: Vec<Line> = body
        .lines()
        .map(|line| Line::from(Span::raw(format!(" {line}"))))
        .collect();
    lines.push(Line::from(Span::styled(
        " Esc to dismiss",
        Style::default().fg(Color::DarkGray),
    )));

    frame.render_widget(Paragraph::new(lines), inner);
}

/// The sign-in panel, in whichever phase the flow has reached.
fn render_sign_in(frame: &mut Frame, phase: &SignIn) {
    match phase {
        SignIn::Connecting { started } => render_connecting(frame, started.elapsed()),
        SignIn::Waiting {
            user_code,
            url,
            deadline,
        } => render_waiting(frame, user_code, url, *deadline),
        SignIn::Failed { reason } => render_sign_in_failed(frame, reason),
    }
}

/// Border, title and backdrop shared by all three phases.
///
/// Shared so the panel keeps one identity as the flow moves through them: a
/// window that changes size and colour under the user looks like a new window,
/// and this one is asking them to keep reading it.
fn sign_in_panel(frame: &mut Frame, width: u16, height: u16, accent: Color) -> Rect {
    let area = centred(frame.area(), width, height);
    let block = Block::bordered()
        .border_style(Style::default().fg(accent))
        .title(" sign in with Google ");
    let inner = block.inner(area);
    // Blanks the cells first: a modal over a list has to hide it, and ratatui
    // draws widgets onto whatever is already in the buffer.
    frame.render_widget(Clear, area);
    frame.render_widget(block, area);
    inner
}

/// Asked Google for a code, waiting on the round trip.
///
/// A phase of its own rather than an empty panel: it lasts a network round trip
/// and is the direct answer to the keypress, so it has to say something.
fn render_connecting(frame: &mut Frame, elapsed: Duration) {
    // Four rows of content and two of border. Sized to what it draws, because a
    // row short here is the footer -- the only way out of a panel whose request
    // may never come back.
    let inner = sign_in_panel(frame, SIGN_IN_MIN_WIDTH, 6, Color::Cyan);
    let lines = vec![
        Line::from(""),
        Line::from(Span::styled(
            format!("  asking Google for a code{}", ellipsis(elapsed)),
            Style::default().fg(Color::White),
        )),
        Line::from(""),
        Line::from(Span::styled(HINT_HIDE, Style::default().fg(Color::DarkGray))),
    ];
    frame.render_widget(Paragraph::new(lines), inner);
}

/// The code is on screen and the thread behind the panel is polling Google.
///
/// This is the one screen in the program a user has to copy something *off* of,
/// so the code is given a box of its own rather than a line among lines: it is
/// the payload, and everything else here is instructions for using it.
fn render_waiting(frame: &mut Frame, user_code: &str, url: &str, deadline: Instant) {
    // Wide enough for the verification URL, which is the one line that must not
    // wrap -- a user cannot type half a URL.
    let width = (display_width(url) as u16).saturating_add(10).max(SIGN_IN_MIN_WIDTH);
    let left = deadline.saturating_duration_since(Instant::now());

    // Short terminals get the same content without the framing. Dropping the
    // code box and the blank rows is a far better failure than letting a fixed
    // height clip the footer, or the code itself, off the bottom.
    if frame.area().height < SIGN_IN_FULL_HEIGHT {
        return render_waiting_compact(frame, user_code, url, left, width);
    }

    let inner = sign_in_panel(frame, width, SIGN_IN_FULL_HEIGHT, Color::Cyan);
    let [_, step_1, url_row, _, step_2, code_row, _, waiting, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(3),
        Constraint::Min(0),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(inner);

    frame.render_widget(Paragraph::new(step("1", "open this page in a browser")), step_1);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            format!("{INDENT}{url}"),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::UNDERLINED),
        ))),
        url_row,
    );
    frame.render_widget(Paragraph::new(step("2", "enter this code")), step_2);
    render_code_box(frame, user_code, code_row);

    let [spinner, expiry] =
        Layout::horizontal([Constraint::Min(1), Constraint::Length(EXPIRY_WIDTH)]).areas(waiting);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            format!("  waiting for approval{}", ellipsis(left)),
            Style::default().fg(Color::Yellow),
        ))),
        spinner,
    );
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            format!("expires in {}", countdown(left)),
            Style::default().fg(Color::DarkGray),
        ))),
        expiry,
    );
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            HINT_HIDE_RUNNING,
            Style::default().fg(Color::DarkGray),
        ))),
        footer,
    );
}

/// Everything [`render_waiting`] says, in five rows, for a terminal too short
/// to hold the full panel.
fn render_waiting_compact(
    frame: &mut Frame,
    user_code: &str,
    url: &str,
    left: Duration,
    width: u16,
) {
    let inner = sign_in_panel(frame, width, 5, Color::Cyan);
    let label = Style::default().fg(Color::DarkGray);
    let lines = vec![
        Line::from(vec![
            Span::styled(" open ", label),
            Span::styled(url.to_string(), Style::default().fg(Color::Cyan)),
        ]),
        Line::from(vec![
            Span::styled(" code ", label),
            Span::styled(
                user_code.to_string(),
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(Span::styled(
            format!(" waiting for approval{} ({} left)", ellipsis(left), countdown(left)),
            Style::default().fg(Color::Yellow),
        )),
    ];
    frame.render_widget(Paragraph::new(lines), inner);
}

/// The code, boxed and bold.
///
/// Boxed because this is the only thing on screen the user has to retype into
/// another device, and a box is what separates "the string to copy" from the
/// prose telling them to copy it.
fn render_code_box(frame: &mut Frame, user_code: &str, row: Rect) {
    let indent = display_width(INDENT) as u16;
    let width = (display_width(user_code) as u16)
        .saturating_add(CODE_BOX_PADDING)
        .min(row.width.saturating_sub(indent));
    let area = Rect {
        x: row.x + indent,
        y: row.y,
        width,
        height: row.height,
    };

    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            format!("  {user_code}"),
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        )))
        .block(Block::bordered().border_style(Style::default().fg(Color::Cyan))),
        area,
    );
}

/// The flow ended without a session, and says why.
///
/// Red rather than cyan: every other phase is an instruction to follow, and
/// this is the one that is not. Retrying is offered here because the thread
/// that was polling is gone -- nothing else on screen will ever change.
fn render_sign_in_failed(frame: &mut Frame, reason: &str) {
    // Capped rather than sized to the reason: these run to a sentence and a
    // half, and a panel stretched to hold one on a single line is both harder
    // to read and no longer recognisably the panel the other phases drew.
    let width = (display_width(reason) as u16)
        .saturating_add(6)
        .clamp(SIGN_IN_MIN_WIDTH, SIGN_IN_MAX_WIDTH)
        .min(frame.area().width);
    // The reason is a sentence from Google, not a fixed string, so it is wrapped
    // to the panel rather than assumed to fit one row of it.
    let body = wrap(reason, width.saturating_sub(4) as usize);
    // The reason, a blank row either side of it, the retry hint, two borders.
    let inner = sign_in_panel(frame, width, body.len() as u16 + 5, Color::Red);

    let mut lines = vec![Line::from("")];
    lines.extend(body.into_iter().map(|line| {
        Line::from(Span::styled(
            format!("  {line}"),
            Style::default().fg(Color::Red),
        ))
    }));
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "  A try again   Esc dismiss",
        Style::default().fg(Color::DarkGray),
    )));

    frame.render_widget(Paragraph::new(lines), inner);
}

/// A numbered instruction: the number in the accent colour, the text beside it.
fn step(number: &str, text: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            format!("  {number}  "),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(text.to_string(), Style::default().fg(Color::White)),
    ])
}

/// A three-state ellipsis, so a panel that is waiting on a person rather than on
/// the network still shows the program is alive.
///
/// Driven from a duration the caller already holds rather than from the clock,
/// which keeps it a pure function and the panel testable.
fn ellipsis(elapsed: Duration) -> &'static str {
    match (elapsed.as_millis() / 400) % 3 {
        0 => ".",
        1 => "..",
        _ => "...",
    }
}

/// `m:ss` left on the clock, rounded up.
///
/// Up rather than down so a code with a fraction of a second on it does not
/// already read `0:00`, and so a ten-minute code reads `10:00` on the frame it
/// appears rather than `9:59`.
fn countdown(left: Duration) -> String {
    let secs = left.as_millis().div_ceil(1000) as u64;
    format!("{}:{:02}", secs / 60, secs % 60)
}

/// Breaks text onto lines of at most `width` columns, at spaces.
///
/// Only used for error text, which is a sentence from Google of no predictable
/// length. A word longer than the width is left to overhang rather than split:
/// the long words in these messages are URLs, and a URL broken across two lines
/// is one the user cannot use.
fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut line = String::new();
    for word in text.split_whitespace() {
        if line.is_empty() {
            line = word.to_string();
        } else if display_width(&line) + 1 + display_width(word) <= width {
            line.push(' ');
            line.push_str(word);
        } else {
            lines.push(std::mem::take(&mut line));
            line = word.to_string();
        }
    }
    if !line.is_empty() {
        lines.push(line);
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

fn render_add_to(frame: &mut Frame, app: &App, title: &str, selected: usize) {
    let rows = app.playlists.len().clamp(1, 12) as u16;
    let area = centred(frame.area(), 56, rows + 4);

    let block = Block::bordered()
        .border_style(Style::default().fg(Color::Cyan))
        .title(" add to playlist ");
    let inner = block.inner(area);
    frame.render_widget(Clear, area);
    frame.render_widget(block, area);

    let [header, list, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .areas(inner);

    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            format!(" {}", truncate(title, inner.width.saturating_sub(2) as usize)),
            Style::default().add_modifier(Modifier::BOLD),
        ))),
        header,
    );

    if app.playlists.is_empty() {
        frame.render_widget(
            Paragraph::new(Span::styled(
                " loading your playlists ...",
                Style::default().fg(Color::DarkGray),
            )),
            list,
        );
    } else {
        // The picker scrolls independently of the library pane behind it, so it
        // keeps the selection in view itself rather than borrowing that offset.
        let height = list.height as usize;
        let offset = selected.saturating_sub(height.saturating_sub(1));
        let end = (offset + height).min(app.playlists.len());
        let items: Vec<ListItem> = app.playlists[offset..end]
            .iter()
            .enumerate()
            .map(|(i, playlist)| {
                ListItem::new(playlist_line(
                    playlist,
                    offset + i == selected,
                    list.width as usize,
                ))
            })
            .collect();
        frame.render_widget(List::new(items), list);
    }

    frame.render_widget(
        Paragraph::new(Span::styled(
            " Enter add   Esc cancel",
            Style::default().fg(Color::DarkGray),
        )),
        footer,
    );
}

/// Centres a box of at most `width` x `height` inside `area`.
fn centred(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 2,
        width,
        height,
    }
}

/// Reserves the pane for an image the event loop will paint, and returns where
/// and how big it should be.
///
/// Nothing is drawn here beyond the frame around it: sixel pixels arrive by
/// escape sequence, outside anything ratatui knows about. The cells underneath
/// are marked [`CellDiffOption::Skip`] so the next redraw leaves them alone --
/// without that, ratatui would repaint blanks over the picture every frame.
fn plan_image(
    frame: &mut Frame,
    cover: &Cover,
    area: Rect,
    graphics: Graphics,
    size: CoverSize,
) -> Option<ImagePlan> {
    let (cell_w, cell_h) = graphics.cell;
    let (cols, rows) = fit_cells(cover, usable(area, size), graphics.cell);
    if cols == 0 || rows == 0 {
        return None;
    }
    let (col, row) = place(frame, area, (cols, rows), size);

    for y in row..row + rows {
        for x in col..col + cols {
            frame.buffer_mut()[(x, y)].set_diff_option(CellDiffOption::Skip);
        }
    }

    Some(ImagePlan {
        col,
        row,
        width: cols * cell_w,
        height: rows * cell_h,
    })
}

/// Largest whole-cell box that holds the cover at its own aspect ratio, given
/// how many pixels a cell is.
///
/// Whole cells matter: an image ending halfway through a cell leaves a sliver
/// that the next text redraw slices in half. Rounding to the nearest cell
/// rather than down keeps the aspect error under half a cell -- truncating
/// could squash a 16:9 cover by a visible seven percent.
fn fit_cells(cover: &Cover, max: (u16, u16), cell: (u16, u16)) -> (u16, u16) {
    let (max_cols, max_rows) = (u32::from(max.0), u32::from(max.1));
    let (cell_w, cell_h) = (u32::from(cell.0), u32::from(cell.1));
    if max_cols == 0 || max_rows == 0 || cover.width == 0 || cover.height == 0 {
        return (0, 0);
    }

    let div_round = |a: u32, b: u32| (a + b / 2) / b;
    let rows_for = |cols: u32| div_round(cols * cell_w * cover.height, cover.width * cell_h).max(1);
    let cols_for = |rows: u32| div_round(rows * cell_h * cover.width, cover.height * cell_w).max(1);

    let mut cols = max_cols;
    let mut rows = rows_for(cols);
    if rows > max_rows {
        rows = max_rows;
        cols = cols_for(rows).min(max_cols);
    }
    (cols as u16, rows as u16)
}

/// Divides the main area between the results and the cover.
///
/// Either side may be absent: there is nothing to show without a cover, and
/// nothing to show it in on a narrow terminal, while a full-size cover takes
/// the area outright and the list stands down.
fn split_cover(app: &App, area: Rect) -> (Option<Rect>, Option<Rect>) {
    if app.cover.is_none() || area.width < MIN_WIDTH_WITH_COVER {
        return (Some(area), None);
    }
    if app.cover_size == CoverSize::Full {
        return (None, Some(area));
    }
    // The landing page is laid out across the full width -- a side pane there
    // costs it a column of cards, and the cover of what is already playing is
    // not what someone browsing for the next track is looking at. `c` still
    // takes the whole window, which is an explicit request rather than a
    // default.
    if app.view == View::Home {
        return (Some(area), None);
    }
    let width = (area.width * 2 / 5).clamp(COVER_MIN_WIDTH, COVER_MAX_WIDTH);
    let [list, cover] =
        Layout::horizontal([Constraint::Min(1), Constraint::Length(width)]).areas(area);
    (Some(list), Some(cover))
}

/// Cells the picture itself may occupy.
///
/// A full-size cover drops the frame: two rows of border is forty pixels of
/// picture, in a window that has none to spare. Beside the list the frame earns
/// its keep by separating the two panes.
fn usable(area: Rect, size: CoverSize) -> (u16, u16) {
    let inner = match size {
        CoverSize::Full => area,
        CoverSize::Side => Block::bordered().inner(area),
    };
    (inner.width, inner.height)
}

/// Draws whatever frame the mode calls for and returns the picture's top-left
/// cell: centred in the window at full size, tucked under the top border and
/// lined up with the results beside them.
fn place(frame: &mut Frame, area: Rect, (cols, rows): (u16, u16), size: CoverSize) -> (u16, u16) {
    match size {
        CoverSize::Full => (
            area.x + (area.width - cols) / 2,
            area.y + (area.height - rows) / 2,
        ),
        CoverSize::Side => {
            let block = Block::bordered()
                .border_style(Style::default().fg(Color::DarkGray))
                .title(" cover ");
            // Only as tall as the picture, so a wide cover does not sit atop an
            // empty box.
            let framed = Rect {
                height: (rows + 2).min(area.height),
                ..area
            };
            let inner = block.inner(framed);
            frame.render_widget(block, framed);
            (inner.x + (inner.width - cols) / 2, inner.y)
        }
    }
}

fn render_search(frame: &mut Frame, app: &App, area: Rect) {
    let editing = app.mode == Mode::Editing;
    let border = if editing {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::DarkGray)
    };

    let block = Block::bordered().border_style(border).title(" search ");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let text = if app.query.is_empty() && !editing {
        Span::styled("press / to search", Style::default().fg(Color::DarkGray))
    } else {
        Span::raw(app.query.as_str())
    };
    frame.render_widget(Paragraph::new(Line::from(text)), inner);

    // Show a real cursor while typing so the terminal's own caret is the
    // affordance, rather than drawing a fake one.
    if editing {
        let x = inner.x + app.query.chars().count().min(inner.width as usize) as u16;
        frame.set_cursor_position((x, inner.y));
    }
}

/// The landing page: YouTube Music's shelves, drawn as rows of cards.
///
/// Virtualized in both directions, on the same principle as the results list --
/// only the shelves on screen are laid out, and only the cards visible within
/// each of them are built. A feed of twelve shelves costs what one screenful
/// costs.
fn render_home(frame: &mut Frame, app: &mut App, area: Rect) {
    let block = Block::bordered().title(app.list_title());
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if app.home.is_empty() || inner.width < MIN_HOME_WIDTH || inner.height < CARD_HEIGHT + 1 {
        let hint = if !app.home.is_empty() {
            // Too small to draw a card in, which is a fact about the window
            // rather than a failure worth explaining.
            "the window is too small for the home feed"
        } else if app.home_pending {
            "loading ..."
        } else {
            // Both halves matter: `r` is the only way to ask again, and a user
            // who cannot get a feed can still use every other part of this.
            "no home feed -- press r to try again, or / to search"
        };
        frame.render_widget(
            Paragraph::new(Span::styled(hint, Style::default().fg(Color::DarkGray))),
            inner,
        );
        return;
    }

    // Cards are widened to fill the row rather than left at [`CARD_WIDTH`] with
    // a ragged margin: the leftover columns are worth more inside the titles
    // than they are as whitespace on the right.
    let across = (inner.width / CARD_WIDTH).max(1);
    let down = (inner.height / SHELF_HEIGHT).max(1);
    app.clamp_home(down as usize, across as usize);

    let cursor = HomeCursor {
        top: app.home_top,
        shelf: app.home_shelf,
        card: app.home_card,
    };
    render_feed(frame, &app.home, inner, cursor, &app.home_scroll);
}

/// Where the cursor stands on the landing page as a whole.
#[derive(Debug, Clone, Copy)]
struct HomeCursor {
    /// First visible shelf.
    top: usize,
    shelf: usize,
    card: usize,
}

/// Stacks the visible shelves down the pane.
///
/// Split from [`render_home`] so the page can be drawn from shelves alone --
/// which is what lets it be previewed and tested without standing up a player
/// and a worker thread behind an `App`.
fn render_feed(
    frame: &mut Frame,
    shelves: &[Shelf],
    area: Rect,
    cursor: HomeCursor,
    scroll: &[usize],
) {
    let across = (area.width / CARD_WIDTH).max(1);
    let slot = area.width / across;

    let mut y = area.y;
    for (index, shelf) in shelves.iter().enumerate().skip(cursor.top) {
        // A shelf that would be cut off halfway is not drawn at all: half a
        // card reads as a rendering fault, where a blank row reads as the end
        // of the page.
        if y + CARD_HEIGHT + 1 > area.y + area.height {
            break;
        }
        let row = Rect {
            x: area.x,
            y,
            width: area.width,
            height: CARD_HEIGHT + 1,
        };
        render_shelf(
            frame,
            shelf,
            row,
            ShelfCursor {
                focused: index == cursor.shelf,
                selected: cursor.card,
                offset: scroll.get(index).copied().unwrap_or(0),
            },
            (across, slot),
        );
        y += SHELF_HEIGHT;
    }
}

/// Where the cursor stands in one shelf, as far as that shelf needs to know.
#[derive(Debug, Clone, Copy)]
struct ShelfCursor {
    /// Whether the cursor is in this shelf at all. Only one shelf draws a
    /// selection, so that the page has a single cursor rather than one per row.
    focused: bool,
    selected: usize,
    /// Leftmost visible card.
    offset: usize,
}

/// One shelf: a heading, then as many cards as fit beside each other.
fn render_shelf(
    frame: &mut Frame,
    shelf: &Shelf,
    area: Rect,
    cursor: ShelfCursor,
    (across, slot): (u16, u16),
) {
    let [heading, cards] =
        Layout::vertical([Constraint::Length(1), Constraint::Length(CARD_HEIGHT)]).areas(area);
    frame.render_widget(
        Paragraph::new(heading_line(shelf, cursor, area.width as usize)),
        heading,
    );

    for column in 0..across {
        let Some(card) = shelf.cards.get(cursor.offset + column as usize) else {
            break;
        };
        let rect = Rect {
            x: cards.x + column * slot,
            // One column of the slot is left as the gap between cards.
            width: slot.saturating_sub(1),
            ..cards
        };
        let selected = cursor.focused && cursor.offset + column as usize == cursor.selected;
        render_card(frame, card, rect, selected);
    }
}

/// A shelf's heading: the name YouTube gave it, and -- when the cursor is in it
/// -- how far along the shelf the cursor has got.
///
/// The counter is only drawn for the focused shelf because it is the only one
/// whose position can change, and a row of counters on shelves nobody is
/// looking at is noise that competes with the headings themselves.
fn heading_line(shelf: &Shelf, cursor: ShelfCursor, width: usize) -> Line<'static> {
    // Narrower than the marker and there is no heading to draw, only an
    // overhang. The pane refuses to draw cards well before this.
    if width < MARKER_WIDTH + 1 {
        return Line::from("");
    }

    let (marker, title_style) = if cursor.focused {
        (
            Style::default().fg(Color::Cyan),
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        )
    } else {
        (
            Style::default().fg(Color::DarkGray),
            Style::default().fg(Color::Gray),
        )
    };

    let counter = if cursor.focused && shelf.cards.len() > 1 {
        format!("{} of {} ", cursor.selected + 1, shelf.cards.len())
    } else {
        String::new()
    };
    // The name outranks the position, the same way a playlist row's name
    // outranks its track count: a counter beside a two-letter stub of a
    // heading says where the cursor is in a shelf the user can no longer
    // identify.
    let counter = if width >= MARKER_WIDTH + MIN_SHELF_TITLE + counter.len() {
        counter
    } else {
        String::new()
    };

    // The marker leads, and the counter is right-aligned against the far edge
    // of the shelf.
    let room = width.saturating_sub(MARKER_WIDTH + counter.len());
    let title = truncate(&shelf.title, room);
    let pad = room.saturating_sub(display_width(&title));

    Line::from(vec![
        Span::styled("▌ ", marker),
        Span::styled(title, title_style),
        Span::styled(
            format!("{:pad$}{counter}", "", pad = pad),
            Style::default().fg(Color::DarkGray),
        ),
    ])
}

/// One card: a box with a title and whatever YouTube wrote under it.
///
/// The leading glyph says what Enter will do, which is the one thing the two
/// kinds of card do not otherwise distinguish: a song plays, and an album or a
/// playlist opens into a list. Without it the difference is only discoverable
/// by pressing Enter and seeing what happens.
fn render_card(frame: &mut Frame, card: &Card, area: Rect, selected: bool) {
    let border = if selected {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(border);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let title = if selected {
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::White)
    };
    // One column of padding either side, so text never touches the border.
    let room = inner.width.saturating_sub(2) as usize;
    let glyph = if card.is_playable() { "▶ " } else { "≡ " };

    let lines = vec![
        Line::from(vec![
            Span::styled(format!(" {glyph}"), Style::default().fg(Color::DarkGray)),
            Span::styled(truncate(&card.title, room.saturating_sub(2)), title),
        ]),
        Line::from(Span::styled(
            format!(" {}", truncate(&card.subtitle, room)),
            Style::default().fg(Color::DarkGray),
        )),
    ];
    frame.render_widget(Paragraph::new(lines), inner);
}

/// The user's playlists, with "Liked songs" pinned at the top.
fn render_playlists(frame: &mut Frame, app: &mut App, area: Rect) {
    let block = Block::bordered().title(app.list_title());
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if app.playlists.is_empty() {
        let hint = if app.busy {
            "loading ..."
        } else if app.signed_in {
            "no playlists on this account"
        } else {
            "press A to sign in with Google"
        };
        frame.render_widget(
            Paragraph::new(Span::styled(hint, Style::default().fg(Color::DarkGray))),
            inner,
        );
        return;
    }

    let height = inner.height as usize;
    app.clamp_playlist_scroll(height);

    // Virtualized on the same principle as the results list.
    let end = (app.playlist_offset + height).min(app.playlists.len());
    let width = inner.width as usize;
    let items: Vec<ListItem> = app.playlists[app.playlist_offset..end]
        .iter()
        .enumerate()
        .map(|(i, playlist)| {
            let selected = app.playlist_offset + i == app.playlist_selected;
            ListItem::new(playlist_line(playlist, selected, width))
        })
        .collect();

    frame.render_widget(List::new(items), inner);
}

/// One playlist row: name on the left, track count right-aligned.
///
/// Shared by the library pane and the add-to picker so the two cannot drift
/// into showing the same list two different ways.
fn playlist_line(playlist: &Playlist, selected: bool, width: usize) -> Line<'static> {
    let style = if selected {
        Style::default()
            .fg(Color::Black)
            .bg(Color::Cyan)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    };

    // Liked songs is not a playlist the user made, and marking it is what
    // stops it reading as one that happens to sort first.
    let marker = if playlist.is_liked() { "♥ " } else { "  " };
    let count = match playlist.count {
        Some(n) => format!("{n} "),
        // Liked songs, whose size the API does not report up front.
        None => String::new(),
    };

    // One leading space, then the marker.
    let fixed = 1 + display_width(marker);
    if width <= fixed {
        // No room for a name, and a marker with nothing beside it is noise.
        // Blank cells keep the selection highlight the right width.
        return Line::from(Span::styled(" ".repeat(width), style));
    }

    // The count is given up before the name is: a name is what identifies the
    // row, and a bare number next to a truncated stub identifies nothing.
    let count = if width >= fixed + MIN_PLAYLIST_NAME + count.len() {
        count
    } else {
        String::new()
    };

    let name = cell(&playlist.title, width - fixed - count.len());

    Line::from(vec![
        Span::styled(format!(" {marker}{name}"), style),
        Span::styled(
            count,
            if selected {
                style
            } else {
                Style::default().fg(Color::DarkGray)
            },
        ),
    ])
}

/// The player page: the cover of what is playing, and beside it the tab the
/// user has open.
///
/// Returns the rect the cover should be centred in, which the caller paints --
/// the picture is not part of ratatui's buffer on the sixel path, so it cannot
/// be drawn from here.
fn render_player(frame: &mut Frame, app: &mut App, area: Rect) -> Option<Rect> {
    // Nothing is playing, so there is no page. Reachable for one frame after a
    // stop, before the key handler's view change is drawn.
    let now = app.now.as_ref()?;

    // An explicit request for the whole window outranks the layout: `c` means
    // the picture, and the panel beside it is what the user asked to be rid of.
    if app.cover_size == CoverSize::Full {
        return Some(area);
    }

    // Too narrow to carry both. The panel wins rather than the picture: it is
    // where the queue, the lyrics and the comments are, and a cover squeezed
    // into twenty columns is a smudge.
    if area.width < MIN_WIDTH_WITH_COVER {
        render_panel(frame, app, area);
        return None;
    }

    let panel_width = (area.width * 2 / 5).clamp(PANEL_MIN_WIDTH, PANEL_MAX_WIDTH);
    let [art_area, panel_area] =
        Layout::horizontal([Constraint::Min(1), Constraint::Length(panel_width)]).areas(area);

    let block = Block::bordered()
        .border_style(Style::default().fg(Color::DarkGray))
        .title(app.list_title());
    let inner = block.inner(art_area);
    frame.render_widget(block, art_area);

    // The picture takes what the track's own details do not need. Below the
    // point where the details would leave it nothing, the details go instead:
    // a title and a progress bar are the parts a player cannot do without.
    let (art, info) = if inner.height > INFO_HEIGHT {
        let [art, info] =
            Layout::vertical([Constraint::Min(1), Constraint::Length(INFO_HEIGHT)]).areas(inner);
        (Some(art), info)
    } else {
        (None, inner)
    };
    render_track_info(frame, now, app.snapshot(), info);

    render_panel(frame, app, panel_area);
    art
}

/// The title, artist and progress of what is playing.
fn render_track_info(frame: &mut Frame, now: &NowPlaying, snap: Snapshot, area: Rect) {
    let width = area.width as usize;
    let mut lines = vec![
        Line::from(Span::styled(
            truncate(&now.title, width),
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            truncate(&now.byline(), width),
            Style::default().fg(Color::Gray),
        )),
    ];
    if area.height > 2 {
        lines.push(Line::from(""));
        lines.push(progress_line(&snap, now.duration, width));
    }
    // Last, because it is the one row here that is about the player rather than
    // about the track: a short panel spends what it has on the song.
    if area.height > 4 {
        lines.push(volume_line(snap.volume, width));
    }

    frame.render_widget(Paragraph::new(lines), area);
}

/// The scrubber: a bar, and the clock beside it.
///
/// The length comes from the queue rather than from the player, which knows
/// only how far it has got -- so a track whose length never arrived gets a
/// clock and no bar rather than a bar that would have to lie about the end.
fn progress_line<'a>(snap: &Snapshot, total: Option<Duration>, width: usize) -> Line<'a> {
    let position = snap.position;
    let clock = match total {
        Some(total) => format!(" {} / {}", clock(position), clock(total)),
        None => format!(" {}", clock(position)),
    };

    let bar_width = width.saturating_sub(clock.chars().count() + 1);
    let Some(total) = total.filter(|total| !total.is_zero() && bar_width >= 4) else {
        return Line::from(Span::styled(clock, Style::default().fg(Color::Gray)));
    };

    // Saturating rather than clamped after the fact: a position past the end is
    // an ordinary thing to see for a moment, since the container's length and
    // what the decoder yields never agree to the sample.
    let filled = ((position.as_secs_f64() / total.as_secs_f64()).min(1.0) * bar_width as f64)
        .round() as usize;

    Line::from(vec![
        Span::styled(
            "━".repeat(filled),
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            "─".repeat(bar_width - filled),
            Style::default().fg(Color::DarkGray),
        ),
        Span::styled(clock, Style::default().fg(Color::Gray)),
    ])
}

/// The volume, as a bar and the number beside it: `vol ━━━━━━━━────  80%`.
///
/// The bar is drawn against 100% rather than against the 200% the player will
/// accept, because 100% is where every track plays until the user says
/// otherwise -- a bar sitting half-full at the ordinary volume reads as a
/// fault. A boost fills it and colours it instead, which says "past the end of
/// the scale" without pretending the scale is twice as long.
fn volume_line<'a>(volume: f32, width: usize) -> Line<'a> {
    const LEAD: &str = "vol ";
    let label = format!("  {:.0}%", volume * 100.0);

    let bar_width = width
        .saturating_sub(LEAD.len() + label.chars().count())
        .min(VOLUME_BAR_WIDTH);
    // Nothing left to draw a bar in, so the number says it on its own -- the
    // part a user actually reads a volume off.
    if bar_width < 4 {
        return Line::from(Span::styled(
            format!("{LEAD}{}", label.trim_start()),
            Style::default().fg(Color::Gray),
        ));
    }

    let filled = (f64::from(volume.min(1.0)) * bar_width as f64).round() as usize;
    let fill = if volume > 1.0 {
        Color::Yellow
    } else {
        Color::Gray
    };

    Line::from(vec![
        Span::styled(LEAD, Style::default().fg(Color::DarkGray)),
        Span::styled("━".repeat(filled), Style::default().fg(fill)),
        Span::styled(
            "─".repeat(bar_width - filled),
            Style::default().fg(Color::DarkGray),
        ),
        Span::styled(label, Style::default().fg(Color::Gray)),
    ])
}

/// The tabbed panel: the row of tab names, and whichever one is open.
///
/// The panel renderers below draw with a cursor they clamp for themselves, and
/// hand back how long their contents turned out to be; this is where that
/// length is written back to the cursor the keys move. Doing it in that order
/// means a frame is never drawn with a cursor past the end of a panel whose
/// length only became knowable while drawing it.
fn render_panel(frame: &mut Frame, app: &mut App, area: Rect) {
    let block = Block::bordered().border_style(Style::default().fg(Color::DarkGray));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.height < 2 {
        return;
    }

    let Some(now) = app.now.as_ref() else {
        return;
    };
    let tab = now.tab;
    let [tabs, content] =
        Layout::vertical([Constraint::Length(2), Constraint::Min(0)]).areas(inner);
    render_tabs(frame, tab, tabs);

    let total = match tab {
        Tab::UpNext => render_up_next(frame, now, content),
        Tab::Lyrics => render_lyrics(frame, now, content),
        Tab::Comments => render_comments(frame, now, content),
        Tab::Related => render_related(frame, now, content),
    };
    // A list stops at its last row; a wall of text stops one screenful short of
    // its end. `render_up_next` keeps its header out of the scroll, so what it
    // reports is rows rather than lines and the viewport must match.
    let selects = matches!(tab, Tab::UpNext | Tab::Related);
    let viewport = (content.height as usize).saturating_sub(if tab == Tab::UpNext {
        UP_NEXT_HEADER
    } else {
        0
    });
    app.clamp_page(viewport, total, selects);
}

/// The tab names, with the open one underlined.
///
/// Underlined rather than highlighted: the panel below is dense, and a filled
/// bar across the top of it would be the loudest thing on the page. The rule is
/// drawn as its own row so it reads as the edge of the panel, which is what it
/// is.
fn render_tabs(frame: &mut Frame, open: Tab, area: Rect) {
    let mut labels = Vec::new();
    let mut rule = Vec::new();

    for tab in Tab::ALL {
        let selected = tab == open;
        let style = if selected {
            Style::default().fg(Color::White).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        labels.push(Span::styled(format!(" {} ", tab.label()), style));

        // Under the letters rather than under the padding, so the rule reads as
        // belonging to the name above it rather than to the gap beside it.
        let width = tab.label().chars().count();
        rule.push(Span::styled(
            format!(" {} ", if selected { "─" } else { " " }.repeat(width)),
            Style::default().fg(Color::Red),
        ));
    }

    frame.render_widget(Paragraph::new(vec![Line::from(labels), Line::from(rule)]), area);
}

/// The queue, headed by what it is a queue of. Returns its length in rows.
///
/// The heading does not scroll with the rows: what a queue is a queue of is the
/// one thing on this panel that stays true however far down it the user is.
fn render_up_next(frame: &mut Frame, now: &NowPlaying, area: Rect) -> usize {
    if now.queue.is_empty() {
        message(frame, "loading the queue ...", area);
        return 0;
    }

    let width = area.width as usize;
    let mut lines = vec![
        Line::from(Span::styled(
            " Playing from",
            Style::default().fg(Color::DarkGray),
        )),
        Line::from(Span::styled(
            format!(" {}", truncate(&now.queue_title, width.saturating_sub(1))),
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
    ];
    debug_assert_eq!(lines.len(), UP_NEXT_HEADER);

    let viewport = (area.height as usize).saturating_sub(UP_NEXT_HEADER);
    let cursor = now.cursor().min(now.queue.len().saturating_sub(1));
    let offset = centred_offset(cursor, viewport, now.queue.len());

    lines.extend(
        now.queue
            .iter()
            .enumerate()
            .skip(offset)
            .take(viewport)
            .map(|(index, track)| {
                queue_line(track, index == cursor, Some(index) == now.playing, width)
            }),
    );

    frame.render_widget(Paragraph::new(lines), area);
    now.queue.len()
}

/// One queue row: a marker for the track playing, the title, and its length.
fn queue_line<'a>(track: &Track, selected: bool, playing: bool, width: usize) -> Line<'a> {
    let style = if selected {
        Style::default()
            .fg(Color::Black)
            .bg(Color::Cyan)
            .add_modifier(Modifier::BOLD)
    } else if playing {
        Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    };
    let dim = |colour: Color| if selected { style } else { Style::default().fg(colour) };

    let duration = track.duration_str();
    // The marker, and the length with a space after it.
    let text_width = width.saturating_sub(QUEUE_MARKER_WIDTH + duration.chars().count() + 1);
    // The artist gets a column when the panel is wide enough to carry one
    // without the titles becoming unreadable, and is dropped rather than
    // stacked below when it is not: two rows per track would halve a queue that
    // only shows a handful of them as it is.
    let (title_width, artist_width) = if text_width >= MIN_QUEUE_TITLE + QUEUE_ARTIST_WIDTH {
        (text_width - QUEUE_ARTIST_WIDTH, QUEUE_ARTIST_WIDTH)
    } else {
        (text_width, 0)
    };

    let mut spans = vec![
        Span::styled(
            format!(" {} ", if playing { "▶" } else { " " }),
            if playing { style } else { dim(Color::DarkGray) },
        ),
        Span::styled(cell(&track.title, title_width), style),
    ];
    if artist_width > 0 {
        spans.push(Span::styled(cell(&track.uploader, artist_width), dim(Color::Gray)));
    }
    spans.push(Span::styled(format!("{duration} "), dim(Color::DarkGray)));

    Line::from(spans)
}

/// The lyrics, wrapped to the panel and scrolled by line. Returns the number of
/// lines the wrap produced.
fn render_lyrics(frame: &mut Frame, now: &NowPlaying, area: Rect) -> usize {
    let width = (area.width as usize).saturating_sub(2);
    let Panel::Ready(lyrics) = &now.lyrics else {
        message(frame, waiting_on(&now.lyrics), area);
        return 0;
    };

    // Wrapped here rather than by `Paragraph::wrap` because the panel scrolls
    // by line, and only a wrap we did ourselves can be counted.
    let mut lines: Vec<String> = Vec::new();
    if let Some(source) = &lyrics.source {
        lines.push(source.clone());
        lines.push(String::new());
    }
    for paragraph in lyrics.text.lines() {
        if paragraph.trim().is_empty() {
            lines.push(String::new());
        } else {
            lines.extend(wrap(paragraph, width));
        }
    }

    let viewport = area.height as usize;
    let source_lines = lyrics.source.is_some() as usize * 2;
    let offset = now.cursor().min(lines.len().saturating_sub(viewport));

    let drawn: Vec<Line> = lines
        .iter()
        .enumerate()
        .skip(offset)
        .take(viewport)
        .map(|(index, line)| {
            let style = if index < source_lines {
                Style::default().fg(Color::DarkGray)
            } else {
                Style::default().fg(Color::Gray)
            };
            Line::from(Span::styled(format!(" {line}"), style))
        })
        .collect();

    frame.render_widget(Paragraph::new(drawn), area);
    lines.len()
}

/// The comment section, as a scrolling column of blocks. Returns its height in
/// lines.
fn render_comments(frame: &mut Frame, now: &NowPlaying, area: Rect) -> usize {
    let width = (area.width as usize).saturating_sub(2);
    let Panel::Ready(comments) = &now.comments else {
        message(frame, waiting_on(&now.comments), area);
        return 0;
    };

    let mut lines: Vec<Line> = Vec::new();
    if !comments.total.is_empty() {
        lines.push(Line::from(Span::styled(
            format!(" {}", truncate(&comments.total, width)),
            Style::default().add_modifier(Modifier::BOLD),
        )));
        lines.push(Line::from(""));
    }

    for comment in &comments.items {
        lines.push(Line::from(vec![
            Span::styled(
                format!(" {}", truncate(&comment.author, width.saturating_sub(1))),
                Style::default().fg(Color::Cyan),
            ),
            Span::styled(
                format!("  {}", comment.published),
                Style::default().fg(Color::DarkGray),
            ),
        ]));
        lines.extend(
            wrap(&comment.text, width)
                .into_iter()
                .map(|line| Line::from(Span::raw(format!(" {line}")))),
        );

        // Only when YouTube gave us either; a comment with no likes and no
        // replies is better off without a row of zeroes under it.
        let mut footer = Vec::new();
        if !comment.likes.is_empty() {
            footer.push(format!("{} likes", comment.likes));
        }
        if !comment.replies.is_empty() {
            footer.push(format!("{} replies", comment.replies));
        }
        if !footer.is_empty() {
            lines.push(Line::from(Span::styled(
                format!(" {}", footer.join("   ")),
                Style::default().fg(Color::DarkGray),
            )));
        }
        lines.push(Line::from(""));
    }

    let viewport = area.height as usize;
    let total = lines.len();
    let offset = now.cursor().min(total.saturating_sub(viewport));

    frame.render_widget(
        Paragraph::new(lines.into_iter().skip(offset).take(viewport).collect::<Vec<_>>()),
        area,
    );
    total
}

/// The Related tab: shelves of recommendations, flattened into one list.
/// Returns its length in rows.
fn render_related(frame: &mut Frame, now: &NowPlaying, area: Rect) -> usize {
    if !matches!(now.related, Panel::Ready(_)) {
        message(frame, waiting_on(&now.related), area);
        return 0;
    }

    let viewport = area.height as usize;
    let width = area.width as usize;
    let rows = now.related_rows();
    let cursor = now.cursor().min(rows.len().saturating_sub(1));
    let offset = centred_offset(cursor, viewport, rows.len());

    let lines: Vec<Line> = rows
        .iter()
        .enumerate()
        .skip(offset)
        .take(viewport)
        .map(|(index, row)| match row {
            RelatedRow::Heading(title) => Line::from(Span::styled(
                format!(" {}", truncate(title, width.saturating_sub(1))),
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD),
            )),
            RelatedRow::Card(card) => related_line(card, index == cursor, width),
        })
        .collect();

    frame.render_widget(Paragraph::new(lines), area);
    rows.len()
}

/// One recommendation: its title, and under it whatever YouTube wrote about it.
fn related_line<'a>(card: &Card, selected: bool, width: usize) -> Line<'a> {
    let style = if selected {
        Style::default()
            .fg(Color::Black)
            .bg(Color::Cyan)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    };
    let subtitle = if selected {
        style
    } else {
        Style::default().fg(Color::DarkGray)
    };

    // Split across the row rather than stacked, because a card's subtitle names
    // its type as often as its artist -- "Playlist • 405 views" is not worth a
    // line of a panel this short.
    let title_width = (width.saturating_sub(2) * 3 / 5).max(1);
    let rest = width.saturating_sub(title_width + 2);

    Line::from(vec![
        Span::styled(format!("   {}", cell(&card.title, title_width)), style),
        Span::styled(cell(&card.subtitle, rest), subtitle),
    ])
}

/// The panel's contents when it has none: what it is waiting for, or why there
/// will never be any.
fn message(frame: &mut Frame, text: &str, area: Rect) {
    let lines: Vec<Line> = wrap(text, (area.width as usize).saturating_sub(2))
        .into_iter()
        .map(|line| Line::from(Span::styled(format!(" {line}"), Style::default().fg(Color::DarkGray))))
        .collect();
    frame.render_widget(Paragraph::new(lines), area);
}

/// What an empty panel should say for itself.
fn waiting_on<T>(panel: &Panel<T>) -> &str {
    match panel {
        // Idle is a fetch that has not started because the watch response has
        // not landed yet, which from here is indistinguishable from loading --
        // and is about to become it.
        Panel::Idle | Panel::Loading => "loading ...",
        Panel::Empty(why) => why,
        Panel::Ready(_) => "",
    }
}

/// First visible row of a list that keeps its cursor near the middle.
///
/// Centred rather than scrolled at the edges, which is what lets the panel hold
/// no scroll offset of its own: where to start is a function of where the
/// cursor is, so nothing has to be remembered between frames.
fn centred_offset(cursor: usize, viewport: usize, total: usize) -> usize {
    if viewport == 0 || total <= viewport {
        return 0;
    }
    cursor
        .saturating_sub(viewport / 2)
        .min(total - viewport)
}

/// `H:MM:SS` / `M:SS`, matching how the results list writes a length.
fn clock(d: Duration) -> String {
    let total = d.as_secs();
    let (h, m, s) = (total / 3600, (total % 3600) / 60, total % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

fn render_results(frame: &mut Frame, app: &mut App, area: Rect) {
    let block = Block::bordered().title(app.list_title());
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if app.results.is_empty() {
        let hint = if app.busy {
            "working ..."
        } else if app.open_playlist.is_some() {
            "this playlist is empty"
        } else if app.query.is_empty() {
            // Nothing has been searched yet, so this is the first thing a new
            // user reads. "no results" would be true and useless.
            "type a search above, or press ^L to open your Google library"
        } else {
            "no results"
        };
        frame.render_widget(
            Paragraph::new(Span::styled(hint, Style::default().fg(Color::DarkGray))),
            inner,
        );
        return;
    }

    let width = inner.width as usize;
    let (title_width, artist_width, album_width) = columns(width);

    // The header costs a row, so it is only drawn when a result would still be
    // visible underneath it -- labelling an empty list helps nobody.
    let list_area = if inner.height >= 2 {
        let [header, list] =
            Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(inner);
        frame.render_widget(
            Paragraph::new(header_line(title_width, artist_width, album_width)),
            header,
        );
        list
    } else {
        inner
    };

    let height = list_area.height as usize;
    app.clamp_scroll(height);

    // Virtualization: slice to the visible window before building any items.
    let end = (app.offset + height).min(app.results.len());
    let visible = &app.results[app.offset..end];

    let items: Vec<ListItem> = visible
        .iter()
        .enumerate()
        .map(|(i, track)| {
            let selected = app.offset + i == app.selected;
            let duration = track.duration_str();

            let style = if selected {
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            // The highlight has to run the full width of the row, so a
            // selected row keeps one style across every column.
            let dim = |colour: Color| {
                if selected {
                    style
                } else {
                    Style::default().fg(colour)
                }
            };

            let mut spans = vec![Span::styled(
                format!(" {}", cell(&track.title, title_width)),
                style,
            )];
            if artist_width > 0 {
                spans.push(Span::styled(
                    cell(&track.uploader, artist_width),
                    dim(Color::Gray),
                ));
            }
            if album_width > 0 {
                spans.push(Span::styled(
                    cell(track.album.as_deref().unwrap_or(""), album_width),
                    dim(Color::DarkGray),
                ));
            }
            spans.push(Span::styled(
                format!("{duration:>DURATION_WIDTH$} "),
                dim(Color::DarkGray),
            ));

            ListItem::new(Line::from(spans))
        })
        .collect();

    frame.render_widget(List::new(items), list_area);
}

/// The column labels, laid out on exactly the same grid as the rows.
///
/// Takes the widths rather than recomputing them, so a header and its rows
/// cannot disagree about which columns the pane is wide enough to carry.
fn header_line(title_width: usize, artist_width: usize, album_width: usize) -> Line<'static> {
    let style = Style::default()
        .fg(Color::DarkGray)
        .add_modifier(Modifier::BOLD);

    let mut spans = vec![Span::styled(
        format!(" {}", cell("TITLE", title_width)),
        style,
    )];
    if artist_width > 0 {
        spans.push(Span::styled(cell("ARTIST", artist_width), style));
    }
    if album_width > 0 {
        spans.push(Span::styled(cell("ALBUM", album_width), style));
    }
    // Right-aligned to sit over the durations, which are right-aligned too.
    spans.push(Span::styled(
        format!("{:>DURATION_WIDTH$} ", "DURATION"),
        style,
    ));

    Line::from(spans)
}

/// Draws the cover with half-block cells: `▀` painted in the upper pixel's
/// colour over a cell background carrying the lower one. Two image rows per text
/// row, which -- since a cell is about twice as tall as it is wide -- makes the
/// result read as roughly square pixels.
///
/// Rebuilt every frame rather than cached. At this size it is a few hundred
/// spans, which is less work than the results list already does.
fn render_cover(frame: &mut Frame, cover: &Cover, area: Rect, size: CoverSize) {
    let (max_cols, max_rows) = usable(area, size);
    let (px_w, px_h) = fit(cover, max_cols, max_rows);
    if px_w == 0 || px_h == 0 {
        return;
    }

    let rows = px_h.div_ceil(2) as u16;
    let (col, row) = place(frame, area, (px_w as u16, rows), size);
    let inner = Rect::new(col, row, px_w as u16, rows);

    // No padding span: `place` already centred the picture, so each line is
    // exactly the cells the image occupies.
    let lines: Vec<Line> = (0..rows)
        .map(|row| {
            let y = u32::from(row) * 2;
            let mut spans = Vec::with_capacity(px_w as usize);
            for x in 0..px_w {
                let mut style = Style::default().fg(sample(cover, x, y, px_w, px_h));
                // An odd pixel height leaves the last lower half unpainted.
                // Left unstyled it shows the terminal background, which is
                // honest -- better than duplicating the row above it.
                if y + 1 < px_h {
                    style = style.bg(sample(cover, x, y + 1, px_w, px_h));
                }
                spans.push(Span::styled("▀", style));
            }
            Line::from(spans)
        })
        .collect();

    frame.render_widget(Paragraph::new(lines), inner);
}

/// Largest image size, in pixels, that fits a `cols` x `rows` box of cells
/// without distorting the aspect ratio.
fn fit(cover: &Cover, cols: u16, rows: u16) -> (u32, u32) {
    let (max_w, max_h) = (u32::from(cols), u32::from(rows) * 2);
    if max_w == 0 || max_h == 0 || cover.width == 0 || cover.height == 0 {
        return (0, 0);
    }

    let height_at_full_width = (max_w * cover.height / cover.width).max(1);
    if height_at_full_width <= max_h {
        (max_w, height_at_full_width)
    } else {
        ((max_h * cover.width / cover.height).max(1), max_h)
    }
}

/// Nearest-neighbour sample of `cover` at a point on the `px_w` x `px_h` grid
/// being drawn. Nearest-neighbour is right here: the cover was already box
/// filtered down to near this size when it was fetched.
fn sample(cover: &Cover, x: u32, y: u32, px_w: u32, px_h: u32) -> Color {
    let (r, g, b) = cover.pixel(x * cover.width / px_w, y * cover.height / px_h);
    Color::Rgb(r, g, b)
}

/// Whether the player page is drawing the track's own progress bar, which
/// carries a clock beside it.
///
/// Mirrors the conditions [`render_player`] and [`render_track_info`] lay
/// themselves out by: a full-size cover takes the window outright, a narrow one
/// spends it all on the panel, and a short one keeps only the title and the
/// artist. The status bar asks this so that the elapsed time is said once on
/// screen rather than twice.
fn page_draws_progress(app: &App, area: Rect) -> bool {
    app.view == View::Playing
        && app.now.is_some()
        && app.cover_size != CoverSize::Full
        && area.width >= MIN_WIDTH_WITH_COVER
        // The two rows of border, and then the rows the details need before a
        // progress bar is among them.
        && area.height.saturating_sub(2) > 2
}

fn render_status(frame: &mut Frame, app: &App, area: Rect, page_draws_progress: bool) {
    let snap = app.snapshot();
    let block = Block::bordered().border_style(Style::default().fg(Color::DarkGray));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let [now_playing, hints] =
        Layout::horizontal([Constraint::Min(1), Constraint::Length(HINTS_WIDTH)]).areas(inner);

    frame.render_widget(
        Paragraph::new(now_playing_line(
            &snap,
            app,
            !page_draws_progress,
            now_playing.width as usize,
        )),
        now_playing,
    );
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            hint_line(app),
            Style::default().fg(Color::DarkGray),
        ))),
        hints,
    );
}

/// The key hints, which vary by mode and view because there is only room for
/// one line of them and the useful keys differ.
///
/// Mode matters as much as view: in the search box every printable key is text,
/// so a hint offering `L` there would be describing a key that types an L. That
/// is exactly the trap this line exists to keep users out of, which is why the
/// editing hint names `^L` and `Esc` instead.
///
/// All three fit the reserved width exactly; adding to any of them means
/// widening the column, not letting it truncate.
fn hint_line(app: &App) -> &'static str {
    // Something is playing and the page built around it is not on screen. That
    // page is entered by playing rather than by asking for it, so a user who
    // leaves has no reason to know it can be gone back to -- and unlike every
    // other view here, there is nothing on screen to stumble into that reopens
    // it. Naming `P` is the whole of what makes it reachable a second time.
    //
    // Not offered in the search box, where `P` types a P like every other
    // printable key, and not on the player page, which is already open.
    let away = app.now.is_some() && app.view != View::Playing;
    match (app.mode, app.view) {
        (Mode::Editing, _) => HINTS_EDITING,
        // Before the signed-out line: every key it names works with no session,
        // and this is the one view a signed-out user reaches by playing
        // something rather than by asking for it.
        (_, View::Playing) => HINTS_PLAYING,
        // The landing page needs no session and offers no key that does, so it
        // keeps its own hints rather than the signed-out ones -- which exist to
        // avoid advertising keys that can only answer with a sign-in.
        (_, View::Home) if away => HINTS_HOME_PLAYING,
        (_, View::Home) => HINTS_HOME,
        _ if !app.signed_in && away => HINTS_SIGNED_OUT_PLAYING,
        _ if !app.signed_in => HINTS_SIGNED_OUT,
        (_, View::Playlists) if away => HINTS_PLAYLISTS_PLAYING,
        (_, View::Playlists) => HINTS_PLAYLISTS,
        (_, View::Tracks) if away => HINTS_TRACKS_PLAYING,
        (_, View::Tracks) => HINTS_TRACKS,
    }
}

/// What is playing, for the bar along the bottom.
///
/// `clock` is false while the player page is showing a progress bar of its own:
/// the elapsed time belongs beside that bar, and a second copy of the same
/// ticking number in the corner is one the eye has to keep checking against.
fn now_playing_line<'a>(snap: &'a Snapshot, app: &'a App, clock: bool, width: usize) -> Line<'a> {
    // An error outranks everything else -- it is the thing the user must see.
    //
    // Truncated rather than left to the buffer's own clipping, which cuts at
    // the column the hints begin at and leaves the last word of the message
    // running into the first word of a hint. An ellipsis at least says that
    // there was more of it.
    if let Some(err) = &snap.error {
        return Line::from(Span::styled(
            format!(" {}", truncate(err, width.saturating_sub(1))),
            Style::default().fg(Color::Red),
        ));
    }

    let (symbol, colour) = match snap.state {
        PlayState::Idle => ("-", Color::DarkGray),
        PlayState::Buffering => ("~", Color::Yellow),
        PlayState::Playing => (">", Color::Green),
        PlayState::Paused => ("=", Color::Yellow),
    };

    if snap.state == PlayState::Idle {
        return Line::from(Span::styled(
            format!(" {}", truncate(&app.status, width.saturating_sub(1))),
            Style::default().fg(Color::DarkGray),
        ));
    }

    let mut spans = vec![Span::styled(
        format!(" {symbol} "),
        Style::default().fg(colour),
    )];
    if clock {
        let secs = snap.position.as_secs();
        spans.push(Span::raw(format!("{}:{:02}  ", secs / 60, secs % 60)));
    }
    spans.push(Span::styled(
        truncate(&snap.title, 60),
        Style::default().add_modifier(Modifier::BOLD),
    ));
    Line::from(spans)
}

/// Splits a results row into title, artist and album widths.
///
/// Columns are granted from the left and dropped from the right, so a narrow
/// pane spends what it has on the title -- the one field a song cannot be
/// identified without. A zero width means the column is not drawn at all.
/// Duration is never dropped: it is eight columns and worth them.
fn columns(width: usize) -> (usize, usize, usize) {
    // One leading space, one trailing, plus the duration column.
    let fixed = DURATION_WIDTH + 2;
    let for_metadata = width.saturating_sub(fixed + MIN_TITLE_WIDTH);

    let artist = if for_metadata >= ARTIST_WIDTH {
        ARTIST_WIDTH
    } else {
        0
    };
    let album = if for_metadata >= ARTIST_WIDTH + ALBUM_WIDTH {
        ALBUM_WIDTH
    } else {
        0
    };

    (
        width.saturating_sub(fixed + artist + album),
        artist,
        album,
    )
}

/// Renders one fixed-width column: truncated to fit, padded out to `width`, and
/// always leaving a trailing space so neighbouring columns cannot run together.
fn cell(text: &str, width: usize) -> String {
    let text = truncate(text, width.saturating_sub(1));
    let pad = width.saturating_sub(display_width(&text));
    format!("{text}{:pad$}", "", pad = pad)
}

/// Truncates to `max` *terminal columns*, adding an ellipsis when it cuts.
///
/// Columns rather than characters, because they are not the same thing: a CJK
/// glyph occupies two cells, so counting characters lets a Japanese title
/// overflow its column and shove everything after it out of alignment. This is
/// the same measure ratatui uses to lay out what it draws.
///
/// Truncation is also per character, never per byte -- slicing a `&str` by byte
/// index would panic in the middle of a codepoint.
fn truncate(s: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    if display_width(s) <= max {
        return s.to_string();
    }

    // Leave a column for the ellipsis, which is itself one cell wide.
    let budget = max.saturating_sub(1);
    let mut out = String::new();
    let mut used = 0;
    for c in s.chars() {
        let w = char_width(c);
        if used + w > budget {
            break;
        }
        out.push(c);
        used += w;
    }
    out.push('…');
    // At most `max` columns, not exactly: a wide glyph can leave one column
    // unfilled, and padding it out is `cell`'s job, not this function's.
    out
}

fn display_width(s: &str) -> usize {
    unicode_width::UnicodeWidthStr::width(s)
}

/// Control characters and other zero-width oddities are treated as one column
/// so that a hostile title cannot make a row measure less than it draws.
fn char_width(c: char) -> usize {
    unicode_width::UnicodeWidthChar::width(c).unwrap_or(1).max(1)
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::*;
    use crate::source::watch::{Comment, Comments, Lyrics};

    /// A 16:9 cover, as every YouTube thumbnail is once shrunk.
    fn wide() -> Cover {
        Cover::solid(160, 90)
    }

    #[test]
    fn fit_fills_the_width_of_a_tall_pane() {
        // 32 usable columns, 20 rows: 16:9 runs out of width first.
        assert_eq!(fit(&wide(), 32, 20), (32, 18));
    }

    #[test]
    fn fit_gives_up_width_in_a_short_pane() {
        // 4 rows is 8 pixel rows, so the width has to come down to match.
        assert_eq!(fit(&wide(), 32, 4), (14, 8));
    }

    #[test]
    fn fit_keeps_a_square_cover_square() {
        assert_eq!(fit(&Cover::solid(300, 300), 32, 40), (32, 32));
    }

    #[test]
    fn fit_yields_nothing_for_a_degenerate_pane() {
        // A pane too small to hold anything must draw nothing rather than
        // dividing by a zero dimension.
        assert_eq!(fit(&wide(), 0, 10), (0, 0));
        assert_eq!(fit(&wide(), 10, 0), (0, 0));
    }

    /// Draws for real and reads the cells back, which is the only way to know
    /// the two halves of a `▀` land on the pixels they are supposed to.
    #[test]
    fn cover_pane_paints_pixel_rows_as_half_blocks() {
        // One column, red over blue: the upper pixel must become a cell's
        // foreground and the lower one its background.
        let cover = Cover::from_rgb(1, 2, vec![255, 0, 0, 0, 0, 255]);
        let mut terminal = Terminal::new(TestBackend::new(12, 8)).unwrap();
        terminal
            .draw(|frame| render_cover(frame, &cover, Rect::new(0, 0, 12, 8), CoverSize::Side))
            .unwrap();
        let buf = terminal.backend().buffer();

        // The image is 6 pixels wide inside a 10-column interior, so it starts
        // two columns in, one row below the top border.
        let top = &buf[(3, 1)];
        assert_eq!(top.symbol(), "▀");
        assert_eq!(top.fg, Color::Rgb(255, 0, 0));
        assert_eq!(top.bg, Color::Rgb(255, 0, 0));

        let bottom = &buf[(3, 6)];
        assert_eq!(bottom.fg, Color::Rgb(0, 0, 255));
        assert_eq!(bottom.bg, Color::Rgb(0, 0, 255));
    }

    #[test]
    fn cover_pane_is_only_as_tall_as_the_picture() {
        // 16:9 across 32 usable columns needs 9 text rows, so the block closes
        // at row 10 rather than running to the bottom of a 20-row pane.
        let cover = Cover::solid(160, 90);
        let mut terminal = Terminal::new(TestBackend::new(34, 20)).unwrap();
        terminal
            .draw(|frame| render_cover(frame, &cover, Rect::new(0, 0, 34, 20), CoverSize::Side))
            .unwrap();
        let buf = terminal.backend().buffer();

        assert_eq!(buf[(0, 0)].symbol(), "┌");
        assert_eq!(buf[(0, 10)].symbol(), "└");
        assert_eq!(buf[(0, 11)].symbol(), " ", "nothing painted past the block");
    }

    #[test]
    fn fit_cells_keeps_the_aspect_ratio_within_half_a_cell() {
        let cover = Cover::solid(160, 90);
        let cell = (10, 20);

        // Width-bound: 42 columns is 420 px, which wants 236 px of height --
        // just under 12 rows, so 12 rows it is rather than a squashed 11.
        assert_eq!(fit_cells(&cover, (42, 30), cell), (42, 12));
        let (w, h) = (42 * 10, 12 * 20);
        let error = (w as f32 / h as f32) / (160.0 / 90.0);
        assert!((0.95..1.05).contains(&error), "aspect off by {error}");
    }

    #[test]
    fn fit_cells_gives_up_columns_when_rows_run_out() {
        let cover = Cover::solid(160, 90);
        assert_eq!(fit_cells(&cover, (42, 8), (10, 20)), (28, 8));
    }

    #[test]
    fn fit_cells_makes_a_square_cover_square() {
        // A cell twice as tall as wide means a square image needs half as many
        // rows as columns.
        assert_eq!(
            fit_cells(&Cover::solid(300, 300), (42, 30), (10, 20)),
            (42, 21)
        );
    }

    #[test]
    fn fit_cells_yields_nothing_for_a_degenerate_pane() {
        let cover = Cover::solid(160, 90);
        assert_eq!(fit_cells(&cover, (0, 10), (10, 20)), (0, 0));
        assert_eq!(fit_cells(&cover, (10, 0), (10, 20)), (0, 0));
    }

    /// The sixel pane must reserve its cells rather than paint them: the
    /// pixels arrive by escape sequence, and a redraw over them would shred
    /// the image.
    #[test]
    fn sixel_pane_reserves_its_cells_and_draws_no_blocks() {
        let cover = Cover::solid(160, 90);
        let graphics = Graphics {
            sixel: true,
            cell: (10, 20),
        };
        let mut terminal = Terminal::new(TestBackend::new(34, 20)).unwrap();
        let mut plan = None;
        let mut reserved = 0;
        terminal
            .draw(|frame| {
                plan = plan_image(
                    frame,
                    &cover,
                    Rect::new(0, 0, 34, 20),
                    graphics,
                    CoverSize::Side,
                );
                // Read back from the frame, not the backend: a skipped cell is
                // by definition one the backend never hears about.
                if let Some(p) = plan {
                    let buffer = frame.buffer_mut();
                    for y in p.row..p.row + p.height / 20 {
                        for x in p.col..p.col + p.width / 10 {
                            if buffer[(x, y)].diff_option == CellDiffOption::Skip {
                                reserved += 1;
                            }
                        }
                    }
                }
            })
            .unwrap();
        let plan = plan.expect("a plan for a pane this size");

        // 32 usable columns at 10 px, and the row count that keeps 16:9.
        assert_eq!((plan.width, plan.height), (320, 180));
        assert_eq!((plan.col, plan.row), (1, 1));
        assert_eq!(reserved, 32 * 9, "every cell under the image is reserved");

        // Nothing but the frame reaches the terminal: no half-blocks, and the
        // block still shrinks to the picture.
        let buf = terminal.backend().buffer();
        assert_eq!(buf[(plan.col, plan.row)].symbol(), " ");
        assert_eq!(buf[(0, 0)].symbol(), "┌");
        assert_eq!(buf[(0, plan.height / 20 + 1)].symbol(), "└");
    }

    /// A full-window cover is the largest picture the terminal can hold, and
    /// the only remaining lever on resolution.
    #[test]
    fn full_size_cover_fills_the_window_and_centres() {
        // A 110x28 main area on a 10x20 cell: 1080 px of usable width against
        // 520 of usable height, so a square cover is bound by the height.
        let cover = Cover::solid(720, 720);
        let graphics = Graphics {
            sixel: true,
            cell: (10, 20),
        };
        let area = Rect::new(0, 0, 110, 28);
        let mut terminal = Terminal::new(TestBackend::new(110, 28)).unwrap();
        let mut plan = None;
        terminal
            .draw(|frame| plan = plan_image(frame, &cover, area, graphics, CoverSize::Full))
            .unwrap();

        let plan = plan.expect("a plan for a window this size");
        // No frame at full size, so all 28 rows are picture: 560 px, and the
        // width follows to keep it square.
        assert_eq!((plan.width, plan.height), (560, 560));
        assert_eq!((plan.col, plan.row), ((110 - 56) / 2, 0), "centred");
    }

    #[test]
    fn full_size_drops_the_frame_that_the_side_pane_keeps() {
        let area = Rect::new(0, 0, 34, 20);
        assert_eq!(usable(area, CoverSize::Full), (34, 20));
        assert_eq!(usable(area, CoverSize::Side), (32, 18));
    }

    use super::{
        ALBUM_WIDTH, ARTIST_WIDTH, DURATION_WIDTH, MIN_TITLE_WIDTH, cell, columns, header_line,
    };

    /// Whatever is dropped, the columns must still exactly fill the row --
    /// otherwise the selected row's highlight stops short or wraps.
    fn assert_fills(width: usize) {
        let (title, artist, album) = columns(width);
        assert_eq!(
            title + artist + album + DURATION_WIDTH + 2,
            width,
            "columns do not fill a {width}-wide row"
        );
    }

    #[test]
    fn a_wide_row_carries_every_column() {
        let (title, artist, album) = columns(120);
        assert_eq!((artist, album), (ARTIST_WIDTH, ALBUM_WIDTH));
        assert!(title >= MIN_TITLE_WIDTH);
        assert_fills(120);
    }

    #[test]
    fn album_goes_first_when_the_row_narrows() {
        // Room for the artist but not both.
        let width = DURATION_WIDTH + 2 + MIN_TITLE_WIDTH + ARTIST_WIDTH;
        let (_, artist, album) = columns(width);
        assert_eq!((artist, album), (ARTIST_WIDTH, 0));
        assert_fills(width);
    }

    #[test]
    fn a_narrow_row_keeps_only_the_title() {
        let (title, artist, album) = columns(40);
        assert_eq!((artist, album), (0, 0));
        assert_eq!(title, 40 - DURATION_WIDTH - 2);
        assert_fills(40);
    }

    #[test]
    fn columns_never_overflow_a_tiny_row() {
        // A pane can be squeezed below the fixed columns entirely, leaving
        // nothing for the title. Saturating arithmetic has to keep that from
        // wrapping into an enormous width.
        for width in 0..=(DURATION_WIDTH + 2) {
            assert_eq!(columns(width), (0, 0, 0), "at width {width}");
        }
    }

    /// Prints the results grid at several widths, for eyeballing a layout
    /// change. It is what caught wide CJK glyphs breaking every column.
    ///
    /// It rebuilds the row by hand rather than calling the renderer, which
    /// needs a live `App`; if the row layout changes, this has to change with
    /// it or the preview quietly stops resembling the real thing.
    ///
    /// `cargo test preview_layout -- --ignored --nocapture`
    #[test]
    #[ignore = "prints a layout preview rather than asserting"]
    fn preview_layout() {
        let rows = [
            ("Harder, Better, Faster, Stronger", "Daft Punk", "Discovery", "3:47"),
            ("Get Lucky (feat. Pharrell Williams and Nile Rodgers)", "Daft Punk, Pharrell Williams & Nile Rodgers", "Random Access Memories", "6:10"),
            ("Da Funk", "Daft Punk", "", "5:35"),
            ("日本語のタイトル", "アーティスト", "アルバム", "4:02"),
        ];
        for width in [96usize, 72, 44] {
            let (t, ar, al) = columns(width);
            println!("\n--- {width} cols (title {t}, artist {ar}, album {al}) ---");
            println!("|{}|", header_line(t, ar, al).spans.iter().map(|s| s.content.as_ref()).collect::<String>());
            for (title, artist, album, duration) in rows {
                let mut line = format!(" {}", cell(title, t));
                if ar > 0 { line += &cell(artist, ar); }
                if al > 0 { line += &cell(album, al); }
                line += &format!("{duration:>DURATION_WIDTH$} ");
                println!("|{line}|");
            }
        }
    }

    /// `cargo test preview_sign_in -- --ignored --nocapture`
    #[test]
    #[ignore = "prints a layout preview rather than asserting"]
    fn preview_sign_in() {
        let phases = [
            (
                "connecting",
                24,
                SignIn::Connecting {
                    started: Instant::now(),
                },
            ),
            ("waiting", 24, waiting()),
            // The same phase in a terminal too short for the full panel.
            ("waiting, short terminal", 9, waiting()),
            (
                "failed",
                24,
                SignIn::Failed {
                    reason: "the saved sign-in is no longer valid -- press A to sign in again"
                        .to_string(),
                },
            ),
        ];
        for (name, height, phase) in phases {
            println!("\n--- {name} ---");
            for row in drawn(72, height, &phase) {
                println!("{}", row.trim_end());
            }
        }
    }

    fn playlist(title: &str, count: Option<u64>) -> Playlist {
        Playlist {
            id: "PL123".to_string(),
            title: title.to_string(),
            count,
        }
    }

    fn liked_songs() -> Playlist {
        Playlist {
            id: crate::source::library::LIKED_ID.to_string(),
            title: "Liked songs".to_string(),
            count: None,
        }
    }

    fn text_of(line: &Line) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn a_playlist_row_fills_its_width_exactly() {
        // Swept from zero for the same reason the results header is: a row that
        // overruns pushes the count off the edge, and one that falls short
        // leaves the selection highlight ragged. Zero included, because a pane
        // that narrow must draw nothing rather than panic.
        for width in 0..120 {
            for row in [playlist("Discovery", Some(14)), liked_songs()] {
                assert_eq!(
                    playlist_line(&row, false, width).width(),
                    width,
                    "{:?} does not fill a {width}-wide pane",
                    row.title
                );
            }
        }
    }

    #[test]
    fn a_narrow_playlist_row_drops_the_count_before_the_name() {
        // Wide enough for both.
        let wide = text_of(&playlist_line(&playlist("Discovery", Some(14)), false, 40));
        assert!(wide.contains("Discovery") && wide.contains("14"));

        // Not wide enough: the name is what identifies the row, so the count
        // is what goes.
        let narrow = text_of(&playlist_line(&playlist("Discovery", Some(14)), false, 12));
        assert!(!narrow.contains("14"));
    }

    #[test]
    fn a_long_playlist_name_is_truncated_rather_than_overflowing() {
        let long = playlist("a playlist with a preposterously long name", Some(3));
        let line = playlist_line(&long, false, 24);
        assert_eq!(line.width(), 24);
        assert!(text_of(&line).contains('…'));
    }

    #[test]
    fn liked_songs_is_marked_and_carries_no_count() {
        // The API does not report how many liked videos there are, and a
        // made-up number would be worse than none. The marker keys off the
        // pseudo-id, not the title, so a real playlist called "Liked songs"
        // does not get it.
        let text = text_of(&playlist_line(&liked_songs(), false, 40));
        assert!(text.contains('♥'));
        assert!(text.contains("Liked songs"));

        let impostor = text_of(&playlist_line(&playlist("Liked songs", Some(7)), false, 40));
        assert!(!impostor.contains('♥'));
        assert!(impostor.contains('7'));
    }

    use crate::source::home::Target;

    fn song(title: &str, subtitle: &str) -> Card {
        Card {
            title: title.to_string(),
            subtitle: subtitle.to_string(),
            target: Target::Play {
                video_id: "aaaaaaaaaaa".to_string(),
            },
        }
    }

    fn album(title: &str, subtitle: &str) -> Card {
        Card {
            title: title.to_string(),
            subtitle: subtitle.to_string(),
            target: Target::Open {
                browse_id: "MPREb_x".to_string(),
            },
        }
    }

    /// Prints the landing page at a couple of window sizes, for eyeballing a
    /// layout change.
    ///
    /// `cargo test preview_home -- --ignored --nocapture`
    #[test]
    #[ignore = "prints a layout preview rather than asserting"]
    fn preview_home() {
        let shelves = vec![
            quick_picks(),
            Shelf {
                title: "Albums for you".to_string(),
                cards: vec![
                    album("Currents", "Album • Tame Impala"),
                    album("Luv(sic) Hexalogy", "Album • Nujabes"),
                    album("kyoto rain", "Album • Seycara Orchestral"),
                    album("Pink Tape", "Album • Lil Uzi Vert"),
                ],
            },
            Shelf {
                title: "New releases".to_string(),
                cards: vec![
                    song("GAMBLER'S FALLACY", "Single • AZALI & stickii B"),
                    song("i have a secret", "Single • ThxSoMch"),
                    song("Người Im Lặng Gặp Người Hay Nói", "Song • HIEUTHUHAI"),
                ],
            },
        ];

        for (width, height) in [(100u16, 22u16), (64, 16)] {
            println!("\n--- {width}x{height} ---");
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal
                .draw(|frame| {
                    let block = Block::bordered().title(" home ");
                    let area = Rect::new(0, 0, width, height);
                    let inner = block.inner(area);
                    frame.render_widget(block, area);
                    let cursor = HomeCursor {
                        top: 0,
                        shelf: 1,
                        card: 1,
                    };
                    render_feed(frame, &shelves, inner, cursor, &[0, 0, 0]);
                })
                .unwrap();

            let buf = terminal.backend().buffer();
            for y in 0..height {
                let row: String = (0..width).map(|x| buf[(x, y)].symbol()).collect();
                println!("{}", row.trim_end());
            }
        }
    }

    /// A track as the queue holds them.
    fn queued(title: &str, artist: &str, secs: u64) -> Track {
        Track {
            id: title.to_string(),
            title: title.to_string(),
            uploader: artist.to_string(),
            duration: Some(Duration::from_secs(secs)),
            album: None,
            playlist_item_id: None,
        }
    }

    /// The player page as it looks a second into a track, queue and all.
    fn playing() -> NowPlaying {
        let mut now = NowPlaying::new(&Track {
            id: "letithappen".to_string(),
            title: "Let It Happen".to_string(),
            uploader: "Tame Impala".to_string(),
            duration: Some(Duration::from_secs(468)),
            album: Some("Currents".to_string()),
            playlist_item_id: None,
        });
        now.queue_title = "Let It Happen Mix".to_string();
        now.queue = vec![
            queued("Let It Happen", "Tame Impala", 468),
            queued("The Moment", "Tame Impala", 256),
            queued("Clean Slate", "Passion Mango", 180),
            queued("Chest Pain (I Love)", "Malcolm Todd", 201),
            queued("Razzmatazz", "I DONT KNOW HOW BUT THEY FOUND ME", 259),
            queued("Show Me How", "Men I Trust", 216),
            queued("Dust Bunny", "Crumb", 185),
        ];
        now.playing = Some(0);
        now
    }

    /// One panel drawn on its own, as one string per row.
    fn drawn_panel(now: &NowPlaying, width: u16, height: u16) -> Vec<String> {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| {
                let area = Rect::new(0, 0, width, height);
                let [tabs, content] =
                    Layout::vertical([Constraint::Length(2), Constraint::Min(0)]).areas(area);
                render_tabs(frame, now.tab, tabs);
                match now.tab {
                    Tab::UpNext => render_up_next(frame, now, content),
                    Tab::Lyrics => render_lyrics(frame, now, content),
                    Tab::Comments => render_comments(frame, now, content),
                    Tab::Related => render_related(frame, now, content),
                };
            })
            .unwrap();

        let buf = terminal.backend().buffer();
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    #[test]
    fn the_open_tab_is_the_underlined_one() {
        let mut now = playing();
        now.tab = Tab::Comments;
        let rows = drawn_panel(&now, 44, 6);

        assert!(rows[0].contains("COMMENTS"), "every tab is named: {:?}", rows[0]);
        // The rule sits under the open tab and nothing else, which is the only
        // thing on the row that says which panel is showing.
        let rule = &rows[1];
        assert_eq!(rule.trim(), "─".repeat("COMMENTS".chars().count()));
        assert_eq!(
            rule.find('─'),
            rows[0].find("COMMENTS"),
            "the rule sits under the letters, not the padding: {rule:?}"
        );
    }

    #[test]
    fn the_queue_marks_what_is_playing() {
        let rows = drawn_panel(&playing(), 44, 12);

        assert!(rows[2].contains("Playing from"));
        assert!(rows[3].contains("Let It Happen Mix"));
        // The marker is on the playing track and on no other row.
        let marked: Vec<&String> = rows.iter().filter(|row| row.contains('▶')).collect();
        assert_eq!(marked.len(), 1, "exactly one row plays: {rows:#?}");
        assert!(marked[0].contains("Let It Happen"));
        assert!(marked[0].contains("7:48"), "and states its length: {:?}", marked[0]);
    }

    #[test]
    fn the_queue_heading_does_not_scroll_with_the_rows() {
        // Scrolled to the bottom of a queue that does not fit. What the queue
        // is a queue of stays true wherever the cursor is, so it stays put.
        let mut now = playing();
        now.cursor[Tab::UpNext.index()] = now.queue.len() - 1;
        let rows = drawn_panel(&now, 44, 9);

        assert!(rows[2].contains("Playing from"), "{rows:#?}");
        assert!(rows[3].contains("Let It Happen Mix"));
        assert!(
            rows.iter().any(|row| row.contains("Dust Bunny")),
            "the last row scrolled to should be visible: {rows:#?}"
        );
        assert!(
            !rows.iter().any(|row| row.contains("The Moment")),
            "and the rows above it should have scrolled off: {rows:#?}"
        );
    }

    #[test]
    fn an_unfetched_panel_says_it_is_loading_and_a_failed_one_says_why() {
        let mut now = playing();
        now.tab = Tab::Lyrics;
        assert!(drawn_panel(&now, 44, 6)[2].contains("loading"));

        now.lyrics = Panel::Empty("no lyrics are published for this track".to_string());
        let rows = drawn_panel(&now, 44, 6);
        assert!(
            rows[2].contains("no lyrics"),
            "the reason belongs in the panel, not the status bar: {rows:#?}"
        );
    }

    #[test]
    fn lyrics_scroll_by_line_and_stop_a_screenful_short_of_the_end() {
        let mut now = playing();
        now.tab = Tab::Lyrics;
        now.lyrics = Panel::Ready(Lyrics {
            text: (1..=20).map(|n| format!("line {n}\n")).collect(),
            source: Some("Source: Musixmatch".to_string()),
        });

        let top = drawn_panel(&now, 44, 8);
        assert!(top[2].contains("Musixmatch"), "{top:#?}");
        assert!(top[4].contains("line 1"));

        // Six lines of content in an eight-row pane, two of them tabs: the last
        // line has to stay on screen rather than scrolling past the top.
        now.cursor[Tab::Lyrics.index()] = usize::MAX;
        let bottom = drawn_panel(&now, 44, 8);
        assert!(
            bottom.iter().any(|row| row.contains("line 20")),
            "the end of the lyrics should be reachable: {bottom:#?}"
        );
    }

    #[test]
    fn a_comment_shows_who_wrote_it_when_and_what_it_got() {
        let mut now = playing();
        now.tab = Tab::Comments;
        now.comments = Panel::Ready(Comments {
            total: "8,407 Comments".to_string(),
            items: vec![Comment {
                author: "@SpeartonFromOrder".to_string(),
                published: "4 months ago".to_string(),
                text: "Don't search for best part. Just let it happen".to_string(),
                likes: "10K".to_string(),
                replies: "67".to_string(),
            }],
        });

        let rows = drawn_panel(&now, 44, 10);
        assert!(rows[2].contains("8,407 Comments"), "{rows:#?}");
        assert!(rows[4].contains("@SpeartonFromOrder"));
        assert!(rows[4].contains("4 months ago"));
        // Wrapped to the panel rather than truncated: a comment cut off at the
        // right margin is one nobody can read the point of.
        assert!(rows[5].contains("Don't search for best part."), "{rows:#?}");
        assert!(rows[6].contains("happen"), "{rows:#?}");
        assert!(rows.iter().any(|row| row.contains("10K likes")));
        assert!(rows.iter().any(|row| row.contains("67 replies")));
    }

    #[test]
    fn related_lists_each_shelf_under_its_own_heading() {
        let mut now = playing();
        now.tab = Tab::Related;
        now.related = Panel::Ready(vec![
            Shelf {
                title: "You might also like".to_string(),
                cards: vec![
                    song("Eventually", "Tame Impala • Currents"),
                    song("Yes I'm Changing", "Tame Impala • Currents"),
                ],
            },
            Shelf {
                title: "Recommended playlists".to_string(),
                cards: vec![album("one step, another step", "Playlist")],
            },
        ]);

        let rows = drawn_panel(&now, 44, 10);
        assert!(rows[2].contains("You might also like"), "{rows:#?}");
        assert!(rows[3].contains("Eventually"));
        assert!(rows[3].contains("Tame Impala"));
        assert!(rows.iter().any(|row| row.contains("Recommended playlists")));
        assert!(rows.iter().any(|row| row.contains("one step, another step")));
    }

    #[test]
    fn the_progress_bar_fills_in_proportion() {
        let snap = |secs: u64| Snapshot {
            position: Duration::from_secs(secs),
            ..Default::default()
        };
        let filled = |line: Line| {
            line.spans
                .iter()
                .map(|span| span.content.matches('━').count())
                .sum::<usize>()
        };

        let total = Some(Duration::from_secs(100));
        // 40 columns less the " 0:00 / 1:40" clock and its space leaves 27.
        assert_eq!(filled(progress_line(&snap(0), total, 40)), 0);
        assert_eq!(filled(progress_line(&snap(100), total, 40)), 27);
        assert_eq!(filled(progress_line(&snap(50), total, 40)), 14);

        // Past the end, which happens for a moment on every track: the
        // container's length and what the decoder yields never quite agree.
        assert_eq!(filled(progress_line(&snap(105), total, 40)), 27);
    }

    #[test]
    fn a_track_of_unknown_length_gets_a_clock_and_no_bar() {
        let snap = Snapshot {
            position: Duration::from_secs(65),
            ..Default::default()
        };
        // A livestream has no end to draw a bar towards, and one drawn anyway
        // would have to invent where it is.
        let line = progress_line(&snap, None, 40);
        assert_eq!(line.spans.len(), 1);
        assert_eq!(line.spans[0].content.trim(), "1:05");
    }

    #[test]
    fn the_cursor_stays_near_the_middle_of_a_long_list() {
        // Nothing to scroll: everything fits.
        assert_eq!(centred_offset(3, 10, 5), 0);
        // Near the top, the list stays pinned there rather than centring a
        // cursor that has nothing above it.
        assert_eq!(centred_offset(1, 10, 40), 0);
        assert_eq!(centred_offset(20, 10, 40), 15);
        // And at the bottom it stops rather than scrolling past the last row.
        assert_eq!(centred_offset(39, 10, 40), 30);
        assert_eq!(centred_offset(5, 0, 40), 0);
    }

    #[test]
    fn the_track_details_name_the_song_the_artist_and_the_album() {
        let now = playing();
        assert_eq!(now.byline(), "Tame Impala • Currents");

        let mut terminal = Terminal::new(TestBackend::new(40, INFO_HEIGHT)).unwrap();
        terminal
            .draw(|frame| {
                let snap = Snapshot {
                    position: Duration::from_secs(1),
                    volume: 0.8,
                    ..Default::default()
                };
                render_track_info(frame, &now, snap, Rect::new(0, 0, 40, INFO_HEIGHT));
            })
            .unwrap();

        let buf = terminal.backend().buffer();
        let row = |y: u16| {
            (0..40)
                .map(|x| buf[(x, y)].symbol())
                .collect::<String>()
                .trim_end()
                .to_string()
        };
        assert_eq!(row(0), "Let It Happen");
        assert_eq!(row(1), "Tame Impala • Currents");
        assert!(row(3).ends_with("0:01 / 7:48"), "{:?}", row(3));
        assert_eq!(row(4), "vol ━━━━━━━━━━──  80%");
    }

    #[test]
    fn the_volume_bar_fills_against_a_hundred_percent() {
        let bar = |volume: f32| volume_line(volume, 40).spans[1].content.chars().count();

        assert_eq!(bar(0.0), 0);
        assert_eq!(bar(0.5), VOLUME_BAR_WIDTH / 2);
        assert_eq!(bar(1.0), VOLUME_BAR_WIDTH);
        // Boost: the bar has nowhere further to go, so it says so in colour
        // rather than by growing a scale nothing else is measured against.
        assert_eq!(bar(2.0), VOLUME_BAR_WIDTH);
        assert_eq!(volume_line(2.0, 40).spans[1].style.fg, Some(Color::Yellow));
        assert_eq!(volume_line(1.0, 40).spans[1].style.fg, Some(Color::Gray));
    }

    #[test]
    fn a_volume_with_no_room_for_a_bar_is_still_a_number() {
        // The panel is the width of the label and no more. A bar of two cells
        // would be decoration; the percentage is what is actually read.
        let line = volume_line(0.75, 12);
        assert_eq!(line.spans.len(), 1);
        assert_eq!(line.spans[0].content, "vol 75%");
    }

    /// Prints the whole player page, for eyeballing it against what YouTube
    /// Music draws.
    ///
    /// `cargo test preview_player -- --ignored --nocapture`
    #[test]
    #[ignore = "prints a layout preview rather than asserting"]
    fn preview_player() {
        let mut now = playing();
        now.lyrics = Panel::Ready(Lyrics {
            text: "It's always around me, all this noise, but\n\n\
                   Not nearly as loud as the voice saying\n\n\
                   \"Let it happen, let it happen\"\n\n\
                   \"Just let it happen, let it happen\"\n\n\
                   All this running around"
                .to_string(),
            source: Some("Source: Musixmatch".to_string()),
        });
        now.comments = Panel::Ready(Comments {
            total: "8,407 Comments".to_string(),
            items: vec![
                Comment {
                    author: "@SpeartonFromOrder".to_string(),
                    published: "4 months ago".to_string(),
                    text: "Don't search for best part. Just let it happen".to_string(),
                    likes: "10K".to_string(),
                    replies: "67".to_string(),
                },
                Comment {
                    author: "@jujuca.lol912".to_string(),
                    published: "5 months ago".to_string(),
                    text: "I won't let depression win".to_string(),
                    likes: "10K".to_string(),
                    replies: "226".to_string(),
                },
            ],
        });
        now.related = Panel::Ready(vec![Shelf {
            title: "You might also like".to_string(),
            cards: vec![
                song("Eventually", "Tame Impala • Currents"),
                song("Yes I'm Changing", "Tame Impala • Currents"),
                song("Breathe Deeper", "Tame Impala • The Slow Rush"),
            ],
        }]);

        for (width, height) in [(44u16, 16u16), (36, 16)] {
            for tab in Tab::ALL {
                now.tab = tab;
                println!("\n--- {} at {width}x{height} ---", tab.label());
                for row in drawn_panel(&now, width, height) {
                    println!("|{row}");
                }
            }
        }

        println!("\n--- the details under the cover, at 46 columns ---");
        let mut terminal = Terminal::new(TestBackend::new(46, INFO_HEIGHT)).unwrap();
        terminal
            .draw(|frame| {
                let snap = Snapshot {
                    position: Duration::from_secs(133),
                    volume: 0.8,
                    ..Default::default()
                };
                render_track_info(frame, &now, snap, Rect::new(0, 0, 46, INFO_HEIGHT));
            })
            .unwrap();
        let buf = terminal.backend().buffer();
        for y in 0..INFO_HEIGHT {
            let row: String = (0..46).map(|x| buf[(x, y)].symbol()).collect();
            println!("|{}", row.trim_end());
        }
    }

    /// A shelf drawn on its own, as one string per row.
    fn drawn_shelf(width: u16, shelf: &Shelf, cursor: ShelfCursor) -> Vec<String> {
        let height = CARD_HEIGHT + 1;
        let across = (width / CARD_WIDTH).max(1);
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| {
                render_shelf(
                    frame,
                    shelf,
                    Rect::new(0, 0, width, height),
                    cursor,
                    (across, width / across),
                );
            })
            .unwrap();
        let buf = terminal.backend().buffer();
        (0..height)
            .map(|y| (0..width).map(|x| buf[(x, y)].symbol()).collect())
            .collect()
    }

    fn quick_picks() -> Shelf {
        Shelf {
            title: "Quick picks".to_string(),
            cards: vec![
                song("Is It True", "Tame Impala"),
                song("Nice Boys", "TEMPOREX"),
                song("Homage", "Mild High Club"),
                song("Feather", "Nujabes"),
                song("Shake", "Yeek"),
            ],
        }
    }

    fn cursor(selected: usize, offset: usize) -> ShelfCursor {
        ShelfCursor {
            focused: true,
            selected,
            offset,
        }
    }

    #[test]
    fn a_shelf_draws_its_heading_and_as_many_cards_as_fit() {
        // 80 columns holds three 26-wide cards with four left over.
        let rows = drawn_shelf(80, &quick_picks(), cursor(0, 0));
        assert!(rows[0].contains("Quick picks"), "{rows:#?}");

        let text = rows.join("\n");
        assert!(text.contains("Is It True"));
        assert!(text.contains("Nice Boys"));
        assert!(text.contains("Homage"));
        // The fourth card is off the end of the row, not squeezed onto it.
        assert!(!text.contains("Feather"), "{rows:#?}");
    }

    /// The counter is the only thing on screen that says a shelf runs past the
    /// edge of the window, so a shelf scrolled off its first card must still
    /// say where in it the cursor is.
    #[test]
    fn a_focused_shelf_says_how_far_along_it_the_cursor_is() {
        let rows = drawn_shelf(80, &quick_picks(), cursor(4, 2));
        assert!(rows[0].contains("5 of 5"), "{:?}", rows[0]);

        // Unfocused, there is no cursor in it to report.
        let unfocused = ShelfCursor {
            focused: false,
            selected: 0,
            offset: 0,
        };
        assert!(!drawn_shelf(80, &quick_picks(), unfocused)[0].contains("of 5"));
    }

    /// Enter does two different things depending on the card, and this is the
    /// only thing on the page that says which.
    #[test]
    fn a_card_says_whether_it_plays_or_opens() {
        let shelf = Shelf {
            title: "Albums for you".to_string(),
            cards: vec![song("Creep", "Radiohead"), album("Currents", "Tame Impala")],
        };
        let text = drawn_shelf(80, &shelf, cursor(0, 0)).join("\n");
        assert!(text.contains("▶ Creep"), "{text}");
        assert!(text.contains("≡ Currents"), "{text}");
    }

    /// The selection is what the whole page is navigated by, so it has to be
    /// visible -- and on exactly one card.
    #[test]
    fn exactly_one_card_is_highlighted() {
        let mut terminal = Terminal::new(TestBackend::new(80, CARD_HEIGHT + 1)).unwrap();
        let shelf = quick_picks();
        terminal
            .draw(|frame| {
                render_shelf(
                    frame,
                    &shelf,
                    Rect::new(0, 0, 80, CARD_HEIGHT + 1),
                    cursor(1, 0),
                    (3, 26),
                );
            })
            .unwrap();
        let buf = terminal.backend().buffer();

        // Top-left corner of each of the three drawn cards, on the row under
        // the heading.
        let corners: Vec<Color> = (0..3).map(|card| buf[(card * 26, 1)].fg).collect();
        assert_eq!(corners, [Color::DarkGray, Color::Cyan, Color::DarkGray]);
    }

    /// Titles are what the page is read by, and a card is narrow. Overflowing
    /// one would run its title into the card beside it.
    #[test]
    fn a_long_title_is_truncated_inside_its_card() {
        let shelf = Shelf {
            title: "New releases".to_string(),
            cards: vec![song(
                "Get Lucky (feat. Pharrell Williams and Nile Rodgers)",
                "Daft Punk, Pharrell Williams & Nile Rodgers • 6:10",
            )],
        };
        for width in [40, 60, 80, 120] {
            for row in drawn_shelf(width, &shelf, cursor(0, 0)) {
                assert_eq!(
                    display_width(&row),
                    width as usize,
                    "a {width}-wide shelf drew {row:?}"
                );
            }
        }
    }

    /// A shelf heading is one row and truncates rather than wrapping, so a long
    /// name must not push the counter off the end of it.
    #[test]
    fn a_heading_fits_its_row_whatever_the_shelf_is_called() {
        let shelf = Shelf {
            title: "Recommended because you listened to something with a very long name"
                .to_string(),
            cards: vec![song("a", "b"), song("c", "d")],
        };
        for width in 4..120 {
            assert!(
                heading_line(&shelf, cursor(1, 0), width).width() <= width,
                "heading overflows a {width}-wide shelf"
            );
        }
    }

    /// Everything the sign-in panel drew, as one string per row.
    ///
    /// Read back from a real draw rather than asserted on the `Line`s going in:
    /// the panel is laid out with nested areas and a block inside a block, and
    /// only the finished buffer says whether any of it landed where it was
    /// meant to -- or fit at all.
    fn drawn(width: u16, height: u16, phase: &SignIn) -> Vec<String> {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| render_sign_in(frame, phase))
            .unwrap();
        let buf = terminal.backend().buffer();
        (0..height)
            .map(|y| (0..width).map(|x| buf[(x, y)].symbol()).collect())
            .collect()
    }

    fn waiting() -> SignIn {
        SignIn::Waiting {
            user_code: "BPQX-KLNT".to_string(),
            url: "https://www.google.com/device".to_string(),
            deadline: Instant::now() + Duration::from_secs(587),
        }
    }

    #[test]
    fn the_sign_in_panel_shows_the_code_the_url_and_the_time_left() {
        let rows = drawn(70, 24, &waiting()).join("\n");
        assert!(rows.contains("https://www.google.com/device"), "{rows}");
        assert!(rows.contains("BPQX-KLNT"), "{rows}");
        // 587 s, rendered from the deadline rather than carried as a string.
        assert!(rows.contains("expires in 9:47"), "{rows}");
        assert!(rows.contains("waiting for approval"), "{rows}");
    }

    /// The code is the one thing on this screen the user has to copy off it, so
    /// it gets a box -- and a box that clipped the code would be worse than no
    /// box at all.
    #[test]
    fn the_code_sits_inside_a_box_of_its_own() {
        let rows = drawn(70, 24, &waiting());
        let code_row = rows
            .iter()
            .find(|row| row.contains("BPQX-KLNT"))
            .expect("the code is drawn");
        // Its own border either side of it, inside the panel's.
        assert!(code_row.contains("│  BPQX-KLNT  │"), "{code_row:?}");
    }

    /// A URL that wrapped would be one the user cannot retype, so the panel is
    /// sized from it rather than to a fixed width.
    #[test]
    fn a_long_verification_url_is_not_wrapped() {
        let phase = SignIn::Waiting {
            user_code: "ABCD-EFGH".to_string(),
            url: "https://accounts.google.com/o/oauth2/device/usercode".to_string(),
            deadline: Instant::now() + Duration::from_secs(600),
        };
        let rows = drawn(100, 24, &phase);
        assert!(
            rows.iter()
                .any(|row| row.contains("https://accounts.google.com/o/oauth2/device/usercode")),
            "{rows:#?}"
        );
    }

    /// A terminal too short for the full panel must still show the code. The
    /// full layout is a fixed 13 rows, so without the fallback the bottom of it
    /// -- including the code box -- would simply be cut off.
    #[test]
    fn a_short_terminal_still_gets_the_code_and_the_url() {
        let rows = drawn(70, 8, &waiting()).join("\n");
        assert!(rows.contains("BPQX-KLNT"), "{rows}");
        assert!(rows.contains("https://www.google.com/device"), "{rows}");
        assert!(rows.contains("9:47"), "{rows}");
    }

    #[test]
    fn the_panel_keeps_one_title_across_every_phase() {
        // Three phases of one flow, not three windows. A title that changed
        // under the user would read as the panel having been replaced.
        for phase in [
            SignIn::Connecting {
                started: Instant::now(),
            },
            waiting(),
            SignIn::Failed {
                reason: "sign-in was declined".to_string(),
            },
        ] {
            let rows = drawn(70, 24, &phase).join("\n");
            assert!(rows.contains("sign in with Google"), "{rows}");
        }
    }

    /// The thread behind a failed sign-in is gone, so nothing on screen will
    /// change again on its own -- the panel has to offer the way out.
    #[test]
    fn a_failed_sign_in_states_the_reason_and_offers_the_retry() {
        let phase = SignIn::Failed {
            reason: "sign-in was declined".to_string(),
        };
        let rows = drawn(70, 24, &phase).join("\n");
        assert!(rows.contains("sign-in was declined"), "{rows}");
        assert!(rows.contains("A try again"), "{rows}");
    }

    /// Every phase is sized to its own content, and the row that a too-short
    /// panel loses is the last one -- which in all three cases is the only way
    /// out of it that the panel names.
    #[test]
    fn no_phase_clips_the_key_that_dismisses_it() {
        for phase in [
            SignIn::Connecting {
                started: Instant::now(),
            },
            waiting(),
            SignIn::Failed {
                reason: "sign-in was declined".to_string(),
            },
        ] {
            let rows = drawn(70, 24, &phase).join("\n");
            assert!(rows.contains("Esc"), "{rows}");
        }
    }

    /// A reason long enough to stretch the panel past the other phases' width
    /// is wrapped instead, so the flow keeps one shape throughout.
    #[test]
    fn a_long_failure_reason_wraps_rather_than_widening_the_panel() {
        let phase = SignIn::Failed {
            reason: "the saved sign-in is no longer valid -- press A to sign in again. \
                     If this happens every week, set the OAuth consent screen's publishing \
                     status to \"In production\""
                .to_string(),
        };
        let rows = drawn(120, 24, &phase);
        let panel = rows
            .iter()
            .find(|row| row.contains("sign in with Google"))
            .expect("the panel is drawn");
        assert!(
            display_width(panel.trim()) <= SIGN_IN_MAX_WIDTH as usize,
            "{:?} is {} columns",
            panel.trim(),
            display_width(panel.trim())
        );
        assert!(rows.iter().filter(|r| r.contains('│')).count() > 4, "wrapped onto several rows");
    }

    #[test]
    fn the_ellipsis_cycles_so_a_long_wait_does_not_look_frozen() {
        assert_eq!(ellipsis(Duration::ZERO), ".");
        assert_eq!(ellipsis(Duration::from_millis(500)), "..");
        assert_eq!(ellipsis(Duration::from_millis(900)), "...");
        assert_eq!(ellipsis(Duration::from_millis(1300)), ".");
    }

    #[test]
    fn the_countdown_pads_its_seconds() {
        // "9:7" would read as nine minutes and seven-something.
        assert_eq!(countdown(Duration::from_secs(587)), "9:47");
        assert_eq!(countdown(Duration::from_secs(67)), "1:07");
        assert_eq!(countdown(Duration::ZERO), "0:00");
    }

    #[test]
    fn wrapping_breaks_at_spaces_but_never_inside_a_word() {
        assert_eq!(wrap("one two three", 8), ["one two", "three"]);
        // A word over the width overhangs rather than splitting: the long words
        // in these messages are URLs, and half a URL is useless.
        assert_eq!(
            wrap("see https://myaccount.google.com/permissions", 10),
            ["see", "https://myaccount.google.com/permissions"]
        );
        assert_eq!(wrap("", 10), [""]);
    }

    #[test]
    fn every_hint_line_fits_the_column_reserved_for_it() {
        // The hints column truncates rather than wrapping, so a hint that grew
        // past it would be silently cut mid-word.
        for hint in [
            HINTS_EDITING,
            HINTS_PLAYLISTS,
            HINTS_TRACKS,
            HINTS_SIGNED_OUT,
            HINTS_HOME,
            HINTS_PLAYING,
            HINTS_PLAYLISTS_PLAYING,
            HINTS_TRACKS_PLAYING,
            HINTS_HOME_PLAYING,
            HINTS_SIGNED_OUT_PLAYING,
        ] {
            assert!(
                display_width(hint) <= HINTS_WIDTH as usize,
                "{hint:?} is {} columns, over the {HINTS_WIDTH} reserved",
                display_width(hint)
            );
        }
    }

    #[test]
    fn every_view_names_the_way_back_to_a_playing_track() {
        // The player page is entered by playing something rather than by asking
        // for it, so nothing about leaving it teaches the user it can be
        // reopened. Whichever list they leave it for has to say so.
        for hint in [
            HINTS_PLAYLISTS_PLAYING,
            HINTS_TRACKS_PLAYING,
            HINTS_HOME_PLAYING,
            HINTS_SIGNED_OUT_PLAYING,
        ] {
            assert!(hint.contains("P player"), "{hint:?} does not name P");
            assert!(hint.contains("B tray"), "{hint:?} does not name B");
        }
        // `B` refuses with nothing playing, so the lines shown then must not
        // offer it -- a key that answers "nothing is playing" is worse than no
        // key at all.
        for hint in [HINTS_PLAYLISTS, HINTS_TRACKS, HINTS_HOME, HINTS_SIGNED_OUT] {
            assert!(!hint.contains("B tray"), "{hint:?} offers B with nothing playing");
        }
    }

    #[test]
    fn the_editing_hint_offers_no_key_the_search_box_would_swallow() {
        // Every printable key is text while typing, so a bare letter here would
        // name a key that inserts that letter -- the exact trap that made the
        // library unreachable from a fresh launch. Only Enter, Esc and a
        // Ctrl-chord survive the search box.
        assert!(!HINTS_EDITING.contains(" L "));
        assert!(HINTS_EDITING.contains("^L"));
        assert!(HINTS_EDITING.contains("Esc"));
    }

    #[test]
    fn an_overlay_is_centred_and_never_escapes_the_window() {
        let window = Rect::new(0, 0, 80, 24);
        let area = centred(window, 40, 10);
        assert_eq!((area.x, area.y, area.width, area.height), (20, 7, 40, 10));

        // A modal larger than the window is clamped, not drawn off-screen.
        let huge = centred(Rect::new(0, 0, 30, 6), 56, 12);
        assert_eq!((huge.x, huge.y, huge.width, huge.height), (0, 0, 30, 6));
    }

    #[test]
    fn the_header_lines_up_with_the_rows() {
        // Swept, because the header and the rows are built by separate code
        // paths off the same widths -- exactly the arrangement that drifts.
        for width in (DURATION_WIDTH + 2)..200 {
            let (title, artist, album) = columns(width);
            assert_eq!(
                header_line(title, artist, album).width(),
                width,
                "header does not fill a {width}-wide row"
            );
        }
    }

    #[test]
    fn the_header_drops_the_same_columns_the_rows_do() {
        // Narrow enough that only the title survives: the labels for the
        // dropped columns must not be drawn anyway.
        let (title, artist, album) = columns(40);
        let header = header_line(title, artist, album);
        let text = header
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect::<String>();
        assert!(text.contains("TITLE"));
        assert!(text.contains("DURATION"));
        assert!(!text.contains("ARTIST"), "artist label survived its column");
        assert!(!text.contains("ALBUM"), "album label survived its column");
    }

    #[test]
    fn columns_fill_every_row_they_can() {
        // Swept rather than sampled: the widths where a column drops out are
        // exactly where an off-by-one would leave the highlight short.
        for width in (DURATION_WIDTH + 2)..200 {
            assert_fills(width);
        }
    }

    #[test]
    fn a_cell_is_exactly_its_width_and_keeps_a_gap() {
        assert_eq!(super::display_width(&cell("Daft Punk", 12)), 12);
        assert_eq!(super::display_width(&cell("", 12)), 12);
        // Too long: truncated with room left for the separating space.
        let packed = cell("Random Access Memories", 12);
        assert_eq!(super::display_width(&packed), 12);
        assert!(packed.ends_with(' '), "columns must not run together");
    }

    /// Wide glyphs are the case that silently broke every column: one
    /// character, two terminal cells.
    #[test]
    fn a_cell_measures_wide_glyphs_in_columns_not_characters() {
        for width in 1..24 {
            for text in [
                "日本語のタイトル",
                "아티스트",
                "ASCII title",
                "mixed 日本語 text",
                "",
            ] {
                assert_eq!(
                    super::display_width(&cell(text, width)),
                    width,
                    "{text:?} at width {width}"
                );
            }
        }
    }

    #[test]
    fn truncate_measures_in_columns() {
        // Four CJK glyphs are eight columns, so a limit of eight fits exactly
        // and must not be cut.
        assert_eq!(truncate("日本語だ", 8), "日本語だ");
        // Seven columns cannot hold it: three glyphs plus an ellipsis is
        // seven, which is the most that fits.
        assert_eq!(super::display_width(&truncate("日本語だ", 7)), 7);
    }

    #[test]
    fn truncate_leaves_short_strings_alone() {
        assert_eq!(truncate("abc", 10), "abc");
        assert_eq!(truncate("abc", 3), "abc");
    }

    #[test]
    fn truncate_adds_ellipsis_when_cutting() {
        assert_eq!(truncate("abcdef", 4), "abc…");
        assert_eq!(truncate("abc", 0), "");
    }

    #[test]
    fn truncate_respects_char_boundaries() {
        // Multi-byte characters must not be sliced mid-codepoint. Each of
        // these glyphs is two terminal columns, so a four-column budget holds
        // one of them plus the ellipsis -- not three, which is what counting
        // characters would have given.
        let s = "日本語のタイトル";
        assert_eq!(truncate(s, 4), "日…");
        assert_eq!(truncate("café", 10), "café");
    }

    #[test]
    fn truncate_never_exceeds_its_budget() {
        // The invariant the columns depend on, swept over mixed-width text.
        for text in ["日本語のタイトル", "mixed 日本語 text", "ASCII", "아티스트"] {
            for max in 0..20 {
                assert!(
                    super::display_width(&truncate(text, max)) <= max,
                    "{text:?} overflowed a {max}-column budget"
                );
            }
        }
    }
}
