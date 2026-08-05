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
use crate::source::{StreamUrl, Track};
use crate::source::cover::Cover;
use crate::source::home::{Card, Shelf, Target};
use crate::source::journal;
use crate::source::library::Playlist;
use crate::source::watch::{Comments, Lyrics, Watch};
use crate::source::worker::{Request, Response, SourceWorker};
use crate::source::youtube::{MAX_RESULTS, extract_video_id};
use crate::tray::TrayCommand;

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

/// Tracks the queue may skip past in a row before it gives up.
///
/// A radio queue is built by YouTube, not by the user, and some of what it
/// offers cannot be resolved from here at all -- so one dead track must not end
/// the session. A run of them, though, is a signal that something is wrong with
/// resolving rather than with the tracks, and silently walking a queue of sixty
/// looking for one that works is not a thing to do on the user's behalf.
const MAX_AUTO_SKIPS: u8 = 3;

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
    /// The landing page: YouTube Music's home feed as shelves of cards. What
    /// the program opens on, because a music player that opens on an empty list
    /// asks the user to think of something before it will do anything.
    Home,
    /// Tracks -- either search results or the contents of an opened playlist.
    Tracks,
    /// The signed-in user's playlists.
    Playlists,
    /// The player page: the cover of what is playing, and beside it the queue,
    /// the lyrics, the comments and what to listen to next. Opened by playing
    /// something, which is the moment all four become answerable.
    Playing,
}

/// Which panel of the player page is showing.
///
/// One at a time rather than side by side, for the same reason the main pane
/// switches between lists: on a terminal, width is the scarce resource, and
/// four columns of it would leave none of them readable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    UpNext,
    Lyrics,
    Comments,
    Related,
}

impl Tab {
    /// In the order they are drawn, which is the order the keys `1`-`4` and the
    /// Tab key walk them in.
    pub const ALL: [Tab; 4] = [Tab::UpNext, Tab::Lyrics, Tab::Comments, Tab::Related];

    pub fn label(self) -> &'static str {
        match self {
            Self::UpNext => "UP NEXT",
            Self::Lyrics => "LYRICS",
            Self::Comments => "COMMENTS",
            Self::Related => "RELATED",
        }
    }

    /// Where this tab sits in [`Self::ALL`], which is also where its cursor
    /// sits in [`NowPlaying::cursor`].
    pub fn index(self) -> usize {
        Self::ALL.iter().position(|tab| *tab == self).unwrap_or(0)
    }

    /// Wraps, in both directions: four tabs on one row are a ring, and a Tab
    /// key that stops at the end just looks broken.
    fn shifted(self, delta: isize) -> Self {
        let len = Self::ALL.len() as isize;
        Self::ALL[((self.index() as isize + delta).rem_euclid(len)) as usize]
    }
}

/// A panel fetched the first time it is looked at.
///
/// Four states rather than `Option`, because the panel has something different
/// to say in each and the user is looking straight at it: an empty box means
/// "not asked for yet" to us and "broken" to them.
pub enum Panel<T> {
    /// Never opened, so never fetched. One HTTPS call per panel is not worth
    /// spending on tabs a play never opens.
    Idle,
    Loading,
    Ready(T),
    /// Nothing to show, and why -- an instrumental has no lyrics, and a track
    /// with comments turned off has no comments.
    Empty(String),
}

impl<T> Panel<T> {
    /// True when this panel has not been asked for yet, which is the only state
    /// a tab switch should start a fetch from -- otherwise opening the same tab
    /// twice would fetch it twice.
    fn is_idle(&self) -> bool {
        matches!(self, Self::Idle)
    }
}

/// What is playing, and everything the player page shows around it.
///
/// Held whole rather than as loose fields on [`App`] so that starting a track
/// replaces it in one move: every panel here belongs to one video id, and a
/// half-replaced page would show one track's lyrics under another's title.
pub struct NowPlaying {
    pub video_id: String,
    pub title: String,
    pub artist: String,
    pub album: Option<String>,
    /// The track's length, which the player itself does not report -- rodio
    /// knows only how far it has got. Without this there is no progress bar,
    /// only a clock.
    pub duration: Option<Duration>,
    /// What YouTube calls the queue: "Let It Happen Mix", or the playlist the
    /// track was started from. Empty until the queue arrives.
    pub queue_title: String,
    pub queue: Vec<Track>,
    /// Where in `queue` the playing track is. `None` until the queue lands, or
    /// when what is playing is not in it.
    pub playing: Option<usize>,
    pub tab: Tab,
    /// One cursor per tab, so switching away and back returns to where the user
    /// left off. For the two list tabs this is the selected row; for lyrics and
    /// comments, which have nothing to select, it is the first visible line.
    pub cursor: [usize; Tab::ALL.len()],
    pub lyrics: Panel<Lyrics>,
    pub comments: Panel<Comments>,
    pub related: Panel<Vec<Shelf>>,
    /// Browse ids from the watch response, held until the tab they belong to is
    /// opened. `None` means the track has no such tab -- most videos have no
    /// lyrics, and that is not a failure.
    lyrics_id: Option<String>,
    related_id: Option<String>,
}

impl NowPlaying {
    pub fn new(track: &Track) -> Self {
        Self {
            video_id: track.id.clone(),
            title: track.title.clone(),
            artist: track.uploader.clone(),
            album: track.album.clone(),
            duration: track.duration,
            queue_title: String::new(),
            queue: Vec::new(),
            playing: None,
            tab: Tab::UpNext,
            cursor: [0; Tab::ALL.len()],
            lyrics: Panel::Idle,
            comments: Panel::Idle,
            related: Panel::Idle,
            lyrics_id: None,
            related_id: None,
        }
    }

    /// One line under the title, as YouTube Music writes it: "Tame Impala •
    /// Currents". The album is dropped rather than shown empty.
    pub fn byline(&self) -> String {
        match (&self.artist, &self.album) {
            (artist, Some(album)) if !artist.is_empty() => format!("{artist} • {album}"),
            (artist, _) => artist.clone(),
        }
    }

    /// The cursor for the tab on screen.
    pub fn cursor(&self) -> usize {
        self.cursor[self.tab.index()]
    }

    fn cursor_mut(&mut self) -> &mut usize {
        let index = self.tab.index();
        &mut self.cursor[index]
    }

    /// The track the queue would play next, if there is one.
    fn next_in_queue(&self) -> Option<&Track> {
        self.queue.get(self.playing? + 1)
    }

    /// The Related tab flattened into rows.
    ///
    /// It arrives as shelves -- "You might also like", "Recommended playlists"
    /// -- which is a grid, and a grid is the wrong shape for a panel this
    /// narrow. Drawn as headings with their cards listed underneath, the whole
    /// tab becomes one list the cursor can walk, and the shelf a card came from
    /// is still on screen above it.
    pub fn related_rows(&self) -> Vec<RelatedRow<'_>> {
        let Panel::Ready(shelves) = &self.related else {
            return Vec::new();
        };
        shelves
            .iter()
            .flat_map(|shelf| {
                std::iter::once(RelatedRow::Heading(&shelf.title))
                    .chain(shelf.cards.iter().map(RelatedRow::Card))
            })
            .collect()
    }
}

/// One row of the flattened Related tab.
pub enum RelatedRow<'a> {
    Heading(&'a str),
    Card(&'a Card),
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
    /// Set by `B`, cleared by the event loop once it has actually let go of the
    /// terminal. A request rather than an action because detaching is the event
    /// loop's business: this type never touches the console, and the terminal
    /// has to be handed back to the shell in good order before the console goes
    /// away -- which is something only the code that put it in raw mode can do.
    pub wants_background: bool,
    /// The same in reverse, set by the tray's "Show the player".
    pub wants_foreground: bool,
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
    /// The track a resolve is in flight for -- what the user is waiting to
    /// hear. Everything about a play hangs off this: it is what makes a resolve
    /// that lands after the user has moved on get dropped instead of played,
    /// and what keeps the queue from advancing over a track that is still on
    /// its way. `None` means nothing is being loaded.
    pending: Option<String>,

    /// Which list the main pane is showing.
    pub view: View,
    /// The landing page, once it has arrived. Empty until then, and empty
    /// forever if YouTube would not answer -- both of which the pane says.
    pub home: Vec<Shelf>,
    /// Which shelf the cursor is on, and which card within it. Two indices
    /// rather than one, because the landing page is a grid of ragged rows: a
    /// flat index would have to be recomputed every time a shelf changed
    /// length, and moving down a shelf is not moving forward by any fixed
    /// number of cards.
    pub home_shelf: usize,
    pub home_card: usize,
    /// First visible shelf, maintained like [`Self::offset`] so the renderer
    /// slices rather than laying out shelves it will not draw.
    pub home_top: usize,
    /// First visible card of each shelf, one per shelf. Kept per shelf rather
    /// than for the focused one alone so that scrolling a shelf, moving away
    /// and coming back returns to where the user left it.
    pub home_scroll: Vec<usize>,
    /// True between asking for the landing page and hearing back.
    ///
    /// Its own flag rather than [`Self::busy`]: this is set before the first
    /// frame is drawn, and `busy` would hold off the prefetch and spin the
    /// event loop at the faster tick for the whole of a request nobody is
    /// waiting on. What it is actually for is the difference between "loading"
    /// and "there is no feed", which are the same empty pane.
    pub home_pending: bool,
    /// Title of whatever was opened from the landing page, when the track list
    /// is showing one. Not [`Self::open_playlist`]: these rows belong to a
    /// YouTube Music album or a stranger's playlist, so there is nothing here
    /// that removing a row could apply to.
    pub browsing: Option<String>,
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

    /// What is playing and the page around it. `None` before the first play and
    /// after a stop -- there is no player page for silence.
    pub now: Option<NowPlaying>,
    /// The view to return to from the player page. Remembered rather than
    /// assumed, because a track can be started from any of the three lists and
    /// Esc should go back to the one it came from.
    back_to: View,
    /// Player state as of the last frame, so a track that has *ended* can be
    /// told from one that was already stopped. The snapshot only says what is
    /// true now; the queue advances on the edge between the two.
    last_state: PlayState,
    /// True while the queue is playing itself rather than the user pressing
    /// Enter. Only these plays are allowed to skip past a failure -- a track
    /// the user chose that will not resolve is worth reporting, not skipping.
    auto: bool,
    /// Consecutive tracks the queue has skipped past for failing to resolve.
    /// Reset by anything that plays.
    skips: u8,
    /// The track being listened to, and the furthest into it playback has got.
    ///
    /// Held here rather than read off the snapshot when it is needed, because by
    /// the time a play is over there is nothing left to read: the track has
    /// ended, the snapshot says `Idle` at position zero, and the page below has
    /// already been replaced with whatever came next. This is what
    /// [`App::finish_listening`] reports from.
    ///
    /// The *furthest*, not the latest: seeking backwards must not shorten what
    /// the user is recorded as having heard.
    listening: Option<(Track, Duration)>,
    /// Steps the radio seeds on the landing page along, so asking for it again
    /// gives a different page rather than the one already on screen.
    rotation: usize,
    /// Whether the browser import has been asked for this session. Once only:
    /// a successful import asks for the landing page again, and without this
    /// that second request would ask for another import behind it, forever.
    imported: bool,

    player: Player,
    source: SourceWorker,
}

/// Turns a panel fetch into what the panel should show.
///
/// A failure becomes [`Panel::Empty`] rather than a status-bar error on
/// purpose: the message here is almost always a fact about the track -- it has
/// no lyrics, its comments are turned off -- and it belongs in the panel that
/// is otherwise blank rather than on top of what is playing.
fn panel<T>(fetched: Result<T, String>) -> Panel<T> {
    match fetched {
        Ok(value) => Panel::Ready(value),
        Err(msg) => Panel::Empty(msg),
    }
}

impl App {
    pub fn new(player: Player, source: SourceWorker, graphics: Graphics) -> Self {
        let mut app = Self {
            // Browse, not Editing: the program now opens on a page there is
            // something to do with, and the keys that move around it are bare
            // letters the search box would otherwise swallow.
            mode: Mode::Browse,
            query: String::new(),
            results: Vec::new(),
            selected: 0,
            offset: 0,
            status: "loading the home feed ...".to_string(),
            busy: false,
            should_quit: false,
            wants_background: false,
            wants_foreground: false,
            cover: None,
            cover_id: None,
            graphics,
            cover_size: CoverSize::default(),
            image: None,
            painted: None,
            selection_settled: None,
            prefetching: None,
            ready: None,
            pending: None,
            view: View::Home,
            home: Vec::new(),
            home_shelf: 0,
            home_card: 0,
            home_top: 0,
            home_scroll: Vec::new(),
            home_pending: false,
            browsing: None,
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
            now: None,
            back_to: View::Home,
            last_state: PlayState::Idle,
            auto: false,
            skips: 0,
            listening: None,
            rotation: 0,
            imported: false,
            player,
            source,
        };

        // Fired before the first frame. It is one HTTPS round trip on a thread
        // that has nothing else to do at launch, and the pane it fills is the
        // whole of what the user is looking at.
        app.request_home();
        app
    }

    /// What the list pane is showing, for its frame title.
    pub fn list_title(&self) -> String {
        match (self.view, &self.open_playlist, &self.browsing) {
            (View::Home, _, _) => " home ".to_string(),
            (View::Playlists, _, _) => " library ".to_string(),
            (View::Playing, _, _) => " now playing ".to_string(),
            (View::Tracks, Some((_, title)), _) | (View::Tracks, None, Some(title)) => {
                format!(" {title} ")
            }
            (View::Tracks, None, None) => " results ".to_string(),
        }
    }

    /// The card under the cursor on the landing page.
    pub fn home_card(&self) -> Option<&Card> {
        self.home.get(self.home_shelf)?.cards.get(self.home_card)
    }

    pub fn snapshot(&self) -> Snapshot {
        self.player.snapshot()
    }

    /// True while the user is waiting on something: a request in flight, or a
    /// track being loaded. What the event loop redraws faster for -- a resolve
    /// that lands from cache is over in milliseconds, and waiting out the idle
    /// tick to notice would be most of the latency it has left.
    pub fn awaiting(&self) -> bool {
        self.busy || self.pending.is_some()
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
        // Nothing here is something the user is waiting on, so none of it may
        // clear the flag a search or a play set. The player page matters as
        // much as the cover does: its panels land on their own threads, and the
        // queue usually arrives while the resolve behind it is still running --
        // clearing the flag there would drop the redraw cadence back to the
        // idle one for the rest of the wait that flag exists to cover.
        //
        // A resolve is excluded for a different reason: it is only the answer
        // to what the user is waiting for when it is for the track they are
        // waiting on, and one for a track they have skipped past must not
        // declare the wait over. `apply_resolved` clears the flag when it is.
        if !matches!(
            response,
            Response::Cover { .. }
                | Response::Prefetched { .. }
                | Response::Resolved { .. }
                | Response::Watch { .. }
                | Response::Lyrics { .. }
                | Response::Related { .. }
                | Response::Comments { .. }
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
                self.browsing = None;
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
            Response::Home(shelves) => {
                self.home_pending = false;
                self.status = if shelves.is_empty() {
                    "nothing came back from YouTube Music".to_string()
                } else {
                    // Names the library here rather than in the hints column,
                    // which has no room left for it and is already spending
                    // what it has on the keys that move the cursor.
                    "Enter to play, / to search, L for your library".to_string()
                };
                self.home_scroll = vec![0; shelves.len()];
                self.home = shelves;
                self.home_shelf = 0;
                self.home_card = 0;
                self.home_top = 0;
                // Warm the first card while the user is still reading the page,
                // the same bargain the results list makes.
                self.selection_settled = Some(Instant::now());
            }
            Response::CookiesImported(browser) => {
                // The page on screen was built without a session. There is a
                // better one available now, so it is asked for again -- this is
                // the whole point of doing the import in the background rather
                // than holding the first frame back behind it.
                self.status = format!("read your YouTube session from {browser}");
                self.request_home();
            }
            Response::PersonalShelves(shelves) => {
                self.home_pending = false;
                if shelves.is_empty() {
                    return;
                }
                // In front of the feed: these are the shelves built from what
                // this account actually listens to, and they are what the page
                // is worth opening for.
                let added = shelves.len();
                self.home_scroll.splice(0..0, vec![0; added]);
                self.home.splice(0..0, shelves);

                // The cursor is on a shelf, not at an index -- so it moves down
                // with the shelf it was on rather than staying put and pointing
                // at something the user was not looking at. Except at the very
                // top, where a user who has not moved yet should be shown the
                // new first shelf rather than pushed off it.
                if self.home_shelf > 0 || self.home_card > 0 {
                    self.home_shelf += added;
                    self.home_top += added;
                }
                self.status = format!("{} shelves", self.home.len());
            }
            Response::Browsed { title, tracks } => {
                self.status = format!("{} tracks in {title}", tracks.len());
                self.results = tracks;
                self.selected = 0;
                self.offset = 0;
                // Not the user's playlist: `d` has nothing to remove a row from.
                self.open_playlist = None;
                self.browsing = Some(title);
                self.view = View::Tracks;
                self.mode = Mode::Browse;
                self.selection_settled = Some(Instant::now());
            }
            Response::Resolved { id, title, stream } => self.apply_resolved(&id, title, stream),
            Response::Watch { video_id, watch } => self.apply_watch(&video_id, *watch),
            Response::Lyrics { video_id, lyrics } => {
                if let Some(now) = self.for_track(&video_id) {
                    now.lyrics = panel(lyrics);
                }
            }
            Response::Related { video_id, related } => {
                if let Some(now) = self.for_track(&video_id) {
                    now.related = panel(related);
                }
            }
            Response::Comments { video_id, comments } => {
                if let Some(now) = self.for_track(&video_id) {
                    now.comments = panel(*comments);
                }
            }
            Response::Prefetched { id, ready } => {
                if self.prefetching.as_deref() == Some(id.as_str()) {
                    self.prefetching = None;
                }
                // Only ever says something about the track it names. Two things
                // prefetch now -- the selection and the queue -- so a
                // not-warmed answer for one of them used to throw away the
                // knowledge that the other was warm, and with it the difference
                // between "starting" and "resolving".
                if ready {
                    self.ready = Some(id);
                } else if self.ready.as_deref() == Some(id.as_str()) {
                    self.ready = None;
                }
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
                self.browsing = None;
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
            Response::Failed(msg) => {
                // A home fetch that failed answers here rather than with an
                // empty `Home`, so this is where the pane stops saying it is
                // loading. Clearing it for any failure costs nothing: nothing
                // else can be in flight while it is set except at launch.
                self.home_pending = false;
                self.report(msg);
            }
        }
    }

    /// Starts a track, or reports why it will not start -- but only when it is
    /// still the track the user is waiting for.
    ///
    /// The worker is serial and a resolve costs seconds, so a run of Enters
    /// leaves answers arriving for tracks that have already been skipped past.
    /// Every one of them used to be played, which is what put the previous song
    /// under the current one's title and cover; here they are dropped instead.
    fn apply_resolved(&mut self, id: &str, title: String, stream: Result<StreamUrl, String>) {
        if self.pending.as_deref() != Some(id) {
            return;
        }
        self.pending = None;

        let why = match stream {
            Ok(stream) => {
                self.status = format!("playing {title}");
                // The track resolved, so whatever run of failures preceded it
                // is over -- the budget is for consecutive failures. Nothing
                // automatic is in flight any more either, so a later failure
                // belongs to whatever asked for it.
                self.skips = 0;
                self.auto = false;
                let _ = self.player.send(Command::Play {
                    url: stream.url,
                    title,
                });
                return;
            }
            Err(why) => why,
        };

        // A queue that cannot resolve its next track steps over it rather than
        // falling silent -- YouTube's radio offers plenty that this program
        // cannot play, and one of them must not end the session. Bounded, so a
        // queue of sixty is not walked in silence looking for one that works.
        if self.auto && self.skips < MAX_AUTO_SKIPS {
            self.skips += 1;
            // First line only: a failure with instructions in it runs to a
            // paragraph, and the status bar is one line -- the rest would be
            // drawn as control characters across it. The user did not ask for
            // this track anyway; the whole explanation is owed to whoever
            // presses Enter on it.
            let first = why.lines().next().unwrap_or(&why);
            self.status = format!("skipping a track that would not play ({first})");
            self.advance(1, true);
            // Nothing to step to: the queue ran out on the track that would not
            // play. The player is still showing that track as loading, and no
            // play is coming to replace it, so it is taken down here.
            if self.pending.is_none() {
                let _ = self.player.send(Command::Stop);
            }
            return;
        }

        self.auto = false;
        // The player was put into `Buffering` for this track the moment it was
        // chosen, and nothing is going to arrive to take it out of it. Left
        // alone it would sit there naming a track that is never going to play,
        // over an error the user cannot see past it.
        let _ = self.player.send(Command::Stop);
        self.report(why);
    }

    /// The player page, but only if it still belongs to `video_id`.
    ///
    /// Every panel is fetched on its own and any of them can land after the
    /// user has skipped on. This is the one gate they all pass through, so a
    /// late response is dropped rather than drawn under the wrong title.
    fn for_track(&mut self, video_id: &str) -> Option<&mut NowPlaying> {
        self.now.as_mut().filter(|now| now.video_id == video_id)
    }

    /// Takes the queue from a watch response.
    ///
    /// The queue is only *replaced* when it does not already contain what is
    /// playing. Advancing through a radio re-asks for the page of each track --
    /// that is where its lyrics and related ids come from -- and adopting each
    /// answer wholesale would reshuffle "Up next" under the user on every
    /// track. Playing something from outside the queue does replace it, which
    /// is what makes a search result start a new mix.
    fn apply_watch(&mut self, video_id: &str, watch: Result<Watch, String>) {
        let Some(now) = self.for_track(video_id) else {
            return;
        };
        let watch = match watch {
            Ok(watch) => watch,
            Err(why) => {
                // No queue is not an error worth a status line: the track is
                // playing, and the panels say so for themselves. Both of the
                // ids the other tabs need came from this response, so neither
                // is ever coming -- settled here rather than left to say
                // "loading" for the rest of the track.
                now.playing = None;
                now.lyrics = Panel::Empty(why.clone());
                now.related = Panel::Empty(why);
                return;
            }
        };

        now.lyrics_id = watch.lyrics_id;
        now.related_id = watch.related_id;
        // A tab YouTube did not offer is one that will never be fetched, so it
        // is settled here rather than left waiting. Without this a track with
        // no lyrics -- an instrumental, or most videos -- shows a panel that
        // says "loading" for as long as it plays, because the fetch that would
        // have finished the sentence is one there is no id to make.
        if now.lyrics_id.is_none() {
            now.lyrics = Panel::Empty("no lyrics are published for this track".to_string());
        }
        if now.related_id.is_none() {
            now.related = Panel::Empty("nothing related came back for this track".to_string());
        }

        match now.queue.iter().position(|track| track.id == video_id) {
            Some(index) => now.playing = Some(index),
            None => {
                now.playing = watch.queue.iter().position(|track| track.id == video_id);
                now.queue_title = watch.queue_title;
                now.queue = watch.queue;
                // Follow the track that is playing rather than leaving the
                // cursor on whatever row a previous queue had under it.
                now.cursor[Tab::UpNext.index()] = now.playing.unwrap_or(0);
            }
        }

        // The length the track was started with may be missing: a card on the
        // landing page carries none, and a pasted link carries nothing at all.
        // The queue states one for every row, and without it the player page
        // has a bare clock where its progress bar belongs -- rodio reports how
        // far it has got, never how far there is to go.
        if now.duration.is_none() {
            now.duration = now
                .playing
                .and_then(|index| now.queue.get(index))
                .and_then(|track| track.duration);
        }

        // Warm the track after this one while the current one plays. This is
        // the whole reason the queue is worth having early: an automatic
        // advance that hits the URL cache starts in milliseconds, where a cold
        // one spends seconds of silence between tracks.
        if let Some(next) = now.next_in_queue().map(|track| track.id.clone()) {
            let _ = self.source.send(Request::Prefetch { id: next });
        }
    }

    /// Plays a track and opens the player page on it.
    ///
    /// The single path every play goes through -- a result row, a card, the
    /// queue, or the track that follows the one that just ended -- so that
    /// nothing can start playing without the page around it being replaced to
    /// match.
    ///
    /// `auto` marks a play the queue started rather than the user; only those
    /// are allowed to step over a track that will not resolve.
    fn play_track(&mut self, track: Track, auto: bool) {
        // Before anything else touches the player: whatever was playing is over
        // as of this call, and this is the last moment its position is still
        // readable. Every play in the program funnels through here, so this
        // covers a skip, a queue advance and a fresh choice alike.
        self.finish_listening();
        self.listening = Some((track.clone(), Duration::ZERO));
        self.auto = auto;
        // Only from a list; the player page is not somewhere to go back to.
        if self.view != View::Playing {
            self.back_to = self.view;
        }
        self.status = if self.ready.as_deref() == Some(track.id.as_str()) {
            format!("starting {} ...", track.label())
        } else {
            format!("resolving {} ...", track.label())
        };

        // Before the resolve, not after it. Resolving costs seconds, and until
        // this lands the speakers are still on the previous track while the
        // page below has already been replaced with this one -- the player
        // saying two different things about what is playing. This stops the old
        // track and puts the new one's name up as loading, so that what is on
        // screen and what is audible never disagree.
        //
        // Every play goes through here, so it also covers the queue advancing
        // itself, where the old track has ended and there is nothing to stop.
        let _ = self.player.send(Command::Load {
            title: track.label(),
        });
        // What a resolve response has to match to be acted on, and what stops
        // the queue advancing over a track that is still loading.
        self.pending = Some(track.id.clone());

        // Carried over so the page does not blink back to the first tab on
        // every track: what the user was reading is a choice about the session,
        // not about the song.
        let tab = self.now.as_ref().map_or(Tab::UpNext, |now| now.tab);
        let mut page = NowPlaying::new(&track);
        page.tab = tab;
        // The queue survives moving *within* it -- that is what makes advancing
        // through a radio keep one stable "Up next" rather than reshuffling on
        // every track. Playing something from outside it drops it instead of
        // carrying it: a queue kept beside a track that is not in it would be
        // labelled as what is playing while listing something else, and if the
        // watch response then failed it would stay that way. The panels never
        // survive, because every one of them belongs to a video id that has
        // just changed.
        if let Some(old) = self.now.take()
            && let Some(index) = old.queue.iter().position(|held| held.id == track.id)
        {
            page.queue_title = old.queue_title;
            page.queue = old.queue;
            page.playing = Some(index);
            page.cursor[Tab::UpNext.index()] = index;
        }
        self.now = Some(page);
        self.view = View::Playing;

        // Deliberately not `busy`, which the other requests set: that flag is
        // one shared bit, and any response clears it -- including responses to
        // things a play did not ask for. A play needs to know *which* track it
        // is waiting on, which is what `pending` is, and it is the only thing
        // that can be cleared by a stop without stranding some other request's
        // flag set for the rest of the session.
        let request = Request::Resolve {
            id: track.id.clone(),
            // The label, not the bare title: this is what the status bar shows
            // for as long as the track plays.
            title: track.label(),
        };
        if self.source.send(request).is_err() {
            self.pending = None;
            let _ = self.player.send(Command::Stop);
            self.status = "source worker is not running".to_string();
            return;
        }
        // Both behind the resolve, which is the order that matters: it is the
        // one producing audio, and these are decoration around it.
        self.request_cover(track.id.clone());
        let _ = self.source.send(Request::Watch { video_id: track.id });
        // A tab the user is already on has to be fetched now; the switch that
        // would otherwise have done it is not going to happen again.
        self.request_tab();
    }

    /// Fetches the panel behind the open tab, if it has not been asked for.
    ///
    /// Called on every tab switch and on every track change, so a panel is
    /// fetched exactly when it is first looked at -- and never twice, which is
    /// what [`Panel::is_idle`] is for.
    fn request_tab(&mut self) {
        let Some(now) = self.now.as_mut() else {
            return;
        };
        let video_id = now.video_id.clone();

        let request = match now.tab {
            // Already in hand: the queue arrives with the watch response.
            Tab::UpNext => return,
            Tab::Lyrics if now.lyrics.is_idle() => match now.lyrics_id.clone() {
                Some(browse_id) => {
                    now.lyrics = Panel::Loading;
                    Request::Lyrics {
                        video_id,
                        browse_id,
                    }
                }
                // The watch response has not arrived yet, so there is nothing
                // to ask with. Left idle deliberately: `tick_page` opens it
                // again the moment the ids land.
                None => return,
            },
            Tab::Related if now.related.is_idle() => match now.related_id.clone() {
                Some(browse_id) => {
                    now.related = Panel::Loading;
                    Request::Related {
                        video_id,
                        browse_id,
                    }
                }
                None => return,
            },
            Tab::Comments if now.comments.is_idle() => {
                now.comments = Panel::Loading;
                Request::Comments { video_id }
            }
            // Already loading, already loaded, or already known to be empty.
            _ => return,
        };

        if self.source.send(request).is_err() {
            self.status = "source worker is not running".to_string();
        }
    }

    /// Picks up a tab whose fetch could not be started when it was opened.
    ///
    /// Lyrics and Related are asked for with a browse id that arrives in the
    /// watch response, which is a round trip behind the play. Opening either
    /// tab in that window would otherwise leave it idle forever.
    pub fn tick_page(&mut self) {
        let waiting = self.now.as_ref().is_some_and(|now| match now.tab {
            Tab::Lyrics => now.lyrics.is_idle() && now.lyrics_id.is_some(),
            Tab::Related => now.related.is_idle() && now.related_id.is_some(),
            _ => false,
        });
        if waiting {
            self.request_tab();
        }
    }

    /// Advances the queue when a track ends. Called once per frame.
    ///
    /// The snapshot only says what is true now, so the end of a track is an
    /// edge rather than a state: `Playing` last frame and `Idle` this one, with
    /// no error to explain it. A stream that died reports one, and a stop
    /// clears the page outright -- neither is a track that ended.
    pub fn tick_playback(&mut self) {
        let snap = self.snapshot();
        let ended = self.last_state == PlayState::Playing
            && snap.state == PlayState::Idle
            && snap.error.is_none();
        self.last_state = snap.state;

        // Sampled every frame rather than read once at the end, because at the
        // end there is nothing to read: an ended track reports position zero.
        if let Some((_, heard)) = self.listening.as_mut()
            && snap.state != PlayState::Idle
        {
            *heard = (*heard).max(snap.position);
        }
        if ended {
            self.finish_listening();
        }

        // Never while a track is on its way. `busy` used to stand in for this
        // and is not the same question: it is set by a search or a playlist
        // fetch as well, and cleared by any of their responses -- so a search
        // that returned during a resolve would let the queue advance over the
        // track the user had just chosen.
        if ended && self.pending.is_none() {
            self.advance(1, true);
        }
    }

    /// Moves `delta` tracks through the queue and plays what it lands on.
    ///
    /// Silent at either end: there is nothing before the first track, and the
    /// end of a radio queue is the end of the session rather than a failure.
    ///
    /// `auto` distinguishes the queue advancing itself from `n` and `p`. Only
    /// the former steps over a track that will not resolve -- a user who
    /// pressed a key is owed the reason it did nothing.
    fn advance(&mut self, delta: isize, auto: bool) {
        let Some(now) = self.now.as_ref() else {
            return;
        };
        let Some(current) = now.playing else {
            return;
        };
        let Ok(index) = usize::try_from(current as isize + delta) else {
            return;
        };
        let Some(track) = now.queue.get(index).cloned() else {
            if delta > 0 {
                self.status = "end of the queue".to_string();
            }
            return;
        };

        self.play_track(track, auto);
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

    /// Asks for the landing page.
    ///
    /// Not `busy`: this runs at launch, and a busy flag set before the first
    /// frame would hold off the prefetch and spin the event loop at the faster
    /// tick for as long as YouTube took to answer.
    /// Hands the finished play to the worker, which journals it and -- with a
    /// cookie saved -- reports it to YouTube.
    ///
    /// Idempotent by construction: the track is taken, so the several paths that
    /// can end a play (the queue advancing, a new choice, a stop, quitting) may
    /// all call this and only the first does anything.
    ///
    /// Nothing is reported for a track that never produced sound, which is what
    /// keeps a resolve that failed out of the history as a play that happened.
    fn finish_listening(&mut self) {
        let Some((track, heard)) = self.listening.take() else {
            return;
        };
        if heard.is_zero() {
            return;
        }
        // Not `busy`, and no response expected: the user is not waiting on this
        // and there is nothing to tell them about it either way.
        let _ = self.source.send(Request::ReportPlay {
            track,
            listened: heard,
        });
    }

    /// Writes the play in progress straight to the journal, for the way out.
    ///
    /// [`Self::finish_listening`] hands the play to the library worker, which is
    /// the right thread for it -- except at exit, where that thread is not
    /// joined and the process may be gone before it runs. Quitting mid-song is
    /// an ordinary way to end a session, so the track it happens on is worth
    /// keeping rather than losing to a race.
    pub fn flush_listening(&mut self) {
        let Some((track, heard)) = self.listening.take() else {
            return;
        };
        if !heard.is_zero() {
            journal::record_final(&track, heard);
        }
    }

    fn request_home(&mut self) {
        self.home_pending = self.source.send(Request::Home).is_ok();
        if !self.home_pending {
            self.status = "source worker is not running".to_string();
            return;
        }
        // Queued behind the feed on the same serial thread, which is the order
        // that matters: the feed is one round trip and puts a page on screen,
        // where these are four and go in front of it when they land.
        let _ = self.source.send(Request::PersonalShelves {
            rotation: self.rotation,
        });
        // Last of the three, because it is the slowest and the only one that
        // spawns a process. It answers with nothing at all when a session is
        // already in hand, which is every launch after the first.
        if !self.imported {
            self.imported = true;
            let _ = self.source.send(Request::ImportCookies { force: false });
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
        if self.awaiting() || self.prefetching.is_some() {
            return;
        }
        // Nothing playable is under the cursor while a modal has it, so there
        // is nothing worth warming.
        if self.overlay.is_open() {
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

        let Some(id) = self.selected_id() else {
            return;
        };
        if self.ready.as_deref() == Some(id.as_str()) {
            return;
        }
        if self.source.send(Request::Prefetch { id: id.clone() }).is_ok() {
            self.prefetching = Some(id);
        }
    }

    /// The video id Enter would play right now, if Enter would play anything.
    ///
    /// Both browsing views can hold one, so both are worth warming -- a card on
    /// the landing page is exactly as likely to be the next thing played as a
    /// row in the results.
    fn selected_id(&self) -> Option<String> {
        match self.view {
            View::Tracks => Some(self.results.get(self.selected)?.id.clone()),
            View::Home => match &self.home_card()?.target {
                Target::Play { video_id } => Some(video_id.clone()),
                // Opening one is a round trip of its own, and nothing about
                // which track it lands on is knowable from here.
                Target::Open { .. } => None,
            },
            // The queue row under the cursor when there is one, and otherwise
            // whatever the queue will play next -- which is the track this view
            // is most likely to need resolved, since it plays itself.
            View::Playing => {
                let now = self.now.as_ref()?;
                let selected = (now.tab == Tab::UpNext)
                    .then(|| now.queue.get(now.cursor()))
                    .flatten();
                Some(selected.or_else(|| now.next_in_queue())?.id.clone())
            }
            View::Playlists => None,
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

    /// Keeps the selected card on screen, in both directions at once.
    ///
    /// Called by the renderer, which is the only thing that knows how many
    /// shelves fit down the pane and how many cards fit across it -- both of
    /// which change with the terminal size and neither of which the key
    /// handler could have known when it moved the cursor.
    pub fn clamp_home(&mut self, shelves: usize, cards: usize) {
        // The two lists are kept in step here rather than at every point that
        // could change either: a shelf list that arrives while the cursor is
        // deep in the previous one is the case that would otherwise index out
        // of a `Vec` that had not caught up yet.
        self.home_scroll.resize(self.home.len(), 0);
        self.home_shelf = self.home_shelf.min(self.home.len().saturating_sub(1));

        if shelves == 0 || self.home.is_empty() {
            self.home_top = 0;
            return;
        }

        if self.home_shelf < self.home_top {
            self.home_top = self.home_shelf;
        } else if self.home_shelf >= self.home_top + shelves {
            self.home_top = self.home_shelf + 1 - shelves;
        }
        self.home_top = self.home_top.min(self.home.len().saturating_sub(shelves));

        let len = self.home[self.home_shelf].cards.len();
        self.home_card = self.home_card.min(len.saturating_sub(1));
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
                View::Home => self.handle_home_key(key),
                View::Tracks => self.handle_browse_key(key),
                View::Playlists => self.handle_playlists_key(key),
                View::Playing => self.handle_playing_key(key),
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

    /// The landing page, which is a grid rather than a list.
    ///
    /// `h`/`l` walk a shelf and `j`/`k` change shelves, with the arrow keys
    /// bound the same way. The arrows seek in the track list and do not here:
    /// there is no fourth direction to give them, and a page whose cursor
    /// cannot be moved with the arrow keys reads as broken far more often than
    /// a page that will not scrub the audio.
    fn handle_home_key(&mut self, key: KeyEvent) -> Result<()> {
        let (was_shelf, was_card) = (self.home_shelf, self.home_card);
        let last_shelf = self.home.len().saturating_sub(1);
        let last_card = self
            .home
            .get(self.home_shelf)
            .map_or(0, |shelf| shelf.cards.len().saturating_sub(1));

        match key.code {
            KeyCode::Char('q') => self.should_quit = true,
            KeyCode::Char('l') | KeyCode::Right => self.home_card = (self.home_card + 1).min(last_card),
            KeyCode::Char('h') | KeyCode::Left => self.home_card = self.home_card.saturating_sub(1),
            KeyCode::Char('j') | KeyCode::Down => self.home_shelf = (self.home_shelf + 1).min(last_shelf),
            KeyCode::Char('k') | KeyCode::Up => self.home_shelf = self.home_shelf.saturating_sub(1),
            KeyCode::Char('g') | KeyCode::Home => (self.home_shelf, self.home_card) = (0, 0),
            KeyCode::Char('G') | KeyCode::End => self.home_shelf = last_shelf,
            KeyCode::PageDown => self.home_shelf = (self.home_shelf + 3).min(last_shelf),
            KeyCode::PageUp => self.home_shelf = self.home_shelf.saturating_sub(3),
            KeyCode::Enter => self.open_card(),
            KeyCode::Char('r') => {
                // Steps the radio seeds on, so this is a genuine refresh rather
                // than a re-fetch of the page already on screen. The old feed
                // rotated itself once an hour and had no way to be asked; this
                // is the same idea put under the key that was already reloading.
                self.rotation += 1;
                self.status = "refreshing the home feed ...".to_string();
                self.request_home();
            }
            KeyCode::Char('/') | KeyCode::Char('i') => {
                self.view = View::Tracks;
                self.mode = Mode::Editing;
                self.status = "type to search, Enter to run".to_string();
            }
            KeyCode::Char('L') => self.open_library(),
            KeyCode::Char('A') => self.begin_sign_in(),
            KeyCode::Char('P') | KeyCode::Char('p') => self.open_player(),
            KeyCode::Char('B') => self.background(),
            // Back to whatever the track list was showing, when there is one.
            KeyCode::Esc if !self.results.is_empty() => self.view = View::Tracks,
            KeyCode::Char('c') => self.cover_size = self.cover_size.toggled(),
            KeyCode::Char(' ') => {
                let _ = self.player.send(Command::TogglePause);
            }
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
            self.selection_settled = Some(Instant::now());
        }
        Ok(())
    }

    /// Enter on a card: play it, or open what it stands for.
    fn open_card(&mut self) {
        let Some(card) = self.home_card() else {
            return;
        };
        // Taken by value before anything below borrows `self` mutably.
        let (playable, name, target) = (card.track(), card.title.clone(), card.target.clone());

        if let Some(track) = playable {
            return self.play_track(track, false);
        }

        let request = match target {
            // Handled above; a card is one or the other.
            Target::Play { .. } => return,
            Target::Open { browse_id } => {
                self.status = format!("opening {name} ...");
                Request::OpenBrowse {
                    browse_id,
                    title: name,
                }
            }
        };

        self.busy = true;
        if self.source.send(request).is_err() {
            self.busy = false;
            self.status = "source worker is not running".to_string();
        }
    }

    /// The player page.
    ///
    /// `h`/`l` walk the tabs, as they walk a shelf on the landing page, and the
    /// digits jump straight to one. `j`/`k` move inside whichever panel is
    /// open -- a row in the two list panels, a line in the two text ones. The
    /// arrow keys keep the meaning they have in the track list: left and right
    /// seek, because this is the view most likely to be open while a long track
    /// plays, and up and down move with `j`/`k`.
    fn handle_playing_key(&mut self, key: KeyEvent) -> Result<()> {
        let mut tab = self.now.as_ref().map(|now| now.tab);

        match key.code {
            KeyCode::Char('q') => self.should_quit = true,
            KeyCode::Char('l') | KeyCode::Tab => tab = tab.map(|tab| tab.shifted(1)),
            KeyCode::Char('h') | KeyCode::BackTab => tab = tab.map(|tab| tab.shifted(-1)),
            KeyCode::Char(digit @ '1'..='4') => {
                tab = Tab::ALL.get(digit as usize - '1' as usize).copied();
            }
            KeyCode::Char('j') | KeyCode::Down => self.scroll_page(1),
            KeyCode::Char('k') | KeyCode::Up => self.scroll_page(-1),
            KeyCode::PageDown => self.scroll_page(10),
            KeyCode::PageUp => self.scroll_page(-10),
            KeyCode::Char('g') | KeyCode::Home => {
                if let Some(now) = self.now.as_mut() {
                    *now.cursor_mut() = 0;
                }
            }
            // No length is known here for the text panels, so this asks for a
            // line no panel has and lets the renderer -- which knows how long
            // each of them is -- clamp it to the last one.
            KeyCode::Char('G') | KeyCode::End => {
                if let Some(now) = self.now.as_mut() {
                    *now.cursor_mut() = usize::MAX;
                }
            }
            KeyCode::Enter => self.open_page_row(),
            KeyCode::Char('n') => self.advance(1, false),
            KeyCode::Char('p') => self.advance(-1, false),
            KeyCode::Char(' ') => {
                let _ = self.player.send(Command::TogglePause);
            }
            KeyCode::Char('s') => self.stop(),
            KeyCode::Char('c') => self.cover_size = self.cover_size.toggled(),
            KeyCode::Char('+') | KeyCode::Char('=') => self.nudge_volume(VOLUME_STEP),
            KeyCode::Char('-') | KeyCode::Char('_') => self.nudge_volume(-VOLUME_STEP),
            KeyCode::Right => self.seek_relative(5),
            KeyCode::Left => self.seek_relative(-5),
            KeyCode::Char('a') => self.begin_add(),
            KeyCode::Char('f') => self.toggle_like(),
            KeyCode::Char('L') => self.open_library(),
            KeyCode::Char('A') => self.begin_sign_in(),
            KeyCode::Char('B') => self.background(),
            KeyCode::Char('/') | KeyCode::Char('i') => {
                self.view = View::Tracks;
                self.mode = Mode::Editing;
                self.status = "type to search, Enter to run".to_string();
            }
            // Back to the list the track was started from. The music keeps
            // playing -- leaving the page is not stopping it, and `P` brings it
            // back.
            KeyCode::Esc | KeyCode::Char('H') => self.view = self.back_to,
            _ => {}
        }

        // Applied in one place rather than in each arm above, so that every way
        // of changing tabs also starts the fetch behind the new one.
        if let (Some(now), Some(tab)) = (self.now.as_mut(), tab)
            && now.tab != tab
        {
            now.tab = tab;
            self.request_tab();
        }
        Ok(())
    }

    /// Moves the cursor of the open tab. Bounded only below; the renderer is
    /// what knows how long each panel is and clamps the other end.
    fn scroll_page(&mut self, delta: isize) {
        if let Some(now) = self.now.as_mut() {
            let cursor = now.cursor_mut();
            *cursor = cursor.saturating_add_signed(delta);
        }
    }

    /// Enter on the player page: plays the queue row or the recommendation
    /// under the cursor. The two text panels have nothing to open.
    fn open_page_row(&mut self) {
        let Some(now) = self.now.as_ref() else {
            return;
        };
        match now.tab {
            Tab::UpNext => {
                if let Some(track) = now.queue.get(now.cursor()).cloned() {
                    self.play_track(track, false);
                }
            }
            Tab::Related => {
                let rows = now.related_rows();
                // Anything else under the cursor is a shelf heading, which
                // names the rows below it rather than standing for one.
                let Some(RelatedRow::Card(card)) = rows.get(now.cursor()) else {
                    return;
                };
                // Taken by value before anything below borrows `self` mutably.
                let (playable, title, target) =
                    (card.track(), card.title.clone(), card.target.clone());

                match (playable, target) {
                    // A song: play it, which reseeds the queue around it.
                    (Some(track), _) => self.play_track(track, false),
                    // An album or a playlist: open it as a track list.
                    (None, Target::Open { browse_id }) => {
                        self.status = format!("opening {title} ...");
                        self.busy = true;
                        let request = Request::OpenBrowse { browse_id, title };
                        if self.source.send(request).is_err() {
                            self.busy = false;
                            self.status = "source worker is not running".to_string();
                        }
                    }
                    // A playable card always yields a track, so this cannot
                    // happen -- and guessing at it would be worse than nothing.
                    (None, Target::Play { .. }) => {}
                }
            }
            Tab::Lyrics | Tab::Comments => {}
        }
    }

    /// Stops playback and closes the player page.
    ///
    /// Shared by every view that offers `s`, and the reason the queue does not
    /// pick up afterwards: dropping the page is what tells [`Self::advance`]
    /// there is nothing to advance through. A stop that left the queue running
    /// would be a pause with extra steps.
    fn stop(&mut self) {
        // Stopping halfway through is still having listened that far, and this
        // is the last point at which how far is known.
        self.finish_listening();
        let _ = self.player.send(Command::Stop);
        self.status = "stopped".to_string();
        self.now = None;
        self.auto = false;
        // A resolve may still be in flight; without this it would start playing
        // moments after the user asked for silence.
        self.pending = None;
        // Nothing is playing, so nothing owns the cover pane.
        self.cover = None;
        self.cover_id = None;
        if self.view == View::Playing {
            self.view = self.back_to;
        }
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
            KeyCode::Char('P') | KeyCode::Char('p') => self.open_player(),
            KeyCode::Char('B') => self.background(),
            KeyCode::Char('x') => self.sign_out(),
            // Back to whatever the track list was showing before, or to the
            // landing page when it was showing nothing.
            KeyCode::Esc | KeyCode::Char('L') | KeyCode::Char('l') => {
                self.view = if self.results.is_empty() {
                    View::Home
                } else {
                    View::Tracks
                };
            }
            KeyCode::Char('H') => self.view = View::Home,
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
                // An empty result list is nothing to leave the user looking at
                // when the landing page is a keypress away and already loaded.
                if self.results.is_empty() && !self.home.is_empty() {
                    self.view = View::Home;
                }
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
            // Leaves an open playlist for the library it came from; anything
            // else falls back to the landing page, which is where every list
            // in the program can be reached from.
            KeyCode::Esc if self.open_playlist.is_some() => self.open_library(),
            KeyCode::Esc | KeyCode::Char('H') => self.view = View::Home,
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
        // as an address bar. Everything the page needs beyond the id -- the
        // real title, the artist, the queue -- arrives with the watch response.
        if let Some(id) = extract_video_id(&query) {
            return self.play_track(Track {
                id,
                title: query,
                uploader: String::new(),
                duration: None,
                album: None,
                playlist_item_id: None,
            }, false);
        }

        self.status = format!("searching for {query} ...");
        self.busy = true;
        let request = Request::Search {
            query,
            limit: SEARCH_LIMIT.min(MAX_RESULTS),
        };
        if self.source.send(request).is_err() {
            self.busy = false;
            self.status = "source worker is not running".to_string();
        }
    }

    fn play_selected(&mut self) {
        // Resolving takes seconds and spawns yt-dlp, so it goes to the worker
        // and playback starts when the response arrives -- unless a prefetch
        // already warmed the cache, in which case the round trip is all that is
        // left and saying "resolving" would be a lie the user can see through.
        if let Some(track) = self.results.get(self.selected).cloned() {
            self.play_track(track, false);
        }
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

    /// Returns to the player page, which the music kept playing behind.
    ///
    /// Only a track that is still playing has a page to return to; with nothing
    /// playing this says so rather than opening an empty one.
    fn open_player(&mut self) {
        if self.now.is_some() {
            self.back_to = self.view;
            self.view = View::Playing;
        } else {
            self.status = "nothing is playing".to_string();
        }
    }

    /// Asks the event loop to let go of the terminal and carry on playing.
    ///
    /// Refused with nothing playing, which is not a limitation but the whole
    /// point: backgrounding a silent player leaves an icon in the notification
    /// area representing nothing, and the user's next move -- finding something
    /// to play -- is one the icon cannot help with.
    fn background(&mut self) {
        if self.now.is_some() {
            self.wants_background = true;
        } else {
            self.status = "nothing is playing to run in the background".to_string();
        }
    }

    /// A click on the notification-area icon.
    ///
    /// The same handful of actions the keys offer, because a tray menu is not
    /// somewhere to put a second, lesser version of the program: what belongs
    /// here is what someone with no window in front of them can still want.
    pub fn handle_tray(&mut self, command: TrayCommand) {
        match command {
            TrayCommand::Show => self.wants_foreground = true,
            TrayCommand::TogglePause => {
                let _ = self.player.send(Command::TogglePause);
            }
            TrayCommand::Next => self.advance(1, false),
            TrayCommand::Previous => self.advance(-1, false),
            TrayCommand::Quit => self.should_quit = true,
        }
    }

    /// What hovering the icon says.
    ///
    /// The tooltip is the only thing MTUI shows while it is detached, so it
    /// carries what the status bar would have: the track, who it is by, and
    /// whether it is actually playing.
    pub fn tray_tip(&self) -> String {
        let Some(now) = self.now.as_ref() else {
            return "MTUI -- nothing playing".to_string();
        };
        let state = match self.snapshot().state {
            PlayState::Paused => "paused -- ",
            PlayState::Buffering => "loading -- ",
            _ => "",
        };
        format!("{state}{} -- {}", now.title, now.byline())
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
        let Some((video_id, title)) = self.acting_on() else {
            return;
        };
        self.overlay = Overlay::AddTo {
            video_id,
            title,
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
        let Some((video_id, title)) = self.acting_on() else {
            return;
        };
        self.status = format!("rating {title} ...");
        self.busy = true;
        let request = Request::ToggleLike { video_id, title };
        if self.source.send(request).is_err() {
            self.busy = false;
            self.status = "source worker is not running".to_string();
        }
    }

    /// The track `a` and `f` apply to: the selected row in the track list, and
    /// on the player page the track that is playing.
    ///
    /// The page keeps a cursor of its own, but liking the row it happens to
    /// rest on is not what `f` means there -- the whole view is about one song,
    /// and that song is the one on screen.
    fn acting_on(&self) -> Option<(String, String)> {
        match self.view {
            View::Playing => {
                let now = self.now.as_ref()?;
                let artist = &now.artist;
                let label = if artist.is_empty() || artist == crate::source::UNKNOWN_ARTIST {
                    now.title.clone()
                } else {
                    format!("{} — {artist}", now.title)
                };
                Some((now.video_id.clone(), label))
            }
            _ => {
                let track = self.results.get(self.selected)?;
                Some((track.id.clone(), track.label()))
            }
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
