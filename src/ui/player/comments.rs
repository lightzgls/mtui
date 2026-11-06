//! Comment panel rendering.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::app::{NowPlaying, Panel, Tab};
use crate::ui::text::{truncate, wrap};

use super::{message, waiting_on};

/// The comment section, as a scrolling column of blocks. Returns its height in
/// lines.
pub(in crate::ui) fn render_comments(frame: &mut Frame, now: &NowPlaying, area: Rect) -> usize {
    let width = (area.width as usize).saturating_sub(2);
    let Panel::Ready(comments) = &now.comments else {
        message(frame, waiting_on(&now.comments), area);
        return 0;
    };

    let mut lines: Vec<Line> = Vec::new();
    if !comments.total.is_empty() {
        lines.push(Line::from(Span::styled(
            format!(" {}", truncate(&comments.total, width)),
            Style::default().add_modifier(Modifier::BOLD),
        )));
        lines.push(Line::from(""));
    }

    for comment in &comments.items {
        lines.push(Line::from(vec![
            Span::styled(
                format!(" {}", truncate(&comment.author, width.saturating_sub(1))),
                Style::default().fg(Color::Cyan),
            ),
            Span::styled(
                format!("  {}", comment.published),
                Style::default().fg(Color::DarkGray),
            ),
        ]));
        lines.extend(
            wrap(&comment.text, width)
                .into_iter()
                .map(|line| Line::from(Span::raw(format!(" {line}")))),
        );

        // Only when YouTube gave us either; a comment with no likes and no
        // replies is better off without a row of zeroes under it.
        let mut footer = Vec::new();
        if !comment.likes.is_empty() {
            footer.push(format!("{} likes", comment.likes));
        }
        if !comment.replies.is_empty() {
            footer.push(format!("{} replies", comment.replies));
        }
        if !footer.is_empty() {
            lines.push(Line::from(Span::styled(
                format!(" {}", footer.join("   ")),
                Style::default().fg(Color::DarkGray),
            )));
        }
        lines.push(Line::from(""));
    }

    let viewport = area.height as usize;
    let total = lines.len();
    let offset = now.cursor[Tab::Comments.index()].min(total.saturating_sub(viewport));

    frame.render_widget(
        Paragraph::new(
            lines
                .into_iter()
                .skip(offset)
                .take(viewport)
                .collect::<Vec<_>>(),
        ),
        area,
    );
    total
}
