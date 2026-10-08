//! Queue spacing and hit targets use the same visible item geometry.

use super::{
    MouseMap, MouseTarget, cell, centred_offset, display_width, focused_cell, highlight, message,
    truncate,
};
use crate::app::{MouseAction, NowPlaying};
use crate::source::Track;
use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
};

const HEADER_HEIGHT: u16 = 3;
const MARKER_WIDTH: usize = 3;
const COLUMN_GAP: usize = 2;
const ARTIST_WIDTH: usize = 14;
const MIN_TITLE_WIDTH: usize = 16;

pub(super) struct Geometry {
    list: Rect,
    stride: u16,
    pub viewport: usize,
    first: usize,
}

impl Geometry {
    pub fn new(now: &NowPlaying, area: Rect) -> Self {
        let header = HEADER_HEIGHT.min(area.height);
        let list = Rect::new(area.x, area.y + header, area.width, area.height - header);
        // Keep a blank row between songs when at least four songs fit.
        let stride = if list.height >= 8 { 2 } else { 1 };
        let viewport = list.height.div_ceil(stride) as usize;
        let cursor = now.cursor().min(now.queue.len().saturating_sub(1));
        Self {
            list,
            stride,
            viewport,
            first: centred_offset(cursor, viewport, now.queue.len()),
        }
    }

    fn row(&self, slot: usize) -> Rect {
        Rect::new(
            self.list.x,
            self.list.y + slot as u16 * self.stride,
            self.list.width,
            1,
        )
    }
}

pub(super) fn register_targets(mouse: &mut MouseMap, now: &NowPlaying, area: Rect) {
    let layout = Geometry::new(now, area);
    for (slot, track) in now
        .queue
        .iter()
        .skip(layout.first)
        .take(layout.viewport)
        .enumerate()
    {
        let index = layout.first + slot;
        let row = layout.row(slot);
        mouse
            .targets
            .push(MouseTarget::Area(row, MouseAction::OpenPageRow(index)));
        if let Some((x, width)) = artist_region(track, row.width as usize) {
            mouse.targets.push(MouseTarget::Area(
                Rect::new(row.x + x as u16, row.y, width as u16, 1),
                MouseAction::OpenQueueArtist(index),
            ));
        }
    }
}

/// The source heading stays fixed while selection scrolls through queue items.
pub(super) fn render(frame: &mut Frame, now: &NowPlaying, area: Rect, accent: Color) -> usize {
    if now.queue.is_empty() {
        message(frame, "loading the queue ...", area);
        return 0;
    }
    let width = area.width as usize;
    let heading = vec![
        Line::from(Span::styled(
            " Playing from",
            Style::default().fg(Color::DarkGray),
        )),
        Line::from(Span::styled(
            format!(" {}", truncate(&now.queue_title, width.saturating_sub(1))),
            Style::default().add_modifier(Modifier::BOLD),
        )),
        if now.repeat.label() == "off" {
            Line::from("")
        } else {
            Line::from(Span::styled(
                format!(" Repeat {}", now.repeat.label()),
                Style::default().fg(accent),
            ))
        },
    ];
    frame.render_widget(
        Paragraph::new(heading),
        Rect::new(area.x, area.y, area.width, HEADER_HEIGHT.min(area.height)),
    );
    let layout = Geometry::new(now, area);
    let cursor = now.cursor().min(now.queue.len().saturating_sub(1));
    for (slot, track) in now
        .queue
        .iter()
        .skip(layout.first)
        .take(layout.viewport)
        .enumerate()
    {
        let index = layout.first + slot;
        frame.render_widget(
            Paragraph::new(line(
                track,
                index == cursor,
                Some(index) == now.playing,
                width,
                accent,
            )),
            layout.row(slot),
        );
    }
    now.queue.len()
}

struct Columns {
    title: usize,
    artist: usize,
}

impl Columns {
    fn new(track: &Track, width: usize) -> Self {
        let available = width
            .saturating_sub(MARKER_WIDTH + display_width(&track.duration_str()) + COLUMN_GAP + 1);
        let artist = if available >= MIN_TITLE_WIDTH + ARTIST_WIDTH + COLUMN_GAP {
            ARTIST_WIDTH
        } else {
            0
        };
        let title = available.saturating_sub(artist + if artist > 0 { COLUMN_GAP } else { 0 });
        Self { title, artist }
    }
}

pub(super) fn line(
    track: &Track,
    selected: bool,
    playing: bool,
    width: usize,
    accent: Color,
) -> Line<'static> {
    let style = if selected {
        highlight(accent)
    } else if playing {
        Style::default().fg(accent).add_modifier(Modifier::BOLD)
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
    if width < MARKER_WIDTH + display_width(&duration) + COLUMN_GAP + 1 {
        return Line::from(Span::styled(
            truncate(
                &format!(" {} {}", if playing { "▶" } else { " " }, track.title),
                width,
            ),
            style,
        ));
    }
    let columns = Columns::new(track, width);
    let mut spans = vec![
        Span::styled(
            format!(" {} ", if playing { "▶" } else { " " }),
            if playing { style } else { dim(Color::DarkGray) },
        ),
        Span::styled(focused_cell(&track.title, columns.title, selected), style),
    ];
    if columns.artist > 0 {
        spans.push(Span::styled(" ".repeat(COLUMN_GAP), style));
        spans.push(Span::styled(
            cell(&track.uploader, columns.artist),
            dim(Color::Gray).add_modifier(if track.artist_ref.is_some() {
                Modifier::UNDERLINED
            } else {
                Modifier::empty()
            }),
        ));
    }
    spans.push(Span::styled(" ".repeat(COLUMN_GAP), style));
    spans.push(Span::styled(format!("{duration} "), dim(Color::DarkGray)));
    Line::from(spans)
}

pub(super) fn artist_region(track: &Track, width: usize) -> Option<(usize, usize)> {
    track.artist_ref.as_ref()?;
    let columns = Columns::new(track, width);
    if columns.artist == 0 {
        return None;
    }
    let artist_width = display_width(&truncate(&track.uploader, columns.artist.saturating_sub(1)));
    (artist_width > 0).then_some((MARKER_WIDTH + columns.title + COLUMN_GAP, artist_width))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::Tab;
    use crate::source::{ArtistRef, BrowseEndpoint};
    use ratatui::{Terminal, backend::TestBackend};
    use std::time::Duration;

    fn fixture() -> NowPlaying {
        let track = Track {
            id: "fixture".into(),
            title: "A long evening song 日本語 👩‍💻".into(),
            uploader: "銀河小魚 and Evening Artist".into(),
            duration: Some(Duration::from_secs(180)),
            album: None,
            artist_ref: Some(ArtistRef {
                name: "銀河小魚".into(),
                endpoint: BrowseEndpoint::new("UCfixture"),
            }),
        };
        let mut now = NowPlaying::new(&track);
        now.queue = (0..30)
            .map(|i| Track {
                id: i.to_string(),
                title: format!("Song {i:02} 日本語"),
                ..track.clone()
            })
            .collect();
        now.queue_title = "Evening Mix".into();
        now.playing = Some(0);
        now
    }

    #[test]
    fn spaced_rows_scroll_to_each_selection_and_blank_rows_have_no_actions() {
        let mut now = fixture();
        for (width, height) in [(20, 4), (40, 10), (50, 11), (64, 12), (64, 40)] {
            for cursor in [0, 15, 29] {
                now.cursor[Tab::UpNext.index()] = cursor;
                let area = Rect::new(0, 0, width, height);
                let geometry = Geometry::new(&now, area);
                let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
                let mut mouse = MouseMap::default();
                terminal
                    .draw(|frame| {
                        render(frame, &now, area, Color::Cyan);
                        register_targets(&mut mouse, &now, area);
                    })
                    .unwrap();
                let buffer = terminal.backend().buffer();
                assert!(
                    (0..height)
                        .any(|y| mouse.action_at(0, y) == Some(MouseAction::OpenPageRow(cursor))),
                    "selected item missing at {width}x{height}"
                );
                for slot in 0..geometry.viewport.min(now.queue.len() - geometry.first) {
                    let row = geometry.row(slot);
                    let index = geometry.first + slot;
                    let rendered: String =
                        (0..width).map(|x| buffer[(x, row.y)].symbol()).collect();
                    assert!(rendered.contains(&format!("Song {index:02}")), "{rendered}");
                    assert_eq!(
                        mouse.action_at(0, row.y),
                        Some(MouseAction::OpenPageRow(index))
                    );
                    assert_eq!(
                        mouse.context_action_at(0, row.y),
                        Some(MouseAction::SelectPageRow(index))
                    );
                    if let Some((x, _)) = artist_region(&now.queue[index], width as usize) {
                        assert_eq!(
                            mouse.action_at(x as u16, row.y),
                            Some(MouseAction::OpenQueueArtist(index))
                        );
                        assert_eq!(
                            mouse.context_action_at(x as u16, row.y),
                            Some(MouseAction::SelectPageRow(index))
                        );
                        assert_ne!(buffer[(x as u16, row.y)].symbol(), " ");
                    }
                    if geometry.stride > 1 && row.y + 1 < height {
                        for x in 0..width {
                            assert_eq!(buffer[(x, row.y + 1)].symbol(), " ");
                            assert_eq!(mouse.action_at(x, row.y + 1), None);
                            assert_eq!(mouse.context_action_at(x, row.y + 1), None);
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn long_unicode_metadata_keeps_column_gaps_and_fits_the_row() {
        let mut track = fixture().queue.remove(0);
        track.title = "An overflowing song title 日本語 👩‍💻".into();
        for duration in [
            None,
            Some(Duration::from_secs(180)),
            Some(Duration::from_secs(360_000)),
        ] {
            track.duration = duration;
            for width in [0, 1, 4, 20, 36, 50, 64, 80] {
                let rendered = line(&track, false, false, width, Color::Cyan);
                let text: String = rendered
                    .spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect();
                assert!(display_width(&text) <= width, "{width}: {text}");
                if let Some((x, _)) = artist_region(&track, width) {
                    let mut terminal = Terminal::new(TestBackend::new(width as u16, 1)).unwrap();
                    terminal
                        .draw(|frame| frame.render_widget(Paragraph::new(rendered), frame.area()))
                        .unwrap();
                    let buffer = terminal.backend().buffer();
                    for gap in 1..=COLUMN_GAP {
                        assert_eq!(buffer[((x - gap) as u16, 0)].symbol(), " ");
                    }
                    let duration_x = width - display_width(&track.duration_str()) - 1;
                    for gap in 1..=COLUMN_GAP {
                        assert_eq!(buffer[((duration_x - gap) as u16, 0)].symbol(), " ");
                    }
                }
            }
        }
    }
}
