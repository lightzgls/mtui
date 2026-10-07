//! Shared, quiet modal chrome and modal-owned mouse regions.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph};

use super::{MouseMap, MouseTarget, centred, shell, truncate};
use crate::app::MouseAction;

pub(super) struct Dialog {
    pub body: Rect,
    pub footer: Rect,
}

pub(super) struct Chrome<'a> {
    pub title: &'a str,
    pub width: u16,
    pub height: u16,
    pub close_label: &'a str,
    pub close: MouseAction,
    pub outside: MouseAction,
}

pub(super) fn render(frame: &mut Frame, chrome: Chrome<'_>, mouse: &mut MouseMap) -> Dialog {
    for cell in &mut frame.buffer_mut().content {
        cell.set_style(
            Style::default()
                .fg(Color::Rgb(55, 55, 55))
                .bg(shell::BACKGROUND),
        );
    }
    let area = centred(frame.area(), chrome.width, chrome.height);
    mouse
        .targets
        .push(MouseTarget::Area(frame.area(), chrome.outside));
    mouse
        .targets
        .push(MouseTarget::Area(area, MouseAction::IgnoreOverlay));
    let block = Block::bordered()
        .border_style(Style::default().fg(Color::Rgb(58, 58, 58)))
        .style(Style::default().bg(shell::SURFACE).fg(Color::White));
    let inner = block.inner(area);
    frame.render_widget(Clear, area);
    frame.render_widget(block, area);
    let [header, body, footer] = Layout::vertical([
        Constraint::Length(if inner.height >= 5 { 2 } else { 1 }),
        Constraint::Min(0),
        Constraint::Length(if inner.height >= 5 { 2 } else { 0 }),
    ])
    .areas(inner);
    let header = Rect {
        height: header.height.min(1),
        ..header
    };
    let [title, close] = Layout::horizontal([
        Constraint::Min(0),
        Constraint::Length(if inner.width >= 16 { 7 } else { 0 }),
    ])
    .areas(header);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            format!(
                " {}",
                truncate(chrome.title, title.width.saturating_sub(1) as usize)
            ),
            Style::default().add_modifier(Modifier::BOLD),
        ))),
        title,
    );
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            chrome.close_label,
            Style::default().fg(Color::Gray),
        ))),
        close,
    );
    if !close.is_empty() {
        mouse.targets.push(MouseTarget::Area(close, chrome.close));
    }
    Dialog {
        body,
        footer: Rect::new(
            footer.x,
            footer.bottom().saturating_sub(1),
            footer.width,
            footer.height.min(1),
        ),
    }
}
