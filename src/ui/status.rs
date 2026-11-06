//! Persistent player strip, footer feedback, and volume controls.

use std::time::Duration;

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph};

use crate::app::{App, CoverSize, Mode, MouseAction, NowPlaying, View};
use crate::player::{PlayState, Snapshot};

use super::{actions, shell};
use super::mouse::{MouseMap, MouseTarget, button_style};
use super::palette::{ambient, highlight};
use super::player::{
    has_player_identity, player_layout, progress_bar_width, progress_line, volume_bar_width,
    volume_line,
};
use super::text::{clock, display_width, marquee, padded, truncate};

/// Widest the volume bar is drawn, however much room the panel has. It is a
/// setting rather than a position, so it is deliberately shorter than the
/// progress bar above it -- two bars of the same length read as two of the
/// same thing.
pub(super) const VOLUME_BAR_WIDTH: usize = 12;

/// Most the status bar gives its right-side key hints. The actual column shrinks
/// to each line, returning every unused cell to the mini-player.
pub(super) const HINTS_WIDTH: u16 = 40;

/// Even a narrow active player keeps enough room to identify itself before the
/// hint line is clipped.
pub(super) const MINI_PLAYER_FLOOR: u16 = 12;

/// Space reserved for `vol`, its bar, and the percentage in the persistent
/// player. It is shown only when the full row can still identify the track.
pub(super) const STATUS_VOLUME_WIDTH: u16 = 30;

pub(super) const MIN_WIDTH_WITH_STATUS_VOLUME: u16 = 80;

/// Three four-cell transport buttons: previous, play/pause, and next.
pub(super) const TRANSPORT_WIDTH: u16 = 12;

/// Named rather than inlined into [`hint_line`] so the test that checks they
/// fit [`HINTS_WIDTH`] measures the strings that are actually drawn.
pub(super) const HINTS_EDITING: &str = "Enter search  Esc cancel";

pub(super) const HINTS_BROWSE: &str = ". song actions  ? help";

pub(super) const HINTS_AWAY: &str = ". song actions  ? help";

pub(super) const HINTS_PLAYING: &str = "Tab panels  . song actions  ? help";

pub(super) fn render_status(frame: &mut Frame, app: &App, area: Rect, mouse: &mut MouseMap) {
    frame.render_widget(Block::default().style(Style::default().bg(shell::SURFACE)), area);
    let area = shell::footer_content(area);
    let show_identity = if app.view != View::Playing { app.now.is_none() } else {
        app.cover_size == CoverSize::Full || app.now.as_ref().is_none_or(|now| {
            let main = shell::regions(frame.area())[1];
            !has_player_identity(&player_layout(main, now, app.cover.as_ref(), app.splits.size(super::Split::PlayerWidth, main.width)))
        })
    };
    let track = app.now.as_ref().map(|now| MiniTrack {
        title: &now.title,
        byline: &now.artist,
        duration: now.duration,
    });
    render_player_strip(
        frame,
        PlayerStrip {
            snap: &app.snapshot(),
            track,
            status: if app.overlay.is_open() || app.menu().is_some() { "" } else { &app.status },
            hint: if app.overlay.is_open() || app.menu().is_some() { "" } else { hint_line(app) },
            accent: ambient(app),
            show_identity,
            keys: (app.mode == Mode::Browse).then_some(app.view),
        },
        area,
        mouse,
    );
    if show_identity && let Some(now) = &app.now {
        register_footer_artist(frame, now, &app.snapshot(), area, mouse);
    }
    if let Some(now) = &app.now { actions::bar(frame, now, area, mouse, (app.mode == Mode::Browse).then_some(app.view)); }
}

pub(super) struct PlayerStrip<'a> {
    pub(super) snap: &'a Snapshot,
    pub(super) track: Option<MiniTrack<'a>>,
    pub(super) status: &'a str,
    pub(super) hint: &'a str,
    pub(super) accent: Color,
    pub(super) show_identity: bool,
    pub(super) keys: Option<View>,
}

pub(super) fn register_footer_artist(frame: &mut Frame, now: &NowPlaying, snap: &Snapshot, area: Rect, mouse: &mut MouseMap) {
    if area.height < 2 || now.artist_ref.is_none() || snap.error.is_some() || snap.state == PlayState::Idle { return; }
    let controls = Rect::new(area.x, area.y + 1, area.width, 1);
    let hint_width = if area.height > 2 { 0 } else { display_width(HINTS_PLAYING) as u16 };
    let centered = area.width >= 40;
    let identity = if centered { centered_control_areas(controls, mouse.show_key_hints())[0] }
        else { status_control_areas(controls, true, hint_width)[0] };
    let start = if centered { 1 } else { usize::from(TRANSPORT_WIDTH) } + display_width(&now.title) + 3;
    let width = display_width(&now.artist);
    if start + width > usize::from(identity.width.saturating_sub(1)) || width == 0 { return; }
    let rect = Rect::new(identity.x + start as u16, identity.y, width as u16, 1);
    frame.render_widget(Paragraph::new(now.artist.clone()).style(Style::default().fg(Color::Gray).add_modifier(Modifier::UNDERLINED)), rect);
    mouse.targets.push(MouseTarget::Area(rect, MouseAction::OpenPlayingArtist));
}

pub(super) fn render_player_strip(
    frame: &mut Frame,
    strip: PlayerStrip<'_>,
    area: Rect,
    mouse: &mut MouseMap,
) {
    if area.is_empty() {
        return;
    }
    frame.render_widget(
        Block::default().style(Style::default().bg(shell::SURFACE)),
        area,
    );
    let PlayerStrip {
        snap,
        track,
        status,
        hint,
        accent,
        show_identity,
        keys,
    } = strip;
    let (progress_area, controls_area, hint_area) = if area.height >= 2 {
        let [progress, controls, hints] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(0),
        ])
        .areas(area);
        (Some(progress), controls, hints)
    } else {
        (None, area, Rect::default())
    };

    if snap.state != PlayState::Idle
        && let Some(progress_area) = progress_area
    {
        let total = track.and_then(|track| track.duration);
        frame.render_widget(
            Paragraph::new(progress_line(
                snap,
                total,
                progress_area.width as usize,
                accent,
            )),
            progress_area,
        );
        if let Some(width) = progress_bar_width(snap, total, progress_area.width as usize) {
            mouse.targets.push(MouseTarget::Progress(Rect {
                width: width as u16,
                ..progress_area
            }));
        }
    }
    if snap.state == PlayState::Idle
        && let Some(progress) = progress_area
    {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "─".repeat(progress.width as usize),
                Style::default().fg(Color::Rgb(38, 38, 38)),
            ))),
            progress,
        );
    }

    if (area.width >= 40 || !show_identity) && snap.state != PlayState::Idle && snap.error.is_none() {
        let [identity, transport, volume] = centered_control_areas(controls_area, mouse.show_key_hints());
        let title = track.map_or(snap.title.as_str(), |track| track.title);
        let artist = track.map_or("", |track| track.byline);
        let room = identity.width.saturating_sub(2) as usize;
        let details = if artist.is_empty() { title.to_owned() } else { format!("{title} · {artist}") };
        let line = if display_width(&details) <= room {
            Line::from(vec![
                Span::raw(" "),
                Span::styled(title.to_owned(), Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
                Span::styled(if artist.is_empty() { String::new() } else { format!(" · {artist}") }, Style::default().fg(Color::Gray)),
            ])
        } else {
            Line::from(Span::styled(format!(" {}", marquee(&details, room)), Style::default().fg(Color::White)))
        };
        if show_identity {
            frame.render_widget(Paragraph::new(line), identity);
        }
        let symbol = match snap.state { PlayState::Paused => crate::navigation::current().play(), PlayState::Buffering => "~", _ => "II" };
        render_centered_transport(frame, symbol, highlight(accent).add_modifier(Modifier::BOLD), transport, keys, mouse);
        if volume.width > 0 {
            render_volume_control(frame, snap.volume, volume);
            register_volume_targets(mouse, volume, snap.volume);
        }
        render_feedback(frame, player_feedback(status, volume.width > 0), hint, hint_area);
        return;
    }

    let [now_playing, volume, hints] = status_control_areas(
        controls_area,
        snap.state != PlayState::Idle,
        if hint_area.height > 0 {
            0
        } else {
            display_width(hint) as u16
        },
    );

    frame.render_widget(
        Paragraph::new(mini_player_line(
            snap,
            track,
            status,
            progress_area.is_none(),
            (now_playing.width as usize).saturating_sub(1),
            accent,
        )),
        now_playing,
    );
    if snap.state != PlayState::Idle && snap.error.is_none() && now_playing.width > 0 {
        register_transport_targets(mouse, now_playing);
    }
    if volume.width > 0 {
        render_volume_control(frame, snap.volume, volume);
        register_volume_targets(mouse, volume, snap.volume);
    }
    let hints = if hint_area.height > 0 {
        hint_area
    } else {
        hints
    };
    render_feedback(frame, if snap.state == PlayState::Idle { "" } else {
        player_feedback(status, volume.width > 0)
    }, hint, hints);
}

/// The volume control already reports successful changes; errors still need feedback.
pub(super) fn player_feedback(status: &str, volume_visible: bool) -> &str {
    let volume_change = matches!(status, "muted" | "muted -- press m or click mut to restore volume")
        || status.strip_prefix("volume ").and_then(|value| value.strip_suffix('%'))
            .and_then(|value| value.parse::<u8>().ok()).is_some_and(|value| value <= 100);
    if volume_visible && volume_change { "" } else { status }
}

/// Feedback owns its row even while transport is active. Narrow windows keep
/// the result of the last action; the menus still expose the shortcut list.
pub(super) fn render_feedback(frame: &mut Frame, status: &str, hint: &str, area: Rect) {
    if area.is_empty() { return; }
    let area = Rect { height: 1, ..area };
    let hint_width = if status.is_empty() { area.width }
        else if area.width >= 90 { (display_width(hint) as u16).min(area.width / 2) } else { 0 };
    let status_width = area.width.saturating_sub(hint_width + u16::from(hint_width > 0 && !status.is_empty()) * 2);
    frame.render_widget(Paragraph::new(truncate(status, status_width as usize))
        .style(Style::default().fg(Color::Gray)), Rect { width: status_width, ..area });
    if hint_width > 0 {
        frame.render_widget(Paragraph::new(truncate(hint, hint_width as usize))
            .style(Style::default().fg(Color::Gray)).alignment(ratatui::layout::Alignment::Right),
            Rect::new(area.right() - hint_width, area.y, hint_width, 1));
    }
}

/// Artwork can dominate the page while transport stays in one predictable place.
pub(super) fn centered_control_areas(area: Rect, show_keys: bool) -> [Rect; 3] {
    let transport_width = if show_keys && area.width >= 110 { 33 }
        else if area.width < TRANSPORT_WIDTH { area.width.min(3) } else { TRANSPORT_WIDTH };
    let left = area.width.saturating_sub(transport_width) / 2;
    let volume_width = if area.width >= MIN_WIDTH_WITH_STATUS_VOLUME {
        STATUS_VOLUME_WIDTH.min(area.width.saturating_sub(left + transport_width))
    } else { 0 };
    [
        Rect::new(area.x, area.y, left, area.height),
        Rect::new(area.x + left, area.y, transport_width, area.height),
        Rect::new(area.right() - volume_width, area.y, volume_width, area.height),
    ]
}

pub(super) fn centered_label(label: &str, width: usize) -> String {
    let left = width.saturating_sub(display_width(label)) / 2;
    padded(&format!("{}{label}", " ".repeat(left)), width)
}

pub(super) fn render_centered_transport(frame: &mut Frame, symbol: &str, toggle_style: Style, area: Rect, keys: Option<View>, mouse: &mut MouseMap) {
    if area.width < 33 {
        frame.render_widget(Paragraph::new(transport_line(symbol, toggle_style, usize::from(area.width))), area);
        register_transport_targets(mouse, area);
        return;
    }
    for (index, label, key, action, style) in [
        (0, "‹‹", (keys == Some(View::Playing)).then_some("p"), MouseAction::PreviousTrack, Style::default().fg(Color::Gray)),
        (1, symbol, keys.map(|_| "Space"), MouseAction::TogglePlayback, toggle_style),
        (2, "››", (keys == Some(View::Playing)).then_some("n"), MouseAction::NextTrack, Style::default().fg(Color::Gray)),
    ] {
        let rect = Rect::new(area.x + index * 11, area.y, 11, area.height);
        let mut line = mouse.hinted_label(label, key, rect.width);
        let padding = usize::from(rect.width).saturating_sub(line.width());
        line.spans.insert(0, Span::raw(" ".repeat(padding / 2)));
        line.spans.push(Span::raw(" ".repeat(padding - padding / 2)));
        frame.render_widget(Paragraph::new(button_style(line, style)), rect);
        mouse.targets.push(MouseTarget::Area(rect, action));
    }
}

pub(super) fn transport_line(symbol: &str, toggle_style: Style, width: usize) -> Line<'static> {
    if width < TRANSPORT_WIDTH as usize {
        return Line::from(Span::styled(centered_label(symbol, width.min(3)), toggle_style));
    }
    Line::from(vec![
        Span::styled(centered_label("‹‹", 4), Style::default().fg(Color::Gray)),
        Span::styled(centered_label(symbol, 4), toggle_style),
        Span::styled(centered_label("››", 4), Style::default().fg(Color::Gray)),
    ])
}

pub(super) fn status_control_areas(area: Rect, active: bool, requested_hint_width: u16) -> [Rect; 3] {
    let volume_width = if active && area.width >= MIN_WIDTH_WITH_STATUS_VOLUME {
        STATUS_VOLUME_WIDTH
    } else {
        0
    };
    let hint_width = requested_hint_width.min(HINTS_WIDTH).min(
        area.width
            .saturating_sub(volume_width)
            .saturating_sub(MINI_PLAYER_FLOOR.min(area.width)),
    );
    Layout::horizontal([
        Constraint::Min(0),
        Constraint::Length(volume_width),
        Constraint::Length(hint_width),
    ])
    .areas(area)
}

pub(super) fn register_transport_targets(mouse: &mut MouseMap, area: Rect) {
    if area.width >= TRANSPORT_WIDTH {
        mouse.targets.extend([
            MouseTarget::Area(
                Rect::new(area.x, area.y, 4, 1),
                MouseAction::PreviousTrack,
            ),
            MouseTarget::Area(
                Rect::new(area.x + 4, area.y, 4, 1),
                MouseAction::TogglePlayback,
            ),
            MouseTarget::Area(
                Rect::new(area.x + 8, area.y, 4, 1),
                MouseAction::NextTrack,
            ),
        ]);
    } else {
        mouse.targets.push(MouseTarget::Area(
            Rect::new(area.x, area.y, area.width.min(3), 1),
            MouseAction::TogglePlayback,
        ));
    }
}

pub(super) fn register_volume_targets(mouse: &mut MouseMap, area: Rect, volume: f32) {
    mouse.targets.push(MouseTarget::VolumeControl(area));
    let inner = volume_control_inner(area);
    if inner != area {
        mouse.targets.push(MouseTarget::Area(Rect::new(area.x, area.y, 3, area.height.min(1)), MouseAction::AdjustVolume(false)));
        mouse.targets.push(MouseTarget::Area(Rect::new(area.right() - 3, area.y, 3, area.height.min(1)), MouseAction::AdjustVolume(true)));
    }
    let area = inner;
    const LABEL_WIDTH: u16 = 4;
    mouse.targets.push(MouseTarget::Area(
        Rect::new(area.x, area.y, LABEL_WIDTH.min(area.width), 1),
        MouseAction::ToggleMute,
    ));
    let bar_width = volume_bar_width(volume, area.width as usize);
    if bar_width >= 4 && area.width > LABEL_WIDTH {
        mouse.targets.push(MouseTarget::Volume(Rect::new(
            area.x + LABEL_WIDTH,
            area.y,
            bar_width as u16,
            1,
        )));
    }
}

pub(super) fn volume_control_inner(area: Rect) -> Rect {
    if area.width >= 18 { Rect::new(area.x + 4, area.y, area.width - 8, area.height) } else { area }
}

pub(super) fn render_volume_control(frame: &mut Frame, volume: f32, area: Rect) {
    let inner = volume_control_inner(area);
    frame.render_widget(Paragraph::new(volume_line(volume, usize::from(inner.width))), inner);
    if inner != area {
        for (x, label) in [(area.x, " - "), (area.right() - 3, " + ")] {
            frame.render_widget(Paragraph::new(label).style(Style::default().fg(Color::White).bg(shell::SURFACE)), Rect::new(x, area.y, 3, area.height.min(1)));
        }
    }
}

/// The key hints name navigation entry points rather than repeating every
/// command. The shared menus carry the full command vocabulary.
///
/// Mode matters as much as view: in the search box every printable key is text,
/// so only Enter and Esc are advertised there.
pub(super) fn hint_line(app: &App) -> &'static str {
    if !app.show_key_hints { "" }
    else if app.mode == Mode::Editing {
        HINTS_EDITING
    } else if app.view == View::Library {
        ". item actions  ? help"
    } else if app.view == View::Playing {
        HINTS_PLAYING
    } else if app.now.is_some() {
        HINTS_AWAY
    } else {
        HINTS_BROWSE
    }
}

#[derive(Clone, Copy)]
pub(super) struct MiniTrack<'a> {
    pub(super) title: &'a str,
    pub(super) byline: &'a str,
    pub(super) duration: Option<Duration>,
}

/// Persistent playback state for the bar along the bottom.
///
/// `progress` is false while the player page already has a clock and scrubber.
/// The helper takes only render data so every width and player state can be
/// tested without opening an audio device through [`App`].
pub(super) fn mini_player_line(
    snap: &Snapshot,
    track: Option<MiniTrack<'_>>,
    idle_status: &str,
    progress: bool,
    width: usize,
    ambient: Color,
) -> Line<'static> {
    if width == 0 {
        return Line::default();
    }

    // An error outranks everything else -- it is the thing the user must see.
    if let Some(err) = &snap.error {
        let err = if snap.state == PlayState::Idle && track.is_some() {
            format!("{err} · r retry · n skip")
        } else {
            err.clone()
        };
        return Line::from(Span::styled(
            format!(" {}", truncate(&err, width.saturating_sub(1))),
            Style::default().fg(Color::Red),
        ));
    }

    let (symbol, colour) = match snap.state {
        PlayState::Idle => ("-", Color::DarkGray),
        PlayState::Buffering => ("~", Color::Yellow),
        PlayState::Playing => ("II", Color::Green),
        PlayState::Paused => (crate::navigation::current().play(), Color::Yellow),
    };

    if snap.state == PlayState::Idle {
        return Line::from(Span::styled(
            format!(" {}", truncate(idle_status, width.saturating_sub(1))),
            Style::default().fg(Color::DarkGray),
        ));
    }

    let mut spans = transport_line(symbol, Style::default().fg(colour), width).spans;
    let mut remaining = width.saturating_sub(
        spans
            .iter()
            .map(|span| display_width(&span.content))
            .sum::<usize>(),
    );

    const BAR_WIDTH: usize = 5;
    const TEXT_FLOOR: usize = 8;
    if progress {
        let total = track
            .and_then(|track| track.duration)
            .filter(|total| !total.is_zero());
        let time = match total {
            Some(total) => format!("{}/{}", clock(snap.position), clock(total)),
            None => clock(snap.position),
        };
        let time_cost = display_width(&time) + 2;
        if remaining >= time_cost + TEXT_FLOOR {
            if let Some(total) =
                total.filter(|_| remaining >= BAR_WIDTH + 1 + time_cost + TEXT_FLOOR)
            {
                let filled = ((snap.position.as_secs_f64() / total.as_secs_f64()).min(1.0)
                    * BAR_WIDTH as f64)
                    .round() as usize;
                spans.extend([
                    Span::styled(
                        "━".repeat(filled),
                        Style::default().fg(ambient).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        "─".repeat(BAR_WIDTH - filled),
                        Style::default().fg(Color::DarkGray),
                    ),
                    Span::raw(" "),
                ]);
                remaining = remaining.saturating_sub(BAR_WIDTH + 1);
            }
            spans.push(Span::styled(
                format!("{time}  "),
                Style::default().fg(Color::Gray),
            ));
            remaining = remaining.saturating_sub(time_cost);
        }
    }

    let title = track.map_or(snap.title.as_str(), |track| track.title);
    let byline = track.map_or("", |track| track.byline);
    let details = if byline.is_empty() {
        title.to_string()
    } else {
        format!("{title} · {byline}")
    };
    if !details.is_empty() && remaining > 0 {
        if !byline.is_empty() && display_width(&details) <= remaining {
            spans.extend([
                Span::styled(
                    truncate(title, remaining),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
                Span::styled(" · ", Style::default().fg(Color::DarkGray)),
                Span::styled(byline.to_string(), Style::default().fg(Color::Gray)),
            ]);
        } else {
            spans.push(Span::styled(
                marquee(&details, remaining),
                Style::default().add_modifier(Modifier::BOLD),
            ));
        }
    }
    Line::from(spans)
}
