//! Library caches retain each category's position without copying collections into Back history.

use super::*;
use crate::source::library::browse::{Category, Item, Page, Sort};

#[derive(Debug, Clone)]
pub struct Collection {
    pub items: Vec<Item>,
    pub selected: usize,
    pub offset: usize,
    pub endpoint: BrowseEndpoint,
    pub sorts: Vec<Sort>,
    pub sort_label: String,
    pub continuation: Option<String>,
    pub error: Option<String>,
    pub loaded: bool,
    reload: Option<String>,
    seen_tokens: Vec<String>,
}

impl Collection {
    fn new(category: Category) -> Self {
        Self {
            items: Vec::new(),
            selected: 0,
            offset: 0,
            endpoint: category.endpoint(),
            sorts: Vec::new(),
            sort_label: if category == Category::All {
                "Recent activity"
            } else {
                "YouTube order"
            }
            .into(),
            continuation: None,
            error: None,
            loaded: false,
            reload: None,
            seen_tokens: Vec::new(),
        }
    }
}

struct Pending {
    id: u64,
    category: Category,
    generation: u64,
    append: bool,
    token: Option<String>,
}

pub struct State {
    pub category: Category,
    pub collections: [Collection; 6],
    pub account: Option<String>,
    pub connected: bool,
    pub columns: usize,
    pub visible: usize,
    sequence: u64,
    pending: Option<Pending>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            category: Category::All,
            collections: Category::ALL.map(Collection::new),
            account: None,
            connected: false,
            columns: 1,
            visible: 1,
            sequence: 0,
            pending: None,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub(super) struct Cursor {
    category: Category,
    selected: usize,
    offset: usize,
}

impl State {
    pub fn collection(&self) -> &Collection {
        &self.collections[self.category.index()]
    }
    pub fn collection_mut(&mut self) -> &mut Collection {
        &mut self.collections[self.category.index()]
    }
    pub fn loading(&self) -> bool {
        self.pending
            .as_ref()
            .is_some_and(|pending| pending.category == self.category)
    }
    pub fn selected(&self) -> Option<&Item> {
        self.collection().items.get(self.collection().selected)
    }
    pub(super) fn cursor(&self) -> Cursor {
        Cursor {
            category: self.category,
            selected: self.collection().selected,
            offset: self.collection().offset,
        }
    }
    pub(super) fn restore(&mut self, cursor: Cursor) {
        self.category = cursor.category;
        let page = self.collection_mut();
        page.selected = cursor.selected.min(page.items.len().saturating_sub(1));
        page.offset = cursor.offset.min(page.selected);
    }
    pub(super) fn suspend(&mut self) {
        self.pending = None;
    }
}

impl App {
    pub(super) fn open_library(&mut self) {
        self.cancel_page_request();
        self.menu = None;
        self.history.clear();
        self.search_page = None;
        self.mode = Mode::Browse;
        self.view = View::Library;
        self.library.connected = config::Cookies::available().ok().flatten().is_some();
        if !self.library.connected {
            self.reset_library();
            self.status = "Sign in to YouTube Music to access your Library.".into();
        } else if !self.library.collection().loaded || self.library.loading() {
            self.request_library(false);
        } else {
            self.status = format!("Library · {}", self.library.category.label());
        }
    }

    pub(super) fn reset_library(&mut self) {
        let sequence = self.library.sequence.wrapping_add(1);
        self.library = State {
            sequence,
            ..Default::default()
        };
        self.history
            .retain(|page| !matches!(page, HistoryEntry::Library { .. }));
        if matches!(self.search_page, Some(HistoryEntry::Library { .. })) {
            self.search_page = None;
        }
        if matches!(self.player_back, Some(HistoryEntry::Library { .. })) {
            self.player_back = None;
            self.back_to = View::Home;
        }
    }

    pub(super) fn refresh_library(&mut self) {
        if !self.library.connected {
            self.open_library();
            return;
        }
        self.request_library(false);
    }

    pub(super) fn request_library(&mut self, append: bool) {
        if !self.library.connected {
            return;
        }
        let category = self.library.category;
        let collection = self.library.collection();
        let token = if append {
            collection.continuation.clone()
        } else {
            collection.reload.clone()
        };
        if append && (token.is_none() || self.library.loading()) {
            return;
        }
        let endpoint = collection.endpoint.clone();
        self.library.sequence = self.library.sequence.wrapping_add(1).max(1);
        let request_id = self.library.sequence;
        let generation = self.source.session_generation();
        self.library.collection_mut().error = None;
        let request = Request::Library {
            request_id,
            generation,
            category,
            endpoint,
            token: token.clone(),
            read_account: self.library.account.is_none() && !append,
        };
        match self.source.send(request) {
            Ok(()) => {
                self.library.pending = Some(Pending {
                    id: request_id,
                    category,
                    generation,
                    append,
                    token,
                });
                self.status = format!("Loading Library · {}…", category.label());
            }
            Err(error) => {
                self.library.pending = None;
                self.library.collection_mut().error = Some(error.to_string());
                self.status = error.to_string();
            }
        }
    }

    pub(super) fn apply_library(
        &mut self,
        request_id: u64,
        generation: u64,
        category: Category,
        page: Result<Page, String>,
    ) {
        if generation != self.source.session_generation()
            || self.library.pending.as_ref().is_none_or(|pending| {
                pending.id != request_id
                    || pending.category != category
                    || pending.generation != generation
            })
        {
            return;
        }
        let pending = self.library.pending.take().unwrap();
        if self.view == View::Library { self.close_page_actions(); }
        match page {
            Err(error) => {
                self.library.collections[category.index()].error = Some(error.clone());
                if self.view == View::Library {
                    self.status = error;
                }
            }
            Ok(page) => {
                if let Some(account) = page.account {
                    self.library.account = Some(account);
                }
                for (category, endpoint) in page.routes {
                    let collection = &mut self.library.collections[category.index()];
                    if !collection.loaded {
                        collection.endpoint = endpoint;
                    }
                }
                let collection = &mut self.library.collections[category.index()];
                let selected_key = collection
                    .items
                    .get(collection.selected)
                    .map(|item| item.card.art_key().to_owned());
                if !pending.append {
                    collection.items.clear();
                    collection.seen_tokens.clear();
                }
                let mut seen: std::collections::HashSet<String> = collection
                    .items
                    .iter()
                    .map(|item| item.card.art_key().to_owned())
                    .collect();
                collection.items.extend(
                    page.items
                        .into_iter()
                        .filter(|item| seen.insert(item.card.art_key().to_owned())),
                );
                if pending.append
                    && let Some(token) = pending.token
                {
                    collection.seen_tokens.push(token);
                }
                collection.continuation = page
                    .continuation
                    .filter(|token| !collection.seen_tokens.contains(token));
                if !page.sorts.is_empty() {
                    collection.sorts = page
                        .sorts
                        .into_iter()
                        .filter(|sort| Category::from_endpoint(&sort.endpoint) == Some(category))
                        .collect();
                    if let Some(sort) = collection.sorts.iter().find(|sort| sort.selected) {
                        collection.sort_label = sort.label.clone();
                        collection.reload = sort.reload.clone();
                    }
                }
                collection.loaded = true;
                collection.error = None;
                if !pending.append
                    && let Some(key) = selected_key
                {
                    collection.selected = collection
                        .items
                        .iter()
                        .position(|item| item.card.art_key() == key)
                        .unwrap_or(0);
                }
                collection.selected = collection
                    .selected
                    .min(collection.items.len().saturating_sub(1));
                collection.offset = collection.offset.min(collection.selected);
                if self.view == View::Library {
                    self.status = format!("{} Library items loaded", collection.items.len());
                }
            }
        }
    }

    pub(super) fn change_library_category(&mut self, category: Category) {
        if self.library.category == category {
            return;
        }
        self.close_page_actions();
        self.library.suspend();
        self.library.category = category;
        self.library.columns = 1;
        if !self.library.collection().loaded {
            self.request_library(false);
        } else {
            self.status = format!("Library · {}", category.label());
        }
        self.selection_settled = Some(Instant::now());
    }

    pub(super) fn cycle_library_sort(&mut self) {
        let collection = self.library.collection_mut();
        if collection.sorts.len() < 2 {
            return;
        }
        let current = collection
            .sorts
            .iter()
            .position(|sort| sort.label == collection.sort_label)
            .unwrap_or(0);
        let sort = collection.sorts[(current + 1) % collection.sorts.len()].clone();
        collection.endpoint = sort.endpoint;
        collection.reload = sort.reload;
        collection.sort_label = sort.label;
        collection.selected = 0;
        collection.offset = 0;
        collection.items.clear();
        collection.loaded = false;
        collection.continuation = None;
        collection.seen_tokens.clear();
        self.request_library(false);
    }

    pub(super) fn open_library_item(&mut self, index: usize) {
        let Some(item) = self.library.collection().items.get(index).cloned() else {
            return;
        };
        self.library.collection_mut().selected = index;
        if !item.available {
            self.status = "This Library item is currently unavailable.".into();
            return;
        }
        if let Some(track) = item.track {
            self.play_track(track, false);
        } else {
            self.activate_card(item.card);
        }
    }

    pub(super) fn open_liked_music(&mut self) {
        if !self.library.connected {
            self.status = "Sign in to YouTube Music to open Liked Music.".into();
            return;
        }
        self.open_browse(BrowseEndpoint::new("VLLM"), "Liked Music".into());
    }

    pub fn clamp_library(&mut self, columns: usize, visible: usize) {
        self.library.columns = columns.max(1);
        self.library.visible = visible.max(1);
        let collection = self.library.collection_mut();
        collection.selected = collection
            .selected
            .min(collection.items.len().saturating_sub(1));
        let columns = columns.max(1);
        if collection.selected < collection.offset {
            collection.offset = collection.selected / columns * columns;
        }
        if collection.selected >= collection.offset + visible.max(1) {
            collection.offset = (collection.selected / columns + 1)
                .saturating_sub(visible.max(1).div_ceil(columns))
                * columns;
        }
        collection.offset = collection.offset.min(collection.selected) / columns * columns;
    }

    pub(super) fn handle_library_key(&mut self, key: KeyEvent) -> Result<()> {
        let columns = self.library.columns.max(1);
        let visible = self.library.visible.max(1);
        let before = self.library.collection().selected;
        let last = self.library.collection().items.len().saturating_sub(1);
        let selected = match key.code {
            KeyCode::Down | KeyCode::Char('j') => Some((before + columns).min(last)),
            KeyCode::Up | KeyCode::Char('k') => Some(before.saturating_sub(columns)),
            KeyCode::Right | KeyCode::Char('l') => Some((before + 1).min(last)),
            KeyCode::Left | KeyCode::Char('h') => Some(before.saturating_sub(1)),
            KeyCode::PageDown => Some((before + visible).min(last)),
            KeyCode::PageUp => Some(before.saturating_sub(visible)),
            KeyCode::Home | KeyCode::Char('g') => Some(0),
            KeyCode::End | KeyCode::Char('G') => Some(last),
            _ => None,
        };
        if let Some(selected) = selected {
            self.library.collection_mut().selected = selected;
            self.selection_settled = Some(Instant::now());
            if selected + columns >= self.library.collection().items.len() {
                self.request_library(true);
            }
            return Ok(());
        }
        match key.code {
            KeyCode::Char('1'..='6') => {
                if let KeyCode::Char(key) = key.code {
                    self.change_library_category(Category::ALL[(key as u8 - b'1') as usize]);
                }
            }
            KeyCode::Tab | KeyCode::BackTab => {
                let delta = if key.code == KeyCode::BackTab { 5 } else { 1 };
                self.change_library_category(
                    Category::ALL[(self.library.category.index() + delta) % 6],
                );
            }
            KeyCode::Enter => self.open_library_item(before),
            KeyCode::Char('o') => self.cycle_library_sort(),
            KeyCode::Char('r') => self.refresh_library(),
            KeyCode::Char('f') => self.open_liked_music(),
            KeyCode::Char('/') | KeyCode::Char('i') => self.begin_search(),
            KeyCode::Char('H') | KeyCode::Esc => self.go_home(),
            KeyCode::Char('P') | KeyCode::Char('p') => self.open_player(),
            KeyCode::Char('M') => self.begin_music_sign_in(false),
            KeyCode::Char('S') => self.open_settings(),
            KeyCode::Char('B') => self.background(),
            KeyCode::Char('q') => self.should_quit = true,
            KeyCode::Char(' ') => self.toggle_pause(),
            KeyCode::Char('m') => self.toggle_mute(),
            KeyCode::Char('s') => self.stop(),
            KeyCode::Char('+') | KeyCode::Char('=') => self.nudge_volume(VOLUME_STEP),
            KeyCode::Char('-') | KeyCode::Char('_') => self.nudge_volume(-VOLUME_STEP),
            _ => {}
        }
        Ok(())
    }

    pub(super) fn library_action_items(&self) -> Vec<MenuItem> {
        vec![
            MenuItem::action(
                "Open or play selected item",
                Some("Enter"),
                self.library.selected().is_some_and(|item| item.available),
                Some("Library"),
                MenuAction::OpenLibrarySelection,
            ),
            MenuItem::action(
                "Liked Music",
                Some("f"),
                self.library.connected,
                None,
                MenuAction::OpenLikedMusic,
            ),
            MenuItem::action(
                "Change sort order",
                Some("o"),
                self.library.collection().sorts.len() > 1,
                None,
                MenuAction::SortLibrary,
            ),
            MenuItem::action(
                "Load more items",
                None,
                self.library.collection().continuation.is_some() && !self.library.loading(),
                None,
                MenuAction::MoreLibrary,
            ),
            MenuItem::action(
                "Refresh Library",
                Some("r"),
                !self.library.loading(),
                None,
                MenuAction::RefreshLibrary,
            ),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(index: usize) -> Item {
        Item {
            card: Card {
                title: format!("Playlist {index}"),
                subtitle: "Playlist • You • 12 songs".into(),
                art: None,
                duration: None,
                artist_ref: None,
                target: Target::Open {
                    endpoint: BrowseEndpoint::new(format!("VLplaylist-{index}")),
                },
            },
            track: None,
            available: true,
        }
    }

    fn fixture() -> (
        App,
        crate::player::TestDriver,
        std::sync::mpsc::Receiver<Request>,
    ) {
        let (mut app, driver, requests) = super::super::scenarios::fixture();
        app.view = View::Library;
        app.library.connected = true;
        app.library.account = Some("Test account".into());
        (app, driver, requests)
    }

    fn deliver(app: &mut App, page: Page) {
        let pending = app.library.pending.as_ref().unwrap();
        let response = Response::Library {
            request_id: pending.id,
            generation: pending.generation,
            category: pending.category,
            page: Box::new(Ok(page)),
        };
        app.apply(response);
    }

    #[test]
    fn category_changes_ignore_late_results_and_reuse_cached_positions() {
        let (mut app, _, requests) = fixture();
        app.request_library(false);
        let old = app.library.pending.as_ref().unwrap().id;
        app.change_library_category(Category::Albums);
        app.apply(Response::Library {
            request_id: old,
            generation: 0,
            category: Category::All,
            page: Box::new(Ok(Page {
                items: vec![item(0)],
                ..Default::default()
            })),
        });
        assert!(app.library.collection().items.is_empty());
        assert!(
            app.library.loading(),
            "a stale response cannot finish the newer request"
        );
        deliver(
            &mut app,
            Page {
                items: (0..12).map(item).collect(),
                ..Default::default()
            },
        );
        app.library.collection_mut().selected = 7;
        app.library.collection_mut().offset = 6;
        app.change_library_category(Category::Songs);
        deliver(&mut app, Page::default());
        requests.try_iter().for_each(drop);
        app.change_library_category(Category::Albums);
        assert_eq!(
            (
                app.library.collection().selected,
                app.library.collection().offset
            ),
            (7, 6)
        );
        assert!(
            requests.try_iter().next().is_none(),
            "returning to a cached category cannot refetch it"
        );
    }

    #[test]
    fn opening_a_collection_and_going_back_restores_library_without_playing() {
        let (mut app, driver, requests) = fixture();
        app.request_library(false);
        deliver(
            &mut app,
            Page {
                items: (0..24).map(item).collect(),
                ..Default::default()
            },
        );
        app.library.collection_mut().offset = 12;
        driver.commands.try_iter().for_each(drop);
        app.open_library_item(17);
        assert_eq!(app.view, View::Tracks);
        assert_eq!(
            app.browsing_endpoint.as_ref().unwrap().browse_id,
            "VLplaylist-17"
        );
        assert!(driver.commands.try_iter().next().is_none());
        assert!(
            requests
                .try_iter()
                .any(|request| matches!(request, Request::OpenBrowse { .. }))
        );
        app.go_back();
        assert_eq!(app.view, View::Library);
        assert_eq!(
            (
                app.library.collection().selected,
                app.library.collection().offset
            ),
            (17, 12)
        );
        assert!(!app.busy);
    }

    #[test]
    fn pagination_deduplicates_overlap_and_stops_repeated_tokens() {
        let (mut app, _driver, _requests) = fixture();
        app.request_library(false);
        deliver(
            &mut app,
            Page {
                items: vec![item(0), item(1)],
                continuation: Some("second".into()),
                ..Default::default()
            },
        );
        app.library.collection_mut().selected = 1;
        app.request_library(true);
        deliver(
            &mut app,
            Page {
                items: vec![item(1), item(2)],
                continuation: Some("second".into()),
                ..Default::default()
            },
        );
        assert_eq!(app.library.collection().items.len(), 3);
        assert_eq!(app.library.collection().selected, 1);
        assert!(app.library.collection().continuation.is_none());
    }

    #[test]
    fn sort_fetches_provider_order_and_preserves_filter_params() {
        let (mut app, _, requests) = fixture();
        app.change_library_category(Category::Albums);
        let mut default = Category::Albums.endpoint();
        default.params = Some("recent-with-filter".into());
        let mut alpha = Category::Albums.endpoint();
        alpha.params = Some("alpha-with-filter".into());
        deliver(
            &mut app,
            Page {
                items: vec![item(1)],
                sorts: vec![
                    Sort {
                        label: "Recently saved".into(),
                        endpoint: default,
                        selected: true,
                        reload: None,
                    },
                    Sort {
                        label: "A–Z".into(),
                        endpoint: alpha,
                        selected: false,
                        reload: None,
                    },
                ],
                ..Default::default()
            },
        );
        requests.try_iter().for_each(drop);
        app.cycle_library_sort();
        assert!(
            app.library.collection().items.is_empty(),
            "old ordering must not appear under the new sort label"
        );
        assert!(requests.try_iter().any(|request| matches!(request, Request::Library { endpoint, token: None, .. } if endpoint.params.as_deref() == Some("alpha-with-filter"))));
    }

    #[test]
    fn saved_song_playback_keeps_metadata_and_returns_to_library() {
        let (mut app, _driver, _requests) = fixture();
        let track = Track {
            id: "song".into(),
            title: "Saved song".into(),
            uploader: "Artist".into(),
            album: Some("Saved album".into()),
            duration: Some(Duration::from_secs(180)),
            artist_ref: None,
        };
        let mut song = item(0);
        song.card.target = Target::Play {
            video_id: track.id.clone(),
        };
        song.track = Some(track);
        app.library.collection_mut().items = vec![song];
        app.library.collection_mut().loaded = true;
        app.open_library_item(0);
        assert_eq!(app.now.as_ref().unwrap().video_id, "song");
        assert_eq!(
            app.now.as_ref().unwrap().album.as_deref(),
            Some("Saved album")
        );
        assert_eq!(app.back_to, View::Library);
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
            .unwrap();
        assert_eq!(app.view, View::Library);
    }

    #[test]
    fn signed_out_state_and_account_changes_cannot_show_previous_library() {
        let (mut app, _driver, _requests) = fixture();
        app.request_library(false);
        let pending = app.library.pending.as_ref().unwrap();
        let late = Response::Library {
            request_id: pending.id,
            generation: pending.generation,
            category: pending.category,
            page: Box::new(Ok(Page {
                items: vec![item(1)],
                ..Default::default()
            })),
        };
        app.source.invalidate_account();
        app.reset_library();
        app.apply(late);
        assert!(app.library.collection().items.is_empty());
        assert!(!app.library.connected);
        assert!(app.library.account.is_none());
        app.open_liked_music();
        assert_eq!(app.view, View::Library);
        assert!(app.status.contains("Sign in"));
    }

    #[test]
    fn library_mouse_paging_reaches_last_items_and_controls_stay_in_bounds() {
        for (width, height) in [(160, 42), (100, 30), (80, 24), (28, 24), (16, 12), (1, 1)] {
            let (mut app, driver, _) = fixture();
            app.library.collection_mut().items = (0..47).map(item).collect();
            app.library.collection_mut().loaded = true;
            driver.commands.try_iter().for_each(drop);
            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
            let mut mouse = crate::ui::MouseMap::default();
            terminal
                .draw(|frame| crate::ui::render(frame, &mut app, &mut mouse))
                .unwrap();
            for y in 0..height + 8 {
                assert!(
                    mouse.action_at(width, y).is_none(),
                    "target outside width {width}"
                );
            }
            for x in 0..width + 8 {
                assert!(
                    mouse.action_at(x, height).is_none(),
                    "target outside height {height}"
                );
            }
            if width >= 24 {
                for _ in 0..48 {
                    if (0..height).any(|y| {
                        (0..width).any(|x| {
                            mouse.action_at(x, y) == Some(MouseAction::OpenLibraryItem(46))
                        })
                    }) {
                        break;
                    }
                    let offset = app.library.collection().offset;
                    let next =
                        (0..height)
                            .find_map(|y| {
                                (0..width).find_map(|x| match mouse.action_at(x, y) {
                                    Some(
                                        action @ MouseAction::PageLibrary { offset: next, .. },
                                    ) if next > offset => Some(action),
                                    _ => None,
                                })
                            })
                            .expect("mouse-only paging must reach the end of Library");
                    app.handle_mouse_action(next).unwrap();
                    terminal
                        .draw(|frame| crate::ui::render(frame, &mut app, &mut mouse))
                        .unwrap();
                }
                assert!((0..height).any(|y| {
                    (0..width)
                        .any(|x| mouse.action_at(x, y) == Some(MouseAction::OpenLibraryItem(46)))
                }));
            }
            assert!(driver.commands.try_iter().next().is_none());
        }
    }
}
