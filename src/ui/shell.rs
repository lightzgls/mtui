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
    PANEL_MAX_WIDTH, PANEL_MIN_WIDTH, STATUS_HEIGHT, display_width, truncate,
};
use crate::app::{Mode, MouseAction, View};

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
    let margin = area.width.min(2) / 2;
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
        let (hero, panel) = if area.height >= HERO_HEIGHT + 6 {
            let [hero, panel] =
                Layout::vertical([Constraint::Length(HERO_HEIGHT), Constraint::Min(0)]).areas(area);
            (Some(inset(hero)), panel)
        } else {
            (None, area)
        };
        return PlayerAreas {
            hero,
            art: None,
            info: None,
            divider: None,
            panel,
        };
    }

    let panel_width = (area.width * 2 / 5).clamp(PANEL_MIN_WIDTH, PANEL_MAX_WIDTH);
    let [left, divider, panel] = Layout::horizontal([
        Constraint::Min(1),
        Constraint::Length(2),
        Constraint::Length(panel_width),
    ])
    .areas(area);
    let left = inset(left);
    let (art, info) = if left.height > INFO_HEIGHT {
        let [art, info] =
            Layout::vertical([Constraint::Min(1), Constraint::Length(INFO_HEIGHT)]).areas(left);
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

pub(super) struct Header<'a> {
    pub mode: Mode,
    pub view: View,
    pub query: &'a str,
    pub accent: Color,
    pub has_player: bool,
    pub accepts_input: bool,
}

fn header_areas(area: Rect) -> [Rect; 5] {
    let row = Rect::new(area.x, area.y, area.width, area.height.min(1));
    let brand = if row.width >= 72 { 8 } else { 0 };
    let home = if row.width >= 30 { 7 } else { 0 };
    let player = if row.width >= 50 { 10 } else { 0 };
    let menu = if row.width >= 20 { 7 } else { 0 };
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
    for (label, rect, active, enabled, action) in [
        (
            "Home",
            home,
            header.view == View::Home,
            true,
            MouseAction::GoHome,
        ),
        (
            "Playing",
            player,
            header.view == View::Playing,
            header.has_player,
            MouseAction::OpenPlayer,
        ),
        ("Menu", menu, false, true, MouseAction::OpenAppMenu),
    ] {
        let style = Style::default().fg(if active {
            header.accent
        } else if enabled {
            Color::Gray
        } else {
            Color::DarkGray
        });
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(label, style))).alignment(Alignment::Center),
            rect,
        );
        if rect.width > 0 && rect.height > 0 && enabled {
            mouse.targets.push(MouseTarget::Area(rect, action));
        }
    }

    const PREFIX: &str = " / ";
    let room = (input.width as usize).saturating_sub(display_width(PREFIX) + 1);
    let editing = header.mode == Mode::Editing;
    let text = if header.query.is_empty() && !editing {
        truncate("Search songs, albums, artists", room)
    } else if editing {
        visible_query(header.query, room)
    } else {
        truncate(header.query, room)
    };
    let cursor_offset = display_width(PREFIX) + display_width(&text);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(PREFIX, Style::default().fg(header.accent)),
            Span::styled(
                text,
                Style::default().fg(if editing { Color::White } else { Color::Gray }),
            ),
        ]))
        .style(Style::default().bg(SURFACE)),
        input,
    );
    if input.width > 0 && input.height > 0 {
        mouse
            .targets
            .push(MouseTarget::Area(input, MouseAction::EditSearch));
        if editing && header.accepts_input {
            let offset = cursor_offset.min(input.width.saturating_sub(1) as usize) as u16;
            frame.set_cursor_position((input.x + offset, input.y));
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
        for width in [1, 19, 30, 50, 72, 100, 160] {
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
}
