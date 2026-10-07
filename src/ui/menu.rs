//! The Ctrl+K menu, account commands, contextual actions and keyboard reference.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::dialog::{self, Chrome};
use super::{MouseMap, MouseTarget, ambient, centred_offset, display_width, truncate};
use crate::app::{App, MenuItem, MenuPage, MouseAction};

struct MenuRenderLine {
    item: Option<usize>,
    line: Line<'static>,
}

pub(super) fn render(frame: &mut Frame, app: &App, mouse: &mut MouseMap) {
    if let Some(menu) = app.menu() {
        draw(
            frame,
            menu.page,
            menu.selected,
            &app.menu_items(),
            ambient(app),
            mouse,
        );
    }
}

pub(super) fn draw(
    frame: &mut Frame,
    page: MenuPage,
    selected: usize,
    items: &[MenuItem],
    accent: Color,
    mouse: &mut MouseMap,
) {
    let width = 52u16.min(frame.area().width);
    let lines = menu_lines(
        items,
        selected,
        page == MenuPage::Help,
        width.saturating_sub(2) as usize,
        accent,
    );
    let account_status = if page == MenuPage::Account {
        Some(if items.iter().all(|item| !item.enabled) {
            "Sign-in in progress…"
        } else if items.len() > 1 {
            "Signed in to YouTube Music"
        } else {
            "Not signed in"
        })
    } else {
        None
    };
    let height = (lines.len() as u16)
        .saturating_add(6 + if account_status.is_some() { 2 } else { 0 })
        .min(24);
    let is_child = matches!(page, MenuPage::Account | MenuPage::Help);
    let dialog = dialog::render(
        frame,
        Chrome {
            title: page.title(),
            width,
            height,
            close_label: if is_child { "Back" } else { "Close" },
            close: if is_child {
                MouseAction::BackMenu
            } else {
                MouseAction::CloseMenu
            },
            outside: MouseAction::CloseMenu,
        },
        mouse,
    );
    let mut content = dialog.body;
    if let Some(status) = account_status {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                format!(
                    "  {}",
                    truncate(status, content.width.saturating_sub(2) as usize)
                ),
                Style::default().fg(Color::Gray),
            ))),
            Rect {
                height: content.height.min(1),
                ..content
            },
        );
        let spent = content.height.min(2);
        content.y += spent;
        content.height -= spent;
    }
    let offset = menu_offset(&lines, selected, content.height as usize);
    register_menu_targets(mouse, &lines, offset, content);
    frame.render_widget(
        Paragraph::new(
            lines
                .into_iter()
                .skip(offset)
                .take(content.height as usize)
                .map(|row| row.line)
                .collect::<Vec<_>>(),
        ),
        content,
    );
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            menu_footer(page, dialog.footer.width as usize),
            Style::default().fg(Color::Gray),
        ))),
        dialog.footer,
    );
}

fn register_menu_targets(
    mouse: &mut MouseMap,
    lines: &[MenuRenderLine],
    offset: usize,
    area: Rect,
) {
    for (row, line) in lines
        .iter()
        .skip(offset)
        .take(area.height as usize)
        .enumerate()
    {
        if let Some(item) = line.item {
            mouse.targets.push(MouseTarget::Area(
                Rect::new(area.x, area.y + row as u16, area.width, 1),
                MouseAction::ActivateMenuItem(item),
            ));
        }
    }
}

fn menu_lines(
    items: &[MenuItem],
    selected: usize,
    informational: bool,
    width: usize,
    ambient: Color,
) -> Vec<MenuRenderLine> {
    let mut lines = Vec::with_capacity(items.len() * 2);
    for (index, item) in items.iter().enumerate() {
        if let Some(section) = item.section {
            lines.push(MenuRenderLine {
                item: None,
                line: menu_heading_line(section, width),
            });
        }
        lines.push(MenuRenderLine {
            item: Some(index),
            line: menu_row_line(
                &if item.opens_panel() {
                    format!("{} ›", item.label)
                } else {
                    item.label.clone()
                },
                item.shortcut,
                item.enabled,
                index == selected,
                informational,
                width,
                ambient,
            ),
        });
    }
    lines
}

fn menu_heading_line(section: &str, width: usize) -> Line<'static> {
    Line::from(Span::styled(
        format!("  {}", truncate(section, width.saturating_sub(2))),
        Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::BOLD),
    ))
}

fn menu_row_line(
    label: &str,
    shortcut: Option<&str>,
    enabled: bool,
    selected: bool,
    informational: bool,
    width: usize,
    ambient: Color,
) -> Line<'static> {
    if width == 0 {
        return Line::default();
    }

    let actionable_selection = selected && enabled && !informational;
    let selected_style = Style::default().fg(Color::White).bg(Color::Rgb(38, 38, 38));
    let label_style = if actionable_selection {
        selected_style
    } else if informational || enabled {
        Style::default().fg(Color::White)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    let shortcut_style = if actionable_selection {
        selected_style
    } else if informational || enabled {
        Style::default().fg(Color::Gray)
    } else {
        Style::default().fg(Color::DarkGray)
    };

    let prefix_width = width.min(2);
    let right_padding = if width >= 8 { 2 } else { 0 };
    let available = width
        .saturating_sub(prefix_width)
        .saturating_sub(right_padding);
    let shortcut_budget = if available >= 8 {
        (available / 3).min(16)
    } else {
        0
    };
    let shortcut = shortcut
        .map(|shortcut| truncate(shortcut, shortcut_budget))
        .unwrap_or_default();
    let shortcut_width = display_width(&shortcut);
    let label_width = available.saturating_sub(shortcut_width + usize::from(shortcut_width > 0));
    let label = truncate(label, label_width);
    let gap = available
        .saturating_sub(display_width(&label))
        .saturating_sub(shortcut_width);
    let prefix = if selected && !informational {
        truncate("› ", prefix_width)
    } else {
        " ".repeat(prefix_width)
    };

    let fill_style = if actionable_selection {
        selected_style
    } else {
        Style::default()
    };
    Line::from(vec![
        Span::styled(
            prefix,
            if selected && !informational {
                label_style.fg(ambient)
            } else {
                label_style
            },
        ),
        Span::styled(label, label_style),
        Span::styled(" ".repeat(gap), fill_style),
        Span::styled(shortcut, shortcut_style),
        Span::styled(" ".repeat(right_padding), fill_style),
    ])
}

fn menu_offset(lines: &[MenuRenderLine], selected: usize, viewport: usize) -> usize {
    if lines.len() <= viewport || viewport == 0 {
        return 0;
    }
    let selected_line = lines
        .iter()
        .position(|line| line.item == Some(selected))
        .unwrap_or(0);
    let mut offset = centred_offset(selected_line, viewport, lines.len());
    // Do not strand a section's first item at the top without its heading.
    if viewport > 1
        && offset > 0
        && lines[offset].item.is_some()
        && lines[offset - 1].item.is_none()
    {
        offset -= 1;
    }
    offset.min(lines.len().saturating_sub(viewport))
}

fn menu_footer(page: MenuPage, width: usize) -> String {
    let (full, compact) = if page == MenuPage::Help {
        (" ↑↓ Scroll   Esc Back", " ↑↓  Esc")
    } else if page == MenuPage::Account {
        (" ↑↓ Select   Enter Open   Esc Back", " ↑↓  Enter  Esc")
    } else {
        (" ↑↓ Select   Enter Open   Esc Close", " ↑↓  Enter  Esc")
    };
    truncate(
        if display_width(full) <= width {
            full
        } else {
            compact
        },
        width,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{account_menu_items, app_menu_items, keyboard_help_items};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn drawn(
        page: MenuPage,
        selected: usize,
        items: &[MenuItem],
        width: u16,
        height: u16,
    ) -> (ratatui::buffer::Buffer, MouseMap) {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        let mut mouse = MouseMap::default();
        terminal
            .draw(|frame| draw(frame, page, selected, items, Color::Cyan, &mut mouse))
            .unwrap();
        (terminal.backend().buffer().clone(), mouse)
    }

    #[test]
    fn menu_rows_and_close_controls_own_only_their_visible_cells() {
        let items = app_menu_items(true);
        for (width, height) in [(100, 36), (48, 18), (30, 10), (8, 4), (1, 1)] {
            for selected in 0..items.len() {
                let (_, mouse) = drawn(MenuPage::Root, selected, &items, width, height);
                for target in &mouse.targets {
                    if let MouseTarget::Area(area, action) = target {
                        assert!(area.right() <= width && area.bottom() <= height);
                        if matches!(action, MouseAction::ActivateMenuItem(_)) {
                            assert_eq!(mouse.action_at(area.x, area.y), Some(*action));
                        }
                    }
                }
                if width >= 30 && height >= 10 {
                    assert!(mouse.targets.iter().any(|target| matches!(target, MouseTarget::Area(_, MouseAction::ActivateMenuItem(index)) if *index == selected)), "selected row clipped at {width}x{height}");
                }
            }
        }
    }

    #[test]
    fn menu_and_account_screens_keep_actions_and_session_state_clear() {
        let (buf, mouse) = drawn(MenuPage::Root, 4, &app_menu_items(true), 100, 36);
        let text: String = buf.content.iter().map(|cell| cell.symbol()).collect();
        for label in [
            "Home",
            "Search",
            "Now Playing",
            "Account",
            "Settings",
            "Keyboard shortcuts",
            "Quit",
        ] {
            assert!(text.contains(label));
        }
        assert_eq!(mouse.action_at(0, 0), Some(MouseAction::CloseMenu));
        let (buf, mouse) = drawn(
            MenuPage::Account,
            0,
            &account_menu_items(false, false),
            100,
            36,
        );
        let text: String = buf.content.iter().map(|cell| cell.symbol()).collect();
        assert!(text.contains("Not signed in") && text.contains("Connect YouTube Music"));
        assert!(
            mouse
                .targets
                .iter()
                .any(|target| matches!(target, MouseTarget::Area(_, MouseAction::BackMenu)))
        );
    }

    #[test]
    fn long_shortcuts_and_labels_are_bounded_without_bright_selection_fills() {
        for width in 0..80 {
            let line = menu_row_line(
                "A very long 日本語 menu label",
                Some("Ctrl+Shift+Enter"),
                true,
                true,
                false,
                width,
                Color::Cyan,
            );
            assert_eq!(line.width(), width);
            assert!(
                line.spans
                    .iter()
                    .all(|span| span.style.bg != Some(Color::Cyan))
            );
        }
    }

    #[test]
    #[ignore = "prints rendered modal previews"]
    fn preview_menus() {
        for (page, items, name) in [
            (MenuPage::Root, app_menu_items(true), "menu"),
            (
                MenuPage::Account,
                account_menu_items(false, false),
                "account",
            ),
            (MenuPage::Help, keyboard_help_items(), "shortcuts"),
        ] {
            for (width, height) in [(100, 36), (48, 18)] {
                let (buf, _) = drawn(page, 4, &items, width, height);
                super::super::tests::print_preview(&format!("{name}-{width}x{height}"), &buf);
            }
        }
    }
}
