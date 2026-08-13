//! Artwork for the landing page: the sleeves behind the cards, held only for
//! as long as they are worth holding.
//!
//! The cover pane keeps exactly one picture, because exactly one track plays.
//! A grid is the other shape of problem: a feed is twelve shelves of up to
//! twenty-four cards, a screen shows a dozen of them, and the user scrolls
//! through the rest. Fetching all three hundred would be absurd, and fetching
//! only what is on screen and forgetting it would re-fetch the same tile every
//! time the cursor moved back a row.
//!
//! So this is a cache, and being a cache it has to be bounded -- in entries,
//! and more importantly in how big an entry can get. Both bounds are set for
//! the same reason the rest of this program is the size it is: see [`EDGE`] and
//! [`CAPACITY`] for the arithmetic. Full together they come to roughly a
//! megabyte, which is a third of what a single full-size cover costs.
//!
//! Nothing here fetches. The cache records what is wanted, the worker's art
//! thread answers, and [`ArtCache::store`] puts the answer away -- so a slow or
//! unreachable CDN can never do worse than leave the cards without pictures.

use std::collections::{HashMap, HashSet, VecDeque};

use crate::source::cover::Cover;

/// Longest edge an entry is kept at, in image pixels.
///
/// A card tile is drawn with half-block cells -- one pixel per column, two per
/// row -- so even a poster card on a wide terminal is about forty pixels
/// across. Sixty-four is comfortable headroom over that and costs 12 KB per
/// entry, where keeping the 226-pixel copy YouTube serves would cost 150 KB to
/// draw the same forty pixels.
pub const EDGE: u32 = 64;

/// Entries kept before the least recently wanted is dropped.
///
/// Two or three screenfuls of a wide terminal: enough that scrolling down a
/// page and back finds the tiles still there, and far short of the whole feed.
/// At [`EDGE`] that is about 800 KB.
pub(crate) const CAPACITY: usize = 64;

#[derive(Default)]
pub struct ArtCache {
    art: HashMap<String, Cover>,
    /// Every key that has been asked for, whether or not a picture came back.
    ///
    /// Separate from `art` because the two answer different questions. `art`
    /// says what can be drawn; this says what need not be requested -- and the
    /// difference between them is precisely the cards whose artwork 404'd or
    /// would not decode. Without it, those cards would be re-requested on every
    /// frame they stayed on screen.
    asked: HashSet<String>,
    /// Keys in the order they were last wanted, oldest first. A `VecDeque`
    /// rather than a timestamp per entry: the list is sixty-odd long, so
    /// finding and moving a key is cheaper than it looks and needs no clock.
    recent: VecDeque<String>,
}

impl ArtCache {
    /// The picture for a card, if one has arrived.
    pub fn get(&self, key: &str) -> Option<&Cover> {
        self.art.get(key)
    }

    /// Notes that a card is on screen, and says whether its artwork has to be
    /// asked for.
    ///
    /// Marks it wanted when it answers `true`, so a card drawn on every frame
    /// for the second it takes the CDN to answer produces one request rather
    /// than a hundred.
    pub fn want(&mut self, key: &str) -> bool {
        self.touch(key);
        self.asked.insert(key.to_string())
    }

    /// Files an answer from the art thread. `None` means there was no usable
    /// picture, which needs nothing stored: [`Self::asked`] already records
    /// that the question was put.
    pub fn store(&mut self, key: String, art: Option<Cover>) {
        if let Some(cover) = art {
            self.art.insert(key.clone(), cover);
        }
        self.asked.insert(key);
        self.evict();
    }

    /// Moves `key` to the newest end of the recency list.
    fn touch(&mut self, key: &str) {
        if let Some(at) = self.recent.iter().position(|held| held == key) {
            let held = self.recent.remove(at).expect("position() found it");
            self.recent.push_back(held);
        } else {
            self.recent.push_back(key.to_string());
        }
    }

    /// Drops the least recently wanted entries until the cache is back inside
    /// [`CAPACITY`].
    ///
    /// A key still waiting on the art thread is dropped here like any other,
    /// and its answer will then be filed under a key nothing is looking at --
    /// which the next [`Self::want`] simply asks for again. That is the right
    /// trade: the alternative is a pinned entry per outstanding request, which
    /// is an unbounded set held open by whatever is slowest to answer.
    fn evict(&mut self) {
        while self.recent.len() > CAPACITY {
            let Some(oldest) = self.recent.pop_front() else {
                return;
            };
            self.art.remove(&oldest);
            self.asked.remove(&oldest);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cover() -> Cover {
        Cover::solid(8, 8)
    }

    #[test]
    fn a_card_is_asked_for_once() {
        let mut cache = ArtCache::default();
        assert!(cache.want("a"), "the first frame has to ask");
        assert!(!cache.want("a"), "and no frame after it may ask again");
    }

    #[test]
    fn a_stored_picture_comes_back() {
        let mut cache = ArtCache::default();
        cache.want("a");
        cache.store("a".to_string(), Some(cover()));
        assert!(cache.get("a").is_some());
        assert!(!cache.want("a"));
    }

    #[test]
    fn art_that_never_arrives_is_not_asked_for_again() {
        // The point of remembering a failure: a 404 must not become a request
        // per frame for as long as the card is on screen.
        let mut cache = ArtCache::default();
        cache.want("a");
        cache.store("a".to_string(), None);
        assert!(cache.get("a").is_none());
        assert!(
            !cache.want("a"),
            "a known-missing picture is not re-requested"
        );
    }

    #[test]
    fn the_cache_stays_inside_its_capacity() {
        let mut cache = ArtCache::default();
        for i in 0..CAPACITY * 2 {
            let key = i.to_string();
            cache.want(&key);
            cache.store(key, Some(cover()));
        }
        assert!(cache.art.len() <= CAPACITY, "held {}", cache.art.len());
        assert!(cache.asked.len() <= CAPACITY);
        assert_eq!(cache.recent.len(), CAPACITY);
    }

    #[test]
    fn the_oldest_unwanted_entry_is_the_one_dropped() {
        let mut cache = ArtCache::default();
        for i in 0..CAPACITY {
            let key = i.to_string();
            cache.want(&key);
            cache.store(key, Some(cover()));
        }
        // Ask for the oldest again, so the second-oldest becomes the victim.
        cache.want("0");
        cache.want("fresh");
        cache.store("fresh".to_string(), Some(cover()));

        assert!(cache.get("0").is_some(), "re-wanting it kept it");
        assert!(cache.get("1").is_none(), "the next oldest went instead");
        assert!(cache.get("fresh").is_some());
    }
}
