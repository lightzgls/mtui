//! Page snapshots, Back history, and correlated page requests.

use crate::source::{BrowseEndpoint, Track};
use crate::source::worker::{PageRequestId, Request};

use super::{App, library};
use super::artist::ArtistView;
use super::playback::playback_failure_status;

/// Exact pages retained for Back. A cap keeps nested artist browsing bounded
/// even when someone walks through a long chain of related artists.
pub(super) const PAGE_HISTORY: usize = 12;

/// Which pane owns keyboard input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Navigating the results list. Single-key commands are active.
    Browse,
    /// Typing a query. Printable keys go into the search box.
    Editing,
}

/// Which list the main pane is showing.
///
/// Both views share the pane rather than splitting it: on a terminal the width
/// is the scarce resource, and a permanent sidebar would cost the results their
/// artist and album columns for something looked at once a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    Library,
    /// The landing page: YouTube Music's home feed as shelves of cards. What
    /// the program opens on, because a music player that opens on an empty list
    /// asks the user to think of something before it will do anything.
    Home,
    /// Tracks -- either search results or the contents of an opened playlist.
    Tracks,
    /// A Music artist's top songs and catalogue shelves.
    Artist,
    /// The player page: the cover of what is playing, and beside it the queue,
    /// the lyrics, what to listen to next and the comments. Opened by playing
    /// something, which is the moment all four become answerable.
    Playing,
}

#[derive(Debug, Clone)]
pub(super) struct TrackPage {
    pub(super) results: Vec<Track>,
    pub(super) query: String,
    pub(super) search_items: Vec<crate::source::search::Item>,
    pub(super) search_filter: crate::source::search::Filter,
    pub(super) selected: usize,
    pub(super) offset: usize,
    pub(super) browsing: Option<String>,
    pub(super) browsing_endpoint: Option<BrowseEndpoint>,
    pub(super) collection: Option<crate::source::collection::Details>,
    pub(super) collection_error: Option<String>,
    pub(super) status: String,
}

#[derive(Debug, Clone)]
pub(super) enum HistoryEntry {
    Library { cursor: library::Cursor, status: String },
    Home {
        status: String,
    },
    Tracks(Box<TrackPage>),
    Artist {
        artist: Box<ArtistView>,
        status: String,
    },
    Playing {
        back_to: View,
        player_back: Option<Box<HistoryEntry>>,
    },
}

impl HistoryEntry {
    pub(super) fn view(&self) -> View {
        match self {
            Self::Library { .. } => View::Library,
            Self::Home { .. } => View::Home,
            Self::Tracks(_) => View::Tracks,
            Self::Artist { .. } => View::Artist,
            Self::Playing { .. } => View::Playing,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PageRequestKind {
    Search,
    Browse,
    Artist,
}

pub(super) fn moved_cursor(selected: usize, delta: isize, len: usize) -> usize {
    let last = len.saturating_sub(1);
    let selected = selected.min(last);
    if delta < 0 {
        selected.saturating_sub(delta.unsigned_abs())
    } else {
        selected.saturating_add(delta as usize).min(last)
    }
}

pub(super) fn page_behind_search(origin: View, player_back: View) -> View {
    match origin {
        View::Playing => player_back,
        origin => origin,
    }
}

/// Player is a transient view over another page, never its own return target.
pub(super) fn player_return_target(
    mut page: Option<HistoryEntry>,
    mut fallback: View,
) -> (Option<HistoryEntry>, View) {
    loop {
        match page {
            Some(HistoryEntry::Playing {
                back_to,
                player_back,
                ..
            }) => {
                fallback = back_to;
                page = player_back.map(|page| *page);
            }
            other => {
                if let Some(page) = &other {
                    fallback = page.view();
                }
                return (other, fallback);
            }
        }
    }
}

pub(super) fn push_history(history: &mut Vec<HistoryEntry>, page: HistoryEntry) {
    if history.len() == PAGE_HISTORY {
        history.remove(0);
    }
    history.push(page);
}

pub(super) fn accept_pending_page(
    pending: &mut Option<(PageRequestId, PageRequestKind)>,
    request_id: PageRequestId,
) -> bool {
    if pending.map(|pending| pending.0) != Some(request_id) {
        return false;
    }
    *pending = None;
    true
}

pub(super) fn has_unsettled_page(pending: Option<(PageRequestId, PageRequestKind)>) -> bool {
    pending.is_some_and(|(_, kind)| kind != PageRequestKind::Search)
}

pub(super) fn page_snapshot_blocked(
    pending: Option<(PageRequestId, PageRequestKind)>,
    busy: bool,
    home_pending: bool,
) -> bool {
    home_pending
        || has_unsettled_page(pending)
        || (busy && !matches!(pending, Some((_, PageRequestKind::Search))))
}

impl App {

    /// What the list pane is showing, for its frame title.
    pub fn list_title(&self) -> String {
        match (self.view, &self.browsing) {
            (View::Library, _) => " library ".into(),
            (View::Home, _) => " home ".to_string(),
            (View::Playing, _) => " now playing ".to_string(),
            (View::Artist, _) => self
                .artist
                .as_ref()
                .map(|artist| format!(" {} ", artist.requested.name))
                .unwrap_or_else(|| " artist ".to_string()),
            (View::Tracks, Some(title)) => format!(" {title} "),
            (View::Tracks, None) => " results ".to_string(),
        }
    }


    pub(super) fn snapshot_blocked(&self) -> bool {
        page_snapshot_blocked(
            self.pending_page_request,
            self.busy,
            self.view == View::Home && self.home_pending,
        )
    }


    pub(super) fn live_player_status(&self) -> String {
        self.snapshot().error.or_else(|| self.playback.failure().map(playback_failure_status)).unwrap_or_default()
    }


    pub(super) fn current_page(&self) -> Option<HistoryEntry> {
        match self.view {
            View::Library => Some(HistoryEntry::Library { cursor: self.library.cursor(), status: self.status.clone() }),
            View::Home => Some(HistoryEntry::Home {
                status: self.status.clone(),
            }),
            View::Tracks => Some(HistoryEntry::Tracks(Box::new(TrackPage {
                results: self.results.clone(),
                query: self.query.clone(),
                search_items: self.search_items.clone(),
                search_filter: self.search_filter,
                selected: self.selected,
                offset: self.offset,
                browsing: self.browsing.clone(),
                browsing_endpoint: self.browsing_endpoint.clone(),
                collection: self.collection.clone(),
                collection_error: self.collection_error.clone(),
                status: self.status.clone(),
            }))),
            View::Artist => self.artist.clone().map(|artist| HistoryEntry::Artist {
                artist: Box::new(artist),
                status: self.status.clone(),
            }),
            View::Playing => {
                let (player_back, back_to) =
                    player_return_target(self.player_back.clone(), self.back_to);
                Some(HistoryEntry::Playing {
                    back_to,
                    player_back: player_back.map(Box::new),
                })
            }
        }
    }


    pub(super) fn push_current_page(&mut self) {
        let Some(page) = self.current_page() else {
            return;
        };
        if self.view == View::Library { self.library.suspend(); }
        self.push_page(page);
    }


    pub(super) fn push_page(&mut self, page: HistoryEntry) {
        push_history(&mut self.history, page);
    }


    pub(super) fn restore_page(&mut self, page: HistoryEntry) {
        self.cancel_page_request();
        self.mode = Mode::Browse;
        match page {
            HistoryEntry::Library { cursor, status } => {
                self.library.restore(cursor);
                self.view = View::Library;
                self.status = status;
                if self.library.connected && !self.library.collection().loaded { self.request_library(false); }
            }
            HistoryEntry::Home { status } => {
                self.status = status;
                self.view = View::Home;
            }
            HistoryEntry::Tracks(page) => {
                self.results = page.results;
                self.query = page.query;
                self.search_items = page.search_items;
                self.search_filter = page.search_filter;
                self.selected = page.selected;
                self.offset = page.offset;
                self.browsing = page.browsing;
                self.browsing_endpoint = page.browsing_endpoint;
                self.collection = page.collection;
                self.collection_error = page.collection_error;
                self.status = page.status;
                self.view = View::Tracks;
            }
            HistoryEntry::Artist { artist, status } => {
                self.artist = Some(*artist);
                if let Some(artist) = self.artist.as_mut() { artist.songs_request = None; }
                self.status = status;
                self.view = View::Artist;
            }
            HistoryEntry::Playing {
                back_to,
                player_back,
            } => {
                let (player_back, back_to) =
                    player_return_target(player_back.map(|page| *page), back_to);
                self.back_to = back_to;
                if self.now.is_some() {
                    self.status = self.live_player_status();
                    self.player_back = player_back;
                    self.view = View::Playing;
                } else if let Some(page) = player_back {
                    self.restore_page(page);
                } else {
                    self.view = back_to;
                }
            }
        }
    }


    pub(super) fn go_back(&mut self) {
        if let Some(page) = self.history.pop() {
            self.restore_page(page);
        } else {
            self.cancel_page_request();
            self.mode = Mode::Browse;
            self.view = View::Home;
        }
    }


    pub fn can_go_back(&self) -> bool {
        self.mode == Mode::Editing || self.view != View::Home || !self.history.is_empty()
    }


    pub(super) fn go_home(&mut self) {
        self.cancel_page_request();
        self.history.clear();
        self.search_page = None;
        self.mode = Mode::Browse;
        self.view = View::Home;
    }


    pub(super) fn begin_page_request(&mut self, kind: PageRequestKind) -> PageRequestId {
        self.page_request = self.page_request.wrapping_add(1).max(1);
        self.pending_page_request = Some((self.page_request, kind));
        self.busy = true;
        self.page_request
    }


    pub(super) fn accept_page_response(&mut self, request_id: PageRequestId) -> bool {
        if !accept_pending_page(&mut self.pending_page_request, request_id) {
            return false;
        }
        self.busy = false;
        true
    }


    pub(super) fn cancel_page_request(&mut self) {
        self.library.suspend();
        if self.pending_page_request.take().is_some() {
            self.busy = false;
        }
    }


    pub(super) fn cancel_search_request(&mut self) {
        if matches!(
            self.pending_page_request,
            Some((_, PageRequestKind::Search))
        ) {
            self.cancel_page_request();
        }
    }


    /// Clamps the open panel's cursor, given how long it is and how much of it
    /// fits. Called by the renderer, for the same reason [`Self::clamp_scroll`]
    /// is: only it knows either number.
    ///
    /// The two kinds of panel clamp differently. A list stops at its last row,
    /// which is what keeps a selection on something; a text panel stops one
    /// screenful short of its end, because scrolling a wall of lyrics past the
    /// bottom of the pane leaves the user looking at nothing.
    pub fn clamp_page(&mut self, viewport: usize, total: usize, selects: bool) {
        let Some(now) = self.now.as_mut() else {
            return;
        };
        let last = if selects {
            total.saturating_sub(1)
        } else {
            total.saturating_sub(viewport)
        };
        let cursor = now.cursor_mut();
        *cursor = (*cursor).min(last);
    }


    /// Puts the open panel's cursor where the panel scrolled itself to.
    ///
    /// Called by the renderer when the lyrics panel is following the singer,
    /// which is the one case where something other than a key decides what is
    /// on screen. Written back rather than kept beside the cursor so that the
    /// moment the user takes over -- one press of `j` -- they carry on from the
    /// line they were looking at, instead of from wherever they had scrolled to
    /// before the follow began.
    pub fn follow_page(&mut self, offset: usize) {
        if let Some(now) = self.now.as_mut() {
            *now.cursor_mut() = offset;
        }
    }


    pub(super) fn open_browse(&mut self, endpoint: BrowseEndpoint, title: String) {
        if self.snapshot_blocked() {
            self.status = "wait for the current page to finish loading".to_string();
            return;
        }
        self.push_current_page();
        self.results.clear();
        self.search_items.clear();
        self.selected = 0;
        self.offset = 0;
        self.browsing = Some(title.clone());
        self.browsing_endpoint = Some(endpoint.clone());
        self.collection = Some(crate::source::collection::Details { title: title.clone(), ..Default::default() });
        self.collection_error = None;
        self.view = View::Tracks;
        self.mode = Mode::Browse;
        self.status = "Loading songs…".into();
        let request_id = self.begin_page_request(PageRequestKind::Browse);
        let request = Request::OpenBrowse {
            request_id,
            endpoint,
            title,
        };
        if self.source.send(request).is_err() {
            self.cancel_page_request();
            self.status = "source worker is not running".to_string();
            self.collection_error = Some(self.status.clone());
        }
    }


    /// Returns to the player page, which the music kept playing behind.
    ///
    /// Only a track that is still playing has a page to return to; with nothing
    /// playing this says so rather than opening an empty one.
    pub(super) fn open_player(&mut self) {
        if self.now.is_some() {
            if self.snapshot_blocked() {
                self.status = "wait for the current page to finish loading".to_string();
                return;
            }
            if self.view != View::Playing {
                let fallback = if self.mode == Mode::Editing {
                    page_behind_search(self.search_origin, self.back_to)
                } else {
                    self.view
                };
                let page = if self.mode == Mode::Editing {
                    self.search_page.take()
                } else {
                    self.current_page()
                };
                (self.player_back, self.back_to) = player_return_target(page, fallback);
            }
            self.cancel_search_request();
            self.search_page = None;
            self.view = View::Playing;
            self.mode = Mode::Browse;
        } else {
            self.status = "nothing is playing".to_string();
        }
    }


    pub(super) fn return_from_player(&mut self) {
        if let Some(page) = self.player_back.take() {
            self.restore_page(page);
        } else {
            self.view = self.back_to;
            self.mode = Mode::Browse;
        }
    }
}
