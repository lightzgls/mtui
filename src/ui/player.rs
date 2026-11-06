//! Now-playing page, identity, transport lines, and panel composition.

use std::time::Duration;

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::app::{App, CardShape, CoverSize, MouseAction, NowPlaying, Panel, PlannedImage, Tab};
use crate::art::ArtCache;
use crate::graphics::Graphics;
use crate::player::{PlayState, Snapshot};
use crate::source::cover::Cover;

use super::{Split, queue, related, shell};
use super::artwork::{cell_pixels, fit_cells};
use super::cards::Tiles;
use super::comments::render_comments;
use super::home::{Hero, render_hero};
use super::lyrics::render_lyrics;
use super::mouse::{MouseMap, MouseTarget, button_style};
use super::palette::ambient;
use super::status::VOLUME_BAR_WIDTH;
use super::text::{clock, display_width, marquee, truncate, wrap};

/// The player page: the cover of what is playing, and beside it the tab the
/// user has open.
///
/// Returns the rect the cover should be centred in, which the caller paints --
/// the picture is not part of ratatui's buffer on the sixel path, so it cannot
/// be drawn from here.
pub(super) fn render_player(
    frame: &mut Frame,
    app: &mut App,
    area: Rect,
    mouse: &mut MouseMap,
) -> Option<Rect> {
    // Nothing is playing, so there is no page. Reachable for one frame after a
    // stop, before the key handler's view change is drawn.
    let now = app.now.as_ref()?;

    // An explicit request for the whole window outranks the layout: `c` means
    // the picture, and the panel beside it is what the user asked to be rid of.
    if app.cover_size == CoverSize::Full {
        return Some(area);
    }

    let layout = player_layout(area, now, app.cover.as_ref(), app.splits.size(Split::PlayerWidth, area.width));
    if let Some(divider) = layout.divider {
        mouse.targets.push(MouseTarget::Divider(divider, Split::PlayerWidth, area));
    }
    let identity = PlayerIdentity {
        now,
        snap: app.snapshot(),
        cover: app.cover.as_ref(),
        accent: ambient(app),
    };
    let images = if app.protocol_images() && !app.overlay.is_open() && app.menu().is_none() {
        Some((&mut app.images, app.graphics))
    } else {
        None
    };
    render_player_identity(frame, identity, &layout, images);
    register_player_identity_targets(mouse, now, &layout);
    if app.cover.is_some() && !app.overlay.is_open() && app.menu().is_none()
        && let Some(hero) = layout.hero
    {
        let height = hero.height.min(3);
        mouse.targets.push(MouseTarget::Cover(Rect::new(
            hero.x, hero.y, (height * 2).min(hero.width), height,
        )));
    }
    super::player_split::render(frame, app, layout.panel, mouse);
    layout.art
}

pub(super) struct PlayerIdentity<'a> {
    pub(super) now: &'a NowPlaying,
    pub(super) snap: Snapshot,
    pub(super) cover: Option<&'a Cover>,
    pub(super) accent: Color,
}

/// Keep the cover and its caption together, even for a short landscape image.
pub(super) fn player_layout(area: Rect, now: &NowPlaying, cover: Option<&Cover>, panel_width: Option<u16>) -> shell::PlayerAreas {
    if now.tab == Tab::Lyrics && now.lyrics_expanded {
        return shell::lyrics_areas(area);
    }
    let mut layout = shell::player_areas(area, panel_width);
    if let (Some(art), Some(info), Some(cover)) = (layout.art, layout.info, cover) {
        let (cols, rows) = fit_cells(cover, (art.width, art.height), cell_pixels());
        if cols > 0 && rows > 0 {
            let art = Rect::new(art.x + (art.width - cols) / 2,
                art.y + (art.height - rows) / 2, cols, rows);
            layout.art = Some(art);
            layout.info = Some(Rect::new(art.x, art.bottom(), art.width, info.height));
        }
    }
    layout
}

pub(super) fn has_player_identity(layout: &shell::PlayerAreas) -> bool {
    layout.info.is_some_and(|area| area.width > 0 && area.height >= 2)
        || layout.hero.is_some_and(|area| area.width > 9 && area.height >= 2)
}

pub(super) fn register_player_identity_targets(mouse: &mut MouseMap, now: &NowPlaying, layout: &shell::PlayerAreas) {
    if let Some(info) = layout.info {
        register_identity_targets(mouse, now, track_info_rows(info)[2]);
    } else if let Some(hero) = layout.hero {
        let cols = (hero.height.min(3) * 2).min(hero.width);
        register_identity_targets(mouse, now, Rect::new(hero.x + cols + 1,
            hero.y + 2, hero.width.saturating_sub(cols + 2), 1));
    }
}

pub(super) fn render_player_identity(
    frame: &mut Frame,
    identity: PlayerIdentity<'_>,
    layout: &shell::PlayerAreas,
    images: Option<(&mut Vec<PlannedImage>, Graphics)>,
) {
    let PlayerIdentity {
        now,
        snap,
        cover,
        accent,
    } = identity;
    if let Some(hero) = layout.hero {
        render_hero(
            frame,
            &Hero {
                title: &now.title,
                artist: &now.byline(),
                art: cover,
                snap: snap.clone(),
            },
            hero,
            images,
        );
    }
    if let Some(divider) = layout.divider {
        frame.render_widget(
            Paragraph::new(
                (0..divider.height)
                    .map(|_| {
                        Line::from(Span::styled(
                            "│",
                            Style::default().fg(Color::from_u32(0x0028_2828)),
                        ))
                    })
                    .collect::<Vec<_>>(),
            ),
            divider,
        );
    }
    if let Some(info) = layout.info {
        render_track_info(frame, now, snap, info, accent);
    }
    if cover.is_none()
        && let Some(art) = layout.art
    {
        message(frame, "Artwork loading…", art);
    }
}

/// Track identity beneath the artwork, without repeating persistent controls.
pub(super) fn render_track_info(
    frame: &mut Frame,
    now: &NowPlaying,
    snap: Snapshot,
    area: Rect,
    accent: Color,
) {
    let width = area.width as usize;
    let lines = [
        Line::from(Span::styled(
            if snap.state == PlayState::Paused {
                "PAUSED"
            } else if snap.state == PlayState::Buffering {
                "LOADING…"
            } else {
                ""
            },
            Style::default().fg(accent).add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            marquee(&now.title, width),
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        )),
        identity_byline(now, width),
    ];
    for (line, row) in lines.into_iter().zip(track_info_rows(area)) {
        frame.render_widget(Paragraph::new(line), row);
    }
}

/// A spare row separates the artwork from its status, title, and byline.
pub(super) fn track_info_rows(area: Rect) -> [Rect; 3] {
    let gap = u16::from(area.height >= 4);
    let offsets = [gap, gap + 1, gap + 2];
    offsets.map(|offset| Rect::new(area.x, area.y + offset.min(area.height), area.width, u16::from(offset < area.height)))
}

pub(super) fn identity_byline(now: &NowPlaying, width: usize) -> Line<'static> {
    let artist = truncate(&now.artist, width);
    let used = display_width(&artist);
    let mut spans = vec![Span::styled(artist, Style::default().fg(Color::Gray)
        .add_modifier(if now.artist_ref.is_some() { Modifier::UNDERLINED } else { Modifier::empty() }))];
    if let Some(album) = &now.album {
        spans.push(Span::styled(truncate(&format!(" • {album}"), width.saturating_sub(used)),
            Style::default().fg(Color::Gray).add_modifier(if now.album_route.is_some() { Modifier::UNDERLINED } else { Modifier::empty() })));
    }
    Line::from(spans)
}

pub(super) fn register_identity_targets(mouse: &mut MouseMap, now: &NowPlaying, area: Rect) {
    let artist_width = (display_width(&now.artist) as u16).min(area.width);
    if now.artist_ref.is_some() && artist_width > 0 {
        mouse.targets.push(MouseTarget::Area(Rect::new(area.x, area.y, artist_width, 1), MouseAction::OpenPlayingArtist));
    }
    if let Some(album) = &now.album {
        let start = artist_width.saturating_add(3);
        let width = (display_width(album) as u16).min(area.width.saturating_sub(start));
        if now.album_route.is_some() && width > 0 {
            mouse.targets.push(MouseTarget::Area(Rect::new(area.x + start, area.y, width, 1), MouseAction::OpenPlayingAlbum));
        }
    }
}

/// The scrubber: a bar, and the clock beside it.
///
/// The length comes from the queue rather than from the player, which knows
/// only how far it has got -- so a track whose length never arrived gets a
/// clock and no bar rather than a bar that would have to lie about the end.
pub(super) fn progress_line<'a>(
    snap: &Snapshot,
    total: Option<Duration>,
    width: usize,
    ambient: Color,
) -> Line<'a> {
    let position = snap.position;
    let clock = match total {
        Some(total) => format!(" {} / {}", clock(position), clock(total)),
        None => format!(" {}", clock(position)),
    };

    let Some(bar_width) = progress_bar_width(snap, total, width) else {
        return Line::from(Span::styled(clock, Style::default().fg(Color::Gray)));
    };
    let total = total.expect("a progress width requires a known duration");

    // Saturating rather than clamped after the fact: a position past the end is
    // an ordinary thing to see for a moment, since the container's length and
    // what the decoder yields never agree to the sample.
    let filled = ((position.as_secs_f64() / total.as_secs_f64()).min(1.0) * bar_width as f64)
        .round() as usize;

    Line::from(vec![
        Span::styled(
            "━".repeat(filled),
            Style::default().fg(ambient).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            "─".repeat(bar_width - filled),
            Style::default().fg(Color::DarkGray),
        ),
        Span::styled(clock, Style::default().fg(Color::Gray)),
    ])
}

/// Width of the clickable part of [`progress_line`]. Keeping this calculation
/// beside the renderer makes the hit target end exactly where the clock begins.
pub(super) fn progress_bar_width(snap: &Snapshot, total: Option<Duration>, width: usize) -> Option<usize> {
    let total = total.filter(|total| !total.is_zero())?;
    let clock = format!(" {} / {}", clock(snap.position), clock(total));
    // `clock` already begins with the separating space, so subtracting another
    // column here would leave every progress row one cell short.
    let bar_width = width.saturating_sub(display_width(&clock));
    (bar_width >= 4).then_some(bar_width)
}

/// The volume, as a bar and the number beside it: `vol ━━━━━━━━────  80%`.
///
/// The bar fills across the supported 0% to 100% volume range.
pub(super) fn volume_line<'a>(volume: f32, width: usize) -> Line<'a> {
    let lead = if volume == 0.0 { "mut " } else { "vol " };
    let label = format!("  {:.0}%", volume * 100.0);

    let bar_width = volume_bar_width(volume, width);
    // Nothing left to draw a bar in, so the number says it on its own -- the
    // part a user actually reads a volume off.
    if bar_width < 4 {
        return Line::from(Span::styled(
            format!("{lead}{}", label.trim_start()),
            Style::default().fg(Color::Gray),
        ));
    }

    let filled = (f64::from(volume.min(1.0)) * bar_width as f64).round() as usize;
    Line::from(vec![
        Span::styled(lead, Style::default().fg(Color::DarkGray)),
        Span::styled("━".repeat(filled), Style::default().fg(Color::Gray)),
        Span::styled(
            "─".repeat(bar_width - filled),
            Style::default().fg(Color::DarkGray),
        ),
        Span::styled(label, Style::default().fg(Color::Gray)),
    ])
}

pub(super) fn volume_bar_width(volume: f32, width: usize) -> usize {
    let label = format!("  {:.0}%", volume * 100.0);
    width
        .saturating_sub(display_width("vol ") + display_width(&label))
        .min(VOLUME_BAR_WIDTH)
}

/// The tabbed panel: the row of tab names, and whichever one is open.
///
/// The panel renderers below draw with a cursor they clamp for themselves, and
/// hand back how long their contents turned out to be; this is where that
/// length is written back to the cursor the keys move. Doing it in that order
/// means a frame is never drawn with a cursor past the end of a panel whose
/// length only became knowable while drawing it.
pub(super) fn render_panel(frame: &mut Frame, app: &mut App, area: Rect, mouse: &mut MouseMap) {
    let Some(tab) = app.now.as_ref().map(|now| now.tab) else { return; };
    render_tabbed_panel(frame, app, area, mouse, tab, &Tab::ALL);
}

pub(super) fn render_tabbed_panel(frame: &mut Frame, app: &mut App, area: Rect, mouse: &mut MouseMap,
    tab: Tab, choices: &[Tab],
) {
    // Both taken before the page is borrowed, since the panels need them and
    // the borrow below is what stops either being asked for down there.
    let snap = app.snapshot();
    let accent = ambient(app);
    let use_images = app.protocol_images() && !app.overlay.is_open() && app.menu().is_none();
    let inner = shell::panel_inset(area);
    if inner.height < 2 {
        return;
    }

    let Some(now) = app.now.as_ref() else {
        return;
    };
    mouse.targets.push(MouseTarget::PlayerPanel(area, tab));
    let [tabs, content] =
        Layout::vertical([Constraint::Length(2), Constraint::Min(0)]).areas(inner);
    render_tab_set(frame, tab, tabs, accent, mouse, choices);
    let content = if tab == Tab::Lyrics && content.height >= 4 {
        let [controls, body] = Layout::vertical([
            Constraint::Length(2), Constraint::Min(0),
        ]).areas(content);
        render_lyrics_controls(frame, now, controls, accent, mouse);
        body
    } else {
        content
    };

    match tab {
        Tab::UpNext => queue::register_targets(mouse, now, content),
        Tab::Related => related::register_targets(mouse, now, content),
        _ => {}
    }

    // Set only by the lyrics panel, and only while it is following the singer:
    // where it scrolled itself to, which becomes the cursor the keys move.
    let (total, following) = if tab == Tab::Related {
        let mut wanted = Vec::new();
        let total = related::render(frame, now, content, accent, &mut Tiles {
            shape: CardShape::Tile, art: &app.art, wanted: &mut wanted,
            images: use_images.then_some((&mut app.images, app.graphics)),
        }, mouse);
        app.want_art(wanted);
        (total, None)
    } else if tab == Tab::Lyrics {
        let minimum_rows = if app.view == crate::app::View::Playing { 12 } else { 5 };
        render_lyrics(frame, now, &snap, content, content.height >= minimum_rows)
    } else {
        render_body_for_tab(frame, now, &snap, content, accent, tab)
    };
    // A list stops at its last row; a wall of text stops one screenful short of
    // its end. Queue item geometry excludes both the heading and spacer rows.
    let viewport = if tab == Tab::UpNext {
        let now = app.now.as_mut().expect("player panel");
        let geometry = queue::Geometry::new(now, content);
        now.queue_offset = geometry.first;
        geometry.viewport
    } else {
        content.height as usize
    };
    app.clamp_panel(tab, viewport, total, following);
}

#[cfg(test)]
pub(super) fn render_panel_body(
    frame: &mut Frame,
    now: &NowPlaying,
    snap: &Snapshot,
    area: Rect,
    accent: Color,
) -> (usize, Option<usize>) {
    render_body_for_tab(frame, now, snap, area, accent, now.tab)
}

fn render_body_for_tab(frame: &mut Frame, now: &NowPlaying, snap: &Snapshot,
    area: Rect, accent: Color, tab: Tab,
) -> (usize, Option<usize>) {
    match tab {
        Tab::UpNext => (queue::render(frame, now, area, accent), None),
        Tab::Lyrics => render_lyrics(frame, now, snap, area, area.height >= 12),
        Tab::Comments => (render_comments(frame, now, area), None),
        Tab::Related => (render_related(frame, now, area, accent), None),
    }
}

pub(super) fn render_lyrics_controls(frame: &mut Frame, now: &NowPlaying, area: Rect, accent: Color, mouse: &mut MouseMap) {
    let timed = matches!(&now.lyrics, Panel::Ready(lyrics) if !lyrics.timed.is_empty());
    let expand = mouse.hinted_label(if now.lyrics_expanded { " Restore " } else { " Expand " }, Some("e"), area.width);
    let expand_width = (expand.width() as u16).min(area.width);
    let expand_area = Rect::new(area.right() - expand_width, area.y, expand_width, 1);
    frame.render_widget(Paragraph::new(expand).style(Style::default().fg(accent).bg(shell::SURFACE)), expand_area);
    mouse.targets.push(MouseTarget::Area(expand_area, MouseAction::ToggleLyricsExpanded));
    let follow = if timed && now.follow_lyrics { " Following " } else if timed { " Follow lyrics " } else if matches!(now.lyrics, Panel::Ready(_)) { " Unsynced lyrics " } else { "" };
    let follow = mouse.hinted_label(follow, timed.then_some("f"), area.width.saturating_sub(expand_width + 1));
    let follow_width = (follow.width() as u16).min(area.width.saturating_sub(expand_width + 1));
    let follow_area = Rect::new(area.x, area.y, follow_width, 1);
    frame.render_widget(Paragraph::new(follow).style(Style::default().fg(if timed { Color::Gray } else { Color::DarkGray })), follow_area);
    if timed && follow_width > 0 {
        mouse.targets.push(MouseTarget::Area(follow_area, MouseAction::ToggleLyricsFollow));
    }
    if let Panel::Ready(lyrics) = &now.lyrics
        && let Some(source) = &lyrics.source
    {
        let x = follow_area.right();
        let width = expand_area.x.saturating_sub(x + 1);
        if width >= 4 {
            let source = truncate(&format!("· {}", source.split_whitespace().collect::<Vec<_>>().join(" ")), usize::from(width));
            frame.render_widget(Paragraph::new(source).style(Style::default().fg(Color::DarkGray)),
                Rect::new(x, area.y, width, 1));
        }
    }
}

/// A subtle filled tab and an underline identify the open panel.
///
/// The fill stays behind the selected name. Its separate underline reads as
/// the edge of the panel without filling the entire bar.
#[cfg(test)]
pub(super) fn render_tabs(frame: &mut Frame, open: Tab, area: Rect, ambient: Color, mouse: &mut MouseMap) {
    render_tab_set(frame, open, area, ambient, mouse, &Tab::ALL);
}

fn render_tab_set(frame: &mut Frame, open: Tab, area: Rect, ambient: Color, mouse: &mut MouseMap, choices: &[Tab]) {
    let mut labels = Vec::new();
    let mut rule = Vec::new();
    let roomy_width = choices
        .iter()
        .map(|tab| display_width(tab.label()) + 2)
        .sum::<usize>();
    let compact_width = choices
        .iter()
        .map(|tab| display_width(tab.label()) + 1)
        .sum::<usize>();
    let roomy = roomy_width <= area.width as usize;
    let show_tab_keys = mouse.show_key_hints() && roomy_width + 8 <= area.width as usize;
    let tiny = compact_width > area.width as usize;
    let initials = area.width >= 12;
    let numbered_padding = if area.width >= 8 { " " } else { "" };

    let mut x = area.x;
    for &tab in choices {
        let index = tab.index();
        let selected = tab == open;
        let style = if selected {
            Style::default()
                .fg(ambient)
                .bg(shell::SURFACE)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        // At the panel's minimum width, one leading cell per tab keeps all four
        // names visible. Extremely narrow windows retain every destination as
        // its number and initial rather than silently clipping whole tabs.
        let label = if tiny {
            if !mouse.show_key_hints() {
                tab.label().chars().next().unwrap_or('?').to_string()
            } else if initials {
                format!("{}{}", index + 1, tab.label().chars().next().unwrap_or('?'))
            } else {
                (index + 1).to_string()
            }
        } else {
            tab.label().to_string()
        };
        let right = if roomy { " " } else { "" };
        let left = if tiny { numbered_padding } else { " " };
        let key = (index + 1).to_string();
        let mut rendered = mouse.hinted_label(&format!("{left}{label}{right}"),
            show_tab_keys.then_some(key.as_str()), area.width);
        let rendered_width = rendered.width() as u16;
        mouse.targets.push(MouseTarget::Area(
            Rect::new(x, area.y, rendered_width, area.height),
            MouseAction::OpenTab(tab),
        ));
        x = x.saturating_add(rendered_width);
        rendered = button_style(rendered, style);
        labels.extend(rendered.spans);

        // Under the letters rather than under the padding, so the rule reads as
        // belonging to the name above it rather than to the gap beside it.
        let width = usize::from(rendered_width).saturating_sub(left.len() + right.len());
        rule.push(Span::styled(
            format!(
                "{left}{}{right}",
                if selected { "─" } else { " " }.repeat(width)
            ),
            Style::default().fg(ambient),
        ));
    }

    frame.render_widget(
        Paragraph::new(vec![Line::from(labels), Line::from(rule)]),
        area,
    );
}

/// The Related tab: shelves of recommendations, flattened into one list.
/// Returns its length in rows.
pub(super) fn render_related(frame: &mut Frame, now: &NowPlaying, area: Rect, ambient: Color) -> usize {
    related::render(frame, now, area, ambient, &mut Tiles {
        shape: CardShape::Tile, art: &ArtCache::default(), wanted: &mut Vec::new(), images: None,
    }, &mut MouseMap::default())
}

/// The panel's contents when it has none: what it is waiting for, or why there
/// will never be any.
pub(super) fn message(frame: &mut Frame, text: &str, area: Rect) {
    let lines: Vec<Line> = wrap(text, (area.width as usize).saturating_sub(2))
        .into_iter()
        .map(|line| {
            Line::from(Span::styled(
                format!(" {line}"),
                Style::default().fg(Color::DarkGray),
            ))
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), area);
}

/// What an empty panel should say for itself.
pub(super) fn waiting_on<T>(panel: &Panel<T>) -> &str {
    match panel {
        // Idle is a fetch that has not started because the watch response has
        // not landed yet, which from here is indistinguishable from loading --
        // and is about to become it.
        Panel::Idle | Panel::Loading => "loading ...",
        Panel::Empty(why) => why,
        Panel::Ready(_) => "",
    }
}
