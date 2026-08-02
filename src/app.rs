//! Application state and input handling.
//!
//! Holds no rendering logic (see [`crate::ui`]) and performs no blocking work:
//! searches and URL resolution are delegated to [`SourceWorker`], playback to
//! [`Player`]. Key handling only mutates state and dispatches messages, so the
//! event loop never stalls.

use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::config::Tokens;
use crate::graphics::Graphics;
use crate::player::{Command, PlayState, Player, Snapshot};
use crate::source::Track;
use crate::source::cover::Cover;
use crate::source::library::Playlist;
use crate::source::worker::{Request, Response, SourceWorker};
use crate::source::youtube::{MAX_RESULTS, extract_video_id};

/// How much of the window the cover is allowed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CoverSize {
    /// A pane beside the results.
    #[default]
    Side,
    /// The whole main area, results hidden. The largest picture the window can
    /// hold -- past this, resolution is bounded by the terminal itself.
    Full,
}

impl CoverSize {
    fn toggled(self) -> Self {
        match self {
            Self::Side => Self::Full,
            Self::Full => Self::Side,
        }
    }
}

/// Where and how large the cover should be painted, in cells and pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImagePlan {
    /// Top-left cell of the image.
    pub col: u16,
    pub row: u16,
    /// Size in pixels. A whole number of cells by construction.
    pub width: u16,
    pub height: u16,
}

/// How many results to request. Bounded by [`MAX_RESULTS`] at the source.
///
/// YouTube pages search results twenty at a time, so this is deliberately one
/// page: asking for fifty costs two further round trips (measured at ~1 s) to
/// fill in rows below the fold that are rarely scrolled to.
const SEARCH_LIMIT: usize = 20;

/// How long the selection must sit still before it is speculatively resolved.
///
/// Long enough that scrolling through the list does not spawn a process per
/// row, short enough that it is usually finished before a decisive user presses
/// Enter.
const PREFETCH_IDLE: Duration = Duration::from_millis(400);

const VOLUME_STEP: f32 = 0.05;

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
    /// Tracks -- either search results or the contents of an opened playlist.
    Tracks,
    /// The signed-in user's playlists.
    Playlists,
}

/// A modal that owns keyboard input for as long as it is up.
///
/// Deliberately a single value rather than a stack: neither of these can
/// meaningfully sit on top of the other, and a stack would only add states that
/// cannot be reached.
pub enum Overlay {
    None,
    /// The sign-in panel. Dismissing this hides it but does not cancel the
    /// sign-in, which is still running on its own thread and will report when
    /// it finishes.
    SignIn(SignIn),
    /// Choosing which playlist to add a track to.
    AddTo {
        video_id: String,
        title: String,
        selected: usize,
    },
    /// A message too long for the status bar. In practice this is the OAuth
    /// setup procedure, which is the single most important thing this feature
    /// ever has to say and is ten lines long -- the status bar would show the
    /// user the first of them and hide the rest.
    Message { body: String },
}

impl Overlay {
    pub fn is_open(&self) -> bool {
        !matches!(self, Self::None)
    }
}

/// How far along the Google device flow is.
///
/// Three states rather than one, because the panel has something different to
/// say in each and the user is looking straight at it the whole time. Collapsing
/// them would mean either a blank window before the code arrives or a failure
/// that vanishes into the status bar the moment it matters most.
pub enum SignIn {
    /// Asked, but Google has not handed back a code yet. Put up the instant the
    /// key is pressed: the request is a round trip, and a keypress with no
    /// visible effect reads as one that did not register.
    Connecting {
        /// Only ever read to animate the panel, which is what distinguishes a
        /// slow round trip from a hung one.
        started: Instant,
    },
    /// The code is on screen and the sign-in thread is polling for approval.
    Waiting {
        user_code: String,
        url: String,
        /// When Google stops accepting the code. Counted down on screen because
        /// the panel is otherwise motionless for however long the user takes to
        /// walk to another device, which is indistinguishable from frozen.
        deadline: Instant,
    },
    /// Terminal failure. Held in the panel rather than dropped into the status
    /// bar: the user is looking here, and the next thing they need is the retry
    /// key -- which this is the only place that offers.
    Failed { reason: String },
}

pub struct App {
    pub mode: Mode,
    pub query: String,
    pub results: Vec<Track>,
    /// Index into `results`. Meaningless when `results` is empty.
    pub selected: usize,
    /// First visible row, maintained so rendering can slice rather than
    /// building a widget item per result.
    pub offset: usize,
    /// Transient one-line message shown in the status bar.
    pub status: String,
    /// True between dispatching a request and receiving its response.
    pub busy: bool,
    pub should_quit: bool,
    /// Thumbnail for the track being played, once it has arrived. Exactly one
    /// is ever held: covers are decoration, not a cache worth growing.
    pub cover: Option<Cover>,
    /// Track the held cover belongs to, so a late response for a track the user
    /// has already skipped past is dropped instead of shown.
    cover_id: Option<String>,
    /// What the terminal can draw, decided once at startup.
    pub graphics: Graphics,
    /// How much of the window the cover gets.
    pub cover_size: CoverSize,
    /// Where the renderer wants the cover painted as real pixels, set on every
    /// frame the sixel path runs. `None` on the half-block path, which needs no
    /// help from the event loop.
    pub image: Option<ImagePlan>,
    /// The plan currently on screen. Pixels are not part of ratatui's buffer,
    /// so they persist until something paints over them -- meaning an unchanged
    /// pane must *not* be repainted every frame.
    painted: Option<ImagePlan>,
    /// When the selection last moved, for the prefetch debounce. `None` once
    /// the current selection has been dealt with, which is what stops the
    /// prefetch from being re-sent on every following frame.
    selection_settled: Option<Instant>,
    /// Id of the speculative resolve in flight, if any. Holding it keeps the
    /// serial worker from stacking several of them in front of a resolve the
    /// user is actually waiting on.
    prefetching: Option<String>,
    /// Id known to be in the worker's URL cache, so playing it is immediate.
    ready: Option<String>,

    /// Which list the main pane is showing.
    pub view: View,
    /// The signed-in user's playlists, once fetched. Empty until then, which is
    /// indistinguishable from an account with none -- and both render the same.
    pub playlists: Vec<Playlist>,
    /// Index into `playlists`, with its own scroll offset: the two lists are
    /// navigated independently, so switching views must not lose either place.
    pub playlist_selected: usize,
    pub playlist_offset: usize,
    /// Id and title of the playlist `results` came from, when it came from one.
    /// `None` means `results` are search results, which have no rows to remove.
    pub open_playlist: Option<(String, String)>,
    pub overlay: Overlay,
    /// Whether a session exists, as far as the last worker response revealed.
    /// Only drives what the status bar offers -- the worker is the authority.
    ///
    /// Seeded from disk at startup rather than left false until the first
    /// response: it decides whether the hints offer `A sign in`, and a user who
    /// signed in last week should not be told to do it again for as long as it
    /// takes them to open the library.
    pub signed_in: bool,

    player: Player,
    source: SourceWorker,
}

impl App {
    pub fn new(player: Player, source: SourceWorker, graphics: Graphics) -> Self {
        Self {
            mode: Mode::Editing,
            query: String::new(),
            results: Vec::new(),
            selected: 0,
            offset: 0,
            // Names the library on the very first frame. It is the one feature
            // here with no visible affordance -- there is nothing on screen to
            // suggest an account exists until someone says so.
            status: "type to search, or ^L for your Google library".to_string(),
            busy: false,
            should_quit: false,
            cover: None,
            cover_id: None,
            graphics,
            cover_size: CoverSize::default(),
            image: None,
            painted: None,
            selection_settled: None,
            prefetching: None,
            ready: None,
            view: View::Tracks,
            playlists: Vec::new(),
            playlist_selected: 0,
            playlist_offset: 0,
            open_playlist: None,
            overlay: Overlay::None,
            // A token file that is present but stale still reads as signed in
            // here. That is the right fidelity for a key hint: the alternative
            // is a refresh round trip before the first frame, and the worker
            // corrects this the moment anything actually needs the session.
            signed_in: Tokens::load().ok().flatten().is_some(),
            player,
            source,
        }
    }

    /// What the list pane is showing, for its frame title.
    pub fn list_title(&self) -> String {
        match (self.view, &self.open_playlist) {
            (View::Playlists, _) => " library ".to_string(),
            (View::Tracks, Some((_, title))) => format!(" {title} "),
            (View::Tracks, None) => " results ".to_string(),
        }
    }

    pub fn snapshot(&self) -> Snapshot {
        self.player.snapshot()
    }

    /// The cover to paint as pixels, if what the renderer planned is not
    /// already on screen. `None` means the pixels there are correct and
    /// repainting would only cost bandwidth and flicker.
    pub fn image_to_paint(&self) -> Option<(&Cover, ImagePlan)> {
        let plan = self.image?;
        if self.painted == Some(plan) {
            return None;
        }
        Some((self.cover.as_ref()?, plan))
    }

    pub fn mark_painted(&mut self) {
        self.painted = self.image;
    }

    /// True when pixels are on screen that no longer belong there: the pane
    /// went away, or moved out from under them. Erasing sixel means painting
    /// cells over it, so only a redraw can undo it.
    pub fn image_needs_clearing(&self) -> bool {
        self.painted.is_some() && self.painted != self.image
    }

    /// Forgets what is on screen so the next frame repaints it. For events that
    /// can destroy the pixels without changing the plan, such as a resize.
    pub fn invalidate_image(&mut self) {
        self.painted = None;
    }

    /// Drains completed source work. Called once per frame.
    ///
    /// Drains rather than taking one: two threads now feed the response queue,
    /// so a cover and a resolve can land in the same frame and handling only
    /// the first would leave the other a frame stale.
    pub fn poll_source(&mut self) {
        while let Some(response) = self.source.poll() {
            self.apply(response);
        }
    }

    fn apply(&mut self, response: Response) {
        // Neither a cover nor a speculative resolve is something the user is
        // waiting on, so neither may clear the flag a search or a play set.
        if !matches!(
            response,
            Response::Cover { .. } | Response::Prefetched { .. }
        ) {
            self.busy = false;
        }

        match response {
            Response::Results(tracks) => {
                self.status = if tracks.is_empty() {
                    "no results".to_string()
                } else {
                    format!("{} results", tracks.len())
                };
                // Search results are not a playlist, so whatever was open no
                // longer describes what is on screen -- and leaving it set
                // would offer a removal that applies to the wrong list.
                self.open_playlist = None;
                self.view = View::Tracks;
                self.results = tracks;
                self.selected = 0;
                self.offset = 0;
                // Start the debounce on the top hit: it is what Enter plays
                // most of the time, so it is the one worth having warm.
                self.selection_settled = Some(Instant::now());
                // Results are the point of a search; move focus to them.
                if !self.results.is_empty() {
                    self.mode = Mode::Browse;
                }
            }
            Response::Resolved { stream, title } => {
                self.status = format!("playing {title}");
                let _ = self.player.send(Command::Play {
                    url: stream.url,
                    title,
                });
            }
            Response::Prefetched { id, ready } => {
                if self.prefetching.as_deref() == Some(id.as_str()) {
                    self.prefetching = None;
                }
                self.ready = ready.then_some(id);
            }
            Response::Cover { id, art } => {
                // Stale unless it is still the track we asked about.
                if self.cover_id.as_deref() == Some(id.as_str()) {
                    self.cover = art;
                }
            }
            Response::DeviceCode {
                user_code,
                url,
                expires_in,
            } => {
                self.status = "waiting for you to approve the sign-in ...".to_string();
                self.overlay = Overlay::SignIn(SignIn::Waiting {
                    user_code,
                    url,
                    deadline: Instant::now() + Duration::from_secs(expires_in),
                });
            }
            Response::SignedIn => {
                self.signed_in = true;
                self.overlay = Overlay::None;
                self.status = "signed in".to_string();
                // Sign-in is only ever reached by asking for something that
                // needs it, so going straight to the library is what the user
                // was after. A pending `AddTo` picks itself up when these land.
                self.request_playlists();
            }
            Response::SignedOut => {
                self.signed_in = false;
                self.playlists.clear();
                self.playlist_selected = 0;
                self.playlist_offset = 0;
                // A playlist on screen is no longer the user's to edit.
                if self.open_playlist.take().is_some() {
                    self.results.clear();
                    self.selected = 0;
                    self.offset = 0;
                }
                self.view = View::Tracks;
                self.status = "signed out (the grant is still listed at \
                               myaccount.google.com/permissions)"
                    .to_string();
            }
            Response::NeedsSignIn => {
                self.signed_in = false;
                // Asking for the library is a clear enough statement of intent
                // to start the flow, rather than reporting an error and making
                // the user press a second key to say so again.
                self.begin_sign_in();
            }
            Response::SignInFailed(msg) => {
                self.signed_in = false;
                // A multi-line failure is the OAuth setup procedure, which is
                // six lines of console instructions -- far more than the panel
                // holds, and not something a retry key can fix. `report` puts
                // that in the overlay built for it; anything shorter is a
                // sign-in outcome and belongs in the panel the user is watching.
                if msg.contains('\n') {
                    self.report(msg);
                } else {
                    self.status = msg.clone();
                    self.overlay = Overlay::SignIn(SignIn::Failed { reason: msg });
                }
            }
            Response::Playlists(playlists) => {
                self.signed_in = true;
                self.playlists = playlists;
                self.playlist_selected = 0;
                self.playlist_offset = 0;

                // A pending add was only waiting on this list; show the picker
                // rather than the library the user did not ask for.
                if let Overlay::AddTo { .. } = self.overlay {
                    self.status = "choose a playlist".to_string();
                } else if self.playlists.is_empty() {
                    self.status = "no playlists".to_string();
                } else {
                    self.view = View::Playlists;
                    // These can land while the keyboard belongs to the search
                    // box -- the sign-in that produced them runs on its own
                    // thread, and the user is free to type meanwhile. Putting a
                    // list on screen without moving focus to it would leave
                    // j/k typing into the query behind it.
                    self.mode = Mode::Browse;
                    self.status = format!("{} playlists", self.playlists.len());
                }
            }
            Response::PlaylistTracks { id, title, tracks } => {
                self.status = if tracks.is_empty() {
                    format!("{title} is empty")
                } else {
                    format!("{} tracks in {title}", tracks.len())
                };
                self.results = tracks;
                self.selected = 0;
                self.offset = 0;
                self.open_playlist = Some((id, title));
                self.view = View::Tracks;
                // Same reason as `Response::Playlists`: the list is the point,
                // so focus follows it.
                if !self.results.is_empty() {
                    self.mode = Mode::Browse;
                }
                self.selection_settled = Some(Instant::now());
            }
            Response::Removed {
                playlist_item_id,
                title,
            } => {
                // Dropped locally rather than re-fetched: losing one line is
                // not worth two round trips and the scroll position.
                self.results
                    .retain(|t| t.playlist_item_id.as_deref() != Some(playlist_item_id.as_str()));
                self.selected = self.selected.min(self.results.len().saturating_sub(1));
                self.status = format!("removed {title}");
            }
            Response::Liked { title, liked } => {
                self.status = if liked {
                    format!("liked {title}")
                } else {
                    format!("unliked {title}")
                };
            }
            Response::Done(msg) => {
                self.status = msg;
            }
            Response::Failed(msg) => self.report(msg),
        }
    }

    /// Shows a failure wherever it will actually be read: the status bar for a
    /// one-liner, an overlay for anything with instructions in it.
    fn report(&mut self, msg: String) {
        if msg.contains('\n') {
            self.status = "press Esc to dismiss".to_string();
            self.overlay = Overlay::Message { body: msg };
        } else {
            self.status = msg;
        }
    }

    /// Asks for the playlist list, reporting a dead worker rather than leaving
    /// the UI stuck on `busy`.
    fn request_playlists(&mut self) {
        self.busy = true;
        if self.source.send(Request::Playlists).is_err() {
            self.busy = false;
            self.status = "source worker is not running".to_string();
        }
    }

    /// Speculatively resolves the selected track once the selection has sat
    /// still for [`PREFETCH_IDLE`]. Called once per frame.
    ///
    /// This is where the latency actually goes. A resolve costs a yt-dlp spawn
    /// of a few seconds; doing it while the user is still reading the list
    /// means the Enter that follows hits a warm cache instead of paying for it.
    pub fn tick_prefetch(&mut self) {
        // Never compete for the serial worker with a request the user is
        // waiting on, and never stack two speculative resolves.
        if self.busy || self.prefetching.is_some() {
            return;
        }
        // Nothing in the results list is under the cursor while another list or
        // a modal has it, so there is nothing worth warming.
        if self.view != View::Tracks || self.overlay.is_open() {
            return;
        }
        let Some(settled) = self.selection_settled else {
            return;
        };
        if settled.elapsed() < PREFETCH_IDLE {
            return;
        }
        // Dealt with, whatever the outcome below.
        self.selection_settled = None;

        let Some(track) = self.results.get(self.selected) else {
            return;
        };
        if self.ready.as_deref() == Some(track.id.as_str()) {
            return;
        }

        let id = track.id.clone();
        if self.source.send(Request::Prefetch { id: id.clone() }).is_ok() {
            self.prefetching = Some(id);
        }
    }

    /// Keeps `offset` such that `selected` is visible in a viewport `height`
    /// rows tall. Called by the renderer, which is what knows the height.
    pub fn clamp_scroll(&mut self, height: usize) {
        if height == 0 || self.results.is_empty() {
            self.offset = 0;
            return;
        }
        if self.selected < self.offset {
            self.offset = self.selected;
        } else if self.selected >= self.offset + height {
            self.offset = self.selected + 1 - height;
        }
        // Avoid a trailing gap when the list shrinks.
        let max_offset = self.results.len().saturating_sub(height);
        self.offset = self.offset.min(max_offset);
    }

    /// [`Self::clamp_scroll`] for the playlist list, which scrolls separately.
    pub fn clamp_playlist_scroll(&mut self, height: usize) {
        if height == 0 || self.playlists.is_empty() {
            self.playlist_offset = 0;
            return;
        }
        if self.playlist_selected < self.playlist_offset {
            self.playlist_offset = self.playlist_selected;
        } else if self.playlist_selected >= self.playlist_offset + height {
            self.playlist_offset = self.playlist_selected + 1 - height;
        }
        let max_offset = self.playlists.len().saturating_sub(height);
        self.playlist_offset = self.playlist_offset.min(max_offset);
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> Result<()> {
        // Ctrl-C quits from any mode.
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.should_quit = true;
            return Ok(());
        }

        // The library needs a way in that does not depend on which pane has the
        // keyboard. The program starts in the search box, where every printable
        // key is text -- a bare `L` there types an L -- so without a modifier
        // the library would only be reachable after a search had already
        // returned something, which is not a precondition it actually has.
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('l') | KeyCode::Char('L'))
        {
            self.open_library();
            return Ok(());
        }

        // A modal owns the keyboard outright while it is up, so a stray `q`
        // aimed at the picker cannot quit the program behind it.
        if self.overlay.is_open() {
            return self.handle_overlay_key(key);
        }

        match self.mode {
            Mode::Editing => self.handle_editing_key(key),
            Mode::Browse => match self.view {
                View::Tracks => self.handle_browse_key(key),
                View::Playlists => self.handle_playlists_key(key),
            },
        }
    }

    fn handle_overlay_key(&mut self, key: KeyEvent) -> Result<()> {
        match &mut self.overlay {
            Overlay::None => {}
            // A failed sign-in is the one phase with somewhere to go: the thread
            // behind it is gone, so `A` here starts a fresh flow rather than
            // making the user dismiss the panel and find the key again.
            Overlay::SignIn(SignIn::Failed { .. }) => match key.code {
                KeyCode::Char('A') | KeyCode::Char('a') => self.begin_sign_in(),
                KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q') => {
                    self.overlay = Overlay::None;
                    self.status = "press A to try signing in again".to_string();
                }
                _ => {}
            },
            // Dismissing only hides it. The sign-in thread is still polling and
            // will report when the user approves -- there is no way to cancel a
            // device code, and pretending otherwise would be a lie.
            Overlay::SignIn(_) => {
                if matches!(key.code, KeyCode::Esc | KeyCode::Enter) {
                    self.overlay = Overlay::None;
                    self.status = "sign-in still pending in the background".to_string();
                }
            }
            Overlay::Message { .. } => {
                if matches!(key.code, KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q')) {
                    self.overlay = Overlay::None;
                    self.status = String::new();
                }
            }
            Overlay::AddTo { selected, .. } => {
                let last = self.playlists.len().saturating_sub(1);
                match key.code {
                    KeyCode::Char('j') | KeyCode::Down => *selected = (*selected + 1).min(last),
                    KeyCode::Char('k') | KeyCode::Up => *selected = selected.saturating_sub(1),
                    KeyCode::Char('g') | KeyCode::Home => *selected = 0,
                    KeyCode::Char('G') | KeyCode::End => *selected = last,
                    KeyCode::Enter => self.confirm_add(),
                    KeyCode::Esc | KeyCode::Char('q') => {
                        self.overlay = Overlay::None;
                        self.status = "cancelled".to_string();
                    }
                    _ => {}
                }
            }
        }
        Ok(())
    }

    fn handle_playlists_key(&mut self, key: KeyEvent) -> Result<()> {
        let last = self.playlists.len().saturating_sub(1);
        match key.code {
            KeyCode::Char('q') => self.should_quit = true,
            KeyCode::Char('j') | KeyCode::Down => {
                self.playlist_selected = (self.playlist_selected + 1).min(last);
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.playlist_selected = self.playlist_selected.saturating_sub(1);
            }
            KeyCode::Char('g') | KeyCode::Home => self.playlist_selected = 0,
            KeyCode::Char('G') | KeyCode::End => self.playlist_selected = last,
            KeyCode::PageDown => self.playlist_selected = (self.playlist_selected + 10).min(last),
            KeyCode::PageUp => self.playlist_selected = self.playlist_selected.saturating_sub(10),
            KeyCode::Enter => self.open_selected_playlist(),
            KeyCode::Char('r') => self.request_playlists(),
            KeyCode::Char('A') => self.begin_sign_in(),
            KeyCode::Char('x') => self.sign_out(),
            // Back to whatever the track list was showing before.
            KeyCode::Esc | KeyCode::Char('L') | KeyCode::Char('l') => self.view = View::Tracks,
            KeyCode::Char('/') | KeyCode::Char('i') => {
                self.view = View::Tracks;
                self.mode = Mode::Editing;
                self.status = "type to search, Enter to run".to_string();
            }
            // Playback keys stay live here: the library is a list to browse,
            // not a reason to lose control of what is already playing.
            KeyCode::Char(' ') => {
                let _ = self.player.send(Command::TogglePause);
            }
            KeyCode::Char('+') | KeyCode::Char('=') => self.nudge_volume(VOLUME_STEP),
            KeyCode::Char('-') | KeyCode::Char('_') => self.nudge_volume(-VOLUME_STEP),
            _ => {}
        }
        Ok(())
    }

    fn handle_editing_key(&mut self, key: KeyEvent) -> Result<()> {
        match key.code {
            KeyCode::Enter => self.submit_search(),
            KeyCode::Esc => {
                // Unconditional, even with an empty list. It used to require
                // results, on the reasoning that there was nothing to go back
                // to -- but that left a fresh launch with no way out of the
                // search box at all, and single-key commands like `L` and `q`
                // only exist on the other side of it.
                self.mode = Mode::Browse;
                self.status = "/ to search, L for your library".to_string();
            }
            KeyCode::Backspace => {
                self.query.pop();
            }
            KeyCode::Char(c) => self.query.push(c),
            _ => {}
        }
        Ok(())
    }

    fn handle_browse_key(&mut self, key: KeyEvent) -> Result<()> {
        let was_selected = self.selected;
        match key.code {
            KeyCode::Char('q') => self.should_quit = true,
            KeyCode::Char('/') | KeyCode::Char('i') => {
                self.mode = Mode::Editing;
                self.status = "type to search, Enter to run".to_string();
            }
            KeyCode::Char('j') | KeyCode::Down => self.move_selection(1),
            KeyCode::Char('k') | KeyCode::Up => self.move_selection(-1),
            KeyCode::Char('g') | KeyCode::Home => self.selected = 0,
            KeyCode::Char('G') | KeyCode::End => {
                self.selected = self.results.len().saturating_sub(1);
            }
            KeyCode::PageDown => self.move_selection(10),
            KeyCode::PageUp => self.move_selection(-10),
            KeyCode::Enter => self.play_selected(),
            KeyCode::Char(' ') => {
                let _ = self.player.send(Command::TogglePause);
            }
            KeyCode::Char('c') => self.cover_size = self.cover_size.toggled(),
            KeyCode::Char('s') => {
                let _ = self.player.send(Command::Stop);
                self.status = "stopped".to_string();
                // Nothing is playing, so nothing owns the cover pane.
                self.cover = None;
                self.cover_id = None;
            }
            KeyCode::Char('+') | KeyCode::Char('=') => self.nudge_volume(VOLUME_STEP),
            KeyCode::Char('-') | KeyCode::Char('_') => self.nudge_volume(-VOLUME_STEP),
            KeyCode::Right => self.seek_relative(5),
            KeyCode::Left => self.seek_relative(-5),
            // Both cases: `l` is otherwise unbound here, and a user who reaches
            // for the library is not thinking about the shift key.
            KeyCode::Char('L') | KeyCode::Char('l') => self.open_library(),
            // Upper case, and deliberately not next to `a`: this is the key
            // `auth` names when it tells the user a saved sign-in has expired,
            // and lower `a` is the add-to-playlist key one row of muscle memory
            // away. Signing in by mistake is a browser tab the user did not ask
            // for; adding to a playlist by mistake edits their account.
            KeyCode::Char('A') => self.begin_sign_in(),
            KeyCode::Char('a') => self.begin_add(),
            KeyCode::Char('d') => self.remove_selected(),
            KeyCode::Char('f') => self.toggle_like(),
            // Leaves an open playlist without disturbing search results, which
            // is the only thing there is to go back to.
            KeyCode::Esc if self.open_playlist.is_some() => self.open_library(),
            _ => {}
        }
        // Restarting the debounce here rather than in each movement arm catches
        // every path that moves the cursor, including ones added later.
        if self.selected != was_selected {
            self.selection_settled = Some(Instant::now());
        }
        Ok(())
    }

    fn move_selection(&mut self, delta: isize) {
        if self.results.is_empty() {
            return;
        }
        let last = self.results.len() - 1;
        let next = self.selected as isize + delta;
        self.selected = next.clamp(0, last as isize) as usize;
    }

    fn submit_search(&mut self) {
        let query = self.query.trim().to_string();
        if query.is_empty() {
            self.status = "enter something to search for".to_string();
            return;
        }

        // A pasted link or bare video id plays directly; the search box doubles
        // as an address bar.
        let (request, playing) = match extract_video_id(&query) {
            Some(id) => {
                self.status = format!("resolving {id} ...");
                let request = Request::Resolve {
                    id: id.clone(),
                    title: query.clone(),
                };
                (request, Some(id))
            }
            None => {
                self.status = format!("searching for {query} ...");
                let request = Request::Search {
                    query,
                    limit: SEARCH_LIMIT.min(MAX_RESULTS),
                };
                (request, None)
            }
        };

        self.busy = true;
        if self.source.send(request).is_err() {
            self.busy = false;
            self.status = "source worker is not running".to_string();
            return;
        }
        if let Some(id) = playing {
            self.request_cover(id);
        }
    }

    fn play_selected(&mut self) {
        let Some(track) = self.results.get(self.selected) else {
            return;
        };
        // Resolving takes seconds and spawns yt-dlp, so it goes to the worker
        // and playback starts when the response arrives -- unless a prefetch
        // already warmed the cache, in which case the round trip is all that is
        // left and saying "resolving" would be a lie the user can see through.
        self.status = if self.ready.as_deref() == Some(track.id.as_str()) {
            format!("starting {} ...", track.title)
        } else {
            format!("resolving {} ...", track.title)
        };
        self.busy = true;
        let id = track.id.clone();
        if self
            .source
            .send(Request::Resolve {
                id: id.clone(),
                // The label, not the bare title: this is what the status bar
                // shows for as long as the track plays.
                title: track.label(),
            })
            .is_err()
        {
            self.busy = false;
            self.status = "source worker is not running".to_string();
            return;
        }
        self.request_cover(id);
    }

    /// Queues a cover fetch for `id` and drops whatever cover is on screen.
    ///
    /// Always sent *after* the resolve it accompanies: the worker is serial, so
    /// the reverse order would put a picture ahead of the audio the user asked
    /// for. A send failure needs no message -- the resolve just ahead of it will
    /// have reported the same dead worker.
    fn request_cover(&mut self, id: String) {
        self.cover = None;
        self.cover_id = Some(id.clone());
        let _ = self.source.send(Request::Cover { id });
    }

    /// Shows the library, fetching it if it has not been fetched this session.
    ///
    /// A stale list is shown immediately rather than blanked while it refetches:
    /// playlists change rarely, and `r` reloads on demand. When the user is not
    /// signed in the worker answers [`Response::NeedsSignIn`], which starts the
    /// flow -- so this key works whether or not there is a session.
    fn open_library(&mut self) {
        self.view = View::Playlists;
        // Reachable from the search box, where the keyboard belongs to the
        // query. Without this the list would appear but j/k would type into the
        // search box behind it.
        self.mode = Mode::Browse;
        if self.playlists.is_empty() {
            self.status = "loading your library ...".to_string();
            self.request_playlists();
        }
    }

    fn open_selected_playlist(&mut self) {
        let Some(playlist) = self.playlists.get(self.playlist_selected) else {
            return;
        };
        self.status = format!("opening {} ...", playlist.title);
        self.busy = true;
        let request = Request::OpenPlaylist {
            id: playlist.id.clone(),
            title: playlist.title.clone(),
        };
        if self.source.send(request).is_err() {
            self.busy = false;
            self.status = "source worker is not running".to_string();
        }
    }

    /// Starts the Google device flow and puts the panel up to say so.
    ///
    /// The panel goes up before the request is answered rather than when the
    /// code arrives: asking Google for one is a round trip over TLS, and until
    /// it returns there is nothing on screen to show the key did anything.
    ///
    /// Not guarded on `signed_in`. A session that exists can still be one Google
    /// has since invalidated -- that is exactly the case `auth` points at this
    /// key -- so refusing to re-run the flow would block the only way out of it.
    fn begin_sign_in(&mut self) {
        self.status = "signing in with Google ...".to_string();
        self.overlay = Overlay::SignIn(SignIn::Connecting {
            started: Instant::now(),
        });
        if self.source.send(Request::SignIn).is_err() {
            self.status = "source worker is not running".to_string();
            self.overlay = Overlay::SignIn(SignIn::Failed {
                reason: "source worker is not running".to_string(),
            });
        }
    }

    fn sign_out(&mut self) {
        if self.source.send(Request::SignOut).is_err() {
            self.status = "source worker is not running".to_string();
        }
    }

    /// Opens the playlist picker for the selected track.
    ///
    /// The overlay is put up before the list arrives when the list is not
    /// already held, so the keypress has a visible effect immediately;
    /// [`Response::Playlists`] fills it in.
    fn begin_add(&mut self) {
        let Some(track) = self.results.get(self.selected) else {
            return;
        };
        self.overlay = Overlay::AddTo {
            video_id: track.id.clone(),
            title: track.label(),
            selected: 0,
        };

        if self.playlists.is_empty() {
            self.status = "loading your playlists ...".to_string();
            self.request_playlists();
        } else {
            self.status = "choose a playlist".to_string();
        }
    }

    fn confirm_add(&mut self) {
        let Overlay::AddTo {
            video_id, selected, ..
        } = &self.overlay
        else {
            return;
        };
        let Some(playlist) = self.playlists.get(*selected) else {
            self.status = "no playlist to add to".to_string();
            return;
        };

        let request = Request::AddToPlaylist {
            playlist_id: playlist.id.clone(),
            playlist_title: playlist.title.clone(),
            video_id: video_id.clone(),
        };
        self.status = format!("adding to {} ...", playlist.title);
        self.overlay = Overlay::None;
        self.busy = true;
        if self.source.send(request).is_err() {
            self.busy = false;
            self.status = "source worker is not running".to_string();
        }
    }

    /// Removes the selected track from the playlist it is being shown from.
    ///
    /// Only meaningful inside a playlist: a search result is not in one, and
    /// "Liked songs" is not a playlist the API will remove a row from -- `f`
    /// unlikes there instead.
    fn remove_selected(&mut self) {
        let Some(track) = self.results.get(self.selected) else {
            return;
        };
        let Some(item_id) = track.playlist_item_id.clone() else {
            self.status = if self.open_playlist.is_some() {
                "this row cannot be removed -- press f to unlike".to_string()
            } else {
                "not in a playlist -- press L to open one".to_string()
            };
            return;
        };

        let title = track.label();
        self.status = format!("removing {title} ...");
        self.busy = true;
        let request = Request::RemoveFromPlaylist {
            playlist_item_id: item_id,
            title,
        };
        if self.source.send(request).is_err() {
            self.busy = false;
            self.status = "source worker is not running".to_string();
        }
    }

    fn toggle_like(&mut self) {
        let Some(track) = self.results.get(self.selected) else {
            return;
        };
        self.status = format!("rating {} ...", track.title);
        self.busy = true;
        let request = Request::ToggleLike {
            video_id: track.id.clone(),
            title: track.label(),
        };
        if self.source.send(request).is_err() {
            self.busy = false;
            self.status = "source worker is not running".to_string();
        }
    }

    fn nudge_volume(&mut self, delta: f32) {
        let volume = (self.snapshot().volume + delta).clamp(0.0, 2.0);
        let _ = self.player.send(Command::SetVolume(volume));
        self.status = format!("volume {:.0}%", volume * 100.0);
    }

    /// Seeks relative to the current position, clamped at zero.
    ///
    /// Seeking backwards beyond what remains in the ring buffer may fail; the
    /// player reports that through the snapshot rather than panicking.
    fn seek_relative(&mut self, secs: i64) {
        let snap = self.snapshot();
        if snap.state == PlayState::Idle {
            return;
        }
        let target = if secs >= 0 {
            snap.position + Duration::from_secs(secs as u64)
        } else {
            snap.position
                .saturating_sub(Duration::from_secs(secs.unsigned_abs()))
        };
        let _ = self.player.send(Command::Seek(target));
    }
}
