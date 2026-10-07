//! Search and collection pages. Only visible rows ask for thumbnail artwork.

use super::*;
use crate::source::collection::Details;

struct Page<'a> {
    title: &'a str,
    art_key: &'a str,
    details: Option<&'a Details>,
    error: Option<&'a str>,
    busy: bool,
    tracks: &'a [Track],
    selected: usize,
    offset: usize,
}

#[derive(Clone, Copy)]
struct Regions {
    header: Rect,
    list: Rect,
    row_height: u16,
}

fn regions(area: Rect, collection: bool, has_results: bool) -> Regions {
    let desired = if collection {
        if area.width >= 56 { 8 } else { 5 }
    } else if has_results && area.width >= 64 && area.height >= 20 {
        7
    } else {
        2
    };
    // Even a small window keeps a song beneath the heading.
    let header_height = desired.min(area.height.saturating_sub(2));
    let [header, list] =
        Layout::vertical([Constraint::Length(header_height), Constraint::Min(0)]).areas(area);
    Regions {
        header,
        list,
        row_height: if list.height >= 4 && list.width >= 30 {
            3
        } else {
            1
        },
    }
}

pub(super) fn render(frame: &mut Frame, app: &mut App, area: Rect, mouse: &mut MouseMap) {
    if area.height <= 7 && app.browsing.is_none() {
        super::render_compact_results(frame, app, area, mouse);
        return;
    }
    let inner = shell::inset(area);
    let layout = regions(inner, app.browsing.is_some(), !app.results.is_empty());
    let visible = usize::from(layout.list.height / layout.row_height.max(1));
    app.clamp_scroll(visible);
    let use_images = app.kitty_images() && !app.overlay.is_open() && app.menu().is_none();
    let art_key = app.collection_art_key().unwrap_or("").to_owned();
    let mut wanted = Vec::new();
    let mut images = use_images.then_some((&mut app.images, app.graphics));
    let title = app.browsing.as_deref().unwrap_or(&app.query);
    draw(
        frame,
        layout,
        Page {
            title,
            art_key: &art_key,
            details: app.collection.as_ref(),
            error: app.collection_error.as_deref(),
            busy: app.busy,
            tracks: &app.results,
            selected: app.selected,
            offset: app.offset,
        },
        &mut Tiles {
            shape: CardShape::Tile,
            art: &app.art,
            wanted: &mut wanted,
            images: images.take(),
        },
        mouse,
        app.graphics,
    );
    app.want_art(wanted);
}

fn draw(
    frame: &mut Frame,
    layout: Regions,
    page: Page<'_>,
    tiles: &mut Tiles<'_>,
    mouse: &mut MouseMap,
    graphics: Graphics,
) {
    if let Some(details) = page.details {
        collection_header(frame, layout.header, &page, details, tiles, mouse, graphics);
    } else {
        search_header(frame, layout.header, &page, tiles, mouse, graphics);
    }
    let list = layout.list;
    if page.tracks.is_empty() {
        let text = page.error.map(str::to_owned).unwrap_or_else(|| {
            if page.busy {
                "Loading songs…".to_owned()
            } else if page.details.is_some() {
                "This playlist has no playable songs yet.".to_owned()
            } else if page.title.is_empty() {
                "Search for a song, artist or album above.".to_owned()
            } else {
                format!("No songs found for “{}”. Try another search.", page.title)
            }
        });
        frame.render_widget(
            Paragraph::new(text)
                .style(Style::default().fg(Color::Gray))
                .wrap(ratatui::widgets::Wrap { trim: true }),
            list,
        );
        return;
    }
    let count = usize::from(list.height / layout.row_height.max(1));
    for (visible, (index, track)) in page
        .tracks
        .iter()
        .enumerate()
        .skip(page.offset)
        .take(count)
        .enumerate()
    {
        let row = Rect::new(
            list.x,
            list.y + visible as u16 * layout.row_height,
            list.width,
            layout.row_height,
        );
        song_row(
            frame,
            row,
            track,
            index,
            page.selected == index,
            tiles,
            graphics,
        );
        if !page.busy {
            mouse
                .targets
                .push(MouseTarget::Area(row, MouseAction::PlayTrack(index)));
        }
    }
}

pub(super) fn square_cols(rows: u16, graphics: Graphics) -> u16 {
    let (w, h) = graphics.cell;
    ((u32::from(rows) * u32::from(h) + u32::from(w.max(1)) / 2) / u32::from(w.max(1)))
        .min(u32::from(u16::MAX)) as u16
}

pub(super) fn artwork(frame: &mut Frame, area: Rect, key: &str, url: Option<String>, tiles: &mut Tiles<'_>) {
    let art = tiles.art.get(key);
    if art.is_some() || url.is_some() {
        tiles.wanted.push((key.to_owned(), url));
    }
    render_tile(frame, art, key, area, &mut tiles.images);
}

fn song_row(
    frame: &mut Frame,
    area: Rect,
    track: &Track,
    index: usize,
    selected: bool,
    tiles: &mut Tiles<'_>,
    graphics: Graphics,
) {
    let bg = if selected {
        Color::from_u32(0x0025_2525)
    } else {
        shell::BACKGROUND
    };
    frame.render_widget(Block::default().style(Style::default().bg(bg)), area);
    let colour = tiles.art.get(&track.id).map_or(Color::Gray, |art| {
        Color::Rgb(art.accent.0, art.accent.1, art.accent.2)
    });
    let number = if selected {
        ">".to_owned()
    } else {
        (index + 1).to_string()
    };
    frame.render_widget(
        Paragraph::new(number).style(Style::default().fg(colour)),
        Rect::new(area.x, area.y, area.width.min(4), 1),
    );
    let thumb_width = if area.height >= 2 && area.width >= 40 {
        square_cols(2, graphics).min(8)
    } else {
        0
    };
    if thumb_width > 0 {
        artwork(
            frame,
            Rect::new(area.x + 4, area.y, thumb_width, 2),
            &track.id,
            Some(format!("https://i.ytimg.com/vi/{}/hqdefault.jpg", track.id)),
            tiles,
        );
    }
    let start = (4 + thumb_width + u16::from(thumb_width > 0)).min(area.width);
    let room = area.width.saturating_sub(start);
    let duration_width = room.min(8);
    let text = Rect::new(
        area.x + start,
        area.y,
        room.saturating_sub(duration_width),
        area.height.min(2),
    );
    let title = focused_text(&track.title, usize::from(text.width), selected);
    let metadata = match track.album.as_deref().filter(|name| !name.is_empty()) {
        Some(album) => format!("{} • {album}", track.uploader),
        None => track.uploader.clone(),
    };
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(Span::styled(
                title,
                Style::default().fg(Color::White).add_modifier(if selected {
                    Modifier::BOLD
                } else {
                    Modifier::empty()
                }),
            )),
            Line::from(Span::styled(
                truncate(&metadata, text.width as usize),
                Style::default().fg(Color::Gray),
            )),
        ]),
        text,
    );
    frame.render_widget(
        Paragraph::new(track.duration_str())
            .alignment(ratatui::layout::Alignment::Right)
            .style(Style::default().fg(Color::DarkGray)),
        Rect::new(
            area.right().saturating_sub(duration_width),
            area.y,
            duration_width,
            1,
        ),
    );
}

fn collection_header(
    frame: &mut Frame,
    area: Rect,
    page: &Page<'_>,
    details: &Details,
    tiles: &mut Tiles<'_>,
    mouse: &mut MouseMap,
    graphics: Graphics,
) {
    if area.height == 0 {
        return;
    }
    let art_rows = area.height.saturating_sub(1).min(7);
    let art_cols = if area.width >= 56 && art_rows >= 4 {
        square_cols(art_rows, graphics).min(area.width / 3)
    } else {
        0
    };
    if art_cols > 0 {
        artwork(
            frame,
            Rect::new(area.x, area.y, art_cols, art_rows),
            page.art_key,
            details.art.clone(),
            tiles,
        );
    }
    let left = art_cols + if art_cols > 0 { 2 } else { 0 };
    let text = Rect::new(
        area.x + left,
        area.y,
        area.width.saturating_sub(left),
        area.height,
    );
    let title = if details.title.is_empty() {
        page.title
    } else {
        &details.title
    };
    let count = if let Some(error) = page.error {
        error.to_owned()
    } else if page.busy {
        "Loading songs…".to_owned()
    } else if details.truncated {
        format!(
            "{} songs loaded • more available on YouTube Music",
            page.tracks.len()
        )
    } else {
        format!(
            "{} songs • {}",
            page.tracks.len(),
            clock(page.tracks.iter().filter_map(|track| track.duration).sum())
        )
    };
    let mut lines = vec![
        Line::from(Span::styled(
            truncate(title, text.width as usize),
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            truncate(&details.subtitle, text.width as usize),
            Style::default().fg(Color::Gray),
        )),
        Line::from(Span::styled(
            truncate(&count, text.width as usize),
            Style::default().fg(Color::DarkGray),
        )),
    ];
    if text.height >= 6 && !details.description.is_empty() {
        lines.push(Line::from(Span::styled(
            truncate(&details.description.replace('\n', " "), text.width as usize),
            Style::default().fg(Color::Gray),
        )));
    }
    frame.render_widget(Paragraph::new(lines), text);
    if text.height < 4 {
        return;
    }
    let y = text.y + text.height.saturating_sub(2);
    let ready = !page.busy && !page.tracks.is_empty() && page.error.is_none();
    let mut x = text.x;
    for (label, action, enabled) in [
        (" Play ", MouseAction::PlayCollection, ready),
        (" Shuffle ", MouseAction::ShuffleCollection, ready),
        (
            if page.error.is_some() {
                " Retry "
            } else {
                " Refresh "
            },
            MouseAction::RetryCollection,
            !page.busy,
        ),
    ] {
        let width = display_width(label) as u16;
        if x + width > text.right() {
            break;
        }
        let button = Rect::new(x, y, width, 1);
        frame.render_widget(
            Paragraph::new(label).style(if enabled {
                Style::default()
                    .fg(Color::White)
                    .bg(Color::from_u32(0x0030_3030))
            } else {
                Style::default().fg(Color::DarkGray)
            }),
            button,
        );
        if enabled {
            mouse.targets.push(MouseTarget::Area(button, action));
        }
        x += width + 1;
    }
}

fn search_header(
    frame: &mut Frame,
    area: Rect,
    page: &Page<'_>,
    tiles: &mut Tiles<'_>,
    mouse: &mut MouseMap,
    graphics: Graphics,
) {
    let heading = if page.title.is_empty() {
        "Search".to_owned()
    } else {
        format!("Search • {}", page.title)
    };
    frame.render_widget(
        Paragraph::new(truncate(&heading, area.width as usize)).style(
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
        Rect::new(area.x, area.y, area.width, area.height.min(1)),
    );
    if area.height >= 6
        && let Some(track) = page.tracks.first()
    {
        let rows = 4;
        let cols = square_cols(rows, graphics).min(12);
        artwork(
            frame,
            Rect::new(area.x, area.y + 2, cols, rows),
            &track.id,
            Some(format!("https://i.ytimg.com/vi/{}/hqdefault.jpg", track.id)),
            tiles,
        );
        let text = Rect::new(
            area.x + cols + 2,
            area.y + 2,
            area.width.saturating_sub(cols + 2),
            rows,
        );
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(Span::styled(
                    "TOP RESULT",
                    Style::default().fg(Color::DarkGray),
                )),
                Line::from(Span::styled(
                    truncate(&track.title, text.width as usize),
                    Style::default()
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD),
                )),
                Line::from(Span::styled(
                    truncate(
                        &format!("{} • {}", track.uploader, track.duration_str()),
                        text.width as usize,
                    ),
                    Style::default().fg(Color::Gray),
                )),
                Line::from(Span::styled("Play", Style::default().fg(Color::White))),
            ]),
            text,
        );
        if !page.busy {
            mouse.targets.push(MouseTarget::Area(
                Rect::new(area.x, area.y + 2, area.width, rows),
                MouseAction::PlayTrack(0),
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};

    #[test]
    fn every_viewport_keeps_rows_and_clicks_inside_its_bounds() {
        let tracks = vec![
            Track {
                id: "example1234".into(),
                title: "Birds of a Feather".into(),
                uploader: "Billie Eilish".into(),
                duration: Some(Duration::from_secs(210)),
                album: None,
                artist_ref: None
            };
            20
        ];
        let art = ArtCache::default();
        for (w, h) in [(120, 35), (80, 24), (40, 12), (20, 5), (1, 1), (0, 0)] {
            let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
            let mut mouse = MouseMap::default();
            let mut wanted = Vec::new();
            let details = Details {
                title: "My playlist".into(),
                subtitle: "Private • Me".into(),
                ..Default::default()
            };
            terminal
                .draw(|frame| {
                    draw(
                        frame,
                        regions(frame.area(), true, true),
                        Page {
                            title: "My playlist",
                            art_key: "VLtest",
                            details: Some(&details),
                            error: None,
                            busy: false,
                            tracks: &tracks,
                            selected: 3,
                            offset: 2,
                        },
                        &mut Tiles {
                            shape: CardShape::Tile,
                            art: &art,
                            wanted: &mut wanted,
                            images: None,
                        },
                        &mut mouse,
                        Graphics::blocks(),
                    )
                })
                .unwrap();
            for target in &mouse.targets {
                if let MouseTarget::Area(rect, _) = target {
                    assert!(rect.right() <= w && rect.bottom() <= h, "{w}x{h}: {rect:?}");
                }
            }
            let visible = regions(Rect::new(0, 0, w, h), true, true);
            assert!(wanted.len() <= usize::from(visible.list.height / visible.row_height) + 1);
        }
    }

    #[test]
    fn playlist_error_is_visible_and_retry_is_clickable() {
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        let mut mouse = MouseMap::default();
        let mut wanted = Vec::new();
        let art = ArtCache::default();
        let details = Details {
            title: "Saved playlist".into(),
            ..Default::default()
        };
        terminal
            .draw(|frame| {
                draw(
                    frame,
                    regions(frame.area(), true, false),
                    Page {
                        title: "Saved playlist",
                        art_key: "VLtest",
                        details: Some(&details),
                        error: Some("Sign in to open this private playlist"),
                        busy: false,
                        tracks: &[],
                        selected: 0,
                        offset: 0,
                    },
                    &mut Tiles {
                        shape: CardShape::Tile,
                        art: &art,
                        wanted: &mut wanted,
                        images: None,
                    },
                    &mut mouse,
                    Graphics::blocks(),
                )
            })
            .unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(text.contains("Sign in to open"));
        assert!(
            mouse
                .targets
                .iter()
                .any(|target| matches!(target, MouseTarget::Area(_, MouseAction::RetryCollection)))
        );
        assert!(
            !mouse
                .targets
                .iter()
                .any(|target| matches!(target, MouseTarget::Area(_, MouseAction::PlayCollection)))
        );
    }

    #[test]
    #[ignore = "exports actual search and playlist terminal cells for visual inspection"]
    fn preview_browse_pages() {
        let tracks: Vec<_> = [
            (
                "Birds of a Feather",
                "Billie Eilish",
                "HIT ME HARD AND SOFT",
                210,
            ),
            ("Nice Boys", "TEMPOREX", "Care", 180),
            ("Let It Happen", "Tame Impala", "Currents", 468),
            ("Just Forget", "FORCE OF NATURE", "Samurai Champloo", 236),
            ("Is It True", "Tame Impala", "The Slow Rush", 239),
            ("Sweet Boys", "Artist", "Album", 201),
        ]
        .into_iter()
        .enumerate()
        .map(|(i, (title, uploader, album, seconds))| Track {
            id: format!("sample{i}"),
            title: title.into(),
            uploader: uploader.into(),
            album: Some(album.into()),
            duration: Some(Duration::from_secs(seconds)),
            artist_ref: None,
        })
        .collect();
        let mut art = ArtCache::default();
        for (i, track) in tracks.iter().enumerate() {
            art.want(&track.id);
            art.store(
                track.id.clone(),
                Some(Cover::from_rgb(
                    4,
                    4,
                    [(60 + i * 25) as u8, 100, 150].repeat(16),
                )),
            );
        }
        art.want("VLtest");
        art.store(
            "VLtest".into(),
            Some(Cover::from_rgb(4, 4, [160, 95, 70].repeat(16))),
        );
        let details = Details {
            title: "Evening favorites".into(),
            subtitle: "Playlist • Private • Your library".into(),
            description: "A few songs to keep close.".into(),
            ..Default::default()
        };
        for (width, height) in [(120, 35), (80, 24), (48, 18)] {
            for collection in [false, true] {
                let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
                let mut mouse = MouseMap::default();
                let mut wanted = Vec::new();
                terminal
                    .draw(|frame| {
                        frame.render_widget(
                            Block::default()
                                .style(Style::default().bg(shell::BACKGROUND).fg(Color::White)),
                            frame.area(),
                        );
                        let [header, main, footer] = shell::regions(frame.area());
                        shell::render_header(
                            frame,
                            header,
                            shell::Header {
                                mode: Mode::Browse,
                                view: View::Tracks,
                                query: "birds of a feather",
                                accent: Color::Gray,
                                has_player: true,
                                accepts_input: true,
                            },
                            &mut mouse,
                        );
                        draw(
                            frame,
                            regions(shell::inset(main), collection, true),
                            Page {
                                title: if collection {
                                    "Evening favorites"
                                } else {
                                    "birds of a feather"
                                },
                                art_key: "VLtest",
                                details: collection.then_some(&details),
                                error: None,
                                busy: false,
                                tracks: &tracks,
                                selected: 1,
                                offset: 0,
                            },
                            &mut Tiles {
                                shape: CardShape::Tile,
                                art: &art,
                                wanted: &mut wanted,
                                images: None,
                            },
                            &mut mouse,
                            Graphics::blocks(),
                        );
                        frame.render_widget(
                            Block::default().style(Style::default().bg(shell::SURFACE)),
                            footer,
                        );
                    })
                    .unwrap();
                super::super::tests::print_preview(
                    &format!(
                        "{}-{width}x{height}",
                        if collection { "playlist" } else { "search" }
                    ),
                    terminal.backend().buffer(),
                );
            }
        }
    }
}
