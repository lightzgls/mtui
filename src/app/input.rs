//! Keyboard and mouse action dispatch.

use std::time::Instant;

use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::{App, Overlay, SignIn, preferences};
use super::artist::card_artist;
use super::menu::MenuPage;
use super::navigation::{HistoryEntry, Mode, View};
use super::now_playing::{Panel, RelatedRow, Tab};
use super::playback::pointer_fraction;
use super::preferences::{PRECISION_VOLUME_STEP, VOLUME_STEP};

/// A semantic action produced by hit-testing the last rendered terminal frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseAction {
    GoHome,
    OpenLibrary,
    LibraryCategory(crate::source::library::browse::Category),
    OpenLibraryItem(usize),
    SelectLibraryItem(usize),
    PageLibrary { offset: usize, visible: usize },
    LibrarySort,
    MoreLibrary,
    RefreshLibrary,
    OpenLikedMusic,
    LibrarySignIn,
    OpenPlayer,
    OpenAppMenu,
    EditSearch,
    OpenHomeCard { shelf: usize, card: usize },
    SelectHomeCard { shelf: usize, card: usize },
    PageHomeShelf { shelf: usize, offset: usize, columns: usize, rows: usize },
    OpenArtistSong(usize),
    MoreArtistSongs,
    PlayArtistSongs,
    GoBack,
    SelectArtistSong(usize),
    OpenArtistCard { shelf: usize, card: usize },
    SelectArtistCard { shelf: usize, card: usize },
    PageArtistSongs { offset: usize, visible: usize },
    PageArtistShelf { shelf: usize, offset: usize, visible: usize },
    PlayTrack(usize),
    SelectTrack(usize),
    PlayCollection,
    ShuffleCollection,
    RetryCollection,
    SearchFilter(crate::source::search::Filter),
    OpenPlayingArtist,
    OpenQueueArtist(usize),
    OpenTrackArtist(usize),
    OpenLibraryArtist(usize),
    OpenRelatedArtist(usize),
    OpenPlayingAlbum,
    ShufflePlayback,
    RepeatPlayback,
    ChooseOutput,
    LikePlaying,
    SharePlaying,
    SavePlaying,
    OpenPlayerActions,
    ChoosePlaylist(usize),
    ClosePlayerDialog,
    RetryPlaylists,
    CopyShareLink,
    OpenTab(Tab),
    ToggleLyricsExpanded,
    ToggleLyricsFollow,
    OpenPageRow(usize),
    SelectPageRow(usize),
    /// Toggle the current transport from the persistent player strip.
    TogglePlayback,
    /// Move through the active queue from the persistent player strip.
    PreviousTrack,
    NextTrack,
    /// Seek to a point in the current track. The value is measured against
    /// [`POINTER_SCALE`] so it stays `Eq` and deterministic in hit-map tests.
    SeekTo(u16),
    /// Set ordinary output volume from the persistent player's volume bar.
    SetVolume(u16),
    AdjustVolume(bool),
    /// One-percent wheel adjustment over the current song's artwork.
    AdjustPrecisionVolume(bool),
    /// Toggle silence while retaining the user's last audible volume.
    ToggleMute,
    /// Open the actions for the current page or selection.
    OpenPageActions,
    /// Invoke an item in the currently open command menu.
    ActivateMenuItem(usize),
    CloseMenu,
    BackMenu,
    /// The modal owns this region, but the region performs no action.
    IgnoreOverlay,
    ActivateSetting(preferences::Setting),
    ChooseSetting(usize),
    CloseSettings,
}

/// Resolution used for mouse positions along a horizontal control.
pub const POINTER_SCALE: u16 = 10_000;

pub(super) fn is_ctrl_key(key: KeyEvent, character: char) -> bool {
    key.modifiers.contains(KeyModifiers::CONTROL)
        && !key.modifiers.contains(KeyModifiers::ALT)
        && matches!(key.code, KeyCode::Char(c) if c.eq_ignore_ascii_case(&character))
}

pub(super) fn is_bare_character(key: KeyEvent, character: char) -> bool {
    key.modifiers.difference(KeyModifiers::SHIFT).is_empty() && key.code == KeyCode::Char(character)
}

impl App {

    pub fn handle_key(&mut self, key: KeyEvent) -> Result<()> {
        // Ctrl-C quits from any mode.
        if is_ctrl_key(key, 'c') {
            self.should_quit = true;
            return Ok(());
        }

        if is_ctrl_key(key, 'k') {
            // Menus never displace a sign-in, picker, message or Settings.
            if !self.overlay.is_open() {
                self.toggle_root_menu();
            }
            return Ok(());
        }

        if is_ctrl_key(key, 's') {
            if !self.overlay.is_open() {
                self.open_settings();
            }
            return Ok(());
        }

        // A modal owns the keyboard outright while it is up, so a stray `q`
        // aimed at the picker cannot quit the program behind it.
        if self.overlay.is_open() {
            return self.handle_overlay_key(key);
        }

        if self.mode == Mode::Browse {
            let action = if is_ctrl_key(key, 'l') { Some(MouseAction::LikePlaying) }
                else if is_ctrl_key(key, 'y') { Some(MouseAction::SharePlaying) }
                else if is_ctrl_key(key, 'a') { Some(MouseAction::SavePlaying) }
                else if is_ctrl_key(key, 'o') { Some(MouseAction::ChooseOutput) }
                else { None };
            if let Some(action) = action {
                if self.now.is_some() || action == MouseAction::ChooseOutput {
                    self.menu = None;
                    self.handle_mouse_action(action)?;
                }
                return Ok(());
            }
        }

        if self.menu.is_some() {
            return self.handle_menu_key(key);
        }
        if key.code == KeyCode::Left && key.modifiers.contains(KeyModifiers::ALT) {
            return self.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        }

        if self.mode == Mode::Browse && is_ctrl_key(key, 'p') {
            self.open_menu(MenuPage::PlayerActions);
            return Ok(());
        }
        if self.mode == Mode::Browse && is_bare_character(key, '?') {
            self.open_menu(MenuPage::Help);
            return Ok(());
        }
        if self.mode == Mode::Browse && is_bare_character(key, '.') {
            self.open_menu(MenuPage::PageActions);
            return Ok(());
        }

        if self.mode == Mode::Browse && is_bare_character(key, 'L') {
            self.open_library();
            return Ok(());
        }

        if self.mode == Mode::Browse
            && (is_bare_character(key, 'r') || is_bare_character(key, 'n'))
            && self.playback_failed(&self.snapshot())
        {
            if key.code == KeyCode::Char('r') {
                self.retry_current_track();
            } else {
                self.advance(1, false);
            }
            return Ok(());
        }

        match self.mode {
            Mode::Editing => self.handle_editing_key(key),
            Mode::Browse => match self.view {
                View::Library => self.handle_library_key(key),
                View::Home => self.handle_home_key(key),
                View::Tracks => self.handle_browse_key(key),
                View::Artist => self.handle_artist_key(key),
                View::Playing => self.handle_playing_key(key),
            },
        }
    }


    /// Applies a click without manufacturing a key whose meaning depends on the
    /// current view. Modal layers retain the same ownership they have for keys.
    pub fn handle_mouse_action(&mut self, action: MouseAction) -> Result<()> {
        if self.handle_player_dialog_mouse(action) { return Ok(()); }
        if matches!(self.overlay, Overlay::Settings) {
            let intent = match action {
                MouseAction::ActivateSetting(setting) if self.preferences.picker.is_none() => Some(preferences::Intent::Activate(setting)),
                MouseAction::ChooseSetting(index) => self.preferences.choose(index),
                MouseAction::CloseSettings if self.preferences.picker.is_some() => {
                    self.preferences.picker = None;
                    None
                }
                MouseAction::CloseSettings => Some(preferences::Intent::Close),
                _ => None,
            };
            if let Some(intent) = intent {
                self.handle_preferences_intent(intent);
            }
            return Ok(());
        }
        if self.overlay.is_open() {
            return Ok(());
        }
        if self.menu.is_some() {
            match action {
                MouseAction::ActivateMenuItem(index) => {
                    let len = self.menu_items().len();
                    if index < len {
                        if let Some(menu) = self.menu.as_mut() {
                            menu.selected = index;
                        }
                        self.invoke_menu_item();
                    }
                }
                MouseAction::CloseMenu | MouseAction::OpenPageActions => self.menu = None,
                MouseAction::BackMenu => {
                    if self.menu.as_ref().is_some_and(|menu| matches!(menu.page, MenuPage::Account | MenuPage::Help)) {
                        self.open_menu(MenuPage::Root);
                    } else {
                        self.menu = None;
                    }
                }
                _ => {}
            }
            return Ok(());
        }
        if self.mode == Mode::Editing && (matches!(action,
            MouseAction::SelectHomeCard { .. } | MouseAction::SelectTrack(_)
                | MouseAction::SelectArtistSong(_) | MouseAction::SelectArtistCard { .. }
                | MouseAction::SelectLibraryItem(_))
            || (self.view == View::Playing && matches!(action, MouseAction::SelectPageRow(_))))
        {
            self.cancel_search_request();
            if let Some(HistoryEntry::Tracks(page)) = self.search_page.take() {
                self.query = page.query;
                self.query_cursor = self.query.len();
            }
            self.mode = Mode::Browse;
            self.status.clear();
        }
        match action {
            MouseAction::GoBack => { self.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))?; }
            MouseAction::MoreArtistSongs if self.view == View::Artist => self.more_artist_songs(false),
            MouseAction::PlayArtistSongs if self.view == View::Artist => self.more_artist_songs(true),
            MouseAction::GoHome => self.go_home(),
            MouseAction::OpenLibrary => self.open_library(),
            MouseAction::LibraryCategory(category) if self.view == View::Library => self.change_library_category(category),
            MouseAction::OpenLibraryItem(index) if self.view == View::Library => self.open_library_item(index),
            MouseAction::SelectLibraryItem(index) if self.view == View::Library => {
                if index < self.library.collection().items.len() {
                    self.library.collection_mut().selected = index;
                    self.prefetch.selection_changed(Instant::now());
                }
            }
            MouseAction::PageLibrary { offset, visible } if self.view == View::Library && visible > 0 => {
                let collection = self.library.collection_mut();
                collection.offset = offset.min(collection.items.len().saturating_sub(1));
                collection.selected = collection.offset;
                if offset + visible >= collection.items.len() { self.request_library(true); }
            }
            MouseAction::LibrarySort if self.view == View::Library => self.cycle_library_sort(),
            MouseAction::MoreLibrary if self.view == View::Library => self.request_library(true),
            MouseAction::RefreshLibrary if self.view == View::Library => self.refresh_library(),
            MouseAction::OpenLikedMusic if self.view == View::Library => self.open_liked_music(),
            MouseAction::LibrarySignIn if self.view == View::Library => self.begin_music_sign_in(false),
            MouseAction::OpenPlayer => self.open_player(),
            MouseAction::OpenAppMenu => self.toggle_root_menu(),
            MouseAction::OpenPageActions if self.mode == Mode::Browse => {
                self.open_menu(MenuPage::PageActions)
            }
            MouseAction::EditSearch => self.begin_search(),
            MouseAction::OpenHomeCard { shelf, card } if self.view == View::Home => {
                let selected = self
                    .home
                    .get(shelf)
                    .and_then(|row| row.cards.get(card))
                    .cloned();
                if let Some(selected) = selected {
                    self.home_shelf = shelf;
                    self.home_card = card;
                    self.prefetch.selection_changed(Instant::now());
                    self.activate_card(selected);
                }
            }
            MouseAction::SelectHomeCard { shelf, card } if self.view == View::Home => {
                if self
                    .home
                    .get(shelf)
                    .is_some_and(|row| card < row.cards.len())
                {
                    self.home_shelf = shelf;
                    self.home_card = card;
                    self.prefetch.selection_changed(Instant::now());
                }
            }
            MouseAction::PageHomeShelf { shelf, offset, columns, rows } if self.view == View::Home => {
                if let Some(len) = self.home.get(shelf).map(|row| row.cards.len())
                    && len > 0 && columns > 0 && rows > 0
                {
                    let first = (offset / rows).min(len.div_ceil(rows).saturating_sub(columns)) * rows;
                    self.home_scroll.resize(self.home.len(), 0);
                    self.home_scroll[shelf] = first;
                    self.home_shelf = shelf;
                    self.home_card = first;
                    self.home_grid_rows = rows;
                    self.prefetch.selection_changed(Instant::now());
                }
            }
            MouseAction::OpenArtistSong(index) | MouseAction::SelectArtistSong(index) if self.view == View::Artist => {
                if let Some(view) = self.artist.as_mut()
                    && matches!(&view.page, Panel::Ready(page) if index < page.top_songs.len())
                {
                    view.section = 0;
                    view.song = index;
                    self.prefetch.selection_changed(Instant::now());
                    if matches!(action, MouseAction::OpenArtistSong(_)) { self.open_artist_selection(); }
                }
            }
            MouseAction::OpenArtistCard { shelf, card } | MouseAction::SelectArtistCard { shelf, card } if self.view == View::Artist => {
                if let Some(view) = self.artist.as_mut()
                    && let Panel::Ready(page) = &view.page
                    && page.shelves.get(shelf).is_some_and(|row| card < row.cards.len())
                {
                    view.section = shelf + usize::from(!page.top_songs.is_empty());
                    view.card = card;
                    self.prefetch.selection_changed(Instant::now());
                    if matches!(action, MouseAction::OpenArtistCard { .. }) { self.open_artist_selection(); }
                }
            }
            MouseAction::PageArtistSongs { offset, visible } if self.view == View::Artist && visible > 0 => {
                if let Some(view) = self.artist.as_mut()
                    && let Panel::Ready(page) = &view.page
                    && !page.top_songs.is_empty()
                {
                    view.section = 0;
                    view.song_offset = offset.min(page.top_songs.len().saturating_sub(visible));
                    view.song = view.song_offset;
                    self.prefetch.selection_changed(Instant::now());
                }
            }
            MouseAction::PageArtistShelf { shelf, offset, visible } if self.view == View::Artist && visible > 0 => {
                if let Some(view) = self.artist.as_mut()
                    && let Panel::Ready(page) = &view.page
                    && let Some(row) = page.shelves.get(shelf)
                    && !row.cards.is_empty()
                {
                    view.section = shelf + usize::from(!page.top_songs.is_empty());
                    view.scroll.resize(page.shelves.len(), 0);
                    view.scroll[shelf] = offset.min(row.cards.len().saturating_sub(visible));
                    view.card = view.scroll[shelf];
                    self.prefetch.selection_changed(Instant::now());
                }
            }
            MouseAction::PlayTrack(index) if self.view == View::Tracks => {
                if index < self.result_count() {
                    self.selected = index;
                    self.prefetch.selection_changed(Instant::now());
                    self.play_selected();
                }
            }
            MouseAction::PlayCollection if self.view == View::Tracks => self.play_collection(0, false),
            MouseAction::ShuffleCollection if self.view == View::Tracks => self.play_collection(0, true),
            MouseAction::RetryCollection if self.view == View::Tracks => self.reload_collection(),
            MouseAction::SearchFilter(filter) if self.view == View::Tracks && self.browsing.is_none() => self.filter_search(filter),
            MouseAction::OpenPlayingArtist => {
                if let Some(artist) = self.now.as_ref().and_then(|now| now.artist_ref.clone()) {
                    self.open_artist(artist);
                }
            }
            MouseAction::OpenQueueArtist(index) => {
                if let Some(artist) = self.now.as_ref().and_then(|now| now.queue.get(index)).and_then(|track| track.artist_ref.clone()) {
                    self.open_artist(artist);
                }
            }
            MouseAction::OpenTrackArtist(index) if self.view == View::Tracks => {
                if let Some(artist) = self.results.get(index).and_then(|track| track.artist_ref.clone()) {
                    self.handle_mouse_action(MouseAction::SelectTrack(index))?;
                    self.open_artist(artist);
                }
            }
            MouseAction::OpenLibraryArtist(index) if self.view == View::Library => {
                if let Some(artist) = self.library.collection().items.get(index).and_then(|item|
                    item.track.as_ref().and_then(|track| track.artist_ref.clone()).or_else(|| card_artist(&item.card)))
                {
                    self.handle_mouse_action(MouseAction::SelectLibraryItem(index))?;
                    self.open_artist(artist);
                }
            }
            MouseAction::OpenRelatedArtist(index) => {
                let artist = self.now.as_ref().filter(|now| now.tab == Tab::Related).and_then(|now|
                    match now.related_rows().get(index) {
                        Some(RelatedRow::Card(card)) => card_artist(card), _ => None,
                    });
                if let Some(artist) = artist {
                    self.handle_mouse_action(MouseAction::SelectPageRow(index))?;
                    self.open_artist(artist);
                }
            }
            MouseAction::OpenPlayingAlbum => {
                if let Some((title, endpoint)) = self.now.as_ref().and_then(|now| now.album_route.clone()) {
                    self.open_browse(endpoint, title);
                }
            }
            MouseAction::LikePlaying => self.like_playing(),
            MouseAction::SharePlaying => self.share_playing(),
            MouseAction::SavePlaying => self.save_playing(),
            MouseAction::OpenPlayerActions => self.open_menu(MenuPage::PlayerActions),
            MouseAction::ShufflePlayback => self.shuffle_upcoming_queue(),
            MouseAction::RepeatPlayback => self.cycle_repeat(),
            MouseAction::ChooseOutput => self.choose_output(),
            MouseAction::SelectTrack(index) if self.view == View::Tracks => {
                if index < self.result_count() {
                    self.selected = index;
                    self.prefetch.selection_changed(Instant::now());
                }
            }
            MouseAction::OpenTab(tab) => {
                if self.now.as_mut().is_some_and(|now| now.open(tab)) {
                    self.request_tab();
                }
            }
            MouseAction::ToggleLyricsExpanded => {
                self.open_player();
                self.toggle_lyrics_expanded();
            }
            MouseAction::ToggleLyricsFollow => {
                self.toggle_lyrics_follow();
            }
            MouseAction::OpenPageRow(index) => {
                if let Some(now) = self.now.as_mut() {
                    *now.cursor_mut() = index;
                }
                self.open_page_row();
            }
            MouseAction::SelectPageRow(index) => {
                if let Some(now) = self.now.as_mut() {
                    if index < now.queue.len() { self.status.clear(); }
                    *now.cursor_mut() = index;
                }
            }
            MouseAction::TogglePlayback => self.toggle_pause(),
            MouseAction::PreviousTrack => self.advance(-1, false),
            MouseAction::NextTrack => self.advance(1, false),
            MouseAction::SeekTo(position) => self.seek_to_fraction(position),
            MouseAction::SetVolume(position) => {
                self.set_volume((pointer_fraction(position) as f32 / VOLUME_STEP).round() * VOLUME_STEP)
            }
            MouseAction::AdjustVolume(up) => self.nudge_volume(if up { VOLUME_STEP } else { -VOLUME_STEP }),
            MouseAction::AdjustPrecisionVolume(up) if self.precision_volume => {
                self.nudge_volume(if up { PRECISION_VOLUME_STEP } else { -PRECISION_VOLUME_STEP });
            }
            MouseAction::ToggleMute => self.toggle_mute(),
            _ => {}
        }
        Ok(())
    }


    /// Mouse wheels follow the active view's existing up/down behavior.
    pub fn handle_mouse_scroll(&mut self, down: bool) -> Result<()> {
        if self.mode == Mode::Editing && !self.overlay.is_open() && self.menu.is_none() {
            return Ok(());
        }
        let code = if down { KeyCode::Down } else { KeyCode::Up };
        self.handle_key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    /// Focusing a visible panel preserves its independent lyric-follow setting.
    pub fn focus_player_panel(&mut self, tab: Tab) {
        if !self.overlay.is_open() && self.menu.is_none()
            && let Some(now) = self.now.as_mut()
            && now.tab != tab
        {
            now.tab = tab;
            if tab != Tab::Lyrics { now.detail_tab = tab; }
            self.request_tab();
        }
    }

    /// A sidebar wheel never changes the browsing page or its search draft.
    pub fn handle_player_panel_scroll(&mut self, tab: Tab, down: bool) -> Result<()> {
        if self.overlay.is_open() || self.menu.is_some() {
            return self.handle_mouse_scroll(down);
        }
        if let Some(now) = self.now.as_mut() {
            let cursor = &mut now.cursor[tab.index()];
            *cursor = cursor.saturating_add_signed(if down { 1 } else { -1 });
            if tab == Tab::Lyrics { now.follow_lyrics = false; }
        }
        Ok(())
    }


    pub(super) fn handle_overlay_key(&mut self, key: KeyEvent) -> Result<()> {
        if self.handle_player_dialog_key(key) { return Ok(()); }
        match &mut self.overlay {
            Overlay::Share { .. } | Overlay::SavePlaylist(_) => {}
            Overlay::None => {}
            // A failed session import is the one phase with somewhere to go:
            // the thread behind it is gone, so `M` starts a fresh attempt.
            Overlay::SignIn(SignIn::Failed { .. }) => match key.code {
                KeyCode::Char('M') | KeyCode::Char('m') => self.begin_music_sign_in(false),
                KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q') => {
                    self.overlay = Overlay::None;
                    self.status = "press M to try YouTube Music sign-in again".to_string();
                }
                _ => {}
            },
            // Dismissing only hides an import already running. Its result still
            // arrives through the worker.
            Overlay::SignIn(SignIn::Music { .. }) => {
                if matches!(key.code, KeyCode::Esc) {
                    self.overlay = Overlay::None;
                    self.status = "session import still pending in the background".to_string();
                }
            }
            Overlay::Message { .. } => {
                if matches!(key.code, KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q')) {
                    self.overlay = Overlay::None;
                    self.status = String::new();
                }
            }
            Overlay::Settings => {
                if let Some(intent) = self.preferences.key(key) {
                    self.handle_preferences_intent(intent);
                }
            }
        }

        Ok(())
    }
}
