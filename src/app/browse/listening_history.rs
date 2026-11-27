//! The local listening journal as a chronological, replayable list.

use std::{sync::Arc, time::Duration};

use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent};

use crate::app::{App, Mode, View};
use crate::app::browse::navigation::{PageRequestKind, moved_cursor};
use crate::source::{Track, listening::journal::Play, worker::Request};

#[derive(Debug, Clone, Default)]
pub(crate) struct State {
    pub rows: Arc<[Play]>,
    pub selected: usize,
    pub offset: usize,
    pub visible: usize,
    pub error: Option<String>,
}

impl State {
    pub(crate) fn viewport(&mut self, visible: usize) {
        self.visible = visible;
        let last = self.rows.len().saturating_sub(1);
        self.selected = self.selected.min(last);
        if visible == 0 { return; }
        self.offset = self.offset.min(self.selected)
            .max((self.selected + 1).saturating_sub(visible))
            .min(self.rows.len().saturating_sub(visible));
    }

    pub(crate) fn track(&self, index: usize) -> Option<Track> {
        let row = self.rows.get(index)?;
        Some(Track {
            id: row.id.clone(), title: row.title.clone(), uploader: row.artist.clone(),
            album: row.album.clone(), duration: row.duration.map(Duration::from_secs), artist_ref: None,
        })
    }
}

impl App {
    pub(in crate::app) fn open_listening_history(&mut self) {
        if self.snapshot_blocked() || self.mode == Mode::Editing { return; }
        if self.view != View::ListeningHistory {
            self.push_current_page();
            self.listening_history = State::default();
        }
        self.view = View::ListeningHistory;
        self.status.clear();
        self.refresh_listening_history();
    }

    pub(in crate::app) fn refresh_listening_history(&mut self) {
        if self.page_loading() { return; }
        self.listening_history.error = None;
        let request_id = self.begin_page_request(PageRequestKind::ListeningHistory);
        if self.source.send(Request::ListeningHistory { request_id }).is_err() {
            self.cancel_page_request();
            self.listening_history.error = Some("Could not open listening history.".into());
        }
    }

    pub(in crate::app) fn apply_listening_history(&mut self, plays: Result<Vec<Play>, String>) {
        if self.view != View::ListeningHistory { return; }
        match plays {
            Ok(plays) => {
                self.listening_history.rows = plays.into();
                self.listening_history.selected = 0;
                self.listening_history.offset = 0;
            }
            Err(error) => self.listening_history.error = Some(error),
        }
    }

    pub(in crate::app) fn play_history_track(&mut self, index: usize) {
        if self.page_loading() || self.listening_history.error.is_some() { return; }
        if let Some(track) = self.listening_history.track(index) {
            self.listening_history.selected = index;
            self.play_track(track, false);
        }
    }

    pub(in crate::app) fn handle_listening_history_key(&mut self, key: KeyEvent) -> Result<()> {
        let history = &mut self.listening_history;
        let delta = match key.code {
            KeyCode::Down | KeyCode::Char('j') => 1,
            KeyCode::Up | KeyCode::Char('k') => -1,
            KeyCode::PageDown => history.visible.max(1) as isize,
            KeyCode::PageUp => -(history.visible.max(1) as isize),
            _ => 0,
        };
        if delta != 0 {
            history.selected = moved_cursor(history.selected, delta, history.rows.len());
            return Ok(());
        }
        match key.code {
            KeyCode::Home | KeyCode::Char('g') => history.selected = 0,
            KeyCode::End | KeyCode::Char('G') => history.selected = history.rows.len().saturating_sub(1),
            KeyCode::Enter => {
                let selected = history.selected;
                self.play_history_track(selected);
            }
            KeyCode::Char('r') => self.refresh_listening_history(),
            KeyCode::Char('/') | KeyCode::Char('i') => self.begin_search(),
            KeyCode::Esc => self.go_back(),
            KeyCode::Char('q' | ' ' | 'm' | 's' | 'p' | 'P' | 'H' | 'M' | 'D' | 'S' | 'B'
                | '+' | '=' | '-' | '_') | KeyCode::Left | KeyCode::Right => return self.handle_browse_key(key),
            _ => {}
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::MouseAction;
    use crate::source::worker::Response;

    #[test]
    fn history_opens_while_signed_out_and_ignores_a_reply_after_back() {
        let (mut app, _, requests) = crate::app::testing::scenarios::fixture();
        app.view = View::Library;
        assert!(!app.library.connected);
        app.handle_mouse_action(MouseAction::OpenListeningHistory).unwrap();
        assert_eq!(app.view, View::ListeningHistory);
        let Request::ListeningHistory { request_id } = requests.try_recv().unwrap() else {
            panic!("expected a local history read");
        };
        app.handle_mouse_action(MouseAction::GoBack).unwrap();
        app.apply(Response::ListeningHistory { request_id, plays: Err("stale reply".into()) });
        assert_eq!(app.view, View::Library);
        assert!(app.listening_history.error.is_none());
    }
}
