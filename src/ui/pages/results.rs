//! Track result columns and viewport rendering.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{List, ListItem, Paragraph};

use crate::app::App;
use crate::ui::shell;
use crate::ui::mouse::{MouseMap, MouseTarget};
use crate::ui::palette::{ambient, highlight};
use crate::ui::text::{cell, focused_cell};

use super::{browse, search};

/// Width reserved for the right-aligned duration column, including padding.
pub(in crate::ui) const DURATION_WIDTH: usize = 8;

/// Width of the artist column, when the row is wide enough to carry one.
pub(in crate::ui) const ARTIST_WIDTH: usize = 22;

/// Width of the album column, when the row is wide enough to carry one.
pub(in crate::ui) const ALBUM_WIDTH: usize = 20;

/// Title width defended before any metadata column is granted. Below this a
/// title truncates to the point of not identifying a song, which costs more
/// than the artist and album are worth.
pub(in crate::ui) const MIN_TITLE_WIDTH: usize = 24;

pub(in crate::ui) fn render_results(frame: &mut Frame, app: &mut App, area: Rect, mouse: &mut MouseMap) {
    if app.browsing.is_none() && (!app.search_items.is_empty() || (!app.query.is_empty() && app.results.is_empty())) {
        search::render(frame, app, area, mouse);
        return;
    }
    browse::render(frame, app, area, mouse);
}

pub(super) fn render_compact_results(frame: &mut Frame, app: &mut App, area: Rect, mouse: &mut MouseMap) {
    let [heading, inner] = Layout::vertical([
        Constraint::Length(1), Constraint::Min(0),
    ]).areas(shell::inset(area));
    frame.render_widget(Paragraph::new(Line::from(Span::styled(
        app.list_title().trim().to_string(),
        Style::default().add_modifier(Modifier::BOLD),
    ))), heading);

    if app.results.is_empty() {
        let hint = if app.page_loading() {
            "working ..."
        } else if app.query.is_empty() {
            // Nothing has been searched yet, so this is the first thing a new
            // user reads. "no results" would be true and useless.
            "type a search above, or press H for the home feed"
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
    mouse.targets.push(MouseTarget::TrackRows {
        area: Rect {
            height: (end - app.offset) as u16,
            ..list_area
        },
        first: app.offset,
    });
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
                format!(" {}", focused_cell(&track.title, title_width, selected)),
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
pub(in crate::ui) fn header_line(title_width: usize, artist_width: usize, album_width: usize) -> Line<'static> {
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

/// Splits a results row into title, artist and album widths.
///
/// Columns are granted from the left and dropped from the right, so a narrow
/// pane spends what it has on the title -- the one field a song cannot be
/// identified without. A zero width means the column is not drawn at all.
/// Duration is never dropped: it is eight columns and worth them.
pub(in crate::ui) fn columns(width: usize) -> (usize, usize, usize) {
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
