//! A simple list of recorded plays, with the newest first.

use chrono::{DateTime, Datelike, Local, Timelike};
use ratatui::{Frame, layout::Rect, style::{Color, Style}, text::{Line, Span}, widgets::Paragraph};

use crate::app::{App, MouseAction};
use crate::ui::{shell, mouse::{MouseMap, MouseTarget}, palette::{ambient, highlight}, text::truncate};

fn timestamp(unix: u64) -> String {
    let Some(utc) = i64::try_from(unix).ok().and_then(|seconds| DateTime::from_timestamp(seconds, 0)) else {
        return "Unknown time".into();
    };
    let local = utc.with_timezone(&Local);
    format!("{:04}-{:02}-{:02} {:02}:{:02}", local.year(), local.month(), local.day(), local.hour(), local.minute())
}

pub(in crate::ui) fn render(frame: &mut Frame, app: &mut App, area: Rect, mouse: &mut MouseMap) {
    let area = shell::inset(area);
    if area.is_empty() { return; }
    let accent = ambient(app);
    let loading = app.page_loading();
    let history = &mut app.listening_history;
    frame.render_widget(Paragraph::new("Listening history").style(Style::default().fg(accent)),
        Rect::new(area.x, area.y, area.width, 1));
    let rows = Rect::new(area.x, area.y + 1, area.width, area.height.saturating_sub(1));
    if rows.is_empty() { return; }
    if loading || history.error.is_some() || history.rows.is_empty() {
        let message = if loading { "Loading listening history…" }
            else { history.error.as_deref().unwrap_or("No completed plays recorded yet.") };
        frame.render_widget(Paragraph::new(message).wrap(ratatui::widgets::Wrap { trim: true }), rows);
        return;
    }
    // Keep date and song visible in narrow panes by putting them on separate lines.
    let wide = rows.width >= 55;
    let height = if wide || rows.height < 2 { 1 } else { 2 };
    let visible = usize::from(rows.height / height);
    history.viewport(visible);
    for (slot, play) in history.rows.iter().skip(history.offset).take(visible).enumerate() {
        let index = history.offset + slot;
        let row = Rect::new(rows.x, rows.y + slot as u16 * height, rows.width, height);
        let selected = index == history.selected;
        let song = if play.artist.is_empty() { play.title.clone() } else { format!("{} · {}", play.title, play.artist) };
        let date = timestamp(play.at);
        let style = if selected { highlight(accent) } else { Style::default() };
        let date_style = if selected { style } else { Style::default().fg(Color::Gray) };
        let lines = if wide {
            vec![Line::from(vec![Span::styled(format!("{date:<16}  "), date_style),
                Span::raw(truncate(&song, row.width.saturating_sub(18) as usize))])]
        } else {
            vec![Line::from(truncate(&song, row.width as usize)),
                Line::from(Span::styled(truncate(&date, row.width as usize), date_style))]
        };
        frame.render_widget(Paragraph::new(lines).style(style), row);
        mouse.targets.push(MouseTarget::Area(row, MouseAction::PlayHistoryTrack(index)));
    }
}
