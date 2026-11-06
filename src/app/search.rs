//! Search input, result selection, and request dispatch.

use std::time::Instant;

use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent};

use crate::source::Track;
use crate::source::worker::Request;
use crate::source::youtube::{MAX_RESULTS, extract_video_id};

use super::{App, search_input};
use super::navigation::{HistoryEntry, Mode, PageRequestKind, View, page_behind_search};
use super::preferences::VOLUME_STEP;

impl App {

    pub fn search_results_query(&self) -> &str {
        if self.mode == Mode::Editing && let Some(HistoryEntry::Tracks(page)) = &self.search_page {
            &page.query
        } else { &self.query }
    }


    /// Keeps `offset` such that `selected` is visible in a viewport `height`
    /// rows tall. Called by the renderer, which is what knows the height.
    pub fn clamp_scroll(&mut self, height: usize) {
        if height == 0 || self.result_count() == 0 {
            self.offset = 0;
            return;
        }
        if self.selected < self.offset {
            self.offset = self.selected;
        } else if self.selected >= self.offset + height {
            self.offset = self.selected + 1 - height;
        }
        // Avoid a trailing gap when the list shrinks.
        let max_offset = self.result_count().saturating_sub(height);
        self.offset = self.offset.min(max_offset);
    }


    pub(super) fn begin_search(&mut self) {
        if self.snapshot_blocked() {
            self.status = "wait for the current page to finish loading".to_string();
            return;
        }
        // Opening the App Menu while editing does not end the edit. Choosing
        // Search from it must therefore retain the page already remembered.
        if self.mode != Mode::Editing {
            self.search_origin = self.view;
            self.search_page = self.current_page();
        }
        if self.view == View::Library { self.library.suspend(); }
        self.mode = Mode::Editing;
        self.query_cursor = self.query.len();
        self.status.clear();
    }


    pub(super) fn handle_editing_key(&mut self, key: KeyEvent) -> Result<()> {
        if let Some(changed) = search_input::edit(&mut self.query, &mut self.query_cursor, key) {
            if changed {
                self.cancel_search_request();
                self.status.clear();
            }
            return Ok(());
        }
        match key.code {
            KeyCode::Enter => self.submit_search(),
            KeyCode::Esc => {
                self.cancel_search_request();
                if let Some(origin) = self.search_page.take() {
                    self.restore_page(origin);
                } else {
                    self.mode = Mode::Browse;
                    self.view = self.search_origin;
                }
            }
            _ => {}
        }
        Ok(())
    }


    pub(super) fn handle_browse_key(&mut self, key: KeyEvent) -> Result<()> {
        let was_selected = self.selected;
        match key.code {
            KeyCode::Char('q') => self.should_quit = true,
            KeyCode::Char('/') | KeyCode::Char('i') => self.begin_search(),
            KeyCode::Char('j') | KeyCode::Down => self.move_selection(1),
            KeyCode::Char('k') | KeyCode::Up => self.move_selection(-1),
            KeyCode::Char('g') | KeyCode::Home => self.selected = 0,
            KeyCode::Char('G') | KeyCode::End => {
                self.selected = self.result_count().saturating_sub(1);
            }
            KeyCode::PageDown => self.move_selection(10),
            KeyCode::PageUp => self.move_selection(-10),
            KeyCode::Enter => self.play_selected(),
            KeyCode::Char(c @ '1'..='6') if self.browsing.is_none() => {
                self.filter_search(crate::source::search::Filter::ALL[(c as u8 - b'1') as usize]);
            }
            KeyCode::Char('a') if self.browsing.is_some() => self.play_collection(0, false),
            KeyCode::Char('z') if self.browsing.is_some() => self.play_collection(0, true),
            KeyCode::Char('r') if self.browsing.is_some() => self.reload_collection(),
            KeyCode::Char(' ') => self.toggle_pause(),
            KeyCode::Char('m') => self.toggle_mute(),
            KeyCode::Char('c') => self.toggle_cover_size(),
            KeyCode::Char('s') => self.stop(),
            // Both cases, for the same reason as `l` below: this is the way back
            // to a track that is already playing, and a user looking for it is
            // not thinking about the shift key. Lower `p` is free in every list
            // view -- it means "previous" only on the player page, which is the
            // one place this key has nothing to do.
            KeyCode::Char('P') | KeyCode::Char('p') => self.open_player(),
            // Upper case only, unlike `P` directly above. The shift key is worth
            // asking for here for the same reason `A` asks for it: this one
            // takes the interface away, and finding the way back means finding
            // an icon in the notification area. `P` costs a keypress to undo.
            KeyCode::Char('B') => self.background(),
            KeyCode::Char('D') => self.toggle_presence(),
            KeyCode::Char('S') => self.open_settings(),
            KeyCode::Char('+') | KeyCode::Char('=') => self.nudge_volume(VOLUME_STEP),
            KeyCode::Char('-') | KeyCode::Char('_') => self.nudge_volume(-VOLUME_STEP),
            KeyCode::Right => self.seek_relative(5),
            KeyCode::Left => self.seek_relative(-5),
            // Both cases: `l` is otherwise unbound here, and a user who reaches
            // for the library is not thinking about the shift key.
            KeyCode::Char('M') => self.begin_music_sign_in(false),
            KeyCode::Esc if !self.history.is_empty() => self.go_back(),
            KeyCode::Esc | KeyCode::Char('H') => self.go_home(),
            _ => {}
        }
        // Restarting the debounce here rather than in each movement arm catches
        // every path that moves the cursor, including ones added later.
        if self.selected != was_selected {
            self.prefetch.selection_changed(Instant::now());
        }
        Ok(())
    }


    pub(super) fn move_selection(&mut self, delta: isize) {
        if self.result_count() == 0 {
            return;
        }
        let last = self.result_count() - 1;
        let next = self.selected as isize + delta;
        self.selected = next.clamp(0, last as isize) as usize;
    }


    pub(super) fn submit_search(&mut self) {
        let query = self.query.trim().to_string();
        if query.is_empty() {
            self.status = "enter something to search for".to_string();
            return;
        }
        self.query = query.clone();
        self.query_cursor = self.query.len();
        if self
            .pending_page_request
            .is_some_and(|(_, kind)| kind != PageRequestKind::Search)
        {
            self.status = "wait for the page behind search to finish loading".to_string();
            return;
        }

        // A pasted link or bare video id plays directly; the search box doubles
        // as an address bar. Everything the page needs beyond the id -- the
        // real title, the artist, the queue -- arrives with the watch response.
        if let Some(id) = extract_video_id(&query) {
            // `play_track` records the view it is called from as Player's back
            // target. Search uses Tracks as its editing surface, not as that
            // target, so restore the page the edit hid before entering Player.
            self.cancel_search_request();
            if let Some(origin) = self.search_page.take() {
                self.restore_page(origin);
            } else {
                self.mode = Mode::Browse;
                self.view = page_behind_search(self.search_origin, self.back_to);
            }
            return self.play_track(
                Track {
                    id,
                    title: query,
                    uploader: String::new(),
                    duration: None,
                    album: None,
                    artist_ref: None,
                },
                false,
            );
        }

        self.status = "Searching…".into();
        let request_id = self.begin_page_request(PageRequestKind::Search);
        let request = Request::Search {
            request_id,
            query,
            limit: 60.min(MAX_RESULTS),
            filter: crate::source::search::Filter::All,
        };
        self.search_filter = crate::source::search::Filter::All;
        if self.source.send(request).is_err() {
            self.cancel_page_request();
            self.status = "source worker is not running".to_string();
        }
    }


    pub fn result_count(&self) -> usize {
        if self.browsing.is_none() && !self.search_items.is_empty() {
            self.search_items.len()
        } else { self.results.len() }
    }


    pub(super) fn selected_result_track(&self) -> Option<&Track> {
        if self.browsing.is_none() && !self.search_items.is_empty() {
            self.search_items.get(self.selected)?.track.as_ref()
        } else { self.results.get(self.selected) }
    }


    pub(super) fn filter_search(&mut self, filter: crate::source::search::Filter) {
        if self.busy || self.query.trim().is_empty() { return; }
        self.search_filter = filter;
        self.status = "Searching…".into();
        let request_id = self.begin_page_request(PageRequestKind::Search);
        if self.source.send(Request::Search {
            request_id, query: self.query.clone(), limit: 60.min(MAX_RESULTS), filter,
        }).is_err() { self.cancel_page_request(); }
    }
}
