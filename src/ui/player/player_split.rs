//! Expanded player: selectable detail panels above independently scrolling lyrics.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::widgets::{Block, Borders};

use crate::app::{App, Tab};
use crate::ui::mouse::{MouseMap, MouseTarget};
use crate::ui::palette::ambient;
use crate::ui::{Split, shell};

use super::lyrics::render_lyrics;

pub(super) fn render(frame: &mut Frame, app: &mut App, area: Rect, mouse: &mut MouseMap) {
    let Some(now) = app.now.as_ref() else { return; };
    if now.tab == Tab::Lyrics && now.lyrics_expanded {
        super::render_panel(frame, app, area, mouse);
        return;
    }
    let tab = if now.tab == Tab::Lyrics { now.detail_tab } else { now.tab };
    // Short windows retain both regions with fewer rows in each.
    let details_height = if area.height >= 12 {
        app.splits.size(Split::PlayerDetails, area.height).unwrap_or(area.height * 2 / 5)
            .clamp(6, area.height - 6)
    } else { (area.height * 2 / 5).max(6).min(area.height.saturating_sub(1)) };
    let [details, lyrics] = Layout::vertical([
        Constraint::Length(details_height),
        Constraint::Min(0),
    ]).areas(area);
    super::render_tabbed_panel(frame, app, details, mouse, tab,
        &[Tab::UpNext, Tab::Related, Tab::Comments]);
    let accent = ambient(app);
    let block = Block::default().borders(Borders::TOP).title(" Lyrics ")
        .border_style(Style::default().fg(Color::DarkGray));
    let inner = shell::panel_inset(block.inner(lyrics));
    frame.render_widget(block, lyrics);
    if area.height > 12 {
        mouse.targets.push(MouseTarget::Divider(Rect { height: 1, ..lyrics }, Split::PlayerDetails, area));
    }
    let [controls, content] = Layout::vertical([
        Constraint::Length(if inner.height >= 8 { 2 } else { u16::from(inner.height >= 4) }),
        Constraint::Min(0),
    ]).areas(inner);
    let snap = app.snapshot();
    let Some(now) = app.now.as_ref() else { return; };
    mouse.targets.push(MouseTarget::PlayerPanel(lyrics, Tab::Lyrics));
    if !controls.is_empty() { super::render_lyrics_controls(frame, now, controls, accent, mouse); }
    if content.is_empty() { return; }
    let (total, following) = render_lyrics(frame, now, &snap, content, content.height >= 12);
    app.clamp_panel(Tab::Lyrics, content.height as usize, total, following);
}
