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
    App, CardShape, CoverSize, ImagePlan, Mode, NowPlaying, Overlay, Panel, RelatedRow, SignIn,
    Tab, View,
};
use crate::art::ArtCache;
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

/// Columns the lyrics panel keeps to the left of every line, for the mark
/// against the one being sung. Held even when nothing is marked, so that a line
/// becoming the current one does not shift the whole panel sideways.
const LYRIC_GUTTER: usize = 2;

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
///
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

/// Card geometry on the landing page, borders included, per shape.
///
/// The heights are where the arithmetic of a terminal shows through. A cell is
/// about twice as tall as it is wide, so a square sleeve `n` columns across is
/// `n / 2` rows tall -- and the poster card, which is the one that looks like a
/// music app, spends ten rows on its picture before a word is written. That is
/// why there are three of these rather than one: see [`CardShape`].
///
/// The widths are nominal. Cards are stretched to fill the row they are on --
/// leftover columns are worth more inside the titles than as a ragged margin --
/// so these decide how many fit across, not how wide they end up.
///
/// Width is the free dimension. Only the height reaches [`shelf_height`] and
/// so only the height decides which shape a window can hold, which means these
/// can be widened to thin out a crowded row without pushing any window down to
/// a smaller card than it draws today. The heights are the opposite: every row
/// added here is a row taken from the shelf below.
///
/// The sleeve is square and sized from the rows left under the text, so a card
/// wider than `2 * (height - 2 - CARD_TEXT_ROWS)` spends the difference on
/// margins either side of its picture rather than on a bigger picture. That is
/// the intended look up to a point and the reason these widths are generous
/// rather than extravagant -- see the centring in [`render_card`].
const TEXT_CARD: (u16, u16) = (32, 4);
/// Also left alone. A tile's sleeve is sized from its four rows, so widening it
/// stretches the text column into whitespace rather than thinning the row --
/// measured at 120 columns, where 32 and 36 both fit three across.
const TILE_CARD: (u16, u16) = (32, 6);
/// Left at the width its own sleeve wants. A poster is flush only while its
/// width is about `2 * rows + 2`, and widening it past that buys margin either
/// side of an unchanged picture rather than a bigger one -- so thinning out a
/// crowded row of posters is [`GALLERY_CARD`]'s job, not a wider poster's.
const POSTER_CARD: (u16, u16) = (22, 15);

/// The roomy shape, sized so its sleeve fills the card edge to edge rather than
/// sitting in margins: fourteen rows of picture make a twenty-eight-pixel
/// sleeve across twenty-eight columns, against the poster's twenty.
const GALLERY_CARD: (u16, u16) = (30, 19);

/// Rows of text under a card's picture: the title, what YouTube wrote under it,
/// and the badge line. The tile shape puts these beside the picture instead, but
/// spends the same three rows on them.
const CARD_TEXT_ROWS: u16 = 3;

/// The now-playing strip above the shelves, borders included: four rows inside
/// for the label, the title, the artist and the progress bar.
const HERO_HEIGHT: u16 = 6;

/// Below this a card holds nothing but borders and an ellipsis, so the page
/// stands down and says so rather than drawing a column of empty boxes.
const MIN_HOME_WIDTH: u16 = 24;

/// Shelves kept together on the landing page before later sections are allowed
/// to spend the remaining height on larger cards.
const MIN_VISIBLE_HOME_SECTIONS: usize = 3;

/// Colour of a card with no artwork -- either because none arrived, or because
/// it has not arrived yet. Grey rather than a guessed hue: a made-up colour on
/// a card is a lie about a record nobody has seen.
const NO_ART: (u8, u8, u8) = (110, 110, 110);

/// Luma below which a fill is too dark to write black on. Chosen against the
/// terminal palette rather than derived: at 140 a mid green takes black ink and
/// a deep red takes white, which is where the eye puts the boundary too.
const INK_FLIP: u32 = 140;

/// The colour the interface takes from whatever is playing.
///
/// One colour across the whole of the chrome -- the progress bar, the open tab,
/// the row under the cursor -- pulled from the sleeve of the track that is
/// playing, so the program is the colour of the song rather than the colour it
/// was compiled in. The cards are deliberately not included: each of those
/// wears its *own* sleeve's colour, and a page of records is the one place
/// where many colours at once is the point.
///
/// Cyan before the first cover lands and whenever nothing is playing, which is
/// exactly what the interface was before any of this.
fn ambient(app: &App) -> Color {
    app.cover.as_ref().map_or(Color::Cyan, |cover| {
        Color::Rgb(cover.accent.0, cover.accent.1, cover.accent.2)
    })
}

/// A row under the cursor, filled in the ambient colour.
///
/// The ink is chosen against the fill rather than fixed at black. The accent is
/// lifted until its brightest *channel* clears a threshold, which is not the
/// same as its luminance clearing one: a deep red sleeve yields (170, 0, 0),
/// bright by that measure and far too dark to write black on. Fixing the ink
/// would make the selected row the one row on the page nobody can read, at the
/// moment they are looking straight at it.
fn highlight(fill: Color) -> Style {
    let ink = match fill {
        Color::Rgb(r, g, b) if luma(r, g, b) < INK_FLIP => Color::White,
        _ => Color::Black,
    };
    Style::default()
        .fg(ink)
        .bg(fill)
        .add_modifier(Modifier::BOLD)
}

/// Rec. 601 luma, in thousandths to stay in integers. The cheap standard
/// answer, and the one the terminal palettes were themselves eyeballed against.
fn luma(r: u8, g: u8, b: u8) -> u32 {
    (299 * u32::from(r) + 587 * u32::from(g) + 114 * u32::from(b)) / 1000
}

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
        Line::from(Span::styled(
            HINT_HIDE,
            Style::default().fg(Color::DarkGray),
        )),
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
    let width = (display_width(url) as u16)
        .saturating_add(10)
        .max(SIGN_IN_MIN_WIDTH);
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

    frame.render_widget(
        Paragraph::new(step("1", "open this page in a browser")),
        step_1,
    );
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
            format!(
                " waiting for approval{} ({} left)",
                ellipsis(left),
                countdown(left)
            ),
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
            format!(
                " {}",
                truncate(title, inner.width.saturating_sub(2) as usize)
            ),
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
                    ambient(app),
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

/// The landing page: what is playing, and then YouTube Music's shelves drawn as
/// rows of cards with their sleeves on them.
///
/// Virtualized in both directions, on the same principle as the results list --
/// only the shelves on screen are laid out, and only the cards visible within
/// each of them are built. A feed of twelve shelves costs what one screenful
/// costs. The artwork obeys the same rule and is what makes it matter: the
/// cards drawn here are the only ones whose pictures are ever fetched, which is
/// a dozen requests for a feed of three hundred cards.
fn render_home(frame: &mut Frame, app: &mut App, area: Rect) {
    let block = Block::bordered().title(app.list_title());
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let smallest = shelf_height(CardShape::Text);
    if app.home.is_empty() || inner.width < MIN_HOME_WIDTH || inner.height + 1 < smallest {
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

    let plan = plan_home(inner, app.now.is_some());

    let shelves = match (plan.hero, app.now.as_ref()) {
        (true, Some(now)) => {
            let [strip, shelves] =
                Layout::vertical([Constraint::Length(HERO_HEIGHT), Constraint::Min(0)])
                    .areas(inner);
            render_hero(
                frame,
                &Hero {
                    title: &now.title,
                    artist: &now.artist,
                    art: app.cover.as_ref(),
                    snap: app.snapshot(),
                    duration: now.duration,
                },
                strip,
            );
            shelves
        }
        _ => inner,
    };

    app.clamp_home_selection();
    app.home_top = app.home_top.min(app.home_shelf);
    let mut layouts = shelf_layouts(&app.home, shelves, app.home_top, plan.max_shape);
    while !layouts.iter().any(|layout| layout.index == app.home_shelf)
        && app.home_top < app.home_shelf
    {
        app.home_top += 1;
        layouts = shelf_layouts(&app.home, shelves, app.home_top, plan.max_shape);
    }
    if let Some(focused) = layouts.iter().find(|layout| layout.index == app.home_shelf) {
        app.clamp_home_cards(focused.across as usize);
    }

    let cursor = HomeCursor {
        shelf: app.home_shelf,
        card: app.home_card,
    };
    // Collected during the draw rather than worked out beside it: which cards
    // are on screen falls out of laying them out, and deriving it twice is two
    // chances for the pictures and the cards to disagree about what is visible.
    let mut wanted = Vec::new();
    render_feed(
        frame,
        &app.home,
        &layouts,
        cursor,
        &app.home_scroll,
        Tiles {
            shape: CardShape::Text,
            art: &app.art,
            wanted: &mut wanted,
        },
    );
    app.want_art(wanted);
}

/// How the landing page is laid out for the window it has: which card shape,
/// and whether there is room left over for the now-playing strip.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct HomePlan {
    max_shape: CardShape,
    hero: bool,
}

/// Chooses the biggest cards the window can hold, and adds the hero strip when
/// they still leave room for a page of shelves under it.
///
/// The order matters and is a judgement rather than a fallout: the picture on
/// the cards outranks the picture of the track already playing. A user looking
/// at the landing page is looking for something to play next, and a strip that
/// pushed every sleeve off the page to show them what they can already hear
/// would be a worse page than one without it.
///
fn plan_home(area: Rect, playing: bool) -> HomePlan {
    let max_shape = CardShape::ALL
        .into_iter()
        .find(|shape| fits(area.height, *shape, 1))
        .unwrap_or(CardShape::Text);
    let hero = playing && fits(area.height.saturating_sub(HERO_HEIGHT), max_shape, 1);
    HomePlan { max_shape, hero }
}

/// Whether `count` shelves of `shape` fit in `height` rows.
///
/// The last shelf keeps its heading and its cards but not the blank row under
/// it, because there is nothing below for that row to separate it from -- which
/// is worth the arithmetic: it is often the difference between two shelves and
/// one.
fn fits(height: u16, shape: CardShape, count: u16) -> bool {
    height >= shelf_height(shape) * count - 1
}

/// One card's nominal size, borders included.
fn card_size(shape: CardShape) -> (u16, u16) {
    match shape {
        CardShape::Text => TEXT_CARD,
        CardShape::Tile => TILE_CARD,
        CardShape::Poster => POSTER_CARD,
        CardShape::Gallery => GALLERY_CARD,
    }
}

/// One shelf: its heading, its cards, and a blank row under them so two shelves
/// do not read as one block of boxes.
fn shelf_height(shape: CardShape) -> u16 {
    card_size(shape).1 + 2
}

/// Where the cursor stands on the landing page as a whole.
#[derive(Debug, Clone, Copy)]
struct HomeCursor {
    shelf: usize,
    card: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ShelfLayout {
    index: usize,
    shape: CardShape,
    area: Rect,
    across: u16,
    slot: u16,
}

fn section_shape(shelf: &Shelf, index: usize, max_shape: CardShape) -> CardShape {
    let playable = shelf.cards.iter().filter(|card| card.is_playable()).count();
    let browsable = shelf.cards.len().saturating_sub(playable);
    let desired = if index == 0 {
        CardShape::Gallery
    } else if matches!(shelf.title.as_str(), "From your listening" | "Quick picks") {
        CardShape::Tile
    } else if browsable > playable {
        CardShape::Poster
    } else {
        match index % 3 {
            0 => CardShape::Gallery,
            1 => CardShape::Tile,
            _ => CardShape::Poster,
        }
    };
    desired.min(max_shape)
}

fn shelf_layouts(
    shelves: &[Shelf],
    area: Rect,
    top: usize,
    max_shape: CardShape,
) -> Vec<ShelfLayout> {
    let mut layouts = Vec::new();
    let mut y = area.y;
    let bottom = area.y + area.height;
    let visible_goal = shelves
        .len()
        .saturating_sub(top)
        .min(MIN_VISIBLE_HOME_SECTIONS);
    for (index, shelf) in shelves.iter().enumerate().skip(top) {
        let desired = section_shape(shelf, index, max_shape);
        let position = index - top;
        let remaining = visible_goal.saturating_sub(position + 1) as u16;
        let available = bottom.saturating_sub(y);
        // Keep the first three section headings and card rows on screen
        // together. Earlier shelves retain their preferred shape; a later one
        // steps down only when doing so is what makes room for the sections
        // still owed below it.
        let shape = if position < visible_goal {
            CardShape::ALL
                .into_iter()
                .find(|shape| {
                    *shape <= desired
                        && available
                            >= shelf_height(*shape)
                                .saturating_add(shelf_height(CardShape::Text) * remaining)
                                .saturating_sub(1)
                })
                .unwrap_or(CardShape::Text)
        } else {
            desired
        };
        let (width, height) = card_size(shape);
        let row_height = height + 1;
        if y + row_height > bottom {
            break;
        }
        let across = (area.width / width).max(1);
        layouts.push(ShelfLayout {
            index,
            shape,
            area: Rect::new(area.x, y, area.width, row_height),
            across,
            slot: area.width / across,
        });
        y += shelf_height(shape);
    }
    layouts
}

/// The artwork side of drawing a shelf: what shape to draw, what pictures are
/// already in hand, and where to write down the ones that are not.
///
/// Bundled into one struct because all three travel together down four calls of
/// nesting, and because the third is the only mutable thing in a render pass
/// that is otherwise read-only -- which is worth being able to see at a glance.
struct Tiles<'a> {
    shape: CardShape,
    art: &'a ArtCache,
    /// Key and URL of every card drawn this frame whose picture is not in
    /// `art`. Handed to [`App::want_art`] once the borrow of the feed ends.
    wanted: &'a mut Vec<(String, String)>,
}

impl Tiles<'_> {
    /// The picture for a card, noting the miss when there is not one.
    fn art(&mut self, card: &Card) -> Option<&Cover> {
        if let Some(url) = &card.art
            && self.art.get(card.art_key()).is_none()
        {
            self.wanted.push((card.art_key().to_string(), url.clone()));
        }
        self.art.get(card.art_key())
    }
}

/// Stacks the visible shelves down the pane.
///
/// Split from [`render_home`] so the page can be drawn from shelves alone --
/// which is what lets it be previewed and tested without standing up a player
/// and a worker thread behind an `App`.
fn render_feed(
    frame: &mut Frame,
    shelves: &[Shelf],
    layouts: &[ShelfLayout],
    cursor: HomeCursor,
    scroll: &[usize],
    mut tiles: Tiles,
) {
    for layout in layouts {
        let index = layout.index;
        let Some(shelf) = shelves.get(index) else {
            continue;
        };
        tiles.shape = layout.shape;
        render_shelf(
            frame,
            shelf,
            layout.area,
            ShelfCursor {
                focused: index == cursor.shelf,
                selected: cursor.card,
                offset: scroll.get(index).copied().unwrap_or(0),
            },
            (layout.across, layout.slot),
            &mut tiles,
        );
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
    tiles: &mut Tiles,
) {
    let cards_height = area.height.saturating_sub(1);
    let [heading, cards] =
        Layout::vertical([Constraint::Length(1), Constraint::Length(cards_height)]).areas(area);
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
        render_card(frame, card, rect, selected, tiles);
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

/// One card: its sleeve, its title, and whatever YouTube wrote under it.
///
/// The three shapes differ only in where the picture goes, so they share
/// everything after the split: the same border, the same accent taken from the
/// same sleeve, the same lines of text. [`CardShape::Text`] is the shape with
/// nowhere to put a picture, which is also what a card falls back to when the
/// window is too small to give it one.
fn render_card(frame: &mut Frame, card: &Card, area: Rect, selected: bool, tiles: &mut Tiles) {
    let shape = tiles.shape;
    let art = tiles.art(card);
    // The card takes its colour from its own sleeve. This is the whole reason
    // the accent is computed at all: a page bordered in one colour says nothing
    // about what is on it, and a page that borrows each record's own is a page
    // of records. A card still waiting on its picture gets the neutral grey,
    // and changes colour under the user when it lands -- which is a page
    // filling in, and reads as one.
    let accent = art.map_or(NO_ART, |cover| cover.accent);
    let accent = Color::Rgb(accent.0, accent.1, accent.2);

    // Selection is drawn in the accent too, doubled in weight rather than
    // switched to a fixed cyan: the cursor has to be unmistakable, and the way
    // to do that without throwing away the card's colour is to make its own
    // colour louder than its neighbours'.
    let border = if selected {
        Style::default().fg(accent).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    let block = Block::bordered()
        .border_type(if selected {
            BorderType::Thick
        } else {
            BorderType::Rounded
        })
        .border_style(border);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    // Where the picture goes and what is left for the words. Both shapes with a
    // picture keep it square by deriving its width from its height: two pixels
    // to a row and one to a column, so a tile `n` rows tall is `2n` wide.
    let text = match shape {
        CardShape::Text => inner,
        CardShape::Tile => {
            let rows = inner.height;
            let cols = (rows * 2).min(inner.width);
            render_tile(
                frame,
                art,
                Rect {
                    width: cols,
                    ..inner
                },
            );
            Rect {
                x: inner.x + cols + 1,
                width: inner.width.saturating_sub(cols + 1),
                ..inner
            }
        }
        // One arm for both: a gallery card is a poster with more of everything,
        // and the arithmetic that centres a square sleeve over three rows of
        // text is the same either way.
        CardShape::Poster | CardShape::Gallery => {
            let rows = inner.height.saturating_sub(CARD_TEXT_ROWS);
            let cols = (rows * 2).min(inner.width);
            render_tile(
                frame,
                art,
                Rect {
                    // Centred: a card stretched wider than its nominal width
                    // must not leave its sleeve pinned to the left edge with a
                    // hole beside it.
                    x: inner.x + (inner.width - cols) / 2,
                    width: cols,
                    height: rows,
                    ..inner
                },
            );
            Rect {
                y: inner.y + rows,
                height: CARD_TEXT_ROWS,
                ..inner
            }
        }
    };

    if text.width == 0 || text.height == 0 {
        return;
    }
    frame.render_widget(
        Paragraph::new(card_lines(
            card,
            text.width as usize,
            selected,
            accent,
            shape,
        )),
        text,
    );
}

/// The words on a card: its title, then what it is, then who made it.
///
/// A text card gets two of the three lines and has to fold the badge into the
/// second, which is the cost of being the shape that fits anywhere.
fn card_lines(
    card: &Card,
    width: usize,
    selected: bool,
    accent: Color,
    shape: CardShape,
) -> Vec<Line<'static>> {
    let title = if selected {
        Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::Gray)
    };
    // One column of padding on the left so text never touches the border.
    let room = width.saturating_sub(1);

    let mut lines = vec![Line::from(vec![
        Span::raw(" "),
        Span::styled(truncate(&card.title, room), title),
    ])];

    let detail = card.detail();
    let badge = badge_spans(card, accent);
    let badge_width: usize = badge.iter().map(|span| display_width(&span.content)).sum();

    if shape == CardShape::Text {
        // Two rows and three things to say. The badge leads, because what a
        // card *is* -- a song to play or an album to open -- is the thing a
        // grid of near-identical boxes hides, and the artist is already the
        // start of the line beside it.
        let mut spans = vec![Span::raw(" ")];
        spans.extend(badge);
        spans.extend(detail_spans(&detail, room.saturating_sub(badge_width)));
        lines.push(Line::from(spans));
        return lines;
    }

    let mut spans = vec![Span::raw(" ")];
    spans.extend(detail_spans(&detail, room));
    lines.push(Line::from(spans));

    let mut spans = vec![Span::raw(" ")];
    spans.extend(badge);
    lines.push(Line::from(spans));
    lines
}

/// The line under a title, styled by what each part of it is.
///
/// YouTube writes this as one string with bullets in it -- "Tame Impala • 2015"
/// -- and drawing the whole thing in one grey is what makes a shelf of cards
/// read as a paragraph. The parts are not equal: the first field is who made
/// the record, which is the one thing on this line anybody scans for, and what
/// follows is a year or a play count that only matters once they have found it.
///
/// So the artist is drawn a step brighter than the rest and the bullets a step
/// dimmer than either, which turns one grey line into a line with a subject.
/// The fields are not re-ordered or dropped: this styles what YouTube wrote
/// rather than second-guessing it, for the reason [`Card::subtitle`] gives.
fn detail_spans(detail: &str, room: usize) -> Vec<Span<'static>> {
    let lead = Style::default().fg(Color::Gray);
    let rest = Style::default().fg(Color::DarkGray);
    // Dimmer than the words either side, so the bullets separate the fields
    // without competing with them.
    let bullet = Style::default().fg(Color::from_u32(0x0044_4444));

    let mut spans = Vec::new();
    let mut left = room;
    for (at, field) in detail.split('•').map(str::trim).enumerate() {
        if left == 0 {
            break;
        }
        if at > 0 {
            // Spent from the same budget as the words, so a line that has to be
            // cut is cut at a field rather than mid-separator.
            let sep = " • ";
            if left <= display_width(sep) {
                break;
            }
            spans.push(Span::styled(sep, bullet));
            left -= display_width(sep);
        }
        let text = truncate(field, left);
        left -= display_width(&text);
        spans.push(Span::styled(text, if at == 0 { lead } else { rest }));
    }
    spans
}

/// The badge under a card: what it is, and how long it runs when that is known.
///
/// Drawn as a filled chip in the card's own accent rather than as more grey
/// text, because it is the one thing on the card that is a fact rather than a
/// name -- and because a page of chips in a dozen colours is the page reading
/// as music rather than as a list.
fn badge_spans(card: &Card, accent: Color) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    // The clock is the actionable metadata and is never sacrificed to the tag
    // on a narrow tile.
    if let Some(duration) = card.duration {
        spans.push(Span::styled(
            format!("{} ", clock(duration)),
            Style::default().fg(Color::Gray),
        ));
    }
    if let Some(kind) = card.kind() {
        spans.push(Span::styled(
            kind.to_lowercase(),
            Style::default().fg(Color::Black).bg(accent),
        ));
    }
    // Nothing known about the card beyond its name, which is most of a
    // "Trending" shelf. A `▶` at least says what Enter will do -- the one thing
    // the two kinds of card do not otherwise distinguish, and without it the
    // difference is only discoverable by pressing Enter and seeing.
    if spans.is_empty() {
        spans.push(Span::styled(
            if card.is_playable() {
                "▶ play "
            } else {
                "≡ open "
            },
            Style::default().fg(Color::DarkGray),
        ));
    }
    spans
}

/// Draws a sleeve into exactly `area`, filling it corner to corner.
///
/// Cropped to fill rather than fitted inside, which is the opposite of what the
/// cover pane does and is right for the opposite reason. A pane is showing one
/// picture and must not distort or crop it; a grid is showing twelve, and
/// twelve tiles of different shapes with different amounts of dead space around
/// them is not a grid. So a 16:9 video thumbnail loses its sides here and every
/// tile is the same square.
///
/// Half-blocks, on every terminal, including the ones that can do better. The
/// sixel path costs tens of milliseconds per image to encode -- fine once per
/// track, and a third of a second per keypress for a screenful of cards, which
/// would make the grid unusable to make it prettier.
fn render_tile(frame: &mut Frame, art: Option<&Cover>, area: Rect) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let Some(cover) = art else {
        // A picture on its way, or one that never arrived. Drawn as a muted
        // hatch rather than left blank so the card keeps its shape either way
        // and nothing on the page moves when the sleeve lands.
        let rows: Vec<Line> = (0..area.height)
            .map(|_| {
                Line::from(Span::styled(
                    "░".repeat(area.width as usize),
                    Style::default().fg(Color::from_u32(0x0020_2020)),
                ))
            })
            .collect();
        frame.render_widget(Paragraph::new(rows), area);
        return;
    };

    let (px_w, px_h) = (u32::from(area.width), u32::from(area.height) * 2);
    let crop = Crop::fill(cover, px_w, px_h);

    let lines: Vec<Line> = (0..area.height)
        .map(|row| {
            let y = u32::from(row) * 2;
            let spans = (0..px_w)
                .map(|x| {
                    let mut style = Style::default().fg(crop.at(cover, x, y, px_w, px_h));
                    if y + 1 < px_h {
                        style = style.bg(crop.at(cover, x, y + 1, px_w, px_h));
                    }
                    Span::styled("▀", style)
                })
                .collect::<Vec<_>>();
            Line::from(spans)
        })
        .collect();

    frame.render_widget(Paragraph::new(lines), area);
}

/// The window of a cover that fills a tile without distorting it: the largest
/// centred rect of the source with the tile's aspect ratio.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Crop {
    x: u32,
    y: u32,
    width: u32,
    height: u32,
}

impl Crop {
    fn fill(cover: &Cover, px_w: u32, px_h: u32) -> Self {
        if cover.width == 0 || cover.height == 0 || px_w == 0 || px_h == 0 {
            return Self {
                x: 0,
                y: 0,
                width: cover.width.max(1),
                height: cover.height.max(1),
            };
        }
        // Compared as a single product rather than as two ratios, so the
        // integer division that would decide a near-square case by rounding
        // never happens.
        if cover.width * px_h > cover.height * px_w {
            // Wider than the tile: keep every row, take a centred band of
            // columns. This is the 16:9 thumbnail losing its sides.
            let width = (cover.height * px_w / px_h).clamp(1, cover.width);
            Self {
                x: (cover.width - width) / 2,
                y: 0,
                width,
                height: cover.height,
            }
        } else {
            let height = (cover.width * px_h / px_w).clamp(1, cover.height);
            Self {
                x: 0,
                y: (cover.height - height) / 2,
                width: cover.width,
                height,
            }
        }
    }

    /// Nearest-neighbour sample of the cropped window at a point on the
    /// `px_w` x `px_h` grid being drawn.
    fn at(self, cover: &Cover, x: u32, y: u32, px_w: u32, px_h: u32) -> Color {
        let (r, g, b) = cover.pixel(
            self.x + x * self.width / px_w,
            self.y + y * self.height / px_h,
        );
        Color::Rgb(r, g, b)
    }
}

/// What the now-playing strip draws, gathered from the `App` before it is
/// borrowed for the frame.
///
/// A struct of plain fields rather than the `App` itself, for the reason
/// [`render_feed`] takes shelves rather than one: the strip can then be drawn
/// -- and previewed, and tested -- without a player thread and a worker behind
/// it.
struct Hero<'a> {
    title: &'a str,
    artist: &'a str,
    /// The sleeve of what is playing. `None` before it arrives, which is a
    /// second or two at the start of every track.
    art: Option<&'a Cover>,
    snap: Snapshot,
    duration: Option<Duration>,
}

/// The now-playing strip across the top of the landing page: the sleeve of what
/// is playing, its name, and how far through it we are.
///
/// A landing page that never mentions the music already playing is a page that
/// makes the user go and look somewhere else for it. This is the same
/// information the status bar carries in one line, given the room to be read at
/// a glance instead -- and the picture, which the status bar has nowhere to put.
fn render_hero(frame: &mut Frame, hero: &Hero, area: Rect) {
    let accent = hero.art.map_or(NO_ART, |cover| cover.accent);
    let accent = Color::Rgb(accent.0, accent.1, accent.2);

    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(accent));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    // The sleeve, square, as tall as the strip is inside.
    let cols = (inner.height * 2).min(inner.width);
    render_tile(
        frame,
        hero.art,
        Rect {
            width: cols,
            ..inner
        },
    );

    let text = Rect {
        x: inner.x + cols + 1,
        width: inner.width.saturating_sub(cols + 2),
        ..inner
    };
    if text.width == 0 {
        return;
    }

    let label = if hero.snap.state == PlayState::Paused {
        "PAUSED"
    } else {
        "NOW PLAYING"
    };
    let width = text.width as usize;

    let mut lines = vec![
        Line::from(Span::styled(
            label,
            Style::default().fg(accent).add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            truncate(hero.title, width),
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            truncate(hero.artist, width),
            Style::default().fg(Color::Gray),
        )),
    ];
    // The bar is the first thing dropped on a short strip: the two names are
    // what the row is for, and the status bar is still carrying the clock.
    if inner.height >= 4 {
        lines.push(progress_line(&hero.snap, hero.duration, width, accent));
    }
    frame.render_widget(Paragraph::new(lines), text);
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
    let accent = ambient(app);
    app.clamp_playlist_scroll(height);

    // Virtualized on the same principle as the results list.
    let end = (app.playlist_offset + height).min(app.playlists.len());
    let width = inner.width as usize;
    let items: Vec<ListItem> = app.playlists[app.playlist_offset..end]
        .iter()
        .enumerate()
        .map(|(i, playlist)| {
            let selected = app.playlist_offset + i == app.playlist_selected;
            ListItem::new(playlist_line(playlist, selected, width, accent))
        })
        .collect();

    frame.render_widget(List::new(items), inner);
}

/// One playlist row: name on the left, track count right-aligned.
///
/// Shared by the library pane and the add-to picker so the two cannot drift
/// into showing the same list two different ways.
fn playlist_line(
    playlist: &Playlist,
    selected: bool,
    width: usize,
    ambient: Color,
) -> Line<'static> {
    let style = if selected {
        highlight(ambient)
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
    render_track_info(frame, now, app.snapshot(), info, ambient(app));

    render_panel(frame, app, panel_area);
    art
}

/// The title, artist and progress of what is playing.
fn render_track_info(
    frame: &mut Frame,
    now: &NowPlaying,
    snap: Snapshot,
    area: Rect,
    ambient: Color,
) {
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
        lines.push(progress_line(&snap, now.duration, width, ambient));
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
fn progress_line<'a>(
    snap: &Snapshot,
    total: Option<Duration>,
    width: usize,
    ambient: Color,
) -> Line<'a> {
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
            Style::default().fg(ambient).add_modifier(Modifier::BOLD),
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
    // Both taken before the page is borrowed, since the panels need them and
    // the borrow below is what stops either being asked for down there.
    let snap = app.snapshot();
    let accent = ambient(app);
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
    render_tabs(frame, tab, tabs, accent);

    // Set only by the lyrics panel, and only while it is following the singer:
    // where it scrolled itself to, which becomes the cursor the keys move.
    let mut following = None;
    let total = match tab {
        Tab::UpNext => render_up_next(frame, now, content, accent),
        Tab::Lyrics => {
            let (total, offset) = render_lyrics(frame, now, &snap, content);
            following = offset;
            total
        }
        Tab::Comments => render_comments(frame, now, content),
        Tab::Related => render_related(frame, now, content, accent),
    };
    if let Some(offset) = following {
        app.follow_page(offset);
    }
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
fn render_tabs(frame: &mut Frame, open: Tab, area: Rect, ambient: Color) {
    let mut labels = Vec::new();
    let mut rule = Vec::new();

    for tab in Tab::ALL {
        let selected = tab == open;
        let style = if selected {
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        labels.push(Span::styled(format!(" {} ", tab.label()), style));

        // Under the letters rather than under the padding, so the rule reads as
        // belonging to the name above it rather than to the gap beside it.
        let width = tab.label().chars().count();
        rule.push(Span::styled(
            format!(" {} ", if selected { "─" } else { " " }.repeat(width)),
            Style::default().fg(ambient),
        ));
    }

    frame.render_widget(
        Paragraph::new(vec![Line::from(labels), Line::from(rule)]),
        area,
    );
}

/// The queue, headed by what it is a queue of. Returns its length in rows.
///
/// The heading does not scroll with the rows: what a queue is a queue of is the
/// one thing on this panel that stays true however far down it the user is.
fn render_up_next(frame: &mut Frame, now: &NowPlaying, area: Rect, ambient: Color) -> usize {
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
                queue_line(
                    track,
                    index == cursor,
                    Some(index) == now.playing,
                    width,
                    ambient,
                )
            }),
    );

    frame.render_widget(Paragraph::new(lines), area);
    now.queue.len()
}

/// One queue row: a marker for the track playing, the title, and its length.
fn queue_line<'a>(
    track: &Track,
    selected: bool,
    playing: bool,
    width: usize,
    ambient: Color,
) -> Line<'a> {
    let style = if selected {
        highlight(ambient)
    } else if playing {
        // The row this colour was taken from in the first place, so it wears it
        // as ink rather than as a fill -- marked without being the loudest
        // thing on a panel the cursor is also moving through.
        Style::default().fg(ambient).add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    };
    let dim = |colour: Color| {
        if selected {
            style
        } else {
            Style::default().fg(colour)
        }
    };

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
        spans.push(Span::styled(
            cell(&track.uploader, artist_width),
            dim(Color::Gray),
        ));
    }
    spans.push(Span::styled(format!("{duration} "), dim(Color::DarkGray)));

    Line::from(spans)
}

/// The lyrics, wrapped to the panel and scrolled by line.
///
/// Returns the number of lines the wrap produced, and -- while the panel is
/// following the singer -- the offset it put itself at, which the caller writes
/// back to the cursor the keys move. Only a wrap done here can be counted, and
/// only a count can be turned into a line to scroll to, which is why both
/// numbers come back from the drawing rather than being worked out before it.
fn render_lyrics(
    frame: &mut Frame,
    now: &NowPlaying,
    snap: &Snapshot,
    area: Rect,
) -> (usize, Option<usize>) {
    let width = (area.width as usize).saturating_sub(LYRIC_GUTTER + 1);
    let Panel::Ready(lyrics) = &now.lyrics else {
        message(frame, waiting_on(&now.lyrics), area);
        return (0, None);
    };

    // Which line the track is on, for a track whose lyrics came back timed.
    // `None` covers both "not timed" and "not there yet", and neither wants a
    // line marked.
    let singing = lyrics.singing(snap.position);

    // Each drawn row: its text, the timed line it was wrapped out of, and
    // whether it is that line's first row -- which is where the mark goes, and
    // which only the wrap done here can know. The credit and the blank line
    // under it belong to no lyric and stay `None`, which is also what every
    // line of an untimed track is.
    let mut lines: Vec<(String, Option<usize>, bool)> = Vec::new();
    if let Some(source) = &lyrics.source {
        lines.push((source.clone(), None, false));
        lines.push((String::new(), None, false));
    }
    let credit = lines.len();

    if lyrics.timed.is_empty() {
        for paragraph in lyrics.text.lines() {
            if paragraph.trim().is_empty() {
                lines.push((String::new(), None, false));
            } else {
                lines.extend(
                    wrap(paragraph, width)
                        .into_iter()
                        .map(|line| (line, None, false)),
                );
            }
        }
    } else {
        for (at, timed) in lyrics.timed.iter().enumerate() {
            if timed.text.trim().is_empty() {
                // A verse gap is one row, and is its own first.
                lines.push((String::new(), Some(at), true));
            } else {
                lines.extend(
                    wrap(&timed.text, width)
                        .into_iter()
                        .enumerate()
                        .map(|(row, line)| (line, Some(at), row == 0)),
                );
            }
        }
    }

    let viewport = area.height as usize;
    // Centred rather than scrolled to the top: what has just been sung is as
    // much of the reason to read along as what is coming, and a line pinned to
    // the top edge shows none of it.
    let following = singing.filter(|_| now.follow_lyrics).map(|at| {
        let first = lines
            .iter()
            .position(|(_, from, _)| *from == Some(at))
            .unwrap_or(0);
        centred_offset(first, viewport, lines.len())
    });
    let offset =
        following.unwrap_or_else(|| now.cursor().min(lines.len().saturating_sub(viewport)));

    let drawn: Vec<Line> = lines
        .iter()
        .enumerate()
        .skip(offset)
        .take(viewport)
        .map(|(index, (text, from, first))| {
            lyric_line(text, index < credit, *from, singing, *first)
        })
        .collect();

    frame.render_widget(Paragraph::new(drawn), area);
    (lines.len(), following)
}

/// One drawn row of the lyrics panel, styled for where the track has got to.
///
/// Two styles only: the lyric being sung, and everything else -- the rest of
/// the song, sung or not yet, and the source credit with it. One thing on the
/// panel is bright and nothing else is, so there is nothing to work out.
///
/// The dim end is [`Color::DarkGray`] rather than [`Color::Gray`] because grey
/// against white is not the contrast it looks like in source: `Gray` is ANSI 7
/// and `White` is ANSI 15, and on a good many terminal themes those are the
/// same colour, which leaves the sung line distinguished by its boldness alone.
/// `DarkGray` is ANSI 8, and 8-against-15 is the widest gap the sixteen-colour
/// palette has.
///
/// The dimming is conditional on there being something to contrast *with*. A
/// track with no timings never has a line being sung, and dimming all of it
/// would leave a panel that is only harder to read than before -- so with
/// nothing sung the words stay at ordinary body grey.
///
/// A marker as well as a colour, because on a terminal that renders bold as
/// bright and nothing else, colour alone is not a reliable way to say which
/// line of twenty is the one. The two are decided separately below -- the
/// colour belongs to every row of the sung lyric, the marker only to its first.
fn lyric_line<'a>(
    text: &str,
    credit: bool,
    from: Option<usize>,
    singing: Option<usize>,
    first: bool,
) -> Line<'a> {
    // The credit is about the panel rather than the song, and is never the line
    // being sung. `from.is_some()` is what stops a row of an *untimed* track
    // counting: there both it and `singing` are `None`, and `None == None`.
    let sung = !credit && from.is_some() && from == singing;

    let style = if sung {
        Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::BOLD)
    } else if credit || singing.is_some() {
        Style::default().fg(Color::DarkGray)
    } else {
        // Nothing is being sung: an untimed track, or a timed one still in its
        // intro. Dimming buys contrast against a bright line, and with no
        // bright line it buys nothing -- it would just leave the whole panel
        // harder to read than it was before any of this.
        Style::default().fg(Color::Gray)
    };

    // Once per lyric rather than once per row: a lyric that wraps is still one
    // line being sung, and a bar down each of its rows reads as several.
    let marker = if sung && first { "▌" } else { " " };

    Line::from(vec![
        Span::styled(format!("{marker} "), style),
        Span::styled(text.to_string(), style),
    ])
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
        Paragraph::new(
            lines
                .into_iter()
                .skip(offset)
                .take(viewport)
                .collect::<Vec<_>>(),
        ),
        area,
    );
    total
}

/// The Related tab: shelves of recommendations, flattened into one list.
/// Returns its length in rows.
fn render_related(frame: &mut Frame, now: &NowPlaying, area: Rect, ambient: Color) -> usize {
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
            RelatedRow::Card(card) => related_line(card, index == cursor, width, ambient),
        })
        .collect();

    frame.render_widget(Paragraph::new(lines), area);
    rows.len()
}

/// One recommendation: its title, and under it whatever YouTube wrote about it.
fn related_line<'a>(card: &Card, selected: bool, width: usize, ambient: Color) -> Line<'a> {
    let style = if selected {
        highlight(ambient)
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
        .map(|line| {
            Line::from(Span::styled(
                format!(" {line}"),
                Style::default().fg(Color::DarkGray),
            ))
        })
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
    cursor.saturating_sub(viewport / 2).min(total - viewport)
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
    let accent = ambient(app);
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
                highlight(accent)
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

    (width.saturating_sub(fixed + artist + album), artist, album)
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
    unicode_width::UnicodeWidthChar::width(c)
        .unwrap_or(1)
        .max(1)
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::*;
    use crate::source::watch::{Comment, Comments, Lyrics, TimedLine};

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
            (
                "Harder, Better, Faster, Stronger",
                "Daft Punk",
                "Discovery",
                "3:47",
            ),
            (
                "Get Lucky (feat. Pharrell Williams and Nile Rodgers)",
                "Daft Punk, Pharrell Williams & Nile Rodgers",
                "Random Access Memories",
                "6:10",
            ),
            ("Da Funk", "Daft Punk", "", "5:35"),
            ("日本語のタイトル", "アーティスト", "アルバム", "4:02"),
        ];
        for width in [96usize, 72, 44] {
            let (t, ar, al) = columns(width);
            println!("\n--- {width} cols (title {t}, artist {ar}, album {al}) ---");
            println!(
                "|{}|",
                header_line(t, ar, al)
                    .spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            );
            for (title, artist, album, duration) in rows {
                let mut line = format!(" {}", cell(title, t));
                if ar > 0 {
                    line += &cell(artist, ar);
                }
                if al > 0 {
                    line += &cell(album, al);
                }
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
                    playlist_line(&row, false, width, Color::Cyan).width(),
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
        let wide = text_of(&playlist_line(
            &playlist("Discovery", Some(14)),
            false,
            40,
            Color::Cyan,
        ));
        assert!(wide.contains("Discovery") && wide.contains("14"));

        // Not wide enough: the name is what identifies the row, so the count
        // is what goes.
        let narrow = text_of(&playlist_line(
            &playlist("Discovery", Some(14)),
            false,
            12,
            Color::Cyan,
        ));
        assert!(!narrow.contains("14"));
    }

    #[test]
    fn a_long_playlist_name_is_truncated_rather_than_overflowing() {
        let long = playlist("a playlist with a preposterously long name", Some(3));
        let line = playlist_line(&long, false, 24, Color::Cyan);
        assert_eq!(line.width(), 24);
        assert!(text_of(&line).contains('…'));
    }

    #[test]
    fn liked_songs_is_marked_and_carries_no_count() {
        // The API does not report how many liked videos there are, and a
        // made-up number would be worse than none. The marker keys off the
        // pseudo-id, not the title, so a real playlist called "Liked songs"
        // does not get it.
        let text = text_of(&playlist_line(&liked_songs(), false, 40, Color::Cyan));
        assert!(text.contains('♥'));
        assert!(text.contains("Liked songs"));

        let impostor = text_of(&playlist_line(
            &playlist("Liked songs", Some(7)),
            false,
            40,
            Color::Cyan,
        ));
        assert!(!impostor.contains('♥'));
        assert!(impostor.contains('7'));
    }

    use crate::source::home::Target;

    fn song(title: &str, subtitle: &str) -> Card {
        Card {
            title: title.to_string(),
            subtitle: subtitle.to_string(),
            // Distinct per title, so a shelf of these is a shelf of cards with
            // their own artwork rather than one card drawn five times.
            art: Some(format!("https://example.invalid/{title}")),
            duration: None,
            target: Target::Play {
                video_id: title.to_string(),
            },
        }
    }

    fn album(title: &str, subtitle: &str) -> Card {
        Card {
            title: title.to_string(),
            subtitle: subtitle.to_string(),
            art: Some(format!("https://example.invalid/{title}")),
            duration: None,
            target: Target::Open {
                browse_id: title.to_string(),
            },
        }
    }

    /// An empty art cache and somewhere to note what a draw asked for.
    ///
    /// Every card in these tests has an artwork URL and none of it has arrived,
    /// which is deliberately the hardest case for the layout: the tiles are
    /// drawn as placeholders at exactly the size the real pictures will take,
    /// so anything that fits here fits once they land.
    fn no_art() -> (ArtCache, Vec<(String, String)>) {
        (ArtCache::default(), Vec::new())
    }

    /// A cache holding one flat-coloured sleeve per card of `shelves`, for the
    /// tests and previews that need pictures rather than placeholders.
    fn stub_art(shelves: &[Shelf]) -> ArtCache {
        let mut cache = ArtCache::default();
        for (n, card) in shelves.iter().flat_map(|shelf| &shelf.cards).enumerate() {
            // A different hue per card, so a preview shows the accents varying
            // the way real sleeves make them vary.
            let (r, g, b) = (
                (n as u8).wrapping_mul(97).saturating_add(40),
                (n as u8).wrapping_mul(53).saturating_add(20),
                (n as u8).wrapping_mul(29).saturating_add(90),
            );
            let rgb = [r, g, b].repeat(32 * 32);
            cache.want(card.art_key());
            cache.store(
                card.art_key().to_string(),
                Some(Cover::from_rgb(32, 32, rgb)),
            );
        }
        cache
    }

    /// Prints the landing page in each card shape, for eyeballing a layout
    /// change.
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
        let art = stub_art(&shelves);

        for (shape, width, height) in [
            (CardShape::Poster, 100u16, 36u16),
            (CardShape::Tile, 100, 22),
            (CardShape::Text, 64, 16),
        ] {
            println!(
                "
--- {shape:?} at {width}x{height} ---"
            );
            let mut wanted = Vec::new();
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal
                .draw(|frame| {
                    let block = Block::bordered().title(" home ");
                    let area = Rect::new(0, 0, width, height);
                    let inner = block.inner(area);
                    frame.render_widget(block, area);
                    let cursor = HomeCursor { shelf: 1, card: 1 };
                    let layouts = shelf_layouts(&shelves, inner, 0, shape);
                    render_feed(
                        frame,
                        &shelves,
                        &layouts,
                        cursor,
                        &[0, 0, 0],
                        Tiles {
                            shape,
                            art: &art,
                            wanted: &mut wanted,
                        },
                    );
                })
                .unwrap();

            let buf = terminal.backend().buffer();
            for y in 0..height {
                let row: String = (0..width).map(|x| buf[(x, y)].symbol()).collect();
                println!("{}", row.trim_end());
            }
        }
    }

    /// The now-playing strip, drawn on its own.
    ///
    /// `cargo test preview_hero -- --ignored --nocapture`
    #[test]
    #[ignore = "prints a layout preview rather than asserting"]
    fn preview_hero() {
        let art = Cover::from_rgb(32, 32, [180, 40, 70].repeat(32 * 32));
        let hero = Hero {
            title: "Người Im Lặng Gặp Người Hay Nói",
            artist: "HIEUTHUHAI",
            art: Some(&art),
            snap: Snapshot {
                position: Duration::from_secs(97),
                ..Default::default()
            },
            duration: Some(Duration::from_secs(214)),
        };

        for width in [70u16, 40] {
            println!(
                "
--- hero at {width} ---"
            );
            let mut terminal = Terminal::new(TestBackend::new(width, HERO_HEIGHT)).unwrap();
            terminal
                .draw(|frame| render_hero(frame, &hero, Rect::new(0, 0, width, HERO_HEIGHT)))
                .unwrap();
            let buf = terminal.backend().buffer();
            for y in 0..HERO_HEIGHT {
                let row: String = (0..width).map(|x| buf[(x, y)].symbol()).collect();
                println!("{}", row.trim_end());
            }
        }
    }

    /// The strip exists to name what is playing without the user going to look
    /// for it, so the name and the artist are the parts that cannot be dropped.
    #[test]
    fn the_hero_names_what_is_playing() {
        let art = Cover::from_rgb(4, 4, [180, 40, 70].repeat(16));
        let hero = Hero {
            title: "Feather",
            artist: "Nujabes",
            art: Some(&art),
            snap: Snapshot {
                position: Duration::from_secs(60),
                ..Default::default()
            },
            duration: Some(Duration::from_secs(180)),
        };

        let mut terminal = Terminal::new(TestBackend::new(60, HERO_HEIGHT)).unwrap();
        terminal
            .draw(|frame| render_hero(frame, &hero, Rect::new(0, 0, 60, HERO_HEIGHT)))
            .unwrap();
        let buf = terminal.backend().buffer();
        let text: String = (0..HERO_HEIGHT)
            .map(|y| (0..60).map(|x| buf[(x, y)].symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join(
                "
",
            );

        assert!(text.contains("NOW PLAYING"), "{text}");
        assert!(text.contains("Feather"), "{text}");
        assert!(text.contains("Nujabes"), "{text}");
        assert!(text.contains("1:00 / 3:00"), "{text}");
    }

    /// A paused player says so. The strip is the only place on the landing page
    /// that can, and "now playing" over silence is simply wrong.
    #[test]
    fn the_hero_says_when_it_is_paused() {
        let hero = Hero {
            title: "Feather",
            artist: "Nujabes",
            art: None,
            snap: Snapshot {
                state: PlayState::Paused,
                ..Default::default()
            },
            duration: None,
        };

        let mut terminal = Terminal::new(TestBackend::new(60, HERO_HEIGHT)).unwrap();
        terminal
            .draw(|frame| render_hero(frame, &hero, Rect::new(0, 0, 60, HERO_HEIGHT)))
            .unwrap();
        let buf = terminal.backend().buffer();
        let text: String = (0..HERO_HEIGHT)
            .map(|y| (0..60).map(|x| buf[(x, y)].symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join(
                "
",
            );

        assert!(text.contains("PAUSED"), "{text}");
        assert!(!text.contains("NOW PLAYING"), "{text}");
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

    /// Lyrics as YouTube returns them for a track it holds timings for: the
    /// lines split as they are sung, each with the moment it starts.
    ///
    /// The words are invented rather than a real song's. Nothing here depends
    /// on what they say -- only on how many lines there are, how long each one
    /// wraps to, and how far apart their starts sit -- and a fixture that is
    /// somebody's copyrighted lyric is one the tests cannot be shown in full.
    /// Each line names its own index, so a test that finds the wrong line
    /// highlighted says which one it found.
    fn timed_lyrics() -> Lyrics {
        let timed: Vec<TimedLine> = (0..8)
            .map(|n| TimedLine {
                text: format!("sung line {n}, a lyric long enough to wrap in a narrow panel"),
                start: Duration::from_secs(n * 5),
            })
            .collect();

        Lyrics {
            text: timed
                .iter()
                .map(|line| line.text.as_str())
                .collect::<Vec<_>>()
                .join("\n"),
            source: Some("Source: Musixmatch".to_string()),
            timed,
        }
    }

    /// One panel drawn on its own, as one string per row.
    fn drawn_panel(now: &NowPlaying, width: u16, height: u16) -> Vec<String> {
        drawn_panel_at(now, Duration::ZERO, width, height)
    }

    /// [`drawn_panel`], with the track a given way through -- which only the
    /// lyrics panel reads, and is the whole of what it follows.
    fn drawn_panel_at(
        now: &NowPlaying,
        position: Duration,
        width: u16,
        height: u16,
    ) -> Vec<String> {
        let snap = Snapshot {
            position,
            ..Default::default()
        };
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| {
                let area = Rect::new(0, 0, width, height);
                let [tabs, content] =
                    Layout::vertical([Constraint::Length(2), Constraint::Min(0)]).areas(area);
                render_tabs(frame, now.tab, tabs, Color::Cyan);
                match now.tab {
                    Tab::UpNext => render_up_next(frame, now, content, Color::Cyan),
                    Tab::Lyrics => render_lyrics(frame, now, &snap, content).0,
                    Tab::Comments => render_comments(frame, now, content),
                    Tab::Related => render_related(frame, now, content, Color::Cyan),
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

        assert!(
            rows[0].contains("COMMENTS"),
            "every tab is named: {:?}",
            rows[0]
        );
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
        assert!(
            marked[0].contains("7:48"),
            "and states its length: {:?}",
            marked[0]
        );
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
            timed: Vec::new(),
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

    /// The row carrying the mark, and where in the panel it sits. `None` when
    /// no line is marked, which is a distinct outcome from the wrong one being
    /// marked and is asserted on in its own right below.
    fn marked(rows: &[String]) -> Option<(usize, &String)> {
        rows.iter().enumerate().find(|(_, row)| row.contains('▌'))
    }

    #[test]
    fn the_line_being_sung_is_the_marked_one() {
        let mut now = playing();
        now.tab = Tab::Lyrics;
        now.lyrics = Panel::Ready(timed_lyrics());

        // Wide enough that every lyric is one row, so what the test is about is
        // which line is marked rather than how a line wrapped.
        let rows = drawn_panel_at(&now, Duration::from_secs(22), 70, 8);
        let (_, row) =
            marked(&rows).unwrap_or_else(|| panic!("a line should be marked: {rows:#?}"));
        assert!(
            row.contains("sung line 4"),
            "the fifth line starts at 20s and the sixth at 25s: {row:?}"
        );
        assert_eq!(
            rows.iter().filter(|row| row.contains('▌')).count(),
            1,
            "one lyric is being sung, so one mark: {rows:#?}"
        );
    }

    #[test]
    fn a_wrapped_lyric_is_marked_once_at_its_first_row() {
        let mut now = playing();
        now.tab = Tab::Lyrics;
        now.lyrics = Panel::Ready(timed_lyrics());

        // Narrow enough that every lyric wraps onto two rows. One mark, on the
        // row the lyric starts at: a bar down each of its rows would read as
        // several lines being sung rather than one occupying several rows.
        let rows = drawn_panel_at(&now, Duration::from_secs(22), 44, 10);
        let (at, row) =
            marked(&rows).unwrap_or_else(|| panic!("a line should be marked: {rows:#?}"));

        assert_eq!(
            rows.iter().filter(|row| row.contains('▌')).count(),
            1,
            "one mark however many rows the lyric wraps onto: {rows:#?}"
        );
        assert!(row.contains("sung line 4"), "{rows:#?}");
        assert!(
            !row.contains("narrow panel"),
            "the mark belongs on the row the lyric starts at: {rows:#?}"
        );
        // The rest of the lyric is still drawn, just unmarked.
        assert!(
            rows[at + 1].contains("narrow panel"),
            "the wrapped remainder should follow it: {rows:#?}"
        );
    }

    #[test]
    fn nothing_is_marked_before_the_first_line_is_sung() {
        let mut now = playing();
        now.tab = Tab::Lyrics;
        now.lyrics = Panel::Ready(timed_lyrics());

        // The fixture's first line starts at zero, so this is the case of a
        // track whose words have not begun: an intro should mark nothing.
        let mut late = timed_lyrics();
        late.timed[0].start = Duration::from_secs(9);
        now.lyrics = Panel::Ready(late);

        let rows = drawn_panel_at(&now, Duration::from_secs(3), 70, 8);
        assert!(
            marked(&rows).is_none(),
            "no line has started yet: {rows:#?}"
        );
    }

    #[test]
    fn the_panel_keeps_the_line_being_sung_near_the_middle() {
        let mut now = playing();
        now.tab = Tab::Lyrics;
        now.lyrics = Panel::Ready(timed_lyrics());

        // Far enough in that the panel has to have scrolled: the sixth line is
        // past the bottom of a six-row viewport showing the top of the lyrics.
        let rows = drawn_panel_at(&now, Duration::from_secs(27), 70, 8);
        let (at, row) =
            marked(&rows).unwrap_or_else(|| panic!("a line should be marked: {rows:#?}"));
        assert!(row.contains("sung line 5"), "{row:?}");
        // Two rows of tabs above a six-row viewport: centred means the middle
        // of the panel, not the top edge, so what has just been sung is still
        // readable above it.
        assert!(
            (4..=6).contains(&at),
            "the marked line should sit mid-panel, not at an edge: row {at} of {rows:#?}"
        );

        // And it moves down the lyrics rather than staying put as time passes.
        let later = drawn_panel_at(&now, Duration::from_secs(35), 70, 8);
        let (_, row) =
            marked(&later).unwrap_or_else(|| panic!("a line should be marked: {later:#?}"));
        assert!(row.contains("sung line 7"), "{row:?}");
    }

    #[test]
    fn a_user_who_has_scrolled_keeps_the_view_they_scrolled_to() {
        let mut now = playing();
        now.tab = Tab::Lyrics;
        now.lyrics = Panel::Ready(timed_lyrics());
        // What `scroll_page` does: the panel stops following and the cursor
        // decides what is on screen, however far through the track it is.
        now.follow_lyrics = false;
        now.cursor[Tab::Lyrics.index()] = 0;

        let rows = drawn_panel_at(&now, Duration::from_secs(35), 70, 8);
        assert!(
            rows[2].contains("Musixmatch"),
            "the panel should still be at the top: {rows:#?}"
        );
        assert!(
            marked(&rows).is_none(),
            "the line being sung is off screen, and the panel stays where it \
             was put rather than chasing it: {rows:#?}"
        );

        // Only the scrolling is handed over, not the highlight: a sung line
        // that happens to be on screen is still marked.
        let early = drawn_panel_at(&now, Duration::from_secs(8), 70, 8);
        let (_, row) =
            marked(&early).unwrap_or_else(|| panic!("a line should be marked: {early:#?}"));
        assert!(row.contains("sung line 1"), "{row:?}");
    }

    #[test]
    fn a_track_with_no_timings_draws_as_it_always_did() {
        let mut now = playing();
        now.tab = Tab::Lyrics;
        now.lyrics = Panel::Ready(Lyrics {
            text: (1..=20).map(|n| format!("line {n}\n")).collect(),
            source: Some("Source: Musixmatch".to_string()),
            timed: Vec::new(),
        });

        // However far through the track, an untimed panel has nothing to mark
        // and nothing to follow: it scrolls by cursor as it did before.
        let rows = drawn_panel_at(&now, Duration::from_secs(120), 44, 8);
        assert!(marked(&rows).is_none(), "{rows:#?}");
        assert!(rows[2].contains("Musixmatch"), "{rows:#?}");
        assert!(rows[4].contains("line 1"), "{rows:#?}");
    }

    #[test]
    fn only_the_lyric_being_sung_is_emphasised() {
        // Bold *and* a marker for the current line: on a terminal that renders
        // bold as bright and nothing else, colour alone would not say which
        // line of twenty is the one.
        let current = lyric_line("now", false, Some(3), Some(3), true);
        assert_eq!(current.spans[0].content, "▌ ");
        assert_eq!(current.spans[1].style.fg, Some(Color::White));
        assert!(current.spans[1].style.add_modifier.contains(Modifier::BOLD));

        // Every other lyric reads the same, whether it has been sung or not:
        // one of them is "here" and the rest are the song around it, and a
        // third shade would only be something to work out.
        //
        // DarkGray rather than Gray, and that is the whole of the contrast:
        // Gray is ANSI 7 to White's ANSI 15, which on many themes is no
        // difference at all and leaves the sung line marked by boldness alone.
        let sung = lyric_line("before", false, Some(2), Some(3), true);
        let ahead = lyric_line("after", false, Some(4), Some(3), true);
        assert_eq!(sung.spans[0].content, "  ");
        assert_eq!(sung.spans[1].style.fg, Some(Color::DarkGray));
        assert_eq!(ahead.spans[1].style.fg, Some(Color::DarkGray));
        assert_eq!(sung.spans[1].style, ahead.spans[1].style);
        assert!(
            !sung.spans[1].style.add_modifier.contains(Modifier::BOLD),
            "and no bold either, or the dimming buys nothing"
        );

        // A track with no timings has no line being sung, so there is nothing
        // for dimming to contrast with: the words stay at ordinary body grey
        // rather than the whole panel going dark and gaining nothing for it.
        let plain = lyric_line("plain", false, None, None, false);
        assert_eq!(plain.spans[0].content, "  ");
        assert_eq!(plain.spans[1].style.fg, Some(Color::Gray));
        // Same for a timed track still in its intro.
        assert_eq!(
            lyric_line("intro", false, Some(0), None, true).spans[1]
                .style
                .fg,
            Some(Color::Gray)
        );

        // The continuation of a wrapped lyric: still emphasised, since it is
        // the same line being sung, but the mark stays on the row it started.
        let wrapped = lyric_line("...and on", false, Some(3), Some(3), false);
        assert_eq!(wrapped.spans[0].content, "  ");
        assert_eq!(wrapped.spans[1].style, current.spans[1].style);

        // The credit is about the panel rather than the song, and is never the
        // line being sung whatever index it happens to sit at.
        let credit = lyric_line("Source: Musixmatch", true, None, Some(0), true);
        assert_eq!(credit.spans[0].content, "  ");
        assert_eq!(credit.spans[1].style.fg, Some(Color::DarkGray));
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
        assert!(
            rows.iter()
                .any(|row| row.contains("one step, another step"))
        );
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
        assert_eq!(filled(progress_line(&snap(0), total, 40, Color::Cyan)), 0);
        assert_eq!(
            filled(progress_line(&snap(100), total, 40, Color::Cyan)),
            27
        );
        assert_eq!(filled(progress_line(&snap(50), total, 40, Color::Cyan)), 14);

        // Past the end, which happens for a moment on every track: the
        // container's length and what the decoder yields never quite agree.
        assert_eq!(
            filled(progress_line(&snap(105), total, 40, Color::Cyan)),
            27
        );
    }

    #[test]
    fn a_track_of_unknown_length_gets_a_clock_and_no_bar() {
        let snap = Snapshot {
            position: Duration::from_secs(65),
            ..Default::default()
        };
        // A livestream has no end to draw a bar towards, and one drawn anyway
        // would have to invent where it is.
        let line = progress_line(&snap, None, 40, Color::Cyan);
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
                render_track_info(
                    frame,
                    &now,
                    snap,
                    Rect::new(0, 0, 40, INFO_HEIGHT),
                    Color::Cyan,
                );
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
        now.lyrics = Panel::Ready(timed_lyrics());
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
                render_track_info(
                    frame,
                    &now,
                    snap,
                    Rect::new(0, 0, 46, INFO_HEIGHT),
                    Color::Cyan,
                );
            })
            .unwrap();
        let buf = terminal.backend().buffer();
        for y in 0..INFO_HEIGHT {
            let row: String = (0..46).map(|x| buf[(x, y)].symbol()).collect();
            println!("|{}", row.trim_end());
        }
    }

    /// A shelf of `shape` drawn on its own, as one string per row.
    fn drawn_shelf(
        width: u16,
        shelf: &Shelf,
        cursor: ShelfCursor,
        shape: CardShape,
    ) -> Vec<String> {
        let (card_width, card_height) = card_size(shape);
        let height = card_height + 1;
        let across = (width / card_width).max(1);
        let (art, mut wanted) = no_art();
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| {
                render_shelf(
                    frame,
                    shelf,
                    Rect::new(0, 0, width, height),
                    cursor,
                    (across, width / across),
                    &mut Tiles {
                        shape,
                        art: &art,
                        wanted: &mut wanted,
                    },
                );
            })
            .unwrap();
        let buf = terminal.backend().buffer();
        (0..height)
            .map(|y| (0..width).map(|x| buf[(x, y)].symbol()).collect())
            .collect()
    }

    /// The subtitle is not one grey string. Whoever made the record is the one
    /// thing on that line anybody scans a shelf for, so it is drawn a step
    /// brighter than the year beside it and the bullets a step dimmer than
    /// either -- which is the difference between a card and a paragraph.
    #[test]
    fn the_subtitle_is_styled_by_what_each_part_of_it_is() {
        let spans = detail_spans("Tame Impala • 2015", 40);
        let colours: Vec<(&str, Option<Color>)> = spans
            .iter()
            .map(|span| (span.content.as_ref(), span.style.fg))
            .collect();

        assert_eq!(
            colours,
            vec![
                ("Tame Impala", Some(Color::Gray)),
                (" • ", Some(Color::from_u32(0x0044_4444))),
                ("2015", Some(Color::DarkGray)),
            ]
        );
    }

    /// Narrow cards are the common case on the shapes that carry a picture, so
    /// the line has to give out at a field boundary rather than halfway through
    /// a separator hanging off the end of the card.
    #[test]
    fn a_subtitle_too_long_for_its_card_stops_at_a_field() {
        let spans = detail_spans("Tame Impala • 2015", 13);
        let written: String = spans.iter().map(|span| span.content.as_ref()).collect();

        assert_eq!(written, "Tame Impala", "no dangling separator");
        assert!(display_width(&written) <= 13);

        // And nothing at all rather than a bare bullet when there is no room.
        assert!(detail_spans("Tame Impala • 2015", 0).is_empty());
    }

    /// A subtitle YouTube wrote as one field is still one field: this styles
    /// what it sent rather than inventing structure that is not there.
    #[test]
    fn a_subtitle_with_no_bullets_is_left_whole() {
        let spans = detail_spans("Mild High Club", 40);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].content.as_ref(), "Mild High Club");
        assert_eq!(spans[0].style.fg, Some(Color::Gray));
    }

    /// The row under the cursor is the one row the user is looking straight at,
    /// so it is the one that must never be unreadable. `lift` guarantees the
    /// accent's brightest *channel*, not its luminance -- a deep red sleeve
    /// yields (170, 0, 0), which passes that bar and is far too dark to write
    /// black on.
    #[test]
    fn a_highlight_picks_ink_that_can_be_read_against_its_fill() {
        let ink = |r, g, b| highlight(Color::Rgb(r, g, b)).fg;

        // Bright fills take black, as the highlight always did.
        assert_eq!(
            ink(255, 220, 90),
            Some(Color::Black),
            "a pale yellow sleeve"
        );
        assert_eq!(
            ink(170, 200, 170),
            Some(Color::Black),
            "a soft green sleeve"
        );

        // Dark fills flip, which is the case that used to be illegible.
        assert_eq!(ink(170, 0, 0), Some(Color::White), "a deep red sleeve");
        assert_eq!(ink(40, 40, 180), Some(Color::White), "a navy sleeve");

        // Every fill is legible one way or the other -- the property that
        // actually matters, stated as itself rather than as a list of cases.
        for r in (0..=255u8).step_by(15) {
            for g in (0..=255u8).step_by(15) {
                for b in (0..=255u8).step_by(15) {
                    let contrast = match highlight(Color::Rgb(r, g, b)).fg {
                        Some(Color::Black) => luma(r, g, b),
                        Some(Color::White) => 255 - luma(r, g, b),
                        other => panic!("unexpected ink {other:?}"),
                    };
                    assert!(
                        contrast >= 115,
                        "({r},{g},{b}) leaves only {contrast} of contrast"
                    );
                }
            }
        }
    }

    /// A terminal colour is not an RGB triple and has no luminance to measure,
    /// so it keeps the black ink the highlight has always used.
    #[test]
    fn a_named_fill_keeps_the_ink_it_always_had() {
        assert_eq!(highlight(Color::Cyan).fg, Some(Color::Black));
        assert_eq!(highlight(Color::Cyan).bg, Some(Color::Cyan));
    }

    /// The gallery card exists to be flush: its sleeve should reach both
    /// borders rather than sit in margins, which is the whole reason its width
    /// is tied to its height rather than chosen for how many fit across.
    #[test]
    fn a_gallery_sleeve_fills_the_card_it_is_drawn_in() {
        let (width, height) = GALLERY_CARD;
        let inner_width = width - 2;
        let picture_rows = height - 2 - CARD_TEXT_ROWS;

        assert_eq!(
            picture_rows * 2,
            inner_width,
            "a square sleeve of {picture_rows} rows is {} columns wide, \
             which must match the {inner_width} columns inside the border",
            picture_rows * 2
        );
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
        // 80 columns holds two 32-wide cards with sixteen left over, which the
        // pair stretch into rather than leave as a margin.
        let rows = drawn_shelf(80, &quick_picks(), cursor(0, 0), CardShape::Text);
        assert!(rows[0].contains("Quick picks"), "{rows:#?}");

        let text = rows.join("\n");
        assert!(text.contains("Is It True"));
        assert!(text.contains("Nice Boys"));
        // The third card is off the end of the row, not squeezed onto it.
        assert!(!text.contains("Homage"), "{rows:#?}");
    }

    /// The counter is the only thing on screen that says a shelf runs past the
    /// edge of the window, so a shelf scrolled off its first card must still
    /// say where in it the cursor is.
    #[test]
    fn a_focused_shelf_says_how_far_along_it_the_cursor_is() {
        let rows = drawn_shelf(80, &quick_picks(), cursor(4, 2), CardShape::Text);
        assert!(rows[0].contains("5 of 5"), "{:?}", rows[0]);

        // Unfocused, there is no cursor in it to report.
        let unfocused = ShelfCursor {
            focused: false,
            selected: 0,
            offset: 0,
        };
        assert!(!drawn_shelf(80, &quick_picks(), unfocused, CardShape::Text)[0].contains("of 5"));
    }

    /// Enter does two different things depending on the card, and this is the
    /// only thing on the page that says which.
    #[test]
    fn a_card_says_whether_it_plays_or_opens() {
        let shelf = Shelf {
            title: "Albums for you".to_string(),
            cards: vec![song("Creep", "Radiohead"), album("Currents", "Tame Impala")],
        };
        let text = drawn_shelf(80, &shelf, cursor(0, 0), CardShape::Text).join("\n");
        assert!(text.contains("▶ play"), "{text}");
        assert!(text.contains("≡ open"), "{text}");
    }

    /// When YouTube did say what a card is, the badge says it instead -- which
    /// is more than the glyph could, and is the line the eye lands on after the
    /// title.
    #[test]
    fn a_card_wears_the_type_youtube_gave_it() {
        let shelf = Shelf {
            title: "Albums for you".to_string(),
            cards: vec![
                song("Creep", "Song • Radiohead"),
                album("Currents", "Album • Tame Impala"),
            ],
        };
        let text = drawn_shelf(80, &shelf, cursor(0, 0), CardShape::Text).join("\n");
        assert!(text.contains("song"), "{text}");
        assert!(text.contains("album"), "{text}");
        // The marker is drawn as the badge, so it must not also be repeated in
        // the words beside it.
        assert!(!text.contains("Song •"), "{text}");
        assert!(text.contains("Radiohead"), "{text}");
    }

    /// A length is drawn when the feed gave one and simply absent when it did
    /// not -- an album has no single duration, and a made-up one is worse than
    /// none.
    #[test]
    fn a_card_shows_a_length_only_when_it_has_one() {
        let mut timed = song("Creep", "Song • Radiohead");
        timed.duration = Some(Duration::from_secs(238));
        let shelf = Shelf {
            title: "Quick picks".to_string(),
            cards: vec![timed, album("Currents", "Album • Tame Impala")],
        };
        let text = drawn_shelf(80, &shelf, cursor(0, 0), CardShape::Text).join("\n");
        assert!(text.contains("3:58"), "{text}");
        assert_eq!(
            text.matches(':').count(),
            1,
            "only the song is timed: {text}"
        );
    }

    /// The selection is what the whole page is navigated by, so it has to be
    /// visible -- and on exactly one card.
    ///
    /// Checked as "one card differs from the rest" rather than against a named
    /// colour, because the selected card is now drawn in its own sleeve's
    /// accent: there is no one colour a highlight is, which is the point of it.
    #[test]
    fn exactly_one_card_is_highlighted() {
        let shelf = quick_picks();
        let shelves = vec![shelf.clone()];
        let art = stub_art(&shelves);
        let mut wanted = Vec::new();
        let height = card_size(CardShape::Text).1 + 1;

        let mut terminal = Terminal::new(TestBackend::new(80, height)).unwrap();
        terminal
            .draw(|frame| {
                render_shelf(
                    frame,
                    &shelf,
                    Rect::new(0, 0, 80, height),
                    cursor(1, 0),
                    (3, 26),
                    &mut Tiles {
                        shape: CardShape::Text,
                        art: &art,
                        wanted: &mut wanted,
                    },
                );
            })
            .unwrap();
        let buf = terminal.backend().buffer();

        // Top-left corner of each of the three drawn cards, on the row under
        // the heading.
        let corners: Vec<Color> = (0..3).map(|card| buf[(card * 26, 1)].fg).collect();
        assert_eq!(corners[0], Color::DarkGray, "{corners:?}");
        assert_eq!(corners[2], Color::DarkGray, "{corners:?}");
        assert_ne!(
            corners[1],
            Color::DarkGray,
            "the selected card has to stand out: {corners:?}"
        );
    }

    /// The card under the cursor takes its frame from its own sleeve, so that a
    /// shelf of records is a shelf of colours rather than a shelf of cyan.
    #[test]
    fn a_selected_card_is_framed_in_its_own_artwork() {
        let shelf = quick_picks();
        let shelves = vec![shelf.clone()];
        let art = stub_art(&shelves);
        let expected = art
            .get(shelf.cards[1].art_key())
            .expect("the stub cache holds every card")
            .accent;

        let mut wanted = Vec::new();
        let height = card_size(CardShape::Text).1 + 1;
        let mut terminal = Terminal::new(TestBackend::new(80, height)).unwrap();
        terminal
            .draw(|frame| {
                render_shelf(
                    frame,
                    &shelf,
                    Rect::new(0, 0, 80, height),
                    cursor(1, 0),
                    (3, 26),
                    &mut Tiles {
                        shape: CardShape::Text,
                        art: &art,
                        wanted: &mut wanted,
                    },
                );
            })
            .unwrap();

        let buf = terminal.backend().buffer();
        assert_eq!(
            buf[(26, 1)].fg,
            Color::Rgb(expected.0, expected.1, expected.2)
        );
    }

    /// The lead shelf always gets the biggest complete card the window fits.
    #[test]
    fn the_window_picks_the_biggest_cards_that_fit() {
        let at = |height| plan_home(Rect::new(0, 0, 100, height), false).max_shape;

        assert_eq!(at(80), CardShape::Gallery, "room for a page of galleries");
        // Two shelves of gallery cards need 41 rows and two of posters 33, so
        // this is the window that has to step down one rather than two -- the
        // case that would regress if the new shape were merely the old one made
        // taller.
        assert_eq!(at(36), CardShape::Gallery);
        assert_eq!(at(20), CardShape::Gallery);
        assert_eq!(at(16), CardShape::Poster);
        assert_eq!(at(8), CardShape::Tile);
        // Narrower than one shelf of anything. The pane refuses to draw the
        // feed at all well before this, so all that matters is that it settles
        // rather than looping or panicking.
        assert_eq!(at(1), CardShape::Text);
    }

    #[test]
    fn sections_receive_distinct_layouts() {
        let shelves = [
            quick_picks(),
            Shelf {
                title: "From your listening".to_string(),
                cards: quick_picks().cards,
            },
            Shelf {
                title: "Albums for you".to_string(),
                cards: vec![
                    album("Currents", "Tame Impala"),
                    album("Discovery", "Daft Punk"),
                    album("Modal Soul", "Nujabes"),
                ],
            },
        ];

        assert_eq!(
            section_shape(&shelves[0], 0, CardShape::Gallery),
            CardShape::Gallery
        );
        assert_eq!(
            section_shape(&shelves[1], 1, CardShape::Gallery),
            CardShape::Tile
        );
        assert_eq!(
            section_shape(&shelves[2], 2, CardShape::Gallery),
            CardShape::Poster
        );
        assert_eq!(
            section_shape(&shelves[0], 0, CardShape::Tile),
            CardShape::Tile,
            "a short terminal must clamp every section"
        );
    }

    #[test]
    fn three_home_sections_stay_visible_in_a_normal_window() {
        let shelves = [
            quick_picks(),
            Shelf {
                title: "From your listening".to_string(),
                cards: quick_picks().cards,
            },
            Shelf {
                title: "Albums for you".to_string(),
                cards: vec![album("Currents", "Tame Impala")],
            },
        ];

        // A 48-row terminal leaves 34 rows for shelves when the search bar,
        // status bar, home border and now-playing strip have taken their room.
        let layouts = shelf_layouts(&shelves, Rect::new(0, 0, 100, 34), 0, CardShape::Gallery);

        assert_eq!(layouts.len(), 3);
        assert_eq!(layouts[0].shape, CardShape::Gallery);
        assert_eq!(layouts[1].shape, CardShape::Tile);
        assert_eq!(layouts[2].shape, CardShape::Text);
    }

    /// The strip is drawn when the shelves under it still make a page, and
    /// dropped when they do not -- a landing page mostly occupied by the track
    /// already playing is the one thing on it the user does not need shown.
    #[test]
    fn the_hero_gives_way_to_the_shelves_on_a_short_window() {
        let plan = |height, playing| plan_home(Rect::new(0, 0, 100, height), playing);

        assert!(!plan(80, false).hero, "nothing is playing");
        assert!(plan(80, true).hero, "room for both");

        // Just enough for two shelves of cards and not for the strip as well:
        // the cards win, and the strip stands down rather than costing a shelf.
        let tight = plan(shelf_height(CardShape::Tile) - 1, true);
        assert_eq!(tight.max_shape, CardShape::Tile);
        assert!(!tight.hero, "{tight:?}");
    }

    /// Only the cards actually on screen have their pictures fetched. This is
    /// what keeps a twelve-shelf feed from being three hundred requests.
    #[test]
    fn only_the_visible_cards_ask_for_artwork() {
        let shelves = vec![quick_picks(), quick_picks(), quick_picks()];
        let (art, mut wanted) = no_art();
        // Two cards across and one shelf tall, so eleven of the fifteen cards
        // are off screen.
        let (width, height) = (2 * TEXT_CARD.0, TEXT_CARD.1 + 1);

        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| {
                render_feed(
                    frame,
                    &shelves,
                    &shelf_layouts(&shelves, Rect::new(0, 0, width, height), 0, CardShape::Text),
                    HomeCursor { shelf: 0, card: 0 },
                    &[0, 0, 0],
                    Tiles {
                        shape: CardShape::Text,
                        art: &art,
                        wanted: &mut wanted,
                    },
                );
            })
            .unwrap();

        assert_eq!(wanted.len(), 2, "asked for {wanted:?}");
        let keys: Vec<&str> = wanted.iter().map(|(key, _)| key.as_str()).collect();
        assert_eq!(keys, ["Is It True", "Nice Boys"]);
    }

    /// A picture already in hand is not asked for again -- the difference
    /// between a cache and a download per frame.
    #[test]
    fn artwork_already_held_is_not_asked_for() {
        let shelves = vec![quick_picks()];
        let art = stub_art(&shelves);
        let mut wanted = Vec::new();
        let (width, height) = (2 * TEXT_CARD.0, TEXT_CARD.1 + 1);

        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| {
                render_feed(
                    frame,
                    &shelves,
                    &shelf_layouts(&shelves, Rect::new(0, 0, width, height), 0, CardShape::Text),
                    HomeCursor { shelf: 0, card: 0 },
                    &[0],
                    Tiles {
                        shape: CardShape::Text,
                        art: &art,
                        wanted: &mut wanted,
                    },
                );
            })
            .unwrap();

        assert!(wanted.is_empty(), "asked again for {wanted:?}");
    }

    /// A tile fills its box corner to corner whatever shape the source is, so
    /// that a shelf of 16:9 video thumbnails and square sleeves is still a
    /// shelf of matching squares.
    #[test]
    fn a_tile_is_cropped_to_fill_rather_than_letterboxed() {
        // Sixteen by nine into a square tile: every row survives, the sides go.
        let wide = Cover::solid(160, 90);
        let crop = Crop::fill(&wide, 20, 20);
        assert_eq!(crop.height, 90, "no rows are dropped");
        assert_eq!(crop.width, 90, "the sides are");
        assert_eq!(crop.x, 35, "and what is kept is the middle");

        // A square into a square is left alone.
        let square = Cover::solid(64, 64);
        assert_eq!(
            Crop::fill(&square, 20, 20),
            Crop {
                x: 0,
                y: 0,
                width: 64,
                height: 64
            }
        );

        // A tall source into a wide tile loses its top and bottom instead.
        let tall = Cover::solid(50, 200);
        let crop = Crop::fill(&tall, 20, 10);
        assert_eq!((crop.width, crop.height), (50, 25));
        assert_eq!(crop.y, 87);
    }

    /// Degenerate sizes reach this from a card squeezed to nothing, and it has
    /// to return a usable rect rather than dividing by zero.
    #[test]
    fn cropping_survives_an_empty_tile() {
        let crop = Crop::fill(&Cover::solid(64, 64), 0, 0);
        assert!(crop.width >= 1 && crop.height >= 1);
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
            for row in drawn_shelf(width, &shelf, cursor(0, 0), CardShape::Text) {
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
        terminal.draw(|frame| render_sign_in(frame, phase)).unwrap();
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
        assert!(
            rows.iter().filter(|r| r.contains('│')).count() > 4,
            "wrapped onto several rows"
        );
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
            assert!(
                !hint.contains("B tray"),
                "{hint:?} offers B with nothing playing"
            );
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
