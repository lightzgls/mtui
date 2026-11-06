//! Artist page state, navigation, and song selection.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{List, ListItem, Paragraph};

use crate::app::{App, CardShape, CoverSize, ImageSource, MouseAction, Panel, PlannedImage};
use crate::terminal::graphics::Graphics;
use crate::source::catalog::artist::{ArtistPage, ArtistSong};
use crate::source::cover::Cover;
use crate::source::catalog::home::Shelf;
use crate::ui::{paging, shell};
use crate::ui::artwork::render_cover;
use crate::ui::cards::{ShelfCursor, Tiles, card_size, plan_tile_image, render_shelf, shelf_card_rect};
use crate::ui::mouse::{MouseMap, MouseTarget};
use crate::ui::palette::{ambient, highlight};
use crate::ui::player::waiting_on;
use crate::ui::text::{bounded_wrap, cell, clock, display_width, focused_cell, truncate};

use super::results::{ALBUM_WIDTH, DURATION_WIDTH};

pub(in crate::ui) const ARTIST_TOP_SONG_ROWS: u16 = 5;

pub(in crate::ui) fn render_artist(frame: &mut Frame, app: &mut App, area: Rect, mouse: &mut MouseMap) {
    let inner = shell::inset(area);

    let accent = ambient(app);
    let use_images = app.protocol_images() && !app.overlay.is_open() && app.menu().is_none();
    let graphics = app.graphics;
    let art = &app.art;
    let images = &mut app.images;
    let mut wanted = Vec::new();
    {
        let Some(view) = app.artist.as_mut() else {
            frame.render_widget(
                Paragraph::new(Span::styled(
                    "artist unavailable",
                    Style::default().fg(Color::DarkGray),
                )),
                inner,
            );
            return;
        };
        let page = match &view.page {
            Panel::Ready(page) => page,
            Panel::Empty(reason) => {
                let message = format!("{reason} -- press r to try again");
                frame.render_widget(
                    Paragraph::new(Span::styled(message, Style::default().fg(Color::DarkGray))),
                    inner,
                );
                return;
            }
            panel => {
                frame.render_widget(
                    Paragraph::new(Span::styled(
                        waiting_on(panel),
                        Style::default().fg(Color::DarkGray),
                    )),
                    inner,
                );
                return;
            }
        };

        let header_height = artist_header_height(inner.height);
        let [header, content] =
            Layout::vertical([Constraint::Length(header_height), Constraint::Min(0)]).areas(inner);
        let header_art = if artist_header_art_width(header, page.art.is_some()) > 0 {
            page.art.as_ref().and_then(|url| {
                let key = page.art_key();
                wanted.push((key.clone(), Some(url.clone())));
                art.get(&key)
            })
        } else {
            None
        };
        render_artist_header(
            frame,
            page,
            header_art,
            header,
            accent,
            use_images.then_some((&mut *images, graphics)),
        );

        let has_songs = !page.top_songs.is_empty();
        let sections = page.shelves.len() + usize::from(has_songs);
        if sections == 0 || content.is_empty() {
            return;
        }
        view.section = view.section.min(sections - 1);
        view.top = view.top.min(view.section);
        view.scroll.resize(page.shelves.len(), 0);

        let max_shape = CardShape::ALL
            .into_iter()
            .find(|shape| card_size(*shape).1 < content.height)
            .unwrap_or(CardShape::Text);
        let mut layouts = artist_layouts(page, content, view.top, max_shape, view.songs_expanded);
        while !layouts.iter().any(|layout| layout.section == view.section)
            && view.top < view.section
        {
            view.top += 1;
            layouts = artist_layouts(page, content, view.top, max_shape, view.songs_expanded);
        }

        for layout in layouts {
            match layout.kind {
                ArtistSection::Songs => {
                    view.song = view.song.min(page.top_songs.len().saturating_sub(1));
                    render_artist_songs(
                        frame,
                        &page.top_songs,
                        layout.area,
                        view.section == layout.section,
                        view.song,
                        &mut view.song_offset,
                        accent,
                    );
                    let list = artist_song_list(layout.area);
                    render_artist_song_actions(frame, view, layout.area, accent, mouse);
                    let visible = usize::from(list.height);
                    for index in view.song_offset..(view.song_offset + visible).min(page.top_songs.len()) {
                        mouse.targets.push(MouseTarget::Area(
                            Rect::new(list.x, list.y + (index - view.song_offset) as u16, list.width, 1),
                            MouseAction::OpenArtistSong(index),
                        ));
                    }
                    if let Some(controls) = paging::Controls::new(layout.area, page.top_songs.len(), view.song_offset, visible, 1) {
                        controls.register(mouse, |offset| MouseAction::PageArtistSongs { offset, visible });
                    }
                }
                ArtistSection::Shelf(index) => {
                    let Some(shelf) = page.shelves.get(index) else {
                        continue;
                    };
                    let offset = &mut view.scroll[index];
                    let cursor = artist_shelf_cursor(
                        &mut view.card,
                        offset,
                        view.section == layout.section,
                        layout.across as usize,
                        shelf.cards.len(),
                    );
                    let mut tiles = Tiles {
                        shape: layout.shape,
                        art,
                        wanted: &mut wanted,
                        images: use_images.then_some((&mut *images, graphics)),
                    };
                    render_shelf(
                        frame,
                        shelf,
                        layout.area,
                        cursor,
                        (layout.across, layout.slot),
                        &mut tiles,
                    );
                    for column in 0..layout.across {
                        let card = cursor.offset + usize::from(column);
                        if card >= shelf.cards.len() { break; }
                        mouse.targets.push(MouseTarget::Area(
                            shelf_card_rect(layout.area, layout.slot, column),
                            MouseAction::OpenArtistCard { shelf: index, card },
                        ));
                    }
                    if let Some(controls) = paging::Controls::new(layout.area, shelf.cards.len(), cursor.offset, usize::from(layout.across), 1) {
                        controls.register(mouse, |offset| MouseAction::PageArtistShelf { shelf: index, offset, visible: usize::from(layout.across) });
                    }
                }
            }
        }
    }
    app.want_art(wanted);
}

pub(in crate::ui) fn artist_header_height(available: u16) -> u16 {
    match available {
        0..=2 => 0,
        3..=8 => 1,
        9 => 2,
        10..=15 => 3,
        16 => 4,
        17 => 5,
        _ => 6,
    }
}

pub(in crate::ui) fn artist_header_art_width(area: Rect, has_art: bool) -> u16 {
    if has_art && area.height >= 3 && area.width >= 48 {
        (area.height * 2).min(16).min(area.width / 3)
    } else {
        0
    }
}

pub(in crate::ui) fn render_artist_header(
    frame: &mut Frame,
    page: &ArtistPage,
    art: Option<&Cover>,
    area: Rect,
    accent: Color,
    images: Option<(&mut Vec<PlannedImage>, Graphics)>,
) {
    if area.is_empty() {
        return;
    }
    // Reserve the same space before and after the image arrives so loading it
    // cannot move the artist name and description between frames.
    let art_width = artist_header_art_width(area, page.art.is_some());
    let [art_area, text_area] =
        Layout::horizontal([Constraint::Length(art_width), Constraint::Min(0)]).areas(area);
    if let Some(art) = art.filter(|_| art_width > 0) {
        let planned = images.is_some_and(|(images, graphics)| {
            plan_tile_image(
                frame,
                ImageSource::Card(page.art_key()),
                art_area,
                images,
                graphics,
            )
        });
        if !planned {
            render_cover(frame, art, art_area, CoverSize::Full);
        }
    }

    let mut lines = vec![Line::from(Span::styled(
        truncate(&page.artist.name, text_area.width as usize),
        Style::default().fg(accent).add_modifier(Modifier::BOLD),
    ))];
    if text_area.height >= 2
        && let Some(audience) = &page.audience
    {
        lines.push(Line::from(Span::styled(
            truncate(audience, text_area.width as usize),
            Style::default().fg(Color::Gray),
        )));
    }
    if text_area.height >= 4
        && let Some(description) = &page.description
    {
        lines.push(Line::from(""));
        lines.extend(
            bounded_wrap(description, text_area.width as usize)
                .into_iter()
                .take(text_area.height.saturating_sub(3) as usize)
                .map(|line| Line::from(Span::styled(line, Style::default().fg(Color::DarkGray)))),
        );
    }
    frame.render_widget(Paragraph::new(lines), text_area);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::ui) enum ArtistSection {
    Songs,
    Shelf(usize),
}

#[derive(Debug, Clone, Copy)]
pub(in crate::ui) struct ArtistLayout {
    pub(in crate::ui) section: usize,
    pub(in crate::ui) kind: ArtistSection,
    pub(in crate::ui) area: Rect,
    pub(in crate::ui) shape: CardShape,
    pub(in crate::ui) across: u16,
    pub(in crate::ui) slot: u16,
}

pub(in crate::ui) fn artist_layouts(
    page: &ArtistPage,
    area: Rect,
    top: usize,
    max_shape: CardShape,
    expanded: bool,
) -> Vec<ArtistLayout> {
    let has_songs = !page.top_songs.is_empty();
    let count = page.shelves.len() + usize::from(has_songs);
    let mut layouts = Vec::new();
    let mut y = area.y;
    let bottom = area.y + area.height;

    for section in top..count {
        let available = bottom.saturating_sub(y);
        if available == 0 {
            break;
        }
        if has_songs && section == 0 {
            let wanted = (page.top_songs.len() as u16 + 3)
                .min(if expanded { u16::MAX } else { ARTIST_TOP_SONG_ROWS + 3 })
                .min(available);
            if wanted < 2 {
                break;
            }
            layouts.push(ArtistLayout {
                section,
                kind: ArtistSection::Songs,
                area: Rect::new(area.x, y, area.width, wanted),
                shape: CardShape::Text,
                across: 1,
                slot: area.width,
            });
            y += wanted.saturating_add(1);
            continue;
        }

        let shelf_index = section - usize::from(has_songs);
        let Some(shelf) = page.shelves.get(shelf_index) else {
            continue;
        };
        let desired = artist_shelf_shape(shelf).min(max_shape);
        let Some(shape) = CardShape::ALL
            .into_iter()
            .find(|shape| *shape <= desired && card_size(*shape).1 < available)
        else {
            break;
        };
        let (width, height) = card_size(shape);
        let row_height = height + 1;
        let across = (area.width / width).max(1);
        layouts.push(ArtistLayout {
            section,
            kind: ArtistSection::Shelf(shelf_index),
            area: Rect::new(area.x, y, area.width, row_height),
            shape,
            across,
            slot: area.width / across,
        });
        y += row_height.saturating_add(1);
    }
    layouts
}

pub(in crate::ui) fn artist_shelf_shape(shelf: &Shelf) -> CardShape {
    let playable = shelf.cards.iter().filter(|card| card.is_playable()).count();
    if playable * 2 < shelf.cards.len() {
        CardShape::Poster
    } else {
        CardShape::Tile
    }
}

pub(in crate::ui) fn clamp_card_offset(selected: &mut usize, offset: &mut usize, across: usize, len: usize) {
    *selected = (*selected).min(len.saturating_sub(1));
    if across == 0 || len == 0 {
        *offset = 0;
    } else if *selected < *offset {
        *offset = *selected;
    } else if *selected >= *offset + across {
        *offset = *selected + 1 - across;
    }
    *offset = (*offset).min(len.saturating_sub(across));
}

pub(in crate::ui) fn artist_shelf_cursor(
    selected: &mut usize,
    offset: &mut usize,
    focused: bool,
    across: usize,
    len: usize,
) -> ShelfCursor {
    if focused {
        clamp_card_offset(selected, offset, across, len);
    } else {
        *offset = (*offset).min(len.saturating_sub(across));
    }
    ShelfCursor {
        focused,
        selected: (*selected).min(len.saturating_sub(1)),
        offset: *offset,
    }
}

pub(in crate::ui) fn render_artist_songs(
    frame: &mut Frame,
    songs: &[ArtistSong],
    area: Rect,
    focused: bool,
    selected: usize,
    offset: &mut usize,
    accent: Color,
) {
    let [heading, _, body] = Layout::vertical([Constraint::Length(1), Constraint::Length(1), Constraint::Min(0)]).areas(area);
    let list = artist_song_list(area);
    let viewport = usize::from(list.height);
    if focused && viewport > 0 {
        if selected < *offset { *offset = selected; }
        else if selected >= *offset + viewport { *offset = selected + 1 - viewport; }
    }
    *offset = (*offset).min(songs.len().saturating_sub(viewport));
    let controls = paging::Controls::new(area, songs.len(), *offset, viewport, 1);
    let heading = controls.as_ref().map_or(heading, |controls| controls.heading);
    let marker = if focused { accent } else { Color::DarkGray };
    let title = if focused {
        Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::Gray)
    };
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("▌ ", Style::default().fg(marker)),
            Span::styled("Top songs", title),
            Span::styled(
                format!("  {} songs", songs.len()),
                Style::default().fg(Color::DarkGray),
            ),
        ])),
        heading,
    );
    if let Some(controls) = controls { controls.render(frame); }
    if body.is_empty() {
        return;
    }

    let width = body.width as usize;
    let columns = artist_song_columns(width);
    if body.height >= 2 {
        let header = Rect::new(body.x, body.y, body.width, 1);
        frame.render_widget(Paragraph::new(artist_song_header(columns)), header);
    }
    if viewport == 0 {
        return;
    }
    let end = (*offset + viewport).min(songs.len());
    let items = songs[*offset..end]
        .iter()
        .enumerate()
        .map(|(index, song)| {
            ListItem::new(artist_song_line(
                song,
                *offset + index == selected && focused,
                columns,
                accent,
            ))
        })
        .collect::<Vec<_>>();
    frame.render_widget(List::new(items), list);
}

pub(in crate::ui) fn artist_song_list(area: Rect) -> Rect {
    let header = if area.height >= 4 { 3 } else { area.height.min(2) };
    Rect::new(area.x, area.y + header, area.width, area.height - header)
}

pub(in crate::ui) fn render_artist_song_actions(frame: &mut Frame, view: &crate::app::ArtistView, area: Rect, accent: Color, mouse: &mut MouseMap) {
    if area.height < 3 { return; }
    let Some(page) = view.content() else { return; };
    let loading = view.songs_request.is_some();
    let more = if loading { " Loading… " } else if view.songs_error.is_some() || page.songs_truncated { " Retry more " }
        else if area.width < 32 { if view.songs_expanded { " Less " } else { " More " } }
        else if view.songs_expanded { " Show less " } else { " Show more " };
    let has_more = page.songs_endpoint.is_some() || page.top_songs.len() > ARTIST_TOP_SONG_ROWS as usize || view.songs_expanded;
    let mut x = area.x;
    for (label, action, enabled) in [
        (" Play all ", MouseAction::PlayArtistSongs, !page.top_songs.is_empty() && !loading),
        (more, MouseAction::MoreArtistSongs, has_more && !loading),
    ] {
        if action == MouseAction::MoreArtistSongs && !has_more { continue; }
        let budget = if action == MouseAction::PlayArtistSongs && has_more {
            area.width.saturating_sub(display_width(more) as u16 + 1)
        } else { area.right().saturating_sub(x) };
        let label = mouse.hinted_label(label, if loading { None } else if action == MouseAction::PlayArtistSongs { Some("a") } else { Some("e") }, budget);
        let width = (label.width() as u16).min(area.right().saturating_sub(x));
        if width == 0 { break; }
        let rect = Rect::new(x, area.y + 1, width, 1);
        frame.render_widget(Paragraph::new(label).style(
            Style::default().bg(shell::SURFACE).fg(if enabled { accent } else { Color::DarkGray })), rect);
        if enabled { mouse.targets.push(MouseTarget::Area(rect, action)); }
        x += width + 1;
    }
}

#[derive(Debug, Clone, Copy)]
pub(in crate::ui) struct ArtistSongColumns {
    pub(in crate::ui) width: usize,
    pub(in crate::ui) title: usize,
    pub(in crate::ui) album: usize,
    pub(in crate::ui) plays: usize,
    pub(in crate::ui) duration: usize,
}

pub(in crate::ui) fn artist_song_columns(width: usize) -> ArtistSongColumns {
    let duration = usize::from(width >= 24) * DURATION_WIDTH;
    let plays = if width >= 48 { 14 } else { 0 };
    let album = if width >= 72 { ALBUM_WIDTH } else { 0 };
    ArtistSongColumns {
        width,
        title: width.saturating_sub(1 + duration + plays + album),
        album,
        plays,
        duration,
    }
}

pub(in crate::ui) fn artist_song_header(columns: ArtistSongColumns) -> Line<'static> {
    if columns.width == 0 {
        return Line::default();
    }
    let style = Style::default()
        .fg(Color::DarkGray)
        .add_modifier(Modifier::BOLD);
    let mut spans = vec![Span::styled(
        format!(" {}", cell("TITLE", columns.title)),
        style,
    )];
    if columns.album > 0 {
        spans.push(Span::styled(cell("ALBUM", columns.album), style));
    }
    if columns.plays > 0 {
        spans.push(Span::styled(cell("PLAYS", columns.plays), style));
    }
    if columns.duration > 0 {
        spans.push(Span::styled(
            format!("{:>width$}", "DURATION", width = columns.duration),
            style,
        ));
    }
    Line::from(spans)
}

pub(in crate::ui) fn artist_song_line(
    song: &ArtistSong,
    selected: bool,
    columns: ArtistSongColumns,
    accent: Color,
) -> Line<'static> {
    if columns.width == 0 {
        return Line::default();
    }
    let style = if selected {
        highlight(accent)
    } else {
        Style::default()
    };
    let dim = |colour| {
        if selected {
            style
        } else {
            Style::default().fg(colour)
        }
    };
    let mut spans = vec![Span::styled(
        format!(
            " {}",
            focused_cell(&song.track.title, columns.title, selected)
        ),
        style,
    )];
    if columns.album > 0 {
        spans.push(Span::styled(
            cell(song.track.album.as_deref().unwrap_or(""), columns.album),
            dim(Color::Gray),
        ));
    }
    if columns.plays > 0 {
        spans.push(Span::styled(
            cell(song.plays.as_deref().unwrap_or(""), columns.plays),
            dim(Color::DarkGray),
        ));
    }
    if columns.duration > 0 {
        let duration = truncate(
            &song.track.duration.map(clock).unwrap_or_default(),
            columns.duration,
        );
        spans.push(Span::styled(
            format!("{duration:>width$}", width = columns.duration),
            dim(Color::DarkGray),
        ));
    }
    Line::from(spans)
}
