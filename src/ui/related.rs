//! Recommendations use the same artwork cards as browsing, with exact hit areas.
use super::*;

pub(super) struct Geometry {
    rows: Vec<(usize, Rect)>,
}

impl Geometry {
    pub fn new(now: &NowPlaying, area: Rect) -> Self {
        let rows = now.related_rows();
        if rows.is_empty() || area.is_empty() {
            return Self { rows: Vec::new() };
        }
        let cursor = now.cursor().min(rows.len() - 1);
        let heading = if area.height >= 14 { 2 } else { 1 };
        let card = match area.height {
            0..=3 => 1,
            4..=13 => 2,
            _ => 3,
        };
        let gap = u16::from(area.height >= 18);
        let height = |index: usize| match rows[index] {
            RelatedRow::Heading(_) => usize::from(heading),
            RelatedRow::Card(_) => usize::from(card + gap),
        };
        let selected_height = match rows[cursor] {
            RelatedRow::Heading(_) => heading,
            RelatedRow::Card(_) => card,
        };
        let mut needed = (0..cursor).map(height).sum::<usize>() + usize::from(selected_height);
        let mut first = 0;
        while first < cursor && needed > usize::from(area.height) {
            needed -= height(first);
            first += 1;
        }
        let mut visible = Vec::new();
        let mut y = area.y;
        // Retain the shelf name when scrolling deep into its recommendations.
        if matches!(rows[first], RelatedRow::Card(_)) && area.height >= 4 {
            while first < cursor && needed > usize::from(area.height - heading) {
                needed -= height(first);
                first += 1;
            }
            let context = (0..first)
                .rev()
                .find(|&index| matches!(rows[index], RelatedRow::Heading(_)));
            if let Some(index) = context
                && matches!(rows[first], RelatedRow::Card(_))
            {
                visible.push((index, Rect::new(area.x, y, area.width, heading)));
                y += heading;
            }
        }
        for (index, row) in rows.iter().enumerate().skip(first) {
            let row_height = match row {
                RelatedRow::Heading(_) => heading,
                RelatedRow::Card(_) => card,
            };
            if y + row_height > area.bottom() {
                break;
            }
            visible.push((index, Rect::new(area.x, y, area.width, row_height)));
            y += row_height;
            if matches!(row, RelatedRow::Card(_)) {
                y += gap;
            }
        }
        Self { rows: visible }
    }
}

pub(super) fn register_targets(mouse: &mut MouseMap, now: &NowPlaying, area: Rect) {
    let rows = now.related_rows();
    for (index, area) in Geometry::new(now, area).rows {
        if matches!(rows[index], RelatedRow::Card(_)) {
            mouse
                .targets
                .push(MouseTarget::Area(area, MouseAction::OpenPageRow(index)));
        }
    }
}

pub(super) fn render(
    frame: &mut Frame,
    now: &NowPlaying,
    area: Rect,
    accent: Color,
    tiles: &mut Tiles<'_>,
) -> usize {
    if !matches!(now.related, Panel::Ready(_)) {
        message(frame, waiting_on(&now.related), area);
        return 0;
    }
    let rows = now.related_rows();
    if rows.is_empty() {
        message(frame, "No recommendations for this track", area);
    }
    for (index, rect) in Geometry::new(now, area).rows {
        match rows[index] {
            RelatedRow::Heading(title) => frame.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled("  ", Style::default().fg(accent)),
                    Span::styled(
                        truncate(title, usize::from(rect.width.saturating_sub(2))),
                        Style::default()
                            .fg(Color::Gray)
                            .add_modifier(Modifier::BOLD),
                    ),
                ])),
                rect,
            ),
            RelatedRow::Card(card) => {
                tiles.shape = if rect.height >= 2 && rect.width >= 16 {
                    CardShape::Tile
                } else {
                    CardShape::Text
                };
                render_card(frame, card, rect, index == now.cursor(), tiles);
            }
        }
    }
    rows.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_selected_card_is_fully_visible_and_headings_and_gaps_are_inert() {
        let mut now = super::super::tests::playing();
        now.tab = Tab::Related;
        now.related = Panel::Ready(vec![
            Shelf {
                title: "You might also like".into(),
                cards: (0..30)
                    .map(|index| super::super::tests::song(&format!("Song {index}"), "Artist"))
                    .collect(),
            },
            Shelf {
                title: "Recommended playlists".into(),
                cards: (0..10)
                    .map(|index| {
                        super::super::tests::song(&format!("Playlist {index}"), "Collection")
                    })
                    .collect(),
            },
        ]);
        for (width, height) in [(1, 1), (20, 4), (40, 8), (70, 20), (100, 40)] {
            let area = Rect::new(2, 3, width, height);
            for cursor in (1..42)
                .filter(|&index| matches!(now.related_rows()[index], RelatedRow::Card(_)))
                .collect::<Vec<_>>()
            {
                now.cursor[Tab::Related.index()] = cursor;
                let mut mouse = MouseMap::default();
                register_targets(&mut mouse, &now, area);
                let geometry = Geometry::new(&now, area);
                assert!(geometry.rows.iter().any(|(index, _)| *index == cursor));
                for (index, rect) in &geometry.rows {
                    assert_eq!(rect.intersection(area), *rect);
                    let expected = if matches!(now.related_rows()[*index], RelatedRow::Card(_)) {
                        Some(MouseAction::OpenPageRow(*index))
                    } else {
                        None
                    };
                    assert_eq!(mouse.action_at(rect.x, rect.y), expected);
                    assert_eq!(
                        mouse.action_at(rect.right() - 1, rect.bottom() - 1),
                        expected
                    );
                }
                for y in area.y..area.bottom() {
                    if !geometry
                        .rows
                        .iter()
                        .any(|(_, rect)| contains(*rect, area.x, y))
                    {
                        assert_eq!(mouse.action_at(area.x, y), None);
                    }
                }
            }
        }
    }
}
