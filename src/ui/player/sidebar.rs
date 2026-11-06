//! Persistent current-track column beside every browsing page.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::app::{App, MouseAction};
use crate::ui::mouse::{MouseMap, MouseTarget};
use crate::ui::palette::ambient;
use crate::ui::{Split, Splits, shell};

/// Keep the column even in small terminals; only an absent track removes it.
pub(in crate::ui) fn areas(area: Rect, has_track: bool, splits: &Splits) -> (Rect, Option<Rect>) {
    if !has_track || area.is_empty() {
        return (area, None);
    }
    let limit = if area.width < 72 { area.width.div_ceil(3) } else { area.width.div_ceil(2) };
    let default = (area.width * 2 / 5).clamp(28, 48).min(limit);
    let width = if area.width >= 40 {
        splits.size(Split::SidebarWidth, area.width)
            .map_or(default, |width| width.clamp(14, area.width - 24))
    } else { default };
    let [content, sidebar] = Layout::horizontal([
        Constraint::Min(0), Constraint::Length(width),
    ]).areas(area);
    (content, Some(sidebar))
}

/// Render identity and tabs, returning the cover box for the shared image pass.
pub(in crate::ui) fn render(frame: &mut Frame, app: &mut App, area: Rect, main: Rect, mouse: &mut MouseMap) -> Option<Rect> {
    let now = app.now.as_ref()?;
    let block = Block::default().borders(Borders::LEFT)
        .border_style(Style::default().fg(Color::DarkGray));
    let column = block.inner(area);
    let inner = shell::panel_inset(column);
    frame.render_widget(block, area);
    if main.width >= 40 {
        let divider = Rect { width: 1, ..area };
        mouse.targets.push(MouseTarget::Divider(divider, Split::SidebarWidth, main));
    }
    let gap = u16::from(inner.height >= 12);
    let max_cover = inner.height.saturating_sub(11 + gap);
    let art_bounds = Rect { y: inner.y + inner.height.min(6), height: inner.height.saturating_sub(6), ..inner };
    let cover_height = app.splits.size(Split::SidebarArtwork, art_bounds.height)
        .unwrap_or(inner.height / 3).clamp(max_cover.min(2), max_cover);
    let [header, art, info, divider, panel] = Layout::vertical([
        Constraint::Length(2), Constraint::Length(cover_height),
        Constraint::Length(4), Constraint::Length(gap), Constraint::Min(0),
    ]).areas(inner);
    let [title, expand] = Layout::horizontal([
        Constraint::Min(0), Constraint::Length(if header.width >= 24 { 8 } else { 3 }),
    ]).areas(Rect { height: header.height.min(1), ..header });
    let accent = ambient(app);
    frame.render_widget(Paragraph::new("NOW PLAYING")
        .style(Style::default().fg(accent).add_modifier(Modifier::BOLD)), title);
    frame.render_widget(Paragraph::new(if expand.width >= 8 { "Expand >" } else { " > " })
        .style(Style::default().fg(accent)), expand);
    mouse.targets.push(MouseTarget::Area(expand, MouseAction::OpenPlayer));
    super::render_track_info(frame, now, app.snapshot(), info, accent);
    super::register_identity_targets(mouse, now, super::track_info_rows(info)[2]);
    if max_cover > 2 && !divider.is_empty() {
        let rule = "─".repeat(usize::from(divider.width));
        frame.render_widget(Paragraph::new(rule).style(Style::default().fg(Color::DarkGray)), divider);
        mouse.targets.push(MouseTarget::Divider(divider, Split::SidebarArtwork, art_bounds));
    }
    if app.cover.is_none() && !art.is_empty() {
        super::message(frame, "Artwork loading…", art);
    }
    // Keyboard shortcuts still belong to the browsing page. Sidebar tabs are
    // mouse controls until the expanded player is opened.
    let hidden = mouse.key_hints_hidden;
    mouse.key_hints_hidden = true;
    super::render_panel(frame, app, Rect { y: panel.y, height: panel.height, ..column }, mouse);
    mouse.key_hints_hidden = hidden;
    (!art.is_empty()).then_some(art)
}
