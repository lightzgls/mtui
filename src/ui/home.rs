//! Music Home: large opening covers, compact song grids, and spaced shelves.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::app::{App, CardShape, ImageSource, MouseAction, PlannedImage};
use crate::graphics::Graphics;
use crate::player::{PlayState, Snapshot};
use crate::source::cover::Cover;
use crate::source::home::Shelf;

use super::{MIN_HOME_WIDTH, home, paging, shell};
use super::cards::{
    NO_ART, ShelfCursor, ShelfLayout, Tiles, card_size, heading_line, plan_tile_image, render_card,
    render_tile_blocks, shelf_height, shelf_layouts,
};
use super::mouse::{MouseMap, MouseTarget};
use super::text::{marquee, truncate};

#[cfg(test)]
use crate::source::home::Card;

pub(super) fn content_area(area: Rect) -> Rect {
    let mut inner = shell::inset(area);
    if inner.height >= 12 {
        inner.y += 1;
        inner.height -= 2;
    }
    inner
}

pub(super) fn shape(shelf: &Shelf, index: usize, maximum: CardShape) -> CardShape {
    (if index == 0 {
        CardShape::Gallery
    } else if crate::app::home::compact(shelf) {
        CardShape::Tile
    } else {
        CardShape::Poster
    })
    .min(maximum)
}

pub(super) fn layouts(
    shelves: &[Shelf],
    area: Rect,
    top: usize,
    maximum: CardShape,
) -> Vec<ShelfLayout> {
    let mut layouts = Vec::new();
    let mut y = area.y;
    let heading_height = if area.height >= 10 { 2 } else { 1 };
    let gap = if area.height >= 10 { 2 } else { 1 };
    let height_for = |shape| {
        if shape == CardShape::Tile {
            3
        } else {
            card_size(shape).1
        }
    };
    for (index, shelf) in shelves.iter().enumerate().skip(top) {
        let desired = super::section_shape(shelf, index, maximum);
        let remaining = area.bottom().saturating_sub(y);
        let Some(shape) = CardShape::ALL
            .into_iter()
            .find(|shape| *shape <= desired && remaining >= heading_height + height_for(*shape))
        else {
            break;
        };
        let (width, _) = card_size(shape);
        let across = (area.width / width.max(1)).max(1);
        let card_height = height_for(shape);
        let rows = if shape == CardShape::Tile {
            (shelf.cards.len().div_ceil(usize::from(across)) as u16)
                .clamp(1, 3)
                .min((remaining - heading_height) / card_height)
        } else {
            1
        };
        let height = card_height * rows + heading_height;
        layouts.push(ShelfLayout {
            index,
            shape,
            area: Rect::new(area.x, y, area.width, height),
            across,
            slot: area.width / across,
            rows,
            heading_height,
        });
        y += height + gap;
    }
    layouts
}

pub(super) fn render_shelf(
    frame: &mut Frame,
    shelf: &Shelf,
    area: Rect,
    cursor: ShelfCursor,
    layout: ShelfLayout,
    tiles: &mut Tiles<'_>,
) {
    let controls = controls(layout, shelf.cards.len(), cursor.offset);
    let heading = controls
        .as_ref()
        .map_or(Rect::new(area.x, area.y, area.width, 1), |controls| {
            controls.heading
        });
    frame.render_widget(
        Paragraph::new(heading_line(shelf, cursor, heading.width as usize)),
        heading,
    );
    if let Some(controls) = controls {
        controls.render(frame);
    }
    for slot in 0..layout.across * layout.rows {
        let index = cursor.offset + usize::from(slot);
        let Some(card) = shelf.cards.get(index) else {
            break;
        };
        render_card(
            frame,
            card,
            layout.card_rect(slot),
            cursor.focused && cursor.selected == index,
            tiles,
        );
    }
}

/// Page by complete columns so compact grids keep their row alignment.
/// The same geometry supplies both the visible controls and their mouse targets.
fn controls(layout: ShelfLayout, count: usize, offset: usize) -> Option<paging::Controls> {
    paging::Controls::new(
        layout.area,
        count,
        offset,
        usize::from(layout.across),
        usize::from(layout.rows),
    )
}

pub(super) fn register_controls(
    mouse: &mut MouseMap,
    layout: ShelfLayout,
    count: usize,
    offset: usize,
) {
    if let Some(controls) = controls(layout, count, offset) {
        controls.register(mouse, |offset| MouseAction::PageHomeShelf {
            shelf: layout.index,
            offset,
            columns: usize::from(layout.across),
            rows: usize::from(layout.rows),
        });
    }
}

/// The landing page: YouTube Music's shelves as borderless artwork cards.
///
/// Virtualized in both directions, on the same principle as the results list --
/// only the shelves on screen are laid out, and only the cards visible within
/// each of them are built. A feed of twelve shelves costs what one screenful
/// costs. The artwork obeys the same rule and is what makes it matter: the
/// cards drawn here are the only ones whose pictures are ever fetched, which is
/// a dozen requests for a feed of three hundred cards.
pub(super) fn render_home(frame: &mut Frame, app: &mut App, area: Rect, mouse: &mut MouseMap) {
    let inner = home::content_area(area);

    let smallest = shelf_height(CardShape::Text);
    if app.home.is_empty() || inner.width < MIN_HOME_WIDTH || inner.height + 1 < smallest {
        let hint = if !app.home.is_empty() {
            // Too small to draw a card in, which is a fact about the window
            // rather than a failure worth explaining.
            "the window is too small for the home feed"
        } else if app.home_pending {
            "loading ..."
        } else {
            // Both halves matter: `r` is the only way to ask again, and a user
            // who cannot get a feed can still use every other part of this.
            "no home feed -- press r to try again, or / to search"
        };
        frame.render_widget(
            Paragraph::new(Span::styled(hint, Style::default().fg(Color::DarkGray))),
            inner,
        );
        return;
    }

    let plan = plan_home(inner);
    let shelves = inner;

    app.clamp_home_selection();
    app.home_top = app.home_top.min(app.home_shelf);
    let mut layouts = shelf_layouts(&app.home, shelves, app.home_top, plan.max_shape);
    while !layouts.iter().any(|layout| layout.index == app.home_shelf)
        && app.home_top < app.home_shelf
    {
        app.home_top += 1;
        layouts = shelf_layouts(&app.home, shelves, app.home_top, plan.max_shape);
    }
    for layout in &layouts {
        let rows = usize::from(layout.rows.max(1));
        let maximum = app.home[layout.index].cards.len().div_ceil(rows).saturating_sub(usize::from(layout.across));
        app.home_scroll[layout.index] = (app.home_scroll[layout.index] / rows).min(maximum) * rows;
    }
    if let Some(focused) = layouts.iter().find(|layout| layout.index == app.home_shelf) {
        app.home_grid_rows = usize::from(focused.rows);
        app.clamp_home_grid(usize::from(focused.across), usize::from(focused.rows));
    }

    for layout in &layouts {
        let Some(shelf) = app.home.get(layout.index) else {
            continue;
        };
        let offset = app.home_scroll.get(layout.index).copied().unwrap_or(0);
        home::register_controls(mouse, *layout, shelf.cards.len(), offset);
        for slot in 0..layout.across * layout.rows {
            let card = offset + slot as usize;
            if card >= shelf.cards.len() {
                break;
            }
            mouse.targets.push(MouseTarget::Area(
                layout.card_rect(slot),
                MouseAction::OpenHomeCard {
                    shelf: layout.index,
                    card,
                },
            ));
        }
    }

    let cursor = HomeCursor {
        shelf: app.home_shelf,
        card: app.home_card,
    };
    // Collected during the draw rather than worked out beside it: which cards
    // are on screen falls out of laying them out, and deriving it twice is two
    // chances for the pictures and the cards to disagree about what is visible.
    let mut wanted = Vec::new();
    let tile_images = if app.protocol_images() && !app.overlay.is_open() && app.menu().is_none() {
        Some((&mut app.images, app.graphics))
    } else {
        None
    };
    render_feed(
        frame,
        &app.home,
        &layouts,
        cursor,
        &app.home_scroll,
        Tiles {
            shape: CardShape::Text,
            art: &app.art,
            wanted: &mut wanted,
            images: tile_images,
        },
    );
    app.want_art(wanted);
}

/// The largest artwork card shape the landing page can hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct HomePlan {
    pub(super) max_shape: CardShape,
}

/// Home keeps its full browsing area during playback; the persistent strip
/// already identifies the current track.
pub(super) fn plan_home(area: Rect) -> HomePlan {
    let max_shape = CardShape::ALL
        .into_iter()
        .find(|shape| fits(area.height, *shape, 1))
        .unwrap_or(CardShape::Text);
    HomePlan { max_shape }
}

/// Whether `count` shelves of `shape` fit in `height` rows.
///
/// The last shelf keeps its heading and its cards but not the blank row under
/// it, because there is nothing below for that row to separate it from -- which
/// is worth the arithmetic: it is often the difference between two shelves and
/// one.
pub(super) fn fits(height: u16, shape: CardShape, count: u16) -> bool {
    height >= shelf_height(shape) * count - 1
}

/// Where the cursor stands on the landing page as a whole.
#[derive(Debug, Clone, Copy)]
pub(super) struct HomeCursor {
    pub(super) shelf: usize,
    pub(super) card: usize,
}

/// Stacks the visible shelves down the pane.
///
/// Split from [`render_home`] so the page can be drawn from shelves alone --
/// which is what lets it be previewed and tested without standing up a player
/// and a worker thread behind an `App`.
pub(super) fn render_feed(
    frame: &mut Frame,
    shelves: &[Shelf],
    layouts: &[ShelfLayout],
    cursor: HomeCursor,
    scroll: &[usize],
    mut tiles: Tiles,
) {
    for layout in layouts {
        let index = layout.index;
        let Some(shelf) = shelves.get(index) else {
            continue;
        };
        tiles.shape = layout.shape;
        home::render_shelf(
            frame,
            shelf,
            layout.area,
            ShelfCursor {
                focused: index == cursor.shelf,
                selected: cursor.card,
                offset: scroll.get(index).copied().unwrap_or(0),
            },
            *layout,
            &mut tiles,
        );
    }
}

/// What the now-playing strip draws, gathered from the `App` before it is
/// borrowed for the frame.
///
/// A struct of plain fields rather than the `App` itself, for the reason
/// [`render_feed`] takes shelves rather than one: the strip can then be drawn
/// -- and previewed, and tested -- without a player thread and a worker behind
/// it.
pub(super) struct Hero<'a> {
    pub(super) title: &'a str,
    pub(super) artist: &'a str,
    /// The sleeve of what is playing. `None` before it arrives, which is a
    /// second or two at the start of every track.
    pub(super) art: Option<&'a Cover>,
    pub(super) snap: Snapshot,
}

/// Compact identity above the tabs on a narrow Now Playing screen.
pub(super) fn render_hero(
    frame: &mut Frame,
    hero: &Hero,
    area: Rect,
    images: Option<(&mut Vec<PlannedImage>, Graphics)>,
) {
    let accent = hero.art.map_or(NO_ART, |cover| cover.accent);
    let accent = Color::Rgb(accent.0, accent.1, accent.2);

    let inner = Rect {
        height: area.height.min(3),
        ..area
    };
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    // The sleeve, square, as tall as the strip is inside.
    let cols = (inner.height * 2).min(inner.width);
    let art_area = Rect {
        width: cols,
        ..inner
    };
    let planned = hero.art.is_some()
        && images.is_some_and(|(images, graphics)| {
            plan_tile_image(frame, ImageSource::Playing, art_area, images, graphics)
        });
    if !planned {
        render_tile_blocks(frame, hero.art, art_area);
    }

    let text = Rect {
        x: inner.x + cols + 1,
        width: inner.width.saturating_sub(cols + 2),
        ..inner
    };
    if text.width == 0 {
        return;
    }

    let label = if hero.snap.state == PlayState::Paused {
        "PAUSED"
    } else if hero.snap.state == PlayState::Buffering {
        "LOADING…"
    } else {
        ""
    };
    let width = text.width as usize;

    let lines = vec![
        Line::from(Span::styled(
            label,
            Style::default().fg(accent).add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            marquee(hero.title, width),
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            truncate(hero.artist, width),
            Style::default().fg(Color::Gray),
        )),
    ];
    frame.render_widget(Paragraph::new(lines), text);
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn grid_hit_boxes_do_not_overlap_and_stay_inside_each_shelf() {
        let shelf = Shelf {
            title: "Quick picks".into(),
            cards: (0..18)
                .map(|i| Card {
                    title: format!("Song {i}"),
                    subtitle: "Artist".into(),
                    art: None,
                    duration: None,
                    artist_ref: None,
                    target: crate::source::home::Target::Play {
                        video_id: i.to_string(),
                    },
                })
                .collect(),
        };
        for (w, h) in [(120, 35), (80, 24), (40, 12), (24, 4)] {
            for layout in layouts(
                &[
                    Shelf {
                        title: "Listen again".into(),
                        cards: shelf.cards.clone(),
                    },
                    shelf.clone(),
                ],
                Rect::new(1, 2, w, h),
                1,
                CardShape::Gallery,
            ) {
                let rects: Vec<_> = (0..layout.across * layout.rows)
                    .map(|i| layout.card_rect(i))
                    .collect();
                for (i, rect) in rects.iter().enumerate() {
                    assert!(
                        rect.right() <= layout.area.right()
                            && rect.bottom() <= layout.area.bottom()
                    );
                    assert!(
                        rects
                            .iter()
                            .skip(i + 1)
                            .all(|other| rect.intersection(*other).is_empty())
                    );
                }
            }
        }
    }
}
