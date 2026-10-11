//! Music Home: large opening covers, compact song grids, and spaced shelves.

use super::*;

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
        for (button, label) in [(controls.previous, " < "), (controls.next, " > ")] {
            let style = if button.offset.is_some() {
                Style::default()
                    .fg(Color::White)
                    .bg(shell::SURFACE)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::DarkGray)
            };
            frame.render_widget(Paragraph::new(Span::styled(label, style)), button.area);
        }
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

struct ShelfButton {
    area: Rect,
    offset: Option<usize>,
}

struct ShelfControls {
    heading: Rect,
    previous: ShelfButton,
    next: ShelfButton,
}

/// Page by complete columns so compact grids keep their row alignment.
/// The same geometry supplies both the visible controls and their mouse targets.
fn controls(layout: ShelfLayout, count: usize, offset: usize) -> Option<ShelfControls> {
    let rows = usize::from(layout.rows.max(1));
    let columns = usize::from(layout.across);
    let capacity = columns * rows;
    if count <= capacity || capacity == 0 || layout.area.width < MIN_HOME_WIDTH {
        return None;
    }
    let maximum = count.div_ceil(rows).saturating_sub(columns) * rows;
    let first = (offset / rows * rows).min(maximum);
    Some(ShelfControls {
        heading: Rect::new(layout.area.x, layout.area.y, layout.area.width - 9, 1),
        previous: ShelfButton {
            area: Rect::new(layout.area.right() - 7, layout.area.y, 3, 1),
            offset: (first > 0).then_some(first.saturating_sub(capacity)),
        },
        next: ShelfButton {
            area: Rect::new(layout.area.right() - 3, layout.area.y, 3, 1),
            offset: (first < maximum).then_some(first.saturating_add(capacity).min(maximum)),
        },
    })
}

pub(super) fn register_controls(
    mouse: &mut MouseMap,
    layout: ShelfLayout,
    count: usize,
    offset: usize,
) {
    if let Some(controls) = controls(layout, count, offset) {
        for button in [controls.previous, controls.next] {
            if let Some(offset) = button.offset {
                mouse.targets.push(MouseTarget::Area(
                    button.area,
                    MouseAction::PageHomeShelf {
                        shelf: layout.index,
                        offset,
                        columns: usize::from(layout.across),
                        rows: usize::from(layout.rows),
                    },
                ));
            }
        }
    }
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
