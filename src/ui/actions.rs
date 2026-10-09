//! Player actions share one row; smaller windows retain a single Actions menu.

use super::{
    MouseMap, MouseTarget, ambient, centred_offset,
    dialog::{self, Chrome},
    display_width, shell, truncate,
};
use crate::app::{App, MouseAction, NowPlaying, actions::PlaylistPicker};
use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    widgets::{Block, Clear, Paragraph},
};

pub(super) fn bar(frame: &mut Frame, now: &NowPlaying, area: Rect, mouse: &mut MouseMap) {
    if area.height < 3 || area.width < 12 {
        return;
    }
    let row = Rect::new(area.x, area.bottom() - 1, area.width, 1);
    frame.render_widget(Clear, row);
    frame.render_widget(
        Block::default().style(Style::default().bg(shell::SURFACE)),
        row,
    );
    let menu_width = 11;
    let end = row.right() - menu_width;
    let mut x = row.x;
    let like = if now.like_pending {
        " Updating… "
    } else if now.liked == Some(true) {
        " Liked "
    } else {
        " Like "
    };
    for (label, action, enabled, active) in [
        (
            like.to_owned(),
            MouseAction::LikePlaying,
            !now.like_pending,
            now.liked == Some(true),
        ),
        (" Share ".into(), MouseAction::SharePlaying, true, false),
        (
            " Save to playlist ".into(),
            MouseAction::SavePlaying,
            true,
            false,
        ),
        (
            " Shuffle ".into(),
            MouseAction::ShufflePlayback,
            true,
            false,
        ),
        (
            format!(" Repeat: {} ", now.repeat.label()),
            MouseAction::RepeatPlayback,
            true,
            now.repeat != crate::app::RepeatMode::Off,
        ),
        (" Output ".into(), MouseAction::ChooseOutput, true, false),
    ] {
        let width = display_width(&label) as u16;
        if x + width > end {
            break;
        }
        let rect = Rect::new(x, row.y, width, 1);
        let style = Style::default().fg(if active {
            Color::White
        } else if enabled {
            Color::Gray
        } else {
            Color::DarkGray
        });
        frame.render_widget(Paragraph::new(label).style(style), rect);
        if enabled {
            mouse.targets.push(MouseTarget::Area(rect, action));
        }
        x += width + 3;
    }
    let rect = Rect::new(end, row.y, menu_width, 1);
    frame.render_widget(
        Paragraph::new("Actions ^P").style(Style::default().fg(Color::Gray)),
        rect,
    );
    mouse
        .targets
        .push(MouseTarget::Area(rect, MouseAction::OpenPlayerActions));
}

fn chrome(frame: &mut Frame, title: &str, height: u16, mouse: &mut MouseMap) -> dialog::Dialog {
    dialog::render(
        frame,
        Chrome {
            title,
            width: 60,
            height,
            close_label: "Close",
            close: MouseAction::ClosePlayerDialog,
            outside: MouseAction::ClosePlayerDialog,
        },
        mouse,
    )
}

pub(super) fn share(frame: &mut Frame, url: &str, notice: Option<&str>, mouse: &mut MouseMap) {
    let dialog = chrome(frame, "Share song", 11, mouse);
    let body = dialog.body;
    frame.render_widget(
        Paragraph::new(url).wrap(ratatui::widgets::Wrap { trim: false }),
        Rect::new(
            body.x + u16::from(body.width > 2),
            body.y,
            body.width.saturating_sub(2),
            body.height.min(3),
        ),
    );
    if body.height >= 4 {
        let button = Rect::new(
            body.x + 1,
            body.y + 3,
            13.min(body.width.saturating_sub(1)),
            1,
        );
        frame.render_widget(
            Paragraph::new(" Copy link ").style(
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::UNDERLINED),
            ),
            button,
        );
        mouse
            .targets
            .push(MouseTarget::Area(button, MouseAction::CopyShareLink));
    }
    frame.render_widget(
        Paragraph::new(truncate(
            notice.unwrap_or("Enter copy link · Esc close"),
            dialog.footer.width as usize,
        ))
        .style(Style::default().fg(Color::Gray)),
        dialog.footer,
    );
}

pub(super) fn playlists(
    frame: &mut Frame,
    picker: &PlaylistPicker,
    accent: Color,
    mouse: &mut MouseMap,
) {
    let dialog = chrome(frame, "Save to playlist", 20, mouse);
    let body = dialog.body;
    let width = body.width.saturating_sub(2);
    frame.render_widget(
        Paragraph::new(truncate(&picker.title, width as usize))
            .style(Style::default().fg(Color::Gray)),
        Rect::new(body.x + 1, body.y, width, body.height.min(1)),
    );
    let list = Rect::new(
        body.x,
        body.y + body.height.min(2),
        body.width,
        body.height.saturating_sub(2),
    );
    let message = if picker.loading {
        Some("Loading your playlists…")
    } else if let Some(error) = picker.error.as_deref() {
        Some(error)
    } else if picker.choices.is_empty() {
        Some("No editable playlists yet. Create one in YouTube Music, then refresh.")
    } else {
        None
    };
    if let Some(message) = message {
        frame.render_widget(
            Paragraph::new(message)
                .wrap(ratatui::widgets::Wrap { trim: true })
                .style(Style::default().fg(Color::Gray)),
            list,
        );
    } else {
        let offset = centred_offset(picker.selected, list.height as usize, picker.choices.len());
        for (index, choice) in picker
            .choices
            .iter()
            .enumerate()
            .skip(offset)
            .take(list.height as usize)
        {
            let row = Rect::new(list.x, list.y + (index - offset) as u16, list.width, 1);
            let suffix = if choice.contains == Some(true) { "  Saved" } else { "" };
            let label = format!(
                " {}{}",
                truncate(
                    &choice.title,
                    list.width.saturating_sub(1 + suffix.len() as u16) as usize
                ),
                suffix
            );
            let style = if index == picker.selected {
                Style::default()
                    .fg(accent)
                    .bg(Color::Rgb(32, 32, 32))
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(if choice.contains == Some(true) {
                    Color::DarkGray
                } else {
                    Color::White
                })
            };
            frame.render_widget(Paragraph::new(label).style(style), row);
            if !picker.saving {
                mouse
                    .targets
                    .push(MouseTarget::Area(row, MouseAction::ChoosePlaylist(index)));
            }
        }
    }
    let footer = dialog.footer;
    let refresh = "Refresh r";
    let right = display_width(refresh) as u16;
    let available = footer.width.saturating_sub(right + 1);
    frame.render_widget(
        Paragraph::new(truncate(
            picker.notice.as_deref().unwrap_or("Enter save · Esc close"),
            available as usize,
        ))
        .style(Style::default().fg(Color::Gray)),
        Rect::new(footer.x, footer.y, available, footer.height),
    );
    if !picker.loading && !picker.saving && footer.width >= right {
        let rect = Rect::new(footer.right() - right, footer.y, right, footer.height);
        frame.render_widget(
            Paragraph::new(refresh).style(Style::default().fg(Color::Gray)),
            rect,
        );
        mouse
            .targets
            .push(MouseTarget::Area(rect, MouseAction::RetryPlaylists));
    }
}

pub(super) fn overlay(frame: &mut Frame, app: &App, mouse: &mut MouseMap) {
    match &app.overlay {
        crate::app::Overlay::Share { url, notice } => share(frame, url, notice.as_deref(), mouse),
        crate::app::Overlay::SavePlaylist(picker) => playlists(frame, picker, ambient(app), mouse),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::Track;
    use ratatui::{Terminal, backend::TestBackend};
    use std::time::Duration;

    #[test]
    fn dialogs_keep_the_selected_playlist_visible_and_respect_pending_states() {
        let mut picker = PlaylistPicker {
            video_id: "v".into(),
            title: "An evening song".into(),
            selected: 17,
            choices: (0..24)
                .map(|i| crate::source::library::Playlist {
                    id: format!("p{i}"),
                    title: format!("Playlist {i} — evening favourites"),
                    contains: Some(i == 3),
                })
                .collect(),
            loading: false,
            saving: false,
            error: None,
            notice: None,
            request_id: 1,
        };
        for (width, height) in [(24, 10), (48, 18), (100, 36), (160, 42)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            let mut mouse = MouseMap::default();
            terminal
                .draw(|f| playlists(f, &picker, Color::Cyan, &mut mouse))
                .unwrap();
            assert!((0..height).any(|y| {
                (0..width).any(|x| mouse.action_at(x, y) == Some(MouseAction::ChoosePlaylist(17)))
            }));
            super::super::preview_buffer(
                &format!("player-playlists-{width}x{height}"),
                terminal.backend().buffer(),
            );
            picker.saving = true;
            mouse = MouseMap::default();
            terminal
                .draw(|f| playlists(f, &picker, Color::Cyan, &mut mouse))
                .unwrap();
            assert!(!mouse.targets.iter().any(|t| matches!(
                t,
                MouseTarget::Area(
                    _,
                    MouseAction::ChoosePlaylist(_) | MouseAction::RetryPlaylists
                )
            )));
            picker.saving = false;
            picker.error =
                Some("The server did not confirm the save. Refresh before retrying.".into());
            mouse = MouseMap::default();
            terminal
                .draw(|f| playlists(f, &picker, Color::Cyan, &mut mouse))
                .unwrap();
            assert!(
                !mouse
                    .targets
                    .iter()
                    .any(|t| matches!(t, MouseTarget::Area(_, MouseAction::ChoosePlaylist(_))))
            );
            assert!((0..height).any(|y| {
                (0..width).any(|x| mouse.action_at(x, y) == Some(MouseAction::RetryPlaylists))
            }));
            picker.error = None;
            mouse = MouseMap::default();
            terminal
                .draw(|f| {
                    share(
                        f,
                        "https://music.youtube.com/watch?v=abcdefghijk",
                        None,
                        &mut mouse,
                    )
                })
                .unwrap();
            assert!((0..height).any(|y| {
                (0..width).any(|x| mouse.action_at(x, y) == Some(MouseAction::CopyShareLink))
            }));
            super::super::preview_buffer(
                &format!("player-share-{width}x{height}"),
                terminal.backend().buffer(),
            );
        }
    }

    #[test]
    fn narrow_bar_retains_actions_and_wide_bar_has_every_control() {
        let track = Track {
            id: "v".into(),
            title: "Song".into(),
            uploader: "Artist".into(),
            album: None,
            artist_ref: None,
            duration: Some(Duration::from_secs(180)),
        };
        let now = NowPlaying::new(&track);
        for width in [24, 48, 80, 100, 160] {
            let mut terminal = Terminal::new(TestBackend::new(width, 3)).unwrap();
            let mut mouse = MouseMap::default();
            terminal
                .draw(|f| {
                    f.render_widget(
                        Paragraph::new("@".repeat(width as usize)),
                        Rect::new(0, 2, width, 1),
                    );
                    bar(f, &now, f.area(), &mut mouse);
                })
                .unwrap();
            assert!(
                (0..width).all(|x| terminal.backend().buffer()[(x, 2)].symbol() != "@"),
                "old hints leaked into the action row"
            );
            assert_eq!(
                mouse.action_at(width - 2, 2),
                Some(MouseAction::OpenPlayerActions)
            );
            if width >= 100 {
                for action in [
                    MouseAction::LikePlaying,
                    MouseAction::SharePlaying,
                    MouseAction::SavePlaying,
                    MouseAction::ShufflePlayback,
                    MouseAction::RepeatPlayback,
                    MouseAction::ChooseOutput,
                ] {
                    assert!(
                        (0..width).any(|x| mouse.action_at(x, 2) == Some(action)),
                        "missing {action:?}"
                    );
                }
            }
        }
    }
}
