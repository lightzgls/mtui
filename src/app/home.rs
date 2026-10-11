//! Home's opening shelves match the cover-led layout while keeping real feed data.

use crate::source::home::Shelf;

pub(crate) fn compact(shelf: &Shelf) -> bool {
    let title = shelf.title.to_lowercase();
    shelf.cards.iter().all(|card| card.is_playable())
        && (title.contains("quick pick")
            || title.contains("from your listening")
            || title.contains("trending songs"))
}

pub(crate) fn arrange(shelves: &mut [Shelf]) {
    let featured = shelves
        .iter()
        .position(|shelf| {
            shelf.title.eq_ignore_ascii_case("Listen again") && !shelf.cards.is_empty()
        })
        .or_else(|| {
            shelves
                .iter()
                .position(|shelf| !compact(shelf) && !shelf.cards.is_empty())
        });
    if let Some(index) = featured {
        shelves[..=index].rotate_right(1);
    }
    let quick = shelves
        .iter()
        .enumerate()
        .skip(1)
        .find_map(|(index, shelf)| {
            (shelf.title.to_lowercase().contains("quick pick") && compact(shelf)).then_some(index)
        });
    if let Some(index) = quick {
        shelves[1..=index].rotate_right(1);
    }
}
