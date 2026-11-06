//! Message and sign-in overlays.

use std::time::Duration;

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph};

use crate::app::{App, Overlay, SignIn};

use super::{actions, preferences};
use super::mouse::MouseMap;
use super::text::{centred, display_width, wrap};

/// Floor on the sign-in panel's width. Wide enough that the footer hint reads
/// as one line, whatever the URL beside it happens to measure.
pub(super) const SIGN_IN_MIN_WIDTH: u16 = 48;

/// Ceiling on the width of wrapped error text. Only the URL is allowed to push
/// the panel past this, and only because it cannot be wrapped.
pub(super) const SIGN_IN_MAX_WIDTH: u16 = 70;

/// Draws whichever modal is up, centred over the whole window.
pub(super) fn render_overlay(frame: &mut Frame, app: &App, mouse: &mut MouseMap) {
    match &app.overlay {
        Overlay::None => {}
        Overlay::SignIn(phase) => render_sign_in(frame, phase),
        Overlay::Message { body } => render_message(frame, body),
        Overlay::Settings => preferences::render(frame, app, mouse),
        Overlay::Share { .. } | Overlay::SavePlaylist(_) => actions::overlay(frame, app, mouse),
    }
}

/// A multi-line message, sized to its own content.
///
/// Laid out from the text rather than to a fixed box so long diagnostic lines
/// remain readable.
pub(super) fn render_message(frame: &mut Frame, body: &str) {
    let widest = body.lines().map(display_width).max().unwrap_or(0);
    let area = centred(
        frame.area(),
        (widest as u16).saturating_add(4),
        (body.lines().count() as u16).saturating_add(3),
    );

    let block = Block::bordered()
        .border_style(Style::default().fg(Color::Yellow))
        .title(" mtui ");
    let inner = block.inner(area);
    frame.render_widget(Clear, area);
    frame.render_widget(block, area);

    let mut lines: Vec<Line> = body
        .lines()
        .map(|line| Line::from(Span::raw(format!(" {line}"))))
        .collect();
    lines.push(Line::from(Span::styled(
        " Esc to dismiss",
        Style::default().fg(Color::DarkGray),
    )));

    frame.render_widget(Paragraph::new(lines), inner);
}

/// The sign-in panel, in whichever phase the flow has reached.
pub(super) fn render_sign_in(frame: &mut Frame, phase: &SignIn) {
    match phase {
        SignIn::Failed { reason } => render_sign_in_failed(frame, reason),
        SignIn::Music {
            started,
            recovering,
        } => render_music_sign_in(frame, started.elapsed(), *recovering),
    }
}

pub(super) fn render_music_sign_in(frame: &mut Frame, elapsed: Duration, recovering: bool) {
    let inner = sign_in_panel(frame, SIGN_IN_MIN_WIDTH, 8, Color::Cyan);
    let (action, waiting, privacy) = if recovering {
        (
            "  Renewing your saved YouTube Music session.",
            "  Checking the saved browser session",
            "  Esc hides this. Sign-in opens only if needed.",
        )
    } else {
        (
            "  Complete sign-in in the YouTube Music window.",
            "  Waiting for a Music session",
            "  MTUI never receives your password. Esc hides this panel.",
        )
    };
    let lines = vec![
        Line::from(""),
        Line::from(Span::styled(action, Style::default().fg(Color::White))),
        Line::from(Span::styled(
            format!("{waiting}{}", ellipsis(elapsed)),
            Style::default().fg(Color::Yellow),
        )),
        Line::from(""),
        Line::from(Span::styled(privacy, Style::default().fg(Color::DarkGray))),
    ];
    frame.render_widget(Paragraph::new(lines), inner);
}

/// Border, title and backdrop shared by all three phases.
///
/// Shared so the panel keeps one identity as the flow moves through them: a
/// window that changes size and colour under the user looks like a new window,
/// and this one is asking them to keep reading it.
pub(super) fn sign_in_panel(frame: &mut Frame, width: u16, height: u16, accent: Color) -> Rect {
    let area = centred(frame.area(), width, height);
    let block = Block::bordered()
        .border_style(Style::default().fg(accent))
        .title(" YouTube Music session ");
    let inner = block.inner(area);
    // Blanks the cells first: a modal over a list has to hide it, and ratatui
    // draws widgets onto whatever is already in the buffer.
    frame.render_widget(Clear, area);
    frame.render_widget(block, area);
    inner
}

/// The flow ended without a session, and says why.
///
/// Red rather than cyan: every other phase is an instruction to follow, and
/// this is the one that is not. Retrying is offered here because the thread
/// that was polling is gone -- nothing else on screen will ever change.
pub(super) fn render_sign_in_failed(frame: &mut Frame, reason: &str) {
    // Capped rather than sized to the reason: these run to a sentence and a
    // half, and a panel stretched to hold one on a single line is both harder
    // to read and no longer recognisably the panel the other phases drew.
    let width = (display_width(reason) as u16)
        .saturating_add(6)
        .clamp(SIGN_IN_MIN_WIDTH, SIGN_IN_MAX_WIDTH)
        .min(frame.area().width);
    // The reason is a sentence from Google, not a fixed string, so it is wrapped
    // to the panel rather than assumed to fit one row of it.
    let body = wrap(reason, width.saturating_sub(4) as usize);
    // The reason, a blank row either side of it, the retry hint, two borders.
    let inner = sign_in_panel(frame, width, body.len() as u16 + 5, Color::Red);

    let mut lines = vec![Line::from("")];
    lines.extend(body.into_iter().map(|line| {
        Line::from(Span::styled(
            format!("  {line}"),
            Style::default().fg(Color::Red),
        ))
    }));
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "  M retry sign-in   Esc dismiss",
        Style::default().fg(Color::DarkGray),
    )));

    frame.render_widget(Paragraph::new(lines), inner);
}

/// A three-state ellipsis, so a panel that is waiting on a person rather than on
/// the network still shows the program is alive.
///
/// Driven from a duration the caller already holds rather than from the clock,
/// which keeps it a pure function and the panel testable.
pub(super) fn ellipsis(elapsed: Duration) -> &'static str {
    match (elapsed.as_millis() / 400) % 3 {
        0 => ".",
        1 => "..",
        _ => "...",
    }
}
