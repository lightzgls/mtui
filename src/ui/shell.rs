//! Shared navigation chrome. Geometry, rendering, and mouse targets are kept
//! together so a visible button always owns exactly the cells it draws in.

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use unicode_segmentation::UnicodeSegmentation;

use super::{
    HEADER_HEIGHT, HERO_HEIGHT, INFO_HEIGHT, MIN_WIDTH_WITH_COVER, MouseMap, MouseTarget,
    PANEL_MIN_WIDTH, STATUS_HEIGHT, display_width, highlight, truncate,
};
use crate::app::{Mode, MouseAction, View};
use crate::navigation::{Icon, Symbols};

pub(super) const BACKGROUND: Color = Color::Rgb(12, 12, 12);
pub(super) const SURFACE: Color = Color::Rgb(26, 26, 26);

pub(super) fn regions(area: Rect) -> [Rect; 3] {
    Layout::vertical([
        Constraint::Length(HEADER_HEIGHT),
        Constraint::Min(1),
        Constraint::Length(STATUS_HEIGHT),
    ])
    .areas(area)
}

pub(super) fn inset(area: Rect) -> Rect {
    let margin = match area.width {
        80.. => 3,
        40.. => 2,
        24.. => 1,
        _ => 0,
    };
    Rect::new(
        area.x + margin,
        area.y,
        area.width.saturating_sub(margin * 2),
        area.height,
    )
}

/// Keep footer text off the side edges and reserve a blank row below actions.
pub(super) fn footer_content(area: Rect) -> Rect {
    let mut inner = inset(area);
    if inner.height >= STATUS_HEIGHT {
        inner.height -= 1;
    }
    inner
}

/// Extra side padding on normal panels, retaining compact tabs at the floor.
pub(super) fn panel_inset(area: Rect) -> Rect {
    let margin = if area.width >= 40 {
        2
    } else {
        area.width.min(2) / 2
    };
    Rect::new(
        area.x + margin,
        area.y,
        area.width.saturating_sub(margin * 2),
        area.height,
    )
}

pub(super) struct PlayerAreas {
    pub hero: Option<Rect>,
    pub art: Option<Rect>,
    pub info: Option<Rect>,
    pub divider: Option<Rect>,
    pub panel: Rect,
}

/// Small windows retain track identity above the tabs. Wide windows reserve
/// the left side for artwork and the right side for all four player panels.
pub(super) fn player_areas(area: Rect) -> PlayerAreas {
    if area.width < MIN_WIDTH_WITH_COVER {
        return lyrics_areas(area);
    }

    // At normal widths keep title, artist, duration, and their gaps visible.
    let minimum = if area.width >= 96 {
        46
    } else {
        PANEL_MIN_WIDTH
    };
    let panel_width = (area.width * 2 / 5).max(minimum);
    let [left, divider, panel] = Layout::horizontal([
        Constraint::Min(1),
        Constraint::Length(2),
        Constraint::Length(panel_width),
    ])
    .areas(area);
    let left = inset(left);
    let info_height = if left.height >= 18 { INFO_HEIGHT } else { 3 };
    let (art, info) = if left.height > info_height {
        let [art, info] =
            Layout::vertical([Constraint::Min(1), Constraint::Length(info_height)]).areas(left);
        (Some(art), info)
    } else {
        (None, left)
    };
    PlayerAreas {
        hero: None,
        art,
        info: Some(info),
        panel,
        divider: Some(Rect::new(
            divider.x + divider.width / 2,
            divider.y,
            1,
            divider.height,
        )),
    }
}

/// Reading mode is explicit and retains track identity above the full-width panel.
pub(super) fn lyrics_areas(area: Rect) -> PlayerAreas {
    let (hero, panel) = if area.height >= HERO_HEIGHT + 6 {
        let [hero, panel] =
            Layout::vertical([Constraint::Length(HERO_HEIGHT), Constraint::Min(0)]).areas(area);
        (Some(inset(hero)), panel)
    } else {
        (None, area)
    };
    PlayerAreas {
        hero,
        art: None,
        info: None,
        divider: None,
        panel,
    }
}

pub(super) struct Header<'a> {
    pub mode: Mode,
    pub view: View,
    pub query: &'a str,
    pub accent: Color,
    pub has_player: bool,
    pub accepts_input: bool,
    pub symbols: Symbols,
}

fn header_areas(area: Rect) -> [Rect; 5] {
    let area = inset(area);
    let row = Rect::new(
        area.x,
        area.y + u16::from(area.height >= 3),
        area.width,
        area.height.min(1),
    );
    let brand = if row.width >= 72 { 8 } else { 0 };
    let home = if row.width >= 30 { 8 } else { 0 };
    let player = if row.width >= 50 { 12 } else { 0 };
    let menu = if row.width >= 20 { 9 } else { 0 };
    Layout::horizontal([
        Constraint::Length(brand),
        Constraint::Length(home),
        Constraint::Length(player),
        Constraint::Min(1),
        Constraint::Length(menu),
    ])
    .areas(row)
}

pub(super) fn render_header(
    frame: &mut Frame,
    area: Rect,
    header: Header<'_>,
    mouse: &mut MouseMap,
) {
    let [brand, home, player, input, menu] = header_areas(area);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("● ", Style::default().fg(header.accent)),
            Span::styled("MTUI", Style::default().add_modifier(Modifier::BOLD)),
        ])),
        brand,
    );
    for (icon, label, rect, active, enabled, action) in [
        (
            Icon::Home,
            "Home",
            home,
            header.view == View::Home,
            true,
            MouseAction::GoHome,
        ),
        (
            Icon::Playing,
            "Playing",
            player,
            header.view == View::Playing,
            header.has_player,
            MouseAction::OpenPlayer,
        ),
        (
            Icon::Menu,
            "Menu",
            menu,
            false,
            true,
            MouseAction::OpenAppMenu,
        ),
    ] {
        let style = if active {
            highlight(header.accent).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(if enabled {
                Color::Gray
            } else {
                Color::DarkGray
            })
        };
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                header.symbols.label(icon, label),
                style,
            )))
            .alignment(Alignment::Center),
            rect,
        );
        if rect.width > 0 && rect.height > 0 && enabled {
            mouse.targets.push(MouseTarget::Area(rect, action));
        }
    }

    let prefix = if header.symbols.symbol(Icon::Search).is_empty() {
        " / "
    } else {
        " ⌕ / "
    };
    let room = (input.width as usize).saturating_sub(display_width(prefix) + 2);
    let editing = header.mode == Mode::Editing;
    let text = if header.query.is_empty() && !editing {
        truncate("Search songs, albums, artists", room)
    } else if editing {
        visible_query(header.query, room)
    } else {
        truncate(header.query, room)
    };
    let cursor_offset = display_width(prefix) + display_width(&text);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(prefix, Style::default().fg(header.accent)),
            Span::styled(
                text,
                Style::default().fg(if editing { Color::White } else { Color::Gray }),
            ),
        ]))
        .style(Style::default().bg(SURFACE)),
        Rect::new(
            input.x + u16::from(input.width > 0),
            input.y,
            input.width.saturating_sub(1),
            input.height,
        ),
    );
    if input.width > 0 && input.height > 0 {
        mouse
            .targets
            .push(MouseTarget::Area(input, MouseAction::EditSearch));
        if editing && header.accepts_input {
            let offset = cursor_offset.min(input.width.saturating_sub(1) as usize) as u16;
            frame.set_cursor_position((
                input.x + (offset + 1).min(input.width.saturating_sub(1)),
                input.y,
            ));
        }
    }
    if area.height > 1 {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "─".repeat(area.width as usize),
                Style::default().fg(Color::Rgb(38, 38, 38)),
            ))),
            Rect::new(area.x, area.y + area.height - 1, area.width, 1),
        );
    }
}

/// Keep the caret's end of a long query visible, without splitting a CJK glyph
/// or a composed emoji as the search field narrows.
fn visible_query(query: &str, room: usize) -> String {
    let mut used = 0;
    let mut visible = Vec::new();
    for grapheme in query.graphemes(true).rev() {
        let width = display_width(grapheme);
        if used + width > room {
            break;
        }
        used += width;
        visible.push(grapheme);
    }
    visible.into_iter().rev().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    #[test]
    fn header_buttons_and_search_keep_distinct_hit_regions_at_every_width() {
        for (width, symbols) in [1, 19, 30, 50, 72, 100, 160].into_iter().flat_map(|width| {
            [Symbols::NONE, Symbols::from_mask(0x15), Symbols::ALL].map(|symbols| (width, symbols))
        }) {
            let mut terminal = Terminal::new(TestBackend::new(width, 2)).unwrap();
            let mut mouse = MouseMap::default();
            terminal
                .draw(|frame| {
                    render_header(
                        frame,
                        frame.area(),
                        Header {
                            mode: Mode::Browse,
                            view: View::Home,
                            query: "",
                            accent: Color::Magenta,
                            has_player: true,
                            accepts_input: true,
                            symbols,
                        },
                        &mut mouse,
                    );
                })
                .unwrap();
            let [_, home, player, input, menu] = header_areas(Rect::new(0, 0, width, 2));
            for (rect, expected) in [
                (home, MouseAction::GoHome),
                (player, MouseAction::OpenPlayer),
                (input, MouseAction::EditSearch),
                (menu, MouseAction::OpenAppMenu),
            ] {
                if rect.width > 0 {
                    assert_eq!(
                        mouse.action_at(rect.x, rect.y),
                        Some(expected),
                        "at width {width}"
                    );
                    assert!(rect.right() <= width);
                }
            }
        }
    }

    #[test]
    fn editing_keeps_the_query_tail_visible_on_grapheme_boundaries() {
        assert_eq!(visible_query("long query 日本語", 6), "日本語");
        assert_eq!(visible_query("long query 👩‍💻", 2), "👩‍💻");
        assert_eq!(visible_query("abc", 0), "");
    }

    #[test]
    fn artwork_dominates_playback_and_only_explicit_reading_mode_hides_it() {
        for width in [100, 160, 240] {
            let area = Rect::new(3, 4, width, 40);
            let playback = player_areas(area);
            let art = playback.art.unwrap();
            assert!(art.width > playback.panel.width);
            assert!(art.height >= 35);
            assert!(art.bottom() <= playback.info.unwrap().y);
            let reading = lyrics_areas(area);
            assert!(reading.art.is_none());
            assert_eq!(reading.panel.width, width);
            assert!(reading.panel.bottom() <= area.bottom());
        }
    }

    #[test]
    fn padded_header_keeps_search_cursor_inside_its_visible_field() {
        for width in [1, 19, 30, 50, 72, 100] {
            let mut terminal = Terminal::new(TestBackend::new(width, HEADER_HEIGHT)).unwrap();
            let mut mouse = MouseMap::default();
            terminal
                .draw(|frame| {
                    render_header(
                        frame,
                        frame.area(),
                        Header {
                            mode: Mode::Editing,
                            view: View::Home,
                            query: "long search 日本語 👩‍💻",
                            accent: Color::Cyan,
                            has_player: true,
                            accepts_input: true,
                            symbols: Symbols::ALL,
                        },
                        &mut mouse,
                    )
                })
                .unwrap();
            let input = header_areas(Rect::new(0, 0, width, HEADER_HEIGHT))[3];
            if input.width > 0 {
                let cursor = terminal.get_cursor_position().unwrap();
                assert!(cursor.x >= input.x && cursor.x < input.right());
                assert_eq!(cursor.y, input.y);
            }
        }
    }
}
