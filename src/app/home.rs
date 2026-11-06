//! Home's opening shelves match the cover-led layout while keeping real feed data.

use std::time::Instant;

use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent};

use crate::source::Track;
use crate::source::home::{Card, Shelf, Target};
use crate::source::worker::Request;

use super::{App, home};
use super::navigation::View;
use super::now_playing::{QUEUE_AHEAD, Tab, shuffle_tracks};
use super::preferences::VOLUME_STEP;

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

impl App {

    /// The card under the cursor on the landing page.
    pub fn home_card(&self) -> Option<&Card> {
        self.home.get(self.home_shelf)?.cards.get(self.home_card)
    }


    /// Asks for the landing page.
    ///
    /// Not `busy`: this runs at launch, and a busy flag set before the first
    /// frame would hold off the prefetch and spin the event loop at the faster
    /// tick for as long as YouTube took to answer. [`Self::home_pending`] is
    /// what stands in for it, and says only what the pane needs -- the
    /// difference between "loading" and "there is no feed".
    pub(super) fn request_home(&mut self) {
        self.home_generation = self.home_generation.wrapping_add(1);
        let generation = self.home_generation;
        self.home_attempts = 0;
        if self
            .source
            .send(Request::PersonalHome { generation })
            .is_ok()
        {
            self.home_attempts += 1;
        }
        self.home_pending = self.home_attempts > 0;
        if !self.home_pending {
            self.status = "source worker is not running".to_string();
        }
    }


    pub(super) fn refresh_home(&mut self) {
        if self.home_pending {
            return;
        }
        self.status = "refreshing the home feed ...".to_string();
        self.request_home();
    }


    pub(super) fn finish_home_attempts(&mut self) {
        if self.home_attempts != 0 {
            return;
        }
        self.home_pending = false;
        if self.home.is_empty() {
            self.status = "YouTube Music is taking too long -- press r to try again".to_string();
        } else if self.status == "refreshing the home feed ..." {
            self.status =
                "YouTube Music is taking too long -- showing the previous Home".to_string();
        }
    }


    pub(super) fn apply_home(&mut self, mut shelves: Vec<Shelf>) {
        let focused = self.home.get(self.home_shelf).and_then(|shelf| {
            shelf
                .cards
                .get(self.home_card)
                .map(|card| (shelf.title.clone(), card.art_key().to_string()))
        });
        self.status = if shelves.is_empty() {
            "nothing came back from YouTube Music".to_string()
        } else {
            String::new()
        };
        home::arrange(&mut shelves);
        self.home_scroll = vec![0; shelves.len()];
        self.home = shelves;
        if let Some((title, key)) = focused
            && let Some((shelf, card)) = self.home.iter().enumerate().find_map(|(i, shelf)| {
                (shelf.title == title).then(|| {
                    shelf
                        .cards
                        .iter()
                        .position(|card| card.art_key() == key)
                        .map(|j| (i, j))
                })?
            })
        {
            self.home_shelf = shelf;
            self.home_card = card;
            self.home_top = self.home_top.min(shelf);
        } else {
            self.home_shelf = 0;
            self.home_card = 0;
            self.home_top = 0;
        }
        self.prefetch.selection_changed(Instant::now());
    }


    /// Keeps the semantic home cursor valid after a feed replacement.
    pub fn clamp_home_selection(&mut self) {
        // The two lists are kept in step here rather than at every point that
        // could change either: a shelf list that arrives while the cursor is
        // deep in the previous one is the case that would otherwise index out
        // of a `Vec` that had not caught up yet.
        self.home_scroll.resize(self.home.len(), 0);
        self.home_shelf = self.home_shelf.min(self.home.len().saturating_sub(1));

        if self.home.is_empty() {
            self.home_top = 0;
            return;
        }
        let len = self.home[self.home_shelf].cards.len();
        self.home_card = self.home_card.min(len.saturating_sub(1));
    }


    /// Keeps the selected card visible using its shelf's own card capacity.
    pub fn clamp_home_cards(&mut self, cards: usize) {
        if self.home.is_empty() || cards == 0 {
            return;
        }
        let len = self.home[self.home_shelf].cards.len();
        if cards == 0 {
            return;
        }
        let offset = &mut self.home_scroll[self.home_shelf];
        if self.home_card < *offset {
            *offset = self.home_card;
        } else if self.home_card >= *offset + cards {
            *offset = self.home_card + 1 - cards;
        }
        *offset = (*offset).min(len.saturating_sub(cards));
    }


    pub fn clamp_home_grid(&mut self, columns: usize, rows: usize) {
        if rows <= 1 { self.clamp_home_cards(columns); return; }
        if self.home.is_empty() || columns == 0 { return; }
        let len = self.home[self.home_shelf].cards.len();
        let offset = &mut self.home_scroll[self.home_shelf];
        let selected_column = self.home_card / rows;
        let mut first = *offset / rows;
        if selected_column < first { first = selected_column; }
        if selected_column >= first + columns { first = selected_column + 1 - columns; }
        first = first.min(len.div_ceil(rows).saturating_sub(columns));
        *offset = first * rows;
    }


    /// The landing page, which is a grid rather than a list.
    ///
    /// `h`/`l` walk a shelf and `j`/`k` change shelves, with the arrow keys
    /// bound the same way. The arrows seek in the track list and do not here:
    /// there is no fourth direction to give them, and a page whose cursor
    /// cannot be moved with the arrow keys reads as broken far more often than
    /// a page that will not scrub the audio.
    pub(super) fn handle_home_key(&mut self, key: KeyEvent) -> Result<()> {
        let (was_shelf, was_card) = (self.home_shelf, self.home_card);
        let last_shelf = self.home.len().saturating_sub(1);
        let rows = self.home_grid_rows.max(1);
        let last_card = self
            .home
            .get(self.home_shelf)
            .map_or(0, |shelf| shelf.cards.len().saturating_sub(1));

        match key.code {
            KeyCode::Char('q') => self.should_quit = true,
            KeyCode::Char('l') | KeyCode::Right => {
                self.home_card = (self.home_card + rows).min(last_card)
            }
            KeyCode::Char('h') | KeyCode::Left => self.home_card = self.home_card.saturating_sub(rows),
            KeyCode::Char('j') | KeyCode::Down => {
                if self.home_card % rows + 1 < rows && self.home_card < last_card { self.home_card += 1; }
                else { self.home_shelf = (self.home_shelf + 1).min(last_shelf); }
            }
            KeyCode::Char('k') | KeyCode::Up => {
                if !self.home_card.is_multiple_of(rows) { self.home_card -= 1; }
                else { self.home_shelf = self.home_shelf.saturating_sub(1); }
            }
            KeyCode::Char('g') | KeyCode::Home => (self.home_shelf, self.home_card) = (0, 0),
            KeyCode::Char('G') | KeyCode::End => self.home_shelf = last_shelf,
            KeyCode::PageDown => self.home_shelf = (self.home_shelf + 3).min(last_shelf),
            KeyCode::PageUp => self.home_shelf = self.home_shelf.saturating_sub(3),
            KeyCode::Enter => self.open_card(),
            KeyCode::Char('r') => self.refresh_home(),
            KeyCode::Char('/') | KeyCode::Char('i') => self.begin_search(),
            KeyCode::Char('M') => self.begin_music_sign_in(false),
            KeyCode::Char('P') | KeyCode::Char('p') => self.open_player(),
            KeyCode::Char('B') => self.background(),
            KeyCode::Char('D') => self.toggle_presence(),
            KeyCode::Char('S') => self.open_settings(),
            // Back to whatever the track list was showing, when there is one.
            KeyCode::Esc if !self.history.is_empty() => self.go_back(),
            KeyCode::Esc if !self.results.is_empty() => self.view = View::Tracks,
            KeyCode::Char('c') => self.toggle_cover_size(),
            KeyCode::Char(' ') => self.toggle_pause(),
            KeyCode::Char('m') => self.toggle_mute(),
            KeyCode::Char('s') => self.stop(),
            KeyCode::Char('+') | KeyCode::Char('=') => self.nudge_volume(VOLUME_STEP),
            KeyCode::Char('-') | KeyCode::Char('_') => self.nudge_volume(-VOLUME_STEP),
            _ => {}
        }

        // Moving to a shelf shorter than the card index would leave the cursor
        // past its end until the next redraw clamped it. Landing on the last
        // card is the closest thing to keeping the user's place across a move
        // that a ragged grid allows.
        if self.home_shelf != was_shelf {
            self.home_card = self.home_card.min(
                self.home
                    .get(self.home_shelf)
                    .map_or(0, |shelf| shelf.cards.len().saturating_sub(1)),
            );
        }
        // Same debounce as the results list, and for the same reason.
        if (self.home_shelf, self.home_card) != (was_shelf, was_card) {
            self.prefetch.selection_changed(Instant::now());
        }
        Ok(())
    }


    /// Enter on a card: play it, or open what it stands for.
    pub(super) fn open_card(&mut self) {
        let Some(card) = self.home_card().cloned() else {
            return;
        };
        self.activate_card(card);
    }


    /// Starts the ordinary endless station for the selected song. This shares
    /// the same watch request as a normal play; the separate action is an
    /// explicit promise about what will follow the seed, not another fetch or
    /// another queue held in memory.
    pub(super) fn start_home_radio(&mut self) {
        let Some(track) = self.home_card().and_then(Card::track) else {
            return;
        };
        self.play_track(track, false);
    }


    /// Plays the playable cards in the current shelf as one finite queue. The
    /// cards already hold all metadata needed here; no collection page is
    /// fetched, and the queue is capped at the same fixed ahead window as a
    /// radio page.
    pub(super) fn play_home_shelf(&mut self, shuffle: bool) {
        let Some(shelf) = self.home.get(self.home_shelf) else {
            return;
        };
        let title = shelf.title.clone();
        let mut tracks: Vec<Track> = shelf
            .cards
            .iter()
            .filter_map(Card::track)
            .take(QUEUE_AHEAD + 1)
            .collect();
        if tracks.is_empty() {
            self.status = "this shelf has nothing directly playable".to_string();
            return;
        }
        if shuffle {
            shuffle_tracks(&mut tracks);
        }

        let first = tracks[0].clone();
        self.play_track(first, false);
        self.queue_epoch = self.queue_epoch.wrapping_add(1);
        if let Some(now) = self.now.as_mut() {
            now.queue_title = title.clone();
            now.queue = tracks;
            now.queue_epoch = self.queue_epoch;
            now.continuation = None;
            now.topping_up = false;
            now.topup_failures = 0;
            now.dropped.clear();
            now.playing = Some(0);
            now.cursor[Tab::UpNext.index()] = 0;
        }
        self.status = if shuffle { "Shuffled shelf".into() } else { String::new() };
    }


    pub(super) fn activate_card(&mut self, card: Card) {
        if let Some(track) = card.track() {
            self.play_track(track, false);
            return;
        }

        match card.target {
            Target::Play { .. } => {}
            Target::Open { endpoint } => self.open_browse(endpoint, card.title),
            Target::Artist { artist } => self.open_artist(artist),
        }
    }
}
