//! Application state and input handling.
//!
//! Holds no rendering logic (see [`crate::ui`]) and performs no blocking work:
//! searches and URL resolution are delegated to [`SourceWorker`], playback to
//! [`Player`]. Key handling only mutates state and dispatches messages, so the
//! event loop never stalls.
//!
//! Feature modules own the behavior; this module declares shared state and
//! constructs its services. See `docs/architecture.md` for module ownership.

use std::time::Instant;

use crate::config;
use crate::art::ArtCache;
use crate::config::{CoverStyle, ImageRenderer};
use crate::discord::Presence;
use crate::graphics::Graphics;
use crate::player::{OutputDevice, Player};
use crate::source::{BrowseEndpoint, Track};
use crate::source::cover::Cover;
use crate::source::home::Shelf;
use crate::source::worker::{PageRequestId, SourceWorker};

use menu::MenuAction;
use navigation::{HistoryEntry, PageRequestKind};
use playback::PlaybackState;
use prefetch::PrefetchState;
use preferences::DEFAULT_UNMUTED_VOLUME;

mod account;
pub mod actions;
mod artist;
mod artwork;
mod desktop;
pub(crate) mod home;
mod input;
pub(crate) mod library;
mod menu;
mod navigation;
mod now_playing;
mod playback;
mod prefetch;
pub mod preferences;
mod responses;
mod search;
pub(crate) mod search_input;
mod selection;

pub(crate) use selection::SongClick;

#[cfg(test)]
pub(crate) use menu::{account_menu_items, app_menu_items, keyboard_help_items};

pub use artist::ArtistView;
pub use artwork::{CardShape, CoverSize, ImageBound, ImagePlan, ImageSource, PlannedImage};
pub use input::{MouseAction, POINTER_SCALE};
pub use menu::{Menu, MenuItem, MenuPage};
pub use navigation::{Mode, View};
pub use now_playing::{NowPlaying, Panel, RelatedRow, RepeatMode, Tab};

/// A non-menu modal that owns keyboard input for as long as it is up.
///
/// Deliberately a single value rather than a stack: none can meaningfully sit on
/// top of another, and a stack would only add states that cannot be reached.
pub enum Overlay {
    None,
    /// The sign-in panel. Dismissing this hides it but does not cancel the
    /// sign-in, which is still running on its own thread and will report when
    /// it finishes.
    SignIn(SignIn),
    /// A message too long for the status bar.
    Message {
        body: String,
    },
    /// Small persisted preferences changed without leaving the current page.
    Settings,
    Share { url: String, notice: Option<String> },
    SavePlaylist(Box<actions::PlaylistPicker>),
}

impl Overlay {
    pub fn is_open(&self) -> bool {
        !matches!(self, Self::None)
    }
}

/// State of the YouTube Music session setup.
pub enum SignIn {
    /// Terminal failure. Held in the panel rather than dropped into the status
    /// bar: the user is looking here, and the next thing they need is the retry
    /// key -- which this is the only place that offers.
    Failed { reason: String },
    /// The shared YouTube Music profile is waiting for a valid session. During
    /// automatic recovery it begins hidden and appears only if Google needs the
    /// user to authenticate again.
    Music { started: Instant, recovering: bool },
}

pub struct App {
    pub mode: Mode,
    pub query: String,
    pub query_cursor: usize,
    pub results: Vec<Track>,
    pub search_items: Vec<crate::source::search::Item>,
    pub search_filter: crate::source::search::Filter,
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
    /// Whether the notification-area icon stays available while the UI is open.
    pub start_in_tray: bool,
    /// Whether a left-click toggles pause instead of opening the terminal.
    pub tray_click_pauses: bool,
    pub precision_volume: bool,
    pub show_key_hints: bool,
    /// Artwork selected for runtime-owned windows and the notification area.
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
    pub(crate) splits: crate::ui::Splits,
    /// Visual language used for the current song's large cover.
    pub cover_style: CoverStyle,
    /// User-selected terminal bitmap backend.
    pub image_renderer: ImageRenderer,
    pub terminal_host: config::TerminalHost,
    /// Outputs currently advertised by the operating system.
    output_devices: Vec<OutputDevice>,
    /// Stable id of the selected output, or `None` to follow the system default.
    output_device: Option<String>,
    /// Mute is session state rather than a destructive volume change. The
    /// remembered level makes a second `m` restore exactly what was audible.
    muted: bool,
    volume_before_mute: f32,
    volume_save: Option<(Instant, f32)>,
    /// Where the renderer wants the cover painted as real pixels, set on every
    /// frame the terminal-image path runs. `None` on the half-block path, which
    /// needs no help from the event loop.
    pub images: Vec<PlannedImage>,
    /// The plan currently on screen. Pixels are not part of ratatui's buffer,
    /// so they persist until something paints over them -- meaning an unchanged
    /// pane must *not* be repainted every frame.
    painted: Vec<PlannedImage>,
    painted_with_kitty: bool,

    /// Which list the main pane is showing.
    pub view: View,
    pub library: library::State,
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
    pub home_grid_rows: usize,
    /// First visible shelf, maintained like [`Self::offset`] so the renderer
    /// slices rather than laying out shelves it will not draw.
    pub home_top: usize,
    /// First visible card of each shelf, one per shelf. Kept per shelf rather
    /// than for the focused one alone so that scrolling a shelf, moving away
    /// and coming back returns to where the user left it.
    pub home_scroll: Vec<usize>,
    /// Sleeves for the cards on screen. See [`crate::art`] for what is kept and
    /// for how long.
    pub art: ArtCache,
    /// True between asking for the landing page and hearing back.
    ///
    /// Its own flag rather than [`Self::busy`]: this is set before the first
    /// frame is drawn, and `busy` would hold off the prefetch and spin the
    /// event loop at the faster tick for the whole of a request nobody is
    /// waiting on. What it is actually for is the difference between "loading"
    /// and "there is no feed", which are the same empty pane.
    pub home_pending: bool,
    /// Number of Home requests still in flight. Home deliberately uses one
    /// worker at a time so no second feed is retained just to race the first.
    home_attempts: u8,
    /// Identifies the latest refresh so late responses cannot mutate it.
    home_generation: u64,
    /// Title of whatever was opened from the landing page, when the track list
    /// is showing one.
    pub browsing: Option<String>,
    browsing_endpoint: Option<BrowseEndpoint>,
    pub collection: Option<crate::source::collection::Details>,
    pub collection_error: Option<String>,
    /// Dedicated mixed-content artist page. Kept while another view is open so
    /// Player and search can return without refetching it.
    pub artist: Option<ArtistView>,
    history: Vec<HistoryEntry>,
    search_page: Option<HistoryEntry>,
    page_request: PageRequestId,
    pending_page_request: Option<(PageRequestId, PageRequestKind)>,
    pub overlay: Overlay,
    /// The shared command menu. It stays separate from asynchronous overlays
    /// such as sign-in so neither can accidentally replace the other; input
    /// maintains the one-modal-at-a-time invariant.
    menu: Option<Menu>,
    /// Settings selection, choice picker, and return navigation.
    preferences: preferences::State,
    updater: crate::update::State,
    /// What is playing and the page around it. `None` before the first play and
    /// after a stop -- there is no player page for silence.
    pub now: Option<NowPlaying>,
    /// Transport transitions and listening progress for the current play.
    playback: PlaybackState,
    /// Debounced selection probes and their cache knowledge.
    prefetch: PrefetchState,
    /// The view to return to from the player page. Remembered rather than
    /// assumed, because a track can be started from any of the three lists and
    /// Esc should go back to the one it came from.
    back_to: View,
    /// Exact page hidden by Player. The bare view above remains useful for
    /// simple fallbacks, while this retains nested Artist and Tracks state.
    player_back: Option<HistoryEntry>,
    /// The page hidden when search editing began. Unlike `back_to`, this also
    /// covers page searches that are cancelled before submission.
    search_origin: View,
    /// The last queue epoch handed out. Incremented for every queue that
    /// arrives, so no two queues in one session share one and a page of tracks
    /// can only ever be applied to the queue that asked for it.
    queue_epoch: u64,
    /// Steps the seed the journal builds a station from, so a station that led
    /// nowhere is not the one built again. Kept here rather than on the queue
    /// because it outlives any one of them: two queues in a row that both run
    /// dry should be rescued by two different stations.
    seed_rotation: usize,
    /// Prevents repeated `M` presses from opening duplicate session imports.
    music_signing_in: bool,
    automatic_sign_in: bool,
    sign_in_prompted: bool,
    session_renewal: crate::session::renewal::Renewal,
    account_request: u64,
    pending_save: Option<(u64, String)>,

    /// The card on the user's Discord profile. Holds the last one published, so
    /// that recomputing it every tick costs a comparison rather than a socket
    /// write -- see [`crate::discord`].
    presence: Presence,

    player: Player,
    source: SourceWorker,
}

impl Drop for App {
    fn drop(&mut self) { self.flush_volume(); }
}

impl App {

    pub fn new(
        player: Player,
        source: SourceWorker,
        graphics: Graphics,
        settings: config::Settings,
    ) -> Self {
        let output_devices = match crate::player::available_output_devices() {
            Ok(devices) => devices,
            Err(error) => {
                crate::diagnostics::warn(
                    "player",
                    &format!("could not list audio outputs: {error:#}"),
                );
                Vec::new()
            }
        };
        let presence = Presence::spawn(config::Discord::application_id(), config::Presence::load());
        Self::with_services(player, source, graphics, settings, output_devices, presence)
    }


    fn with_services(
        player: Player,
        source: SourceWorker,
        graphics: Graphics,
        settings: config::Settings,
        output_devices: Vec<OutputDevice>,
        presence: Presence,
    ) -> Self {
        let output_device = settings.output_device.clone();
        let muted = settings.volume == 0.0;
        let volume_before_mute = if muted {
            DEFAULT_UNMUTED_VOLUME
        } else {
            settings.volume
        };
        let mut app = Self {
            // Browse, not Editing: the program now opens on a page there is
            // something to do with, and the keys that move around it are bare
            // letters the search box would otherwise swallow.
            mode: Mode::Browse,
            query: String::new(),
            query_cursor: 0,
            results: Vec::new(),
            search_items: Vec::new(),
            search_filter: crate::source::search::Filter::All,
            selected: 0,
            offset: 0,
            status: String::new(),
            busy: false,
            should_quit: false,
            updater: crate::update::State::default(),
            wants_background: false,
            wants_foreground: false,
            start_in_tray: settings.start_in_tray,
            tray_click_pauses: settings.tray_click_pauses,
            precision_volume: settings.precision_volume,
            show_key_hints: settings.show_key_hints,
            cover: None,
            cover_id: None,
            graphics,
            cover_size: CoverSize::default(),
            splits: crate::ui::Splits::default(),
            cover_style: settings.cover_style,
            image_renderer: settings.image_renderer,
            terminal_host: settings.terminal_host,
            output_devices,
            output_device,
            muted,
            volume_before_mute,
            volume_save: None,
            images: Vec::new(),
            painted: Vec::new(),
            painted_with_kitty: false,
            view: View::Home,
            library: library::State::default(),
            home: Vec::new(),
            home_shelf: 0,
            home_card: 0,
            home_grid_rows: 1,
            home_top: 0,
            home_scroll: Vec::new(),
            art: ArtCache::default(),
            home_pending: false,
            home_attempts: 0,
            home_generation: 0,
            browsing: None,
            browsing_endpoint: None,
            collection: None,
            collection_error: None,
            artist: None,
            history: Vec::new(),
            search_page: None,
            page_request: 0,
            pending_page_request: None,
            overlay: Overlay::None,
            menu: None,
            preferences: preferences::State::default(),
            now: None,
            playback: PlaybackState::default(),
            prefetch: PrefetchState::default(),
            back_to: View::Home,
            player_back: None,
            search_origin: View::Home,
            queue_epoch: 0,
            seed_rotation: 0,
            music_signing_in: false,
            automatic_sign_in: false,
            sign_in_prompted: false,
            session_renewal: crate::session::renewal::Renewal::default(),
            account_request: 0,
            pending_save: None,
            // Started whether or not Discord is running and whether or not the
            // switch is on: what this costs while idle is one sleeping thread,
            // and deciding later would mean deciding on the render path.
            presence,
            player,
            source,
        };

        // Fired before the first frame. It is one HTTPS round trip on a thread
        // that has nothing else to do at launch, and the pane it fills is the
        // whole of what the user is looking at.
        app.request_home();
        app
    }


    /// Shows a failure wherever it will actually be read: the status bar for a
    /// one-liner, an overlay for anything with instructions in it.
    fn report(&mut self, msg: String) {
        crate::diagnostics::error("source", &msg);
        if msg.contains('\n') {
            self.status.clear();
            self.menu = None;
            self.overlay = Overlay::Message { body: msg };
        } else {
            self.status = msg;
        }
    }
}

/// Deterministic App scenarios use inert services; live checks stay ignored.
#[cfg(test)]
#[path = "app/scenarios.rs"]
mod scenarios;

#[cfg(test)]
#[path = "app/sidebar_tests.rs"]
mod sidebar_tests;

#[cfg(test)]
#[path = "app/performance.rs"]
mod performance;

#[cfg(test)]
mod tests;
