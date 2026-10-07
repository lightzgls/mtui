//! Music Home: compact song grids and consistent square collection shelves.

use super::*;

pub(super) fn shape(shelf: &Shelf, maximum: CardShape) -> CardShape {
    let songs = shelf.cards.iter().all(Card::is_playable);
    let title = shelf.title.to_lowercase();
    let compact = songs
        && (title.contains("quick pick")
            || title.contains("from your listening")
            || title.contains("trending songs"));
    (if compact {
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
    for (index, shelf) in shelves.iter().enumerate().skip(top) {
        let mut shape = super::section_shape(shelf, index, maximum);
        let (width, nominal_height) = card_size(shape);
        let across = (area.width / width.max(1)).max(1);
        let remaining = area.bottom().saturating_sub(y);
        let mut rows = if shape == CardShape::Tile && remaining >= 9 {
            (shelf.cards.len().div_ceil(usize::from(across)) as u16).clamp(1, 3)
        } else {
            1
        };
        let mut card_height = if shape == CardShape::Tile {
            3
        } else {
            nominal_height
        };
        if remaining < card_height * rows + 1 {
            // Keep the next section recognizable at the viewport edge. It has
            // the same compact thumbnail form as song rows, rather than a
            // partially cut cover. Scrolling brings back the square shelf.
            if remaining >= 4 && shape > CardShape::Text {
                shape = CardShape::Tile;
                card_height = 3;
                rows = 1;
            } else {
                break;
            }
        }
        let height = card_height * rows + 1;
        layouts.push(ShelfLayout {
            index,
            shape,
            area: Rect::new(area.x, y, area.width, height),
            across,
            slot: area.width / across,
            rows,
        });
        y += height + 1;
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
    if layout.rows == 1 {
        super::render_shelf(
            frame,
            shelf,
            area,
            cursor,
            (layout.across, layout.slot),
            tiles,
        );
        return;
    }
    frame.render_widget(
        Paragraph::new(heading_line(shelf, cursor, area.width as usize)),
        Rect::new(area.x, area.y, area.width, 1),
    );
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
                std::slice::from_ref(&shelf),
                Rect::new(1, 2, w, h),
                0,
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
