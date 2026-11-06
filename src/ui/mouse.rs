//! Mouse hit testing and continuous control positions.

use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use crate::app::{MouseAction, POINTER_SCALE};

use super::shell;
use super::text::{display_width, truncate};

#[derive(Default)]
pub struct MouseMap {
    pub(super) targets: Vec<MouseTarget>,
    pub(super) key_hints_hidden: bool,
}

#[derive(Clone, Copy)]
pub(super) enum MouseTarget {
    Area(Rect, MouseAction),
    TrackRows { area: Rect, first: usize },
    Progress(Rect),
    Volume(Rect),
    VolumeControl(Rect),
    Cover(Rect),
    PlayerPanel(Rect, crate::app::Tab),
    Divider(Rect, super::Split, Rect),
}

pub(super) fn hinted_label(label: &str, key: Option<&str>, budget: u16, show: bool) -> Line<'static> {
    let trailing = label.len() - label.trim_end().len();
    let body = label.trim_end();
    let key = key.filter(|key| show
        && display_width(body) + display_width(key) + 1 + trailing <= usize::from(budget));
    let mut spans = vec![Span::raw(truncate(body, usize::from(budget).saturating_sub(trailing)))];
    if let Some(key) = key {
        spans.push(Span::styled(format!(" {key}"), Style::default().fg(Color::DarkGray)));
    }
    spans.push(Span::raw(" ".repeat(trailing.min(usize::from(budget)))));
    Line::from(spans)
}

pub(super) fn button_style(mut line: Line<'static>, style: Style) -> Line<'static> {
    for span in &mut line.spans {
        let hint = span.style.fg == Some(Color::DarkGray);
        span.style = style.patch(span.style);
        if hint && style.bg.is_some_and(|bg| bg != shell::SURFACE) {
            span.style.fg = style.fg;
            span.style = span.style.remove_modifier(Modifier::BOLD);
        }
    }
    line
}

impl MouseMap {
    pub(crate) fn divider_at(&self, x: u16, y: u16) -> Option<(super::Split, Rect)> {
        self.targets.iter().rev().find_map(|target| match *target {
            MouseTarget::Divider(area, split, bounds) if contains(area, x, y) => Some((split, bounds)),
            _ => None,
        })
    }
    pub(crate) fn player_panel_at(&self, column: u16, row: u16) -> Option<crate::app::Tab> {
        self.targets.iter().rev().find_map(|target| match *target {
            MouseTarget::Area(area, MouseAction::IgnoreOverlay) if contains(area, column, row) => Some(None),
            MouseTarget::PlayerPanel(area, tab) if contains(area, column, row) => Some(Some(tab)),
            _ => None,
        }).flatten()
    }
    pub(super) fn show_key_hints(&self) -> bool { !self.key_hints_hidden }

    /// Keep shortcuts quiet and drop the whole hint before shortening a label.
    pub(super) fn hinted_label(&self, label: &str, key: Option<&str>, budget: u16) -> Line<'static> {
        hinted_label(label, key, budget, self.show_key_hints())
    }

    pub fn over_cover(&self, column: u16, row: u16) -> bool {
        self.targets.iter().rev().find_map(|target| match *target {
            MouseTarget::Cover(area) if contains(area, column, row) => Some(true),
            MouseTarget::Area(area, _) | MouseTarget::Progress(area)
                | MouseTarget::Volume(area) | MouseTarget::VolumeControl(area)
                | MouseTarget::TrackRows { area, .. }
                if contains(area, column, row) => Some(false),
            _ => None,
        }).unwrap_or(false)
    }

    pub fn over_volume(&self, column: u16, row: u16) -> bool {
        self.targets.iter().rev().find_map(|target| match *target {
            MouseTarget::Area(area, action) if contains(area, column, row) => Some(matches!(action, MouseAction::AdjustVolume(_) | MouseAction::ToggleMute)),
            MouseTarget::Volume(area) | MouseTarget::VolumeControl(area) if contains(area, column, row) => Some(true),
            MouseTarget::Progress(area) | MouseTarget::Cover(area) | MouseTarget::TrackRows { area, .. } if contains(area, column, row) => Some(false),
            _ => None,
        }).unwrap_or(false)
    }

    pub fn action_at(&self, column: u16, row: u16) -> Option<MouseAction> {
        self.targets.iter().rev().find_map(|target| match *target {
            MouseTarget::Area(area, action) if contains(area, column, row) => Some(action),
            MouseTarget::TrackRows { area, first } if contains(area, column, row) => Some(
                MouseAction::PlayTrack(first + usize::from(row.saturating_sub(area.y))),
            ),
            MouseTarget::Progress(area) if contains(area, column, row) => {
                Some(MouseAction::SeekTo(horizontal_position(area, column)))
            }
            MouseTarget::Volume(area) if contains(area, column, row) => {
                Some(MouseAction::SetVolume(horizontal_position(area, column)))
            }
            MouseTarget::Cover(area) if contains(area, column, row) => Some(MouseAction::TogglePlayback),
            _ => None,
        })
    }

    /// Dragging adjusts continuous controls only. Crossing a button or row
    /// while scrubbing must not start a track or repeatedly toggle playback.
    pub fn drag_action_at(&self, column: u16, row: u16) -> Option<MouseAction> {
        self.action_at(column, row).filter(|action| {
            matches!(action, MouseAction::SeekTo(_) | MouseAction::SetVolume(_))
        })
    }

    /// The full queue row is a drop destination, including its artist link.
    pub fn queue_row_at(&self, column: u16, row: u16) -> Option<usize> {
        self.targets.iter().rev().find_map(|target| match *target {
            MouseTarget::Area(area, MouseAction::SelectPageRow(index) | MouseAction::OpenPageRow(index))
                if contains(area, column, row) => Some(index),
            _ => None,
        })
    }

    /// Selects the item under a context click without activating it. The
    /// following `OpenPageActions` therefore describes the row the user
    /// actually clicked rather than whichever row the keyboard last selected.
    pub fn context_action_at(&self, column: u16, row: u16) -> Option<MouseAction> {
        self.targets.iter().rev().find_map(|target| match *target {
            MouseTarget::Area(area, MouseAction::OpenArtistSong(index)) if contains(area, column, row) => Some(MouseAction::SelectArtistSong(index)),
            MouseTarget::Area(area, MouseAction::OpenLibraryItem(index)) if contains(area, column, row) => Some(MouseAction::SelectLibraryItem(index)),
            MouseTarget::Area(area, MouseAction::OpenArtistCard { shelf, card }) if contains(area, column, row) => Some(MouseAction::SelectArtistCard { shelf, card }),
            MouseTarget::Area(area, MouseAction::OpenHomeCard { shelf, card })
                if contains(area, column, row) =>
            {
                Some(MouseAction::SelectHomeCard { shelf, card })
            }
            MouseTarget::TrackRows { area, first } if contains(area, column, row) => Some(
                MouseAction::SelectTrack(first + usize::from(row.saturating_sub(area.y))),
            ),
            MouseTarget::Area(area, MouseAction::PlayTrack(index)) if contains(area, column, row) => Some(MouseAction::SelectTrack(index)),
            MouseTarget::Area(area, MouseAction::OpenPageRow(index) | MouseAction::SelectPageRow(index)) if contains(area, column, row) => Some(MouseAction::SelectPageRow(index)),
            _ => None,
        })
    }
}

pub(super) fn horizontal_position(area: Rect, column: u16) -> u16 {
    if area.width <= 1 {
        return 0;
    }
    let offset = column.saturating_sub(area.x).min(area.width - 1);
    ((u32::from(offset) * u32::from(POINTER_SCALE)) / u32::from(area.width - 1)) as u16
}

pub(super) fn contains(area: Rect, column: u16, row: u16) -> bool {
    column >= area.x
        && column < area.x.saturating_add(area.width)
        && row >= area.y
        && row < area.y.saturating_add(area.height)
}
