//! Artist song expansion and ordered playback retain the artist as Back's target.
use std::collections::HashSet;
use std::time::Instant;

use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent};

use crate::source::{ArtistRef, Track, UNKNOWN_ARTIST};
use crate::source::catalog::artist::{ArtistPage, ArtistSong};
use crate::source::catalog::home::{Card, Target};
use crate::source::worker::{PageRequestId, Request};

use crate::app::App;
use crate::app::browse::navigation::{Mode, PageRequestKind, View};
use crate::app::playback::now_playing::{Panel, Tab};
use crate::app::controls::preferences::VOLUME_STEP;

impl App {
    pub(in crate::app) fn more_artist_songs(&mut self, play: bool) {
        let Some(view) = self.artist.as_mut() else { return; };
        if let Some((_, play_after)) = view.songs_request.as_mut() {
            *play_after |= play;
            return;
        }
        let Some(page) = view.content() else { return; };
        let endpoint = page.songs_endpoint.clone();
        if let Some(endpoint) = endpoint {
            let artist = view.requested.clone();
            self.page_request = self.page_request.wrapping_add(1).max(1);
            let request_id = self.page_request;
            view.songs_request = Some((request_id, play));
            view.songs_error = None;
            self.status = format!("Loading all top songs by {}…", artist.name);
            if self.source.send(Request::ArtistSongs { request_id, artist, endpoint }).is_err() {
                view.songs_request = None;
                view.songs_error = Some("Music service is unavailable. Try again.".into());
                self.status = view.songs_error.clone().unwrap();
            }
        } else if play {
            self.play_artist_songs();
        } else {
            view.songs_expanded = !view.songs_expanded;
            view.section = 0;
            view.top = 0;
            view.song = 0;
            view.song_offset = 0;
        }
    }

    pub(in crate::app) fn apply_artist_songs(&mut self, request_id: PageRequestId, browse_id: &str,
        result: Result<crate::source::catalog::collection::Page, String>) {
        if self.view != View::Artist { return; }
        let Some(_) = self.artist.as_ref().filter(|view|
            view.requested.endpoint.browse_id == browse_id
                && view.songs_request.is_some_and(|(id, _)| id == request_id)) else { return; };
        self.close_page_actions();
        let view = self.artist.as_mut().expect("validated artist request");
        let play = view.songs_request.take().is_some_and(|(_, play)| play);
        match result {
            Err(error) => {
                view.songs_error = Some(error);
                self.status = "Could not load more top songs. Choose Retry more.".into();
            }
            Ok(listing) => {
                let Panel::Ready(page) = &mut view.page else { return; };
                let mut seen = HashSet::new();
                let songs: Vec<_> = listing.tracks.into_iter().filter(|track| seen.insert(track.id.clone()))
                    .map(|mut track| {
                        let preview = page.top_songs.iter().find(|song| song.track.id == track.id);
                        if track.album.is_none() { track.album = preview.and_then(|song| song.track.album.clone()); }
                        if track.uploader.is_empty() || track.uploader == UNKNOWN_ARTIST {
                            track.uploader = page.artist.name.clone();
                        }
                        if track.artist_ref.is_none() { track.artist_ref = Some(page.artist.clone()); }
                        ArtistSong { track, plays: preview.and_then(|song| song.plays.clone()) }
                    }).collect();
                if songs.is_empty() {
                    view.songs_error = Some("The song list was empty. Try again.".into());
                    self.status = "No additional songs loaded. Your artist preview is still available.".into();
                    return;
                }
                page.top_songs = songs;
                if !listing.details.truncated { page.songs_endpoint = None; }
                page.songs_truncated = listing.details.truncated;
                view.songs_expanded = true;
                view.songs_error = None;
                self.status = if page.songs_truncated {
                    "The provider's top-song list is incomplete.".into()
                } else { String::new() };
                if play { self.play_artist_songs(); }
            }
        }
    }

    fn play_artist_songs(&mut self) {
        let Some(page) = self.artist.as_ref().and_then(ArtistView::content) else { return; };
        let tracks: Vec<_> = page.top_songs.iter().map(|song| song.track.clone()).collect();
        let Some(first) = tracks.first().cloned() else { return; };
        let title = format!("{} · Top songs", page.artist.name);
        let truncated = page.songs_truncated;
        self.play_track(first, false);
        self.queue_epoch = self.queue_epoch.wrapping_add(1);
        if let Some(now) = self.now.as_mut() {
            now.queue = tracks;
            now.queue_title = title.clone();
            now.queue_epoch = self.queue_epoch;
            now.finite_queue = true;
            now.continuation = None;
            now.topping_up = false;
            now.playing = Some(0);
            now.open(Tab::UpNext);
            now.cursor[Tab::UpNext.index()] = 0;
        }
        self.status = if truncated { "The loaded top-song list is incomplete.".into() } else { String::new() };
    }
}

/// One artist page and its independent cursor state.
#[derive(Debug, Clone)]
pub struct ArtistView {
    pub requested: ArtistRef,
    pub page: Panel<ArtistPage>,
    /// Section zero is Top songs when present; the remaining sections are the
    /// catalogue shelves in API order.
    pub section: usize,
    pub song: usize,
    pub song_offset: usize,
    pub card: usize,
    pub top: usize,
    pub scroll: Vec<usize>,
    pub songs_expanded: bool,
    pub songs_request: Option<(PageRequestId, bool)>,
    pub songs_error: Option<String>,
}

impl ArtistView {
    pub(in crate::app) fn loading(artist: ArtistRef) -> Self {
        Self {
            requested: artist,
            page: Panel::Loading,
            section: 0,
            song: 0,
            song_offset: 0,
            card: 0,
            top: 0,
            scroll: Vec::new(),
            songs_expanded: false,
            songs_request: None,
            songs_error: None,
        }
    }

    pub fn content(&self) -> Option<&ArtistPage> {
        match &self.page {
            Panel::Ready(page) => Some(page),
            _ => None,
        }
    }

    pub(in crate::app) fn set_page(&mut self, page: Result<ArtistPage, String>) {
        match page {
            Ok(page) => {
                self.requested = page.artist.clone();
                self.scroll = vec![0; page.shelves.len()];
                self.page = Panel::Ready(page);
            }
            Err(reason) => self.page = Panel::Empty(reason),
        }
        self.section = 0;
        self.song = 0;
        self.song_offset = 0;
        self.card = 0;
        self.top = 0;
        self.songs_expanded = false;
        self.songs_request = None;
        self.songs_error = None;
    }

    pub(in crate::app) fn has_songs(&self) -> bool {
        self.content()
            .is_some_and(|page| !page.top_songs.is_empty())
    }

    pub(in crate::app) fn shelf_index(&self) -> Option<usize> {
        let first = usize::from(self.has_songs());
        self.section.checked_sub(first)
    }

    pub fn selected_song(&self) -> Option<&ArtistSong> {
        (self.has_songs() && self.section == 0)
            .then(|| self.content()?.top_songs.get(self.song))
            .flatten()
    }

    pub fn selected_card(&self) -> Option<&Card> {
        let shelf = self.shelf_index()?;
        self.content()?.shelves.get(shelf)?.cards.get(self.card)
    }

    pub(in crate::app) fn selected_track(&self) -> Option<&Track> {
        self.selected_song().map(|song| &song.track)
    }
}

pub(in crate::app) fn card_artist(card: &Card) -> Option<ArtistRef> {
    match &card.target {
        Target::Artist { artist } => Some(artist.clone()),
        Target::Play { .. } | Target::Open { .. } => card.artist_ref.clone(),
    }
}

impl App {

    pub(in crate::app) fn open_artist(&mut self, artist: ArtistRef) {
        if self.snapshot_blocked() {
            self.status = "wait for the current page to finish loading".to_string();
            return;
        }
        self.push_current_page();
        self.status = "Loading artist…".into();
        self.artist = Some(ArtistView::loading(artist.clone()));
        self.view = View::Artist;
        self.mode = Mode::Browse;
        let request_id = self.begin_page_request(PageRequestKind::Artist);
        let request = Request::OpenArtist { request_id, artist };
        if self.source.send(request).is_err() {
            self.cancel_page_request();
            if let Some(artist) = self.artist.as_mut() {
                artist.page = Panel::Empty("source worker is not running".to_string());
            }
            self.status = "source worker is not running".to_string();
        }
    }


    pub(in crate::app) fn reload_artist(&mut self) {
        if self.snapshot_blocked() {
            self.status = "wait for the current artist to finish loading".to_string();
            return;
        }
        let Some(artist) = self.artist.as_ref().map(|page| page.requested.clone()) else {
            return;
        };
        self.status = "Refreshing artist…".into();
        self.artist = Some(ArtistView::loading(artist.clone()));
        self.view = View::Artist;
        self.mode = Mode::Browse;
        let request_id = self.begin_page_request(PageRequestKind::Artist);
        if self
            .source
            .send(Request::OpenArtist { request_id, artist })
            .is_err()
        {
            self.cancel_page_request();
            if let Some(artist) = self.artist.as_mut() {
                artist.page = Panel::Empty("source worker is not running".to_string());
            }
            self.status = "source worker is not running".to_string();
        }
    }


    pub(in crate::app) fn context_artist(&self) -> Option<ArtistRef> {
        match self.view {
            View::Library => card_artist(&self.library.selected()?.card),
            View::Home => card_artist(self.home_card()?),
            View::Tracks => self.search_items.get(self.selected)
                .filter(|_| self.browsing.is_none()).and_then(|item| card_artist(&item.card))
                .or_else(|| self.selected_result_track()?.artist_ref.clone()),
            View::Artist => {
                let artist = self.artist.as_ref()?;
                artist
                    .selected_song()
                    .and_then(|song| song.track.artist_ref.clone())
                    .or_else(|| artist.selected_card().and_then(card_artist))
            }
            View::Playing => self.now.as_ref()?.artist_ref.clone(),
        }
    }


    pub(in crate::app) fn open_context_artist(&mut self) {
        let Some(artist) = self.context_artist() else {
            return;
        };
        if self.view == View::Artist
            && self
                .artist
                .as_ref()
                .is_some_and(|page| page.requested.endpoint == artist.endpoint)
        {
            return;
        }
        self.open_artist(artist);
    }


    pub(in crate::app) fn open_artist_selection(&mut self) {
        let Some(artist) = self.artist.as_ref() else {
            return;
        };
        if let Some(track) = artist.selected_song().map(|song| song.track.clone()) {
            self.play_track(track, false);
        } else if let Some(card) = artist.selected_card().cloned() {
            self.activate_card(card);
        }
    }


    pub(in crate::app) fn handle_artist_key(&mut self, key: KeyEvent) -> Result<()> {
        let before = self
            .artist
            .as_ref()
            .map(|artist| (artist.section, artist.song, artist.card));

        match key.code {
            KeyCode::Char('a') => self.more_artist_songs(true),
            KeyCode::Char('e') => self.more_artist_songs(false),
            KeyCode::Char('q') => self.should_quit = true,
            KeyCode::Enter => self.open_artist_selection(),
            KeyCode::Char('r') => self.reload_artist(),
            KeyCode::Char('/') | KeyCode::Char('i') => self.begin_search(),
            KeyCode::Char('P') | KeyCode::Char('p') => self.open_player(),
            KeyCode::Char('B') => self.background(),
            KeyCode::Char('D') => self.toggle_presence(),
            KeyCode::Char('S') => self.open_settings(),
            KeyCode::Char('M') => self.begin_music_sign_in(false),
            KeyCode::Char(' ') => self.toggle_pause(),
            KeyCode::Char('m') => self.toggle_mute(),
            KeyCode::Char('+') | KeyCode::Char('=') => self.nudge_volume(VOLUME_STEP),
            KeyCode::Char('-') | KeyCode::Char('_') => self.nudge_volume(-VOLUME_STEP),
            KeyCode::Esc => self.go_back(),
            KeyCode::Char('H') => self.go_home(),
            KeyCode::Char('g') | KeyCode::Home => self.artist_first(),
            KeyCode::Char('G') | KeyCode::End => self.artist_last(),
            KeyCode::Char('j') | KeyCode::Down => self.move_artist_vertical(1),
            KeyCode::Char('k') | KeyCode::Up => self.move_artist_vertical(-1),
            KeyCode::PageDown => self.move_artist_vertical(8),
            KeyCode::PageUp => self.move_artist_vertical(-8),
            KeyCode::Char('l') | KeyCode::Right => self.move_artist_card(1),
            KeyCode::Char('h') | KeyCode::Left => self.move_artist_card(-1),
            _ => {}
        }

        let after = self
            .artist
            .as_ref()
            .map(|artist| (artist.section, artist.song, artist.card));
        if before != after {
            self.prefetch.selection_changed(Instant::now());
        }
        Ok(())
    }


    pub(in crate::app) fn artist_first(&mut self) {
        if let Some(artist) = self.artist.as_mut() {
            artist.section = 0;
            artist.song = 0;
            artist.card = 0;
        }
    }


    pub(in crate::app) fn artist_last(&mut self) {
        let Some(artist) = self.artist.as_mut() else {
            return;
        };
        let (songs, shelf_cards) = artist.content().map_or((0, Vec::new()), |page| {
            (
                page.top_songs.len(),
                page.shelves.iter().map(|shelf| shelf.cards.len()).collect(),
            )
        });
        artist.section = (shelf_cards.len() + usize::from(songs > 0)).saturating_sub(1);
        artist.song = songs.saturating_sub(1);
        artist.card = shelf_cards.last().copied().unwrap_or(0).saturating_sub(1);
    }


    pub(in crate::app) fn move_artist_vertical(&mut self, delta: isize) {
        let Some(artist) = self.artist.as_mut() else {
            return;
        };
        let Some((song_count, shelf_lengths)) = artist.content().map(|page| {
            (
                page.top_songs.len(),
                page.shelves
                    .iter()
                    .map(|shelf| shelf.cards.len())
                    .collect::<Vec<_>>(),
            )
        }) else {
            return;
        };
        let has_songs = song_count > 0;
        let section_count = shelf_lengths.len() + usize::from(has_songs);
        if section_count == 0 {
            return;
        }

        if has_songs && artist.section == 0 {
            let next = artist.song.saturating_add_signed(delta);
            if delta > 0 && next >= song_count && section_count > 1 {
                artist.section = 1;
                artist.card = 0;
            } else {
                artist.song = next.min(song_count.saturating_sub(1));
            }
            return;
        }

        let next = artist.section.saturating_add_signed(delta.signum());
        artist.section = next.min(section_count - 1);
        if has_songs && artist.section == 0 {
            artist.song = song_count.saturating_sub(1);
        } else {
            let shelf = artist.section.saturating_sub(usize::from(has_songs));
            let last = shelf_lengths
                .get(shelf)
                .copied()
                .unwrap_or(0)
                .saturating_sub(1);
            artist.card = artist.card.min(last);
        }
    }


    pub(in crate::app) fn move_artist_card(&mut self, delta: isize) {
        let Some(artist) = self.artist.as_mut() else {
            return;
        };
        let shelf = artist.shelf_index();
        let last = shelf
            .and_then(|index| artist.content()?.shelves.get(index))
            .map(|shelf| shelf.cards.len().saturating_sub(1));
        let Some(last) = last else {
            return;
        };
        artist.card = artist.card.saturating_add_signed(delta).min(last);
    }
}
