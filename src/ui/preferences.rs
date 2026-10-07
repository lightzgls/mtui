//! Settings rows and explicit choice pickers. The draw regions own their hit
//! targets; keyboard and mouse dispatch the same typed setting actions.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::dialog::{self, Chrome};
use super::{
    MouseMap, MouseTarget, ambient, bounded_wrap, centred_offset, display_width, padded, truncate,
};
use crate::app::preferences::{Picker, SettingRow, State};
use crate::app::{App, MouseAction};

pub(super) fn render(frame: &mut Frame, app: &App, mouse: &mut MouseMap) {
    draw(
        frame,
        app.preferences(),
        &app.preference_rows(),
        ambient(app),
        mouse,
    );
}

pub(super) fn draw(
    frame: &mut Frame,
    state: &State,
    rows: &[SettingRow],
    accent: Color,
    mouse: &mut MouseMap,
) {
    if let Some(picker) = &state.picker {
        draw_picker(frame, picker, accent, mouse);
        return;
    }
    let dialog = dialog::render(
        frame,
        Chrome {
            title: "Settings",
            width: 68,
            height: 21,
            close_label: if state.return_to_menu {
                "Back"
            } else {
                "Close"
            },
            close: MouseAction::CloseSettings,
            outside: MouseAction::CloseSettings,
        },
        mouse,
    );
    let help_height = if dialog.body.height >= 10 {
        3
    } else if state.notice.is_some() && dialog.body.height >= 4 {
        2
    } else {
        0
    };
    let [content, help] =
        Layout::vertical([Constraint::Min(0), Constraint::Length(help_height)]).areas(dialog.body);
    let mut lines = Vec::new();
    let mut last_section = "";
    for row in rows {
        let section = row.setting.section();
        if section != last_section {
            if !lines.is_empty() && content.height >= 11 {
                lines.push((None, Line::default()));
            }
            lines.push((
                None,
                Line::from(Span::styled(
                    format!("  {section}"),
                    Style::default()
                        .fg(Color::Gray)
                        .add_modifier(Modifier::BOLD),
                )),
            ));
            last_section = section;
        }
        lines.push((
            Some(row.setting),
            setting_line(
                row,
                row.setting == state.selected_setting(),
                content.width as usize,
                accent,
            ),
        ));
    }
    let selected = lines
        .iter()
        .position(|(setting, _)| *setting == Some(state.selected_setting()))
        .unwrap_or(0);
    let offset = centred_offset(selected, content.height as usize, lines.len());
    for (y, (setting, line)) in lines
        .iter()
        .skip(offset)
        .take(content.height as usize)
        .enumerate()
    {
        let area = Rect::new(content.x, content.y + y as u16, content.width, 1);
        frame.render_widget(Paragraph::new(line.clone()), area);
        if let Some(setting) = setting {
            mouse.targets.push(MouseTarget::Area(
                area,
                MouseAction::ActivateSetting(*setting),
            ));
        }
    }
    let selected_row = rows
        .iter()
        .find(|row| row.setting == state.selected_setting());
    let detail = if let Some((notice, _)) = &state.notice {
        notice.as_str()
    } else if selected_row.is_some_and(|row| !row.enabled) {
        "Available on Windows."
    } else {
        state.selected_setting().description()
    };
    let error = state.notice.as_ref().is_some_and(|(_, error)| *error);
    let detail = bounded_wrap(detail, help.width.saturating_sub(4) as usize)
        .into_iter()
        .take(help.height.saturating_sub(1) as usize)
        .map(|line| {
            Line::from(Span::styled(
                format!("  {line}"),
                Style::default().fg(if error { Color::Red } else { Color::Gray }),
            ))
        })
        .collect::<Vec<_>>();
    frame.render_widget(
        Paragraph::new(detail),
        Rect {
            y: help.y + u16::from(help.height > 0),
            height: help.height.saturating_sub(1),
            ..help
        },
    );
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            footer(
                if state.return_to_menu {
                    " ↑↓ Select   Enter Change   ←→ Cycle   Esc Back"
                } else {
                    " ↑↓ Select   Enter Change   ←→ Cycle   Esc Close"
                },
                " ↑↓ Enter ←→ Esc",
                dialog.footer.width as usize,
            ),
            Style::default().fg(Color::Gray),
        ))),
        dialog.footer,
    );
}

fn setting_line(row: &SettingRow, selected: bool, width: usize, accent: Color) -> Line<'static> {
    let enabled = row.enabled;
    let style = Style::default().fg(if enabled {
        Color::White
    } else {
        Color::DarkGray
    });
    let base = if selected {
        style.bg(Color::Rgb(38, 38, 38))
    } else {
        style
    };
    let prefix = if selected { "› " } else { "  " };
    if width <= 12 {
        return Line::from(Span::styled(
            truncate(&format!("{prefix}{}", row.setting.label()), width),
            base,
        ));
    }
    let room = width.saturating_sub(4);
    let value_room = (room / 2).max(8).min(room);
    let value = if enabled {
        format!(
            "{}{}",
            truncate(
                &row.value,
                value_room.saturating_sub(if row.setting.is_toggle() { 0 } else { 2 })
            ),
            if row.setting.is_toggle() { "" } else { " ›" }
        )
    } else {
        truncate("Windows only", value_room)
    };
    let label_room = room.saturating_sub(display_width(&value) + 1);
    let label = truncate(row.setting.label(), label_room);
    let gap = width.saturating_sub(2 + display_width(&label) + display_width(&value));
    Line::from(vec![
        Span::styled(
            truncate(prefix, width),
            base.fg(if selected { accent } else { Color::White }),
        ),
        Span::styled(label, base),
        Span::styled(" ".repeat(gap), base),
        Span::styled(
            value,
            base.fg(if enabled {
                Color::Gray
            } else {
                Color::DarkGray
            }),
        ),
    ])
}

fn draw_picker(frame: &mut Frame, picker: &Picker, accent: Color, mouse: &mut MouseMap) {
    let height = (picker.choices.len() as u16).saturating_add(6).min(22);
    let dialog = dialog::render(
        frame,
        Chrome {
            title: picker.setting.label(),
            width: 64,
            height,
            close_label: "Back",
            close: MouseAction::CloseSettings,
            outside: MouseAction::CloseSettings,
        },
        mouse,
    );
    let offset = centred_offset(
        picker.selected,
        dialog.body.height as usize,
        picker.choices.len(),
    );
    for (y, choice) in picker
        .choices
        .iter()
        .skip(offset)
        .take(dialog.body.height as usize)
        .enumerate()
    {
        let width = dialog.body.width as usize;
        let current = if choice.current { "Current" } else { "" };
        let prefix = if y + offset == picker.selected {
            "› "
        } else {
            "  "
        };
        let label_room = width.saturating_sub(2 + display_width(current) + 2);
        let label = truncate(&choice.label, label_room);
        let text = format!(
            "{prefix}{label}{}{}",
            " ".repeat(width.saturating_sub(2 + display_width(&label) + display_width(current))),
            current
        );
        let style = if y + offset == picker.selected {
            Style::default()
                .fg(accent)
                .bg(Color::Rgb(38, 38, 38))
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::White)
        };
        let area = Rect::new(
            dialog.body.x,
            dialog.body.y + y as u16,
            dialog.body.width,
            1,
        );
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(padded(&text, width), style))),
            area,
        );
        mouse.targets.push(MouseTarget::Area(
            area,
            MouseAction::ChooseSetting(y + offset),
        ));
    }
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            footer(
                " ↑↓ Select   Enter Apply   Esc Cancel",
                " ↑↓ Enter Esc",
                dialog.footer.width as usize,
            ),
            Style::default().fg(Color::Gray),
        ))),
        dialog.footer,
    );
}

fn footer(full: &str, compact: &str, width: usize) -> String {
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
    use crate::app::preferences::{Choice, ChoiceValue, Setting};
    use crate::config::ImageRenderer;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn rows() -> Vec<SettingRow> {
        Setting::ALL
            .into_iter()
            .zip([
                "System default",
                "Artwork",
                "Automatic",
                "Signal",
                "Off",
                "On",
            ])
            .map(|(setting, value)| SettingRow {
                setting,
                value: value.into(),
                enabled: true,
            })
            .collect()
    }

    fn drawn(state: &State, width: u16, height: u16) -> (ratatui::buffer::Buffer, MouseMap) {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        let mut mouse = MouseMap::default();
        terminal
            .draw(|frame| draw(frame, state, &rows(), Color::Cyan, &mut mouse))
            .unwrap();
        (terminal.backend().buffer().clone(), mouse)
    }

    fn picker() -> Picker {
        Picker {
            setting: Setting::Renderer,
            selected: 1,
            choices: [
                ImageRenderer::Automatic,
                ImageRenderer::PixelArt,
                ImageRenderer::Kitty,
            ]
            .into_iter()
            .enumerate()
            .map(|(i, renderer)| Choice {
                label: crate::app::preferences::renderer_label(renderer).into(),
                value: ChoiceValue::Renderer(renderer),
                current: i == 0,
            })
            .collect(),
        }
    }

    #[test]
    fn settings_show_all_existing_preferences_in_three_groups() {
        let (buf, mouse) = drawn(&State::default(), 100, 36);
        let text: String = buf.content.iter().map(|cell| cell.symbol()).collect();
        for label in [
            "Playback",
            "Appearance",
            "Integrations",
            "Audio output",
            "Cover style",
            "Artwork display",
            "Tray icon",
            "Discord presence",
            "Keep tray icon visible",
        ] {
            assert!(text.contains(label), "missing {label:?}: {text}");
        }
        assert_eq!(
            mouse
                .targets
                .iter()
                .filter(|target| matches!(
                    target,
                    MouseTarget::Area(_, MouseAction::ActivateSetting(_))
                ))
                .count(),
            6
        );
        assert_eq!(mouse.action_at(0, 0), Some(MouseAction::CloseSettings));
        assert_eq!(
            mouse.action_at(18, 12),
            Some(MouseAction::IgnoreOverlay),
            "a section heading consumes clicks"
        );
    }

    #[test]
    fn selected_settings_and_mouse_targets_survive_short_and_narrow_windows() {
        for (width, height) in [(100, 36), (64, 24), (48, 18), (30, 12)] {
            for selected in 0..Setting::ALL.len() {
                let state = State {
                    selected,
                    ..Default::default()
                };
                let (_, mouse) = drawn(&state, width, height);
                if width >= 48 {
                    assert_eq!(
                        mouse
                            .targets
                            .iter()
                            .filter(|target| matches!(
                                target,
                                MouseTarget::Area(_, MouseAction::ActivateSetting(_))
                            ))
                            .count(),
                        6,
                        "all six settings should fit at {width}x{height}"
                    );
                }
                assert!(mouse.targets.iter().any(|target| matches!(target, MouseTarget::Area(_, MouseAction::ActivateSetting(setting)) if *setting == state.selected_setting())), "selection lost at {width}x{height}");
                for target in &mouse.targets {
                    if let MouseTarget::Area(area, _) = target {
                        assert!(area.right() <= width && area.bottom() <= height);
                    }
                }
            }
        }
        for (width, height) in [(1, 1), (8, 4)] {
            drawn(&State::default(), width, height);
        }
    }

    #[test]
    fn picker_current_value_and_pending_selection_have_distinct_targets() {
        let state = State {
            picker: Some(picker()),
            ..Default::default()
        };
        let (buf, mouse) = drawn(&state, 80, 24);
        let text: String = buf.content.iter().map(|cell| cell.symbol()).collect();
        assert!(
            text.contains("Automatic")
                && text.contains("Terminal cells")
                && text.contains("Kitty images")
        );
        assert_eq!(text.matches("Current").count(), 1);
        for index in 0..3 {
            let area = mouse
                .targets
                .iter()
                .find_map(|target| match target {
                    MouseTarget::Area(area, MouseAction::ChooseSetting(i)) if *i == index => {
                        Some(*area)
                    }
                    _ => None,
                })
                .unwrap();
            assert_eq!(
                mouse.action_at(area.x, area.y),
                Some(MouseAction::ChooseSetting(index))
            );
        }
        assert_eq!(mouse.action_at(0, 0), Some(MouseAction::CloseSettings));
    }

    #[test]
    fn long_unicode_values_and_disabled_rows_stay_in_their_columns() {
        let row = SettingRow {
            setting: Setting::Output,
            value: "日本語 speakers 👩‍💻 with a very long name".into(),
            enabled: true,
        };
        let disabled = SettingRow {
            setting: Setting::Icon,
            value: "Signal".into(),
            enabled: false,
        };
        for width in 0..90 {
            for row in [&row, &disabled] {
                let line = setting_line(row, true, width, Color::Cyan);
                assert!(line.width() <= width, "overflow at {width}: {line:?}");
            }
        }
    }

    #[test]
    fn settings_save_errors_remain_visible_in_a_short_window() {
        let state = State {
            notice: Some(("could not save settings".into(), true)),
            ..Default::default()
        };
        let (buf, _) = drawn(&state, 30, 12);
        let text: String = buf.content.iter().map(|cell| cell.symbol()).collect();
        assert!(text.contains("could not save settings"), "{text}");
        assert!(buf.content.iter().any(|cell| cell.fg == Color::Red));
    }

    #[test]
    #[ignore = "prints rendered modal previews"]
    fn preview_settings() {
        for (width, height) in [(100, 36), (48, 18)] {
            let state = State {
                selected: Setting::Renderer.index(),
                ..Default::default()
            };
            let (buf, _) = drawn(&state, width, height);
            super::super::tests::print_preview(&format!("settings-{width}x{height}"), &buf);
            let state = State {
                picker: Some(picker()),
                ..Default::default()
            };
            let (buf, _) = drawn(&state, width, height);
            super::super::tests::print_preview(&format!("settings-picker-{width}x{height}"), &buf);
        }
    }
}
