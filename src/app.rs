//! Application state and input handling.
//!
//! Holds no rendering logic (see [`crate::ui`]) and performs no blocking work:
//! searches and URL resolution are delegated to [`SourceWorker`], playback to
//! [`Player`]. Key handling only mutates state and dispatches messages, so the
//! event loop never stalls.

use std::collections::VecDeque;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::art::ArtCache;
use crate::config::{self, CoverStyle, IconTheme, ImageRenderer};
use crate::discord::{Activity, Clock, Presence};
use crate::graphics::Graphics;
use crate::player::{Command, OutputDevice, PlayState, Player, PlayerEvent, Snapshot};
use crate::source::artist::{ArtistPage, ArtistSong};
use crate::source::cover::Cover;
use crate::source::home::{Card, Shelf, Target};
use crate::source::journal;
use crate::source::listening::Listening;
use crate::source::lrclib;
use crate::source::watch::{Comments, Lyrics, QueuePage, Watch};
use crate::source::worker::{PageRequestId, Request, Response, SourceWorker};
use crate::source::youtube::{MAX_RESULTS, extract_video_id};
use crate::source::{ArtistRef, BrowseEndpoint, StreamUrl, Track, UNKNOWN_ARTIST};
use crate::tray::TrayCommand;

pub mod preferences;
pub mod actions;

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

/// How a landing-page card is drawn.
///
/// Four shapes rather than one because a terminal is not one size. A cell is
/// twice as tall as it is wide, so a square picture `n` columns across costs
/// `n / 2` rows -- which means a card with its sleeve above the title, the way
/// every music app draws one, is a dozen rows tall before a word is written.
/// That is the best-looking card and it does not fit on a short window, so it
/// is what a tall window gets and not what every window is forced into.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum CardShape {
    /// Title and subtitle only, no picture. The narrowest and shortest card,
    /// and the one every terminal can draw.
    Text,
    /// A small square sleeve to the left of the text. Costs one row over
    /// [`Self::Text`] and is what most windows end up showing.
    Tile,
    /// The sleeve across the top of the card with the text beneath it, as a
    /// music app draws it. Wants a tall window -- the renderer only chooses it
    /// on its own when two shelves of them fit.
    Poster,
    /// [`Self::Poster`] with room to breathe: half again the sleeve, and few
    /// enough across a row that the page reads as a shelf of records rather
    /// than a grid of thumbnails.
    ///
    /// Chosen for the lead shelf whenever one complete gallery card fits.
    Gallery,
}

impl CardShape {
    /// Largest first, which is the order the renderer tries them in.
    pub const ALL: [Self; 4] = [Self::Gallery, Self::Poster, Self::Tile, Self::Text];
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
    /// The cell dimension Kitty should hold fixed. Supplying only one axis
    /// makes the terminal derive the other from the source aspect ratio rather
    /// than stretching a square into an inaccurately reported cell box.
    pub bound: ImageBound,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageBound {
    Columns(u16),
    Rows(u16),
}

/// Which cached bitmap a terminal-image placement paints.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImageSource {
    Playing,
    Card(String),
}

/// One bitmap placement in the current frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedImage {
    pub source: ImageSource,
    pub plan: ImagePlan,
}

/// How long the selection must sit still before it is speculatively resolved.
///
/// Long enough that scrolling through the list does not spawn a process per
/// row, short enough that it is usually finished before a decisive user presses
/// Enter.
const PREFETCH_IDLE: Duration = Duration::from_millis(400);

const VOLUME_STEP: f32 = 0.05;
const DEFAULT_UNMUTED_VOLUME: f32 = 1.0;

/// Exact pages retained for Back. A cap keeps nested artist browsing bounded
/// even when someone walks through a long chain of related artists.
const PAGE_HISTORY: usize = 12;

/// Tracks the queue may skip past in a row before it gives up.
///
/// A radio queue is built by YouTube, not by the user, and some of what it
/// offers cannot be resolved from here at all -- so one dead track must not end
/// the session. A run of them, though, is a signal that something is wrong with
/// resolving rather than with the tracks, and silently walking the queue looking
/// for one that works is not a thing to do on the user's behalf.
///
/// This matters more now than when the queue was one page. A queue that pages
/// itself indefinitely has no natural end to stop a broken resolver at, so
/// without this bound a program that could no longer play anything would walk
/// forward through stations for as long as it was left running.
const MAX_AUTO_SKIPS: u8 = 3;

/// Tracks kept behind the one playing when the queue is trimmed.
///
/// A radio queue has no end, so it cannot simply be accumulated: a session left
/// running would grow the heap by a page every hour and a half, which is the
/// one thing this program is built not to do. What is kept instead is a window
/// around the playing track -- deep enough behind that `p` still walks back
/// through an evening's listening, and shallow enough that the queue costs a
/// few kilobytes however long the session runs.
const QUEUE_BEHIND: usize = 20;

/// Tracks kept ahead of the one playing. Large enough for the five-track
/// low-water mark plus a complete 60-row continuation page: advancing the
/// continuation token after retaining only part of that page loses its tail.
const QUEUE_AHEAD: usize = 65;

/// Tracks remaining ahead at which the next page is asked for.
///
/// Not zero, which is the whole point. At zero the user hears silence for a
/// round trip *plus* a cold resolve -- seconds of it -- where five tracks of
/// warning means the page lands, and the track after it is prefetched, long
/// before either is needed. The same argument that puts the prefetch behind the
/// watch response rather than behind the track ending.
const QUEUE_LOW: usize = 5;

/// Ids remembered after they leave the window, so a radio that comes back round
/// to a track does not replay it.
///
/// A radio legitimately repeats over hours; an endless queue that loops the
/// same eight songs is worse than one that ends honestly. Bounded because this
/// outlives everything else here: at eleven characters an id, this is a couple
/// of kilobytes.
const QUEUE_MEMORY: usize = 200;

/// Failed top-ups in a row before the queue stops asking.
///
/// One failure is a passing network blip and must not end an endless queue for
/// the session; a run of them is YouTube declining to page this queue, and
/// asking again on every track would be a request per song for nothing.
const MAX_TOPUP_FAILURES: u8 = 3;

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
    /// A Music artist's top songs and catalogue shelves.
    Artist,
    /// The player page: the cover of what is playing, and beside it the queue,
    /// the lyrics, what to listen to next and the comments. Opened by playing
    /// something, which is the moment all four become answerable.
    Playing,
}

/// A semantic action produced by hit-testing the last rendered terminal frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseAction {
    GoHome,
    OpenPlayer,
    OpenAppMenu,
    EditSearch,
    OpenHomeCard { shelf: usize, card: usize },
    SelectHomeCard { shelf: usize, card: usize },
    PlayTrack(usize),
    SelectTrack(usize),
    PlayCollection,
    ShuffleCollection,
    RetryCollection,
    SearchFilter(crate::source::search::Filter),
    OpenPlayingArtist,
    OpenQueueArtist(usize),
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

/// Which panel of the player page is showing.
///
/// One at a time rather than side by side, for the same reason the main pane
/// switches between lists: on a terminal, width is the scarce resource, and
/// four columns of it would leave none of them readable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    UpNext,
    Lyrics,
    Related,
    Comments,
}

/// What automatic advancement does when the current track ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RepeatMode {
    #[default]
    Off,
    All,
    One,
}

impl RepeatMode {
    pub fn label(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::All => "all",
            Self::One => "one",
        }
    }

    fn next(self) -> Self {
        match self {
            Self::Off => Self::All,
            Self::All => Self::One,
            Self::One => Self::Off,
        }
    }
}

impl Tab {
    /// In the order they are drawn, which is the order the keys `1`-`4` and the
    /// Tab key walk them in.
    pub const ALL: [Tab; 4] = [Tab::UpNext, Tab::Lyrics, Tab::Related, Tab::Comments];

    pub fn label(self) -> &'static str {
        match self {
            Self::UpNext => "Queue",
            Self::Lyrics => "Lyrics",
            Self::Related => "Related",
            Self::Comments => "Comments",
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
#[derive(Debug, Clone)]
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
}

impl ArtistView {
    fn loading(artist: ArtistRef) -> Self {
        Self {
            requested: artist,
            page: Panel::Loading,
            section: 0,
            song: 0,
            song_offset: 0,
            card: 0,
            top: 0,
            scroll: Vec::new(),
        }
    }

    pub fn content(&self) -> Option<&ArtistPage> {
        match &self.page {
            Panel::Ready(page) => Some(page),
            _ => None,
        }
    }

    fn set_page(&mut self, page: Result<ArtistPage, String>) {
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
    }

    fn has_songs(&self) -> bool {
        self.content()
            .is_some_and(|page| !page.top_songs.is_empty())
    }

    fn shelf_index(&self) -> Option<usize> {
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

    fn selected_track(&self) -> Option<&Track> {
        self.selected_song().map(|song| &song.track)
    }
}

#[derive(Debug, Clone)]
struct TrackPage {
    results: Vec<Track>,
    query: String,
    search_items: Vec<crate::source::search::Item>,
    search_filter: crate::source::search::Filter,
    selected: usize,
    offset: usize,
    browsing: Option<String>,
    browsing_endpoint: Option<BrowseEndpoint>,
    collection: Option<crate::source::collection::Details>,
    collection_error: Option<String>,
    status: String,
}

#[derive(Debug, Clone)]
enum HistoryEntry {
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
    fn view(&self) -> View {
        match self {
            Self::Home { .. } => View::Home,
            Self::Tracks(_) => View::Tracks,
            Self::Artist { .. } => View::Artist,
            Self::Playing { .. } => View::Playing,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PageRequestKind {
    Search,
    Browse,
    Artist,
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
    pub artist_ref: Option<ArtistRef>,
    pub album: Option<String>,
    pub album_route: Option<(String, BrowseEndpoint)>,
    /// The track's length, which the player itself does not report -- rodio
    /// knows only how far it has got. Without this there is no progress bar,
    /// only a clock.
    pub duration: Option<Duration>,
    /// What YouTube calls the queue: "Let It Happen Mix", or the playlist the
    /// track was started from. Empty until the queue arrives, and rewritten if
    /// the queue is later carried on into a station built from the journal --
    /// at which point the old name would be describing different music.
    pub queue_title: String,
    /// A window onto the queue rather than the whole of it: [`QUEUE_BEHIND`]
    /// tracks back from the playing one and up to [`QUEUE_AHEAD`] forward. The
    /// queue itself has no end -- see [`NowPlaying::continuation`] -- so this is
    /// what keeps a session left running all day from growing the heap.
    pub queue: Vec<Track>,
    /// Identifies the queue itself, as opposed to the track playing inside it.
    ///
    /// A queue outlives the page around it: advancing through a radio replaces
    /// this whole struct on every track while carrying the same queue across,
    /// so `video_id` cannot say whether a page of tracks still belongs here.
    /// This changes only when the queue is genuinely replaced, which is exactly
    /// when a top-up in flight has become stale. See [`Request::MoreQueue`].
    queue_epoch: u64,
    /// What to redeem for the next page of the queue.
    ///
    /// `None` in three different cases the queue does not need to tell apart:
    /// no queue yet, a finite queue that has run out, and a queue that has
    /// given up paging itself. All three mean the same thing here -- there is
    /// nothing more to ask for -- and the queue simply ends as it always did.
    continuation: Option<String>,
    /// Set while a page is in flight, so the low-water check fires once rather
    /// than on every track for as long as the round trip takes.
    topping_up: bool,
    /// Consecutive failed top-ups. See [`MAX_TOPUP_FAILURES`].
    topup_failures: u8,
    /// Ids that have been in this queue and have since left the window.
    ///
    /// Oldest first, bounded at [`QUEUE_MEMORY`]. What stops a radio that comes
    /// back round to a track from queueing it again behind itself.
    dropped: VecDeque<String>,
    /// Where in `queue` the playing track is. `None` until the queue lands, or
    /// when what is playing is not in it.
    pub playing: Option<usize>,
    pub liked: Option<bool>,
    pub like_pending: bool,
    rating_request: u64,
    pub repeat: RepeatMode,
    pub tab: Tab,
    /// One cursor per tab, so switching away and back returns to where the user
    /// left off. For the two list tabs this is the selected row; for lyrics and
    /// comments, which have nothing to select, it is the first visible line.
    pub cursor: [usize; Tab::ALL.len()],
    /// Whether the lyrics panel is scrolling itself to keep the line being sung
    /// on screen. On until the user scrolls, because a panel that yanks itself
    /// back every 200 ms is one they cannot read ahead in; back on when they
    /// open the tab again, which is the one gesture that means "show me where
    /// we are" and needs no key of its own.
    ///
    /// Only ever true of a track whose lyrics came back timed -- with no
    /// timings there is nothing to follow, and the panel scrolls as it always
    /// did.
    pub follow_lyrics: bool,
    pub lyrics: Panel<Lyrics>,
    pub comments: Panel<Comments>,
    pub related: Panel<Vec<Shelf>>,
    /// Browse ids from the watch response, held until the tab they belong to is
    /// opened. `None` means the track has no such tab -- most videos have no
    /// lyrics, and that is not a failure.
    lyrics_id: Option<String>,
    related_id: Option<String>,
    /// Whether the watch response has come back, successfully or not.
    ///
    /// Distinct from `lyrics_id.is_some()`, which it used to be read as: a
    /// track with no lyrics page is still worth asking LRCLIB about, and only
    /// this says the difference between "there is no page" and "we have not
    /// heard yet".
    watched: bool,
}

impl NowPlaying {
    pub fn new(track: &Track) -> Self {
        Self {
            video_id: track.id.clone(),
            title: track.title.clone(),
            artist: track.uploader.clone(),
            artist_ref: track.artist_ref.clone(),
            album: track.album.clone(),
            album_route: None,
            duration: track.duration,
            queue_title: String::new(),
            queue: Vec::new(),
            // Replaced along with the queue itself the moment one arrives. Zero
            // is never minted by `App`, so a page that somehow answered a page
            // with no queue cannot be applied to it.
            queue_epoch: 0,
            continuation: None,
            topping_up: false,
            topup_failures: 0,
            dropped: VecDeque::new(),
            playing: None,
            liked: None,
            like_pending: false,
            rating_request: 0,
            repeat: RepeatMode::Off,
            tab: Tab::UpNext,
            cursor: [0; Tab::ALL.len()],
            follow_lyrics: true,
            lyrics: Panel::Idle,
            comments: Panel::Idle,
            related: Panel::Idle,
            lyrics_id: None,
            related_id: None,
            watched: false,
        }
    }

    /// What LRCLIB should be asked about this track, if it can be asked at all.
    ///
    /// Built from the page rather than the queue row because it is the playing
    /// track this is for, and `None` when the track is too thinly named to
    /// match on -- see [`lrclib::Query::new`].
    fn lyrics_query(&self) -> Option<lrclib::Query> {
        lrclib::Query::new(
            &self.artist,
            &self.title,
            self.album.as_deref(),
            self.duration,
        )
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

    /// Moves the open tab's cursor, and hands the lyrics panel back to the
    /// user: they have said where they want to be looking, and a panel that
    /// yanks itself back to the singer is one they cannot read ahead in.
    ///
    /// Only the near end is clamped here. The far end is the renderer's, which
    /// is what knows how long each panel is once it has wrapped it.
    fn scroll(&mut self, delta: isize) {
        self.follow_lyrics = false;
        let cursor = self.cursor_mut();
        *cursor = cursor.saturating_add_signed(delta);
    }

    /// [`Self::scroll`] straight to a line, for `g` and `G`.
    fn jump(&mut self, to: usize) {
        self.follow_lyrics = false;
        *self.cursor_mut() = to;
    }

    /// Opens a tab, and reports whether that was a change.
    ///
    /// Landing on Lyrics is what asks for the line being sung, whether or not
    /// the tab changed: pressing `2` while already there is how a user who has
    /// scrolled off says "back to the song", and it needs no key of its own.
    /// The caller uses the return to decide whether to start a fetch.
    fn open(&mut self, tab: Tab) -> bool {
        if tab == Tab::Lyrics {
            self.follow_lyrics = true;
        }
        let changed = self.tab != tab;
        self.tab = tab;
        changed
    }

    /// Replaces the deliberately thin metadata used for a pasted URL or bare
    /// id with the real row from the queue once the watch response arrives.
    fn hydrate_from_queue(&mut self) -> Option<Track> {
        let track = self
            .playing
            .and_then(|index| self.queue.get(index))
            .cloned()?;

        if self.artist.is_empty() {
            self.artist = track.uploader.clone();
        }
        if self.artist_ref.is_none() {
            self.artist_ref = track.artist_ref.clone();
        }
        if self.title == self.video_id
            || extract_video_id(&self.title).as_deref() == Some(self.video_id.as_str())
        {
            self.title = track.title.clone();
        }
        if self.album.is_none() {
            self.album = track.album.clone();
        }
        if self.duration.is_none() {
            self.duration = track.duration;
        }
        Some(track)
    }

    /// The track the queue would play next, if there is one.
    fn next_in_queue(&self) -> Option<&Track> {
        self.queue.get(self.playing? + 1)
    }

    fn advanced_index(&self, delta: isize, auto: bool) -> Option<usize> {
        let current = self.playing?;
        if auto && delta > 0 && self.repeat == RepeatMode::One {
            return Some(current);
        }
        let index = usize::try_from(current as isize + delta).ok();
        if index.is_some_and(|index| index < self.queue.len()) {
            return index;
        }
        if self.repeat == RepeatMode::All && !self.topping_up && !self.queue.is_empty() {
            return Some(if delta > 0 { 0 } else { self.queue.len() - 1 });
        }
        None
    }

    /// Inserts an explicit user choice while preserving the queue's fixed
    /// memory window. When the ahead window is full, its farthest item yields
    /// to the requested track; no user action can grow the queue past the same
    /// bound used by radio continuations.
    fn insert_user_track(&mut self, track: Track, next: bool) -> bool {
        let Some(playing) = self.playing else {
            return false;
        };
        if self.queue.iter().any(|held| held.id == track.id) {
            return false;
        }

        let ceiling = playing + 1 + QUEUE_AHEAD;
        if self.queue.len() >= ceiling {
            self.queue.pop();
        }
        self.dropped.retain(|id| *id != track.id);
        if next {
            self.queue
                .insert((playing + 1).min(self.queue.len()), track);
        } else {
            self.queue.push(track);
        }
        true
    }

    /// Removes only a future track. Played rows and the track producing sound
    /// are history, not editable queue entries.
    fn remove_selected_upcoming(&mut self) -> Option<Track> {
        let playing = self.playing?;
        let selected = self.cursor[Tab::UpNext.index()];
        if selected <= playing || selected >= self.queue.len() {
            return None;
        }
        let removed = self.queue.remove(selected);
        self.cursor[Tab::UpNext.index()] = selected.min(self.queue.len().saturating_sub(1));
        Some(removed)
    }

    /// Moves one future row without letting it cross the currently playing
    /// track. Returns false at either edge or outside the upcoming queue.
    fn move_selected_upcoming(&mut self, delta: isize) -> bool {
        let Some(playing) = self.playing else {
            return false;
        };
        let selected = self.cursor[Tab::UpNext.index()];
        if selected <= playing || selected >= self.queue.len() {
            return false;
        }
        let first = playing + 1;
        let last = self.queue.len() - 1;
        let moved = selected.saturating_add_signed(delta).clamp(first, last);
        if moved == selected {
            return false;
        }
        self.queue.swap(selected, moved);
        self.cursor[Tab::UpNext.index()] = moved;
        true
    }

    fn clear_upcoming(&mut self) -> usize {
        let Some(playing) = self.playing else {
            return 0;
        };
        let removed = self.queue.len().saturating_sub(playing + 1);
        self.queue.truncate(playing + 1);
        self.cursor[Tab::UpNext.index()] = playing;
        removed
    }

    /// Shuffles only what has not played, keeping the current track and the
    /// cursor's selected song stable.
    fn shuffle_upcoming(&mut self) -> usize {
        let Some(first) = self.playing.map(|playing| playing + 1) else {
            return 0;
        };
        let count = self.queue.len().saturating_sub(first);
        if count < 2 {
            return 0;
        }
        let selected_id = self
            .queue
            .get(self.cursor[Tab::UpNext.index()])
            .map(|track| track.id.clone());
        shuffle_tracks(&mut self.queue[first..]);
        if let Some(id) = selected_id
            && let Some(index) = self.queue.iter().position(|track| track.id == id)
        {
            self.cursor[Tab::UpNext.index()] = index;
        }
        count
    }

    /// How many tracks are left ahead of the one playing.
    ///
    /// Zero when nothing here is playing inside this queue, rather than the
    /// whole length: a queue nothing is being consumed from has no low-water
    /// mark to cross, and reporting its length would top up a queue that is
    /// standing still.
    fn remaining(&self) -> usize {
        match self.playing {
            Some(playing) => self.queue.len().saturating_sub(playing + 1),
            None => 0,
        }
    }

    /// Drops what has fallen out of the window behind the playing track,
    /// remembering the ids so the radio cannot offer them back.
    ///
    /// Both indices move with the tracks, or they stop meaning anything:
    /// `playing` is what every advance counts from and the Up-next cursor is
    /// what the user is looking at. Taking five rows off the front without
    /// subtracting five from both is a queue that silently jumps.
    fn trim(&mut self) {
        let Some(playing) = self.playing else {
            return;
        };
        let Some(excess) = playing.checked_sub(QUEUE_BEHIND) else {
            return;
        };

        for track in self.queue.drain(..excess) {
            if self.dropped.len() >= QUEUE_MEMORY {
                self.dropped.pop_front();
            }
            self.dropped.push_back(track.id);
        }
        self.playing = Some(playing - excess);
        let cursor = &mut self.cursor[Tab::UpNext.index()];
        *cursor = cursor.saturating_sub(excess);
    }

    /// Appends a page of tracks, and reports how many were new.
    ///
    /// Anything already in the window or recently dropped out of it is
    /// discarded: a radio repeats over hours, and a queue that grows by
    /// re-appending what it just played is a loop rather than an endless queue.
    /// The count is what tells the caller a page of nothing but repeats has
    /// arrived -- the radio has run out of material, and the queue should stop
    /// asking rather than spend a round trip per track discovering it again.
    fn absorb(&mut self, tracks: Vec<Track>) -> usize {
        let ceiling = self.playing.map_or(QUEUE_AHEAD, |at| at + 1 + QUEUE_AHEAD);
        let mut added = 0;
        for track in tracks {
            if self.queue.len() >= ceiling {
                break;
            }
            if self.queue.iter().any(|held| held.id == track.id) || self.dropped.contains(&track.id)
            {
                continue;
            }
            self.queue.push(track);
            added += 1;
        }
        added
    }

    /// Takes a page onto the queue and settles what to do about the next one.
    ///
    /// The policy for both kinds of page, held in one place because they only
    /// differ in where the tracks came from: a page that carried the queue
    /// forward is trusted to say where the page after it lives, and a page that
    /// carried it nowhere gives up the token that bought it.
    ///
    /// Returns how many tracks were new, which is the only part the caller
    /// still has a decision to make about.
    fn take_page(&mut self, page: QueuePage) -> usize {
        let added = self.absorb(page.tracks);
        if added == 0 {
            // Every track was one already held or already played. The station
            // has run out of material rather than out of pages, so its token
            // goes: asking again would buy this same page, where letting the
            // next top-up fall through to the journal builds somewhere new.
            //
            // Counted against the same budget as a refusal. Without that, a run
            // of stations that each turn out to be material already heard would
            // be a round trip per track, forever.
            self.continuation = None;
            self.topup_failures += 1;
            return 0;
        }

        self.topup_failures = 0;
        self.continuation = page.continuation;
        // A station built by the journal renames the queue; a continuation of
        // the one already playing leaves the name alone.
        if let Some(title) = page.title {
            self.queue_title = title;
        }
        added
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

/// Which page the shared modal menu is showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuPage {
    Root,
    Account,
    Help,
    PageActions,
    PlayerActions,
}

impl MenuPage {
    pub fn title(self) -> &'static str {
        match self {
            Self::Root => "Menu",
            Self::Account => "Account",
            Self::Help => "Keyboard shortcuts",
            Self::PageActions => "Page Actions",
            Self::PlayerActions => "Player actions",
        }
    }
}

/// Cursor state for the one menu overlay.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Menu {
    pub page: MenuPage,
    pub selected: usize,
    items: Vec<MenuItem>,
}

impl Menu {
    fn new(page: MenuPage, items: Vec<MenuItem>) -> Self {
        Self {
            page,
            selected: 0,
            items,
        }
    }

    fn move_by(&mut self, delta: isize, len: usize) {
        let mut next = moved_cursor(self.selected, delta, len);
        while self.page != MenuPage::Help && self.items.get(next).is_some_and(|item| !item.enabled) {
            let following = moved_cursor(next, delta.signum(), len);
            if following == next { return; }
            next = following;
        }
        self.selected = next;
    }

    fn shortcut_index(&self, key: KeyEvent) -> Option<usize> {
        self.items.iter().position(|item| {
            item.enabled && item.action.is_some() && (
                item.shortcut.is_some_and(|shortcut| menu_shortcut_matches(shortcut, key))
                || (item.action == Some(MenuAction::OpenSettings) && is_bare_character(key, 'S'))
            )
        })
    }

    fn first(&mut self) {
        self.selected = 0;
    }

    fn last(&mut self, len: usize) {
        self.selected = len.saturating_sub(1);
    }

    fn clamp(&mut self, len: usize) {
        self.selected = self.selected.min(len.saturating_sub(1));
    }
}

/// A renderer-facing menu row. The semantic action stays private so drawing and
/// execution cannot grow separate command maps.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MenuItem {
    pub label: String,
    pub shortcut: Option<&'static str>,
    pub enabled: bool,
    /// A heading to draw immediately before this row.
    pub section: Option<&'static str>,
    action: Option<MenuAction>,
}

impl MenuItem {
    pub fn opens_panel(&self) -> bool {
        matches!(self.action, Some(MenuAction::OpenAccount | MenuAction::OpenSettings | MenuAction::OpenHelp))
    }

    fn action(
        label: impl Into<String>,
        shortcut: Option<&'static str>,
        enabled: bool,
        section: Option<&'static str>,
        action: MenuAction,
    ) -> Self {
        Self {
            label: label.into(),
            shortcut,
            enabled,
            section,
            action: Some(action),
        }
    }

    fn help(label: &'static str, shortcut: &'static str, section: Option<&'static str>) -> Self {
        Self {
            label: label.to_string(),
            shortcut: Some(shortcut),
            enabled: false,
            section,
            action: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MenuAction {
    GoHome,
    BeginSearch,
    OpenPlayer,
    OpenAccount,
    OpenSettings,
    OpenHelp,
    #[cfg(windows)]
    Background,
    Quit,
    ConnectMusic,
    LogOutMusic,
    OpenHomeSelection,
    StartHomeRadio,
    PlayHomeNext,
    QueueHomeTrack,
    PlayHomeShelf,
    ShuffleHomeShelf,
    RefreshHome,
    PlaySelected,
    OpenArtist,
    OpenArtistSelection,
    ReloadArtist,
    OpenPageSelection,
    RemoveQueueSelection,
    MoveQueueSelectionUp,
    MoveQueueSelectionDown,
    ClearUpcomingQueue,
    ShuffleUpcomingQueue,
    CycleRepeat,
    FollowLyrics,
    TogglePause,
    ToggleMute,
    RetryTrack,
    Next,
    Previous,
    Stop,
    ToggleCoverSize,
    LikePlaying,
    SharePlaying,
    SavePlaying,
    ChooseOutput,
}

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
    /// Artwork selected for runtime-owned windows and the notification area.
    pub icon_theme: IconTheme,
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
    /// Visual language used for the current song's large cover.
    pub cover_style: CoverStyle,
    /// User-selected terminal bitmap backend.
    pub image_renderer: ImageRenderer,
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
    /// What is playing and the page around it. `None` before the first play and
    /// after a stop -- there is no player page for silence.
    pub now: Option<NowPlaying>,
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
    /// The last queue epoch handed out. Incremented for every queue that
    /// arrives, so no two queues in one session share one and a page of tracks
    /// can only ever be applied to the queue that asked for it.
    queue_epoch: u64,
    /// Steps the seed the journal builds a station from, so a station that led
    /// nowhere is not the one built again. Kept here rather than on the queue
    /// because it outlives any one of them: two queues in a row that both run
    /// dry should be rescued by two different stations.
    seed_rotation: usize,
    /// The track being listened to, and the furthest into it playback has got.
    ///
    /// Held here rather than read off the snapshot when it is needed, because by
    /// the time a play is over there is nothing left to read: the track has
    /// ended, the snapshot says `Idle` at position zero, and the page below has
    /// already been replaced with whatever came next. This is what
    /// [`App::finish_listening`] reports from.
    ///
    /// Measured audio progress, excluding paused time and seek jumps.
    listening: Option<(Track, Listening)>,
    /// Prevents repeated `M` presses from opening duplicate session imports.
    music_signing_in: bool,
    automatic_sign_in: bool,
    sign_in_prompted: bool,
    session_renewal: crate::session::renewal::Renewal,
    account_request: u64,
    pending_save: Option<(u64, String)>,

    /// A track whose stream died, waiting on a fresh URL to carry on with.
    ///
    /// Held separately from [`Self::pending`] because the two mean opposite
    /// things about what is on screen: `pending` is a track the user chose and
    /// is waiting to hear, so its answer replaces the title, the page and the
    /// cover. This is a track already playing, and its answer must change none
    /// of that -- only where the audio is read from.
    resuming: Option<Resuming>,
    /// Concise category for a terminal playback failure. Detailed resolver
    /// output stays in diagnostics; this is the actionable text shown beside
    /// Retry and Skip.
    playback_error: Option<String>,

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

/// A track being recovered mid-play, and where playback has to pick up.
struct Resuming {
    id: String,
    from: Duration,
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

fn moved_cursor(selected: usize, delta: isize, len: usize) -> usize {
    let last = len.saturating_sub(1);
    let selected = selected.min(last);
    if delta < 0 {
        selected.saturating_sub(delta.unsigned_abs())
    } else {
        selected.saturating_add(delta as usize).min(last)
    }
}

/// Fisher-Yates with a tiny local xorshift state. Pulling a random-number
/// crate into the player solely to reorder at most 66 already-held tracks
/// would cost more code and dependency surface than the feature warrants.
fn shuffle_tracks(tracks: &mut [Track]) {
    let mut state = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64
        ^ tracks.len() as u64;
    for end in (1..tracks.len()).rev() {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        let index = (state as usize) % (end + 1);
        tracks.swap(end, index);
    }
}

fn is_ctrl_key(key: KeyEvent, character: char) -> bool {
    key.modifiers.contains(KeyModifiers::CONTROL)
        && !key.modifiers.contains(KeyModifiers::ALT)
        && matches!(key.code, KeyCode::Char(c) if c.eq_ignore_ascii_case(&character))
}

fn is_bare_character(key: KeyEvent, character: char) -> bool {
    key.modifiers.difference(KeyModifiers::SHIFT).is_empty() && key.code == KeyCode::Char(character)
}

fn menu_shortcut_matches(shortcut: &str, key: KeyEvent) -> bool {
    match shortcut {
        "Space" => is_bare_character(key, ' '),
        "Ctrl+S" => is_ctrl_key(key, 's'),
        "Ctrl+C" => is_ctrl_key(key, 'c'),
        _ => {
            let mut characters = shortcut.chars();
            characters.next().is_some_and(|character| {
                characters.next().is_none() && is_bare_character(key, character)
            })
        }
    }
}

fn page_behind_search(origin: View, player_back: View) -> View {
    match origin {
        View::Playing => player_back,
        origin => origin,
    }
}

/// Player is a transient view over another page, never its own return target.
fn player_return_target(
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

fn card_artist(card: &Card) -> Option<ArtistRef> {
    match &card.target {
        Target::Artist { artist } => Some(artist.clone()),
        Target::Play { .. } | Target::Open { .. } => card.artist_ref.clone(),
    }
}

fn push_history(history: &mut Vec<HistoryEntry>, page: HistoryEntry) {
    if history.len() == PAGE_HISTORY {
        history.remove(0);
    }
    history.push(page);
}

fn accept_pending_page(
    pending: &mut Option<(PageRequestId, PageRequestKind)>,
    request_id: PageRequestId,
) -> bool {
    if pending.map(|pending| pending.0) != Some(request_id) {
        return false;
    }
    *pending = None;
    true
}

fn has_unsettled_page(pending: Option<(PageRequestId, PageRequestKind)>) -> bool {
    pending.is_some_and(|(_, kind)| kind != PageRequestKind::Search)
}

fn page_snapshot_blocked(
    pending: Option<(PageRequestId, PageRequestKind)>,
    busy: bool,
    home_pending: bool,
) -> bool {
    home_pending
        || has_unsettled_page(pending)
        || (busy && !matches!(pending, Some((_, PageRequestKind::Search))))
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
            results: Vec::new(),
            search_items: Vec::new(),
            search_filter: crate::source::search::Filter::All,
            selected: 0,
            offset: 0,
            status: "loading the home feed ...".to_string(),
            busy: false,
            should_quit: false,
            wants_background: false,
            wants_foreground: false,
            start_in_tray: settings.start_in_tray,
            icon_theme: settings.icon_theme,
            cover: None,
            cover_id: None,
            graphics,
            cover_size: CoverSize::default(),
            cover_style: settings.cover_style,
            image_renderer: settings.image_renderer,
            output_devices,
            output_device,
            muted,
            volume_before_mute,
            volume_save: None,
            images: Vec::new(),
            painted: Vec::new(),
            painted_with_kitty: false,
            selection_settled: None,
            prefetching: None,
            ready: None,
            pending: None,
            view: View::Home,
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
            back_to: View::Home,
            player_back: None,
            search_origin: View::Home,
            last_state: PlayState::Idle,
            auto: false,
            skips: 0,
            listening: None,
            queue_epoch: 0,
            seed_rotation: 0,
            music_signing_in: false,
            automatic_sign_in: false,
            sign_in_prompted: false,
            session_renewal: crate::session::renewal::Renewal::default(),
            account_request: 0,
            pending_save: None,
            resuming: None,
            playback_error: None,
            // Started whether or not Discord is running and whether or not the
            // switch is on: what this costs while idle is one sleeping thread,
            // and deciding later would mean deciding on the render path.
            presence: Presence::spawn(config::Discord::application_id(), config::Presence::load()),
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
        match (self.view, &self.browsing) {
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

    pub fn collection_art_key(&self) -> Option<&str> {
        self.browsing_endpoint.as_ref().map(|endpoint| endpoint.browse_id.as_str())
    }

    fn snapshot_blocked(&self) -> bool {
        page_snapshot_blocked(
            self.pending_page_request,
            self.busy,
            self.view == View::Home && self.home_pending,
        )
    }

    fn live_player_status(&self) -> String {
        let snapshot = self.snapshot();
        if let Some(error) = snapshot.error {
            return error;
        }
        let Some(now) = self.now.as_ref() else {
            return "nothing is playing".to_string();
        };
        let label = if now.artist.is_empty() || now.artist == UNKNOWN_ARTIST {
            now.title.clone()
        } else {
            format!("{} — {}", now.title, now.artist)
        };
        let state = match snapshot.state {
            PlayState::Idle if self.pending.is_some() => "resolving",
            PlayState::Idle => "stopped",
            PlayState::Buffering => "loading",
            PlayState::Playing => "playing",
            PlayState::Paused => "paused",
        };
        format!("{state} {label}")
    }

    fn current_page(&self) -> Option<HistoryEntry> {
        match self.view {
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

    fn push_current_page(&mut self) {
        let Some(page) = self.current_page() else {
            return;
        };
        self.push_page(page);
    }

    fn push_page(&mut self, page: HistoryEntry) {
        push_history(&mut self.history, page);
    }

    fn restore_page(&mut self, page: HistoryEntry) {
        self.cancel_page_request();
        self.mode = Mode::Browse;
        match page {
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

    fn go_back(&mut self) {
        if let Some(page) = self.history.pop() {
            self.restore_page(page);
        } else {
            self.cancel_page_request();
            self.mode = Mode::Browse;
            self.view = View::Home;
        }
    }

    fn go_home(&mut self) {
        self.cancel_page_request();
        self.history.clear();
        self.search_page = None;
        self.mode = Mode::Browse;
        self.view = View::Home;
    }

    fn begin_page_request(&mut self, kind: PageRequestKind) -> PageRequestId {
        self.page_request = self.page_request.wrapping_add(1).max(1);
        self.pending_page_request = Some((self.page_request, kind));
        self.busy = true;
        self.page_request
    }

    fn accept_page_response(&mut self, request_id: PageRequestId) -> bool {
        if !accept_pending_page(&mut self.pending_page_request, request_id) {
            return false;
        }
        self.busy = false;
        true
    }

    fn cancel_page_request(&mut self) {
        if self.pending_page_request.take().is_some() {
            self.busy = false;
        }
    }

    fn cancel_search_request(&mut self) {
        if matches!(
            self.pending_page_request,
            Some((_, PageRequestKind::Search))
        ) {
            self.cancel_page_request();
        }
    }

    /// State for the active menu overlay, if the keyboard belongs to one.
    pub fn menu(&self) -> Option<&Menu> {
        self.menu.as_ref()
    }

    /// Rows for the active menu. Enter resolves its action from this same list,
    /// so a renderer can never show a command different from the one invoked.
    pub fn menu_items(&self) -> Vec<MenuItem> {
        self.menu()
            .map(|menu| menu.items.clone())
            .unwrap_or_default()
    }

    pub fn presence_enabled(&self) -> bool {
        self.presence.enabled()
    }

    pub fn icon_theme(&self) -> IconTheme {
        self.icon_theme
    }

    pub fn cover_style(&self) -> CoverStyle {
        self.cover_style
    }

    pub fn image_renderer(&self) -> ImageRenderer {
        self.image_renderer
    }

    pub fn output_device_id(&self) -> Option<&str> {
        self.output_device.as_deref()
    }

    pub fn output_device_label(&self) -> &str {
        let Some(id) = self.output_device.as_deref() else {
            return "System default";
        };
        self.output_devices
            .iter()
            .find(|output| output.id == id)
            .map(|output| output.name.as_str())
            .unwrap_or("Unavailable output")
    }

    /// Kitty is used automatically when detected, or explicitly when a
    /// multiplexer hides the terminal name from capability detection.
    pub fn kitty_images(&self) -> bool {
        self.image_renderer == ImageRenderer::Kitty
            || (self.image_renderer == ImageRenderer::Automatic && self.graphics.kitty)
    }

    pub fn sixel_images(&self) -> bool {
        self.image_renderer == ImageRenderer::Automatic && self.graphics.sixel
    }

    pub fn protocol_images(&self) -> bool {
        self.kitty_images() || self.sixel_images()
    }

    fn root_menu_items(&self) -> Vec<MenuItem> {
        app_menu_items(self.now.is_some())
    }

    fn account_menu_items(&self) -> Vec<MenuItem> {
        let connected = config::Cookies::available().ok().flatten().is_some();
        account_menu_items(connected, self.music_signing_in)
    }

    fn help_menu_items(&self) -> Vec<MenuItem> {
        keyboard_help_items()
    }

    fn page_action_items(&self) -> Vec<MenuItem> {
        match self.view {
            View::Home => {
                let selected = self.home_card();
                let (label, enabled) = match selected {
                    Some(card) if matches!(&card.target, Target::Play { .. }) => {
                        ("Play selected item", true)
                    }
                    Some(_) => ("Open selected item", true),
                    None => ("Open selected item", false),
                };
                let playable = selected.is_some_and(Card::is_playable);
                let queue_ready =
                    playable && self.now.as_ref().is_some_and(|now| now.playing.is_some());
                let artist = selected.and_then(card_artist).is_some();
                let playable_in_shelf = self.home.get(self.home_shelf).map_or(0, |shelf| {
                    shelf.cards.iter().filter(|card| card.is_playable()).count()
                });
                vec![
                    MenuItem::action(
                        label,
                        Some("Enter"),
                        enabled,
                        Some("Home"),
                        MenuAction::OpenHomeSelection,
                    ),
                    MenuItem::action(
                        "Start radio from selected song",
                        None,
                        playable,
                        None,
                        MenuAction::StartHomeRadio,
                    ),
                    MenuItem::action(
                        "Play next",
                        None,
                        queue_ready,
                        None,
                        MenuAction::PlayHomeNext,
                    ),
                    MenuItem::action(
                        "Add to queue",
                        None,
                        queue_ready,
                        None,
                        MenuAction::QueueHomeTrack,
                    ),
                    MenuItem::action(
                        "Open selected artist",
                        None,
                        artist,
                        None,
                        MenuAction::OpenArtist,
                    ),
                    MenuItem::action(
                        "Play this shelf",
                        None,
                        playable_in_shelf > 0,
                        Some("Current shelf"),
                        MenuAction::PlayHomeShelf,
                    ),
                    MenuItem::action(
                        "Shuffle this shelf",
                        None,
                        playable_in_shelf > 1,
                        None,
                        MenuAction::ShuffleHomeShelf,
                    ),
                    MenuItem::action(
                        "Refresh Home",
                        Some("r"),
                        !self.home_pending,
                        Some("Home"),
                        MenuAction::RefreshHome,
                    ),
                ]
            }
            View::Tracks => {
                let selected = self.selected_result_track();
                let mut items = vec![MenuItem::action(
                    if selected.is_some() { "Play selected track" } else { "Open selected result" },
                    Some("Enter"),
                    self.result_count() > 0,
                    Some("Selected track"),
                    MenuAction::PlaySelected,
                )];
                if selected
                    .and_then(|track| track.artist_ref.as_ref())
                    .is_some()
                {
                    items.push(MenuItem::action(
                        "Open selected artist",
                        None,
                        true,
                        None,
                        MenuAction::OpenArtist,
                    ));
                }
                items
            }
            View::Artist => self.artist_action_items(),
            View::Playing => self.playing_action_items(),
        }
    }

    fn artist_action_items(&self) -> Vec<MenuItem> {
        let Some(artist) = self.artist.as_ref() else {
            return Vec::new();
        };
        if matches!(artist.page, Panel::Empty(_)) {
            return vec![MenuItem::action(
                "Retry artist page",
                Some("r"),
                true,
                Some("Artist"),
                MenuAction::ReloadArtist,
            )];
        }

        let selected_song = artist.selected_song();
        let selected_card = artist.selected_card();
        let (label, enabled) = match (selected_song, selected_card) {
            (Some(_), _) => ("Play selected top song", true),
            (_, Some(card)) if card.is_playable() => ("Play selected item", true),
            (_, Some(_)) => ("Open selected item", true),
            _ => ("Open selected item", false),
        };
        let mut items = vec![MenuItem::action(
            label,
            Some("Enter"),
            enabled,
            Some("Artist"),
            MenuAction::OpenArtistSelection,
        )];
        items.push(MenuItem::action(
            "Refresh artist page",
            Some("r"),
            !matches!(artist.page, Panel::Loading),
            None,
            MenuAction::ReloadArtist,
        ));
        items
    }

    fn playing_action_items(&self) -> Vec<MenuItem> {
        let mut items = Vec::new();
        let Some(now) = self.now.as_ref() else {
            return items;
        };

        match now.tab {
            Tab::UpNext => {
                let cursor = now.cursor();
                let playing = now.playing;
                let upcoming =
                    playing.is_some_and(|index| cursor > index && cursor < now.queue.len());
                let can_move_up = playing.is_some_and(|index| cursor > index + 1);
                let can_move_down = upcoming && cursor + 1 < now.queue.len();
                items.extend([
                    MenuItem::action(
                        "Play selected queue track",
                        Some("Enter"),
                        now.queue.get(cursor).is_some(),
                        Some("Selection"),
                        MenuAction::OpenPageSelection,
                    ),
                    MenuItem::action(
                        "Remove from queue",
                        Some("d"),
                        upcoming,
                        None,
                        MenuAction::RemoveQueueSelection,
                    ),
                    MenuItem::action(
                        "Move up",
                        Some("K"),
                        can_move_up,
                        None,
                        MenuAction::MoveQueueSelectionUp,
                    ),
                    MenuItem::action(
                        "Move down",
                        Some("J"),
                        can_move_down,
                        None,
                        MenuAction::MoveQueueSelectionDown,
                    ),
                    MenuItem::action(
                        "Shuffle upcoming",
                        Some("z"),
                        now.remaining() > 1,
                        Some("Queue"),
                        MenuAction::ShuffleUpcomingQueue,
                    ),
                    MenuItem::action(
                        "Clear upcoming",
                        Some("C"),
                        now.remaining() > 0,
                        None,
                        MenuAction::ClearUpcomingQueue,
                    ),
                ]);
            }
            Tab::Related => {
                let (label, enabled) = match now.related_rows().get(now.cursor()) {
                    Some(RelatedRow::Card(card)) if matches!(&card.target, Target::Play { .. }) => {
                        ("Play selected related track", true)
                    }
                    Some(RelatedRow::Card(_)) => ("Open selected related item", true),
                    _ => ("Open selected related item", false),
                };
                items.push(MenuItem::action(
                    label,
                    Some("Enter"),
                    enabled,
                    Some("Selection"),
                    MenuAction::OpenPageSelection,
                ));
            }
            Tab::Lyrics if matches!(&now.lyrics, Panel::Ready(lyrics) if !lyrics.timed.is_empty()) =>
            {
                items.push(MenuItem::action(
                    "Follow current lyric",
                    Some("2"),
                    true,
                    Some("Lyrics"),
                    MenuAction::FollowLyrics,
                ));
            }
            Tab::Lyrics | Tab::Comments => {}
        }

        let snapshot = self.snapshot();
        let failed = self.playback_failed(&snapshot);
        let current = now.playing;
        let can_previous =
            current.is_some_and(|index| index > 0 && now.queue.get(index - 1).is_some());
        let can_next = current.is_some_and(|index| now.queue.get(index + 1).is_some());
        items.extend([
            MenuItem::action(
                "Retry current track",
                Some("r"),
                failed,
                Some("Playback failure"),
                MenuAction::RetryTrack,
            ),
            MenuItem::action(
                "Open current artist",
                None,
                now.artist_ref.is_some(),
                Some("Current track"),
                MenuAction::OpenArtist,
            ),
            MenuItem::action(
                if snapshot.state == PlayState::Paused {
                    "Resume current track"
                } else {
                    "Pause current track"
                },
                Some("Space"),
                snapshot.state != PlayState::Idle,
                Some("Playback"),
                MenuAction::TogglePause,
            ),
            MenuItem::action(
                if self.muted {
                    "Restore volume"
                } else {
                    "Mute audio"
                },
                Some("m"),
                snapshot.state != PlayState::Idle,
                None,
                MenuAction::ToggleMute,
            ),
            MenuItem::action(
                if failed {
                    "Skip failed track"
                } else {
                    "Play next track"
                },
                Some("n"),
                can_next,
                None,
                MenuAction::Next,
            ),
            MenuItem::action(
                "Play previous track",
                Some("p"),
                can_previous,
                None,
                MenuAction::Previous,
            ),
            MenuItem::action(
                format!("Repeat: {}", now.repeat.label()),
                Some("R"),
                current.is_some(),
                None,
                MenuAction::CycleRepeat,
            ),
            MenuItem::action("Stop playback", Some("s"), true, None, MenuAction::Stop),
            MenuItem::action(
                if self.cover_size == CoverSize::Side {
                    "Show full-size cover"
                } else {
                    "Show side cover"
                },
                Some("c"),
                true,
                None,
                MenuAction::ToggleCoverSize,
            ),
        ]);
        items
    }

    /// The card under the cursor on the landing page.
    pub fn home_card(&self) -> Option<&Card> {
        self.home.get(self.home_shelf)?.cards.get(self.home_card)
    }

    pub fn snapshot(&self) -> Snapshot {
        self.player.snapshot()
    }

    fn playback_failed(&self, snapshot: &Snapshot) -> bool {
        self.now.is_some()
            && self.pending.is_none()
            && self.resuming.is_none()
            && snapshot.state == PlayState::Idle
            && (self.playback_error.is_some() || snapshot.error.is_some())
    }

    /// True while the user is waiting on something: a request in flight, or a
    /// track being loaded. What the event loop redraws faster for -- a resolve
    /// that lands from cache is over in milliseconds, and waiting out the idle
    /// tick to notice would be most of the latency it has left.
    pub fn awaiting(&self) -> bool {
        self.busy || self.pending_page_request.is_some() || self.pending.is_some()
    }

    /// The cover to paint as pixels, if what the renderer planned is not
    /// already on screen. `None` means the pixels there are correct and
    /// repainting would only cost bandwidth and flicker.
    pub fn images_to_paint(&self) -> Vec<(&Cover, &PlannedImage)> {
        if self.painted == self.images {
            return Vec::new();
        }
        self.images
            .iter()
            .filter(|image| !self.painted.contains(image))
            .filter_map(|image| {
                let art = match &image.source {
                    ImageSource::Playing => self.cover.as_ref(),
                    ImageSource::Card(key) => self.art.get(key),
                }?;
                Some((art, image))
            })
            .collect()
    }

    pub fn mark_painted(&mut self, kitty: bool) {
        self.painted.clone_from(&self.images);
        self.painted_with_kitty = kitty;
    }

    /// True when pixels are on screen that no longer belong there: the pane
    /// went away, or moved out from under them. Erasing protocol pixels means
    /// painting cells over it, so only a redraw can undo it.
    pub fn image_needs_clearing(&self) -> bool {
        // A new cover arriving does not invalidate pictures already on screen.
        // Only removals or changed placements need their old pixels erased.
        self.painted.iter().any(|image| !self.images.contains(image))
    }

    pub fn painted_images(&self) -> &[PlannedImage] {
        &self.painted
    }

    pub fn painted_with_kitty(&self) -> bool {
        self.painted_with_kitty
    }

    /// Forgets what is on screen so the next frame repaints it. For events that
    /// can destroy the pixels without changing the plan, such as a resize.
    pub fn invalidate_image(&mut self) {
        self.painted.clear();
        self.painted_with_kitty = false;
    }

    /// Drains completed source work. Called once per frame.
    ///
    /// Drains rather than taking one: two threads now feed the response queue,
    /// so a cover and a resolve can land in the same frame and handling only
    /// the first would leave the other a frame stale.
    pub fn poll_source(&mut self) {
        if self.volume_save.is_some_and(|(at, _)| at.elapsed() >= Duration::from_millis(250)) {
            self.flush_volume();
        }
        while let Some(response) = self.source.poll() {
            self.apply(response);
        }
        self.poll_player();
        self.tick_session();
    }

    /// The same loop runs in the tray. Only the slow policy check touches disk;
    /// browser startup and renewal remain on a separate, bounded worker.
    fn tick_session(&mut self) {
        if crate::session::take_authentication_failure() {
            self.session_renewal.authentication_failed();
        }
        if self.music_signing_in
            || matches!(&self.overlay, Overlay::SavePlaylist(picker) if picker.loading || picker.saving)
            || self.pending_save.is_some()
            || self.now.as_ref().is_some_and(|now| now.like_pending)
            || !self.session_renewal.take_due()
        { return; }
        self.music_signing_in = true;
        if self.source.send(Request::RenewMusicSession).is_err() {
            self.music_signing_in = false;
            crate::diagnostics::warn("auth", "could not queue automatic session renewal");
        }
    }

    /// Answers what the player thread cannot do for itself.
    ///
    /// It owns the decoder and the output device and nothing else on purpose --
    /// resolving a URL means yt-dlp, a cache and a network client, none of
    /// which belong on the thread feeding the speakers. So when a stream dies
    /// and needs a new one, it asks here.
    fn poll_player(&mut self) {
        while let Some(event) = self.player.poll_event() {
            match event {
                PlayerEvent::NeedsUrl { id, from } => self.resume_track(id, from),
                PlayerEvent::OutputChanged { id, name } => {
                    self.output_device = id;
                    self.refresh_output_devices();
                    let settings = config::Settings {
                        start_in_tray: self.start_in_tray,
                        icon_theme: self.icon_theme,
                        cover_style: self.cover_style,
                        image_renderer: self.image_renderer,
                        volume: self.snapshot().volume,
                        output_device: self.output_device.clone(),
                    };
                    self.status = match settings.save() {
                        Ok(()) => format!("audio output set to {name}"),
                        Err(error) => {
                            crate::diagnostics::error(
                                "config",
                                &format!("could not save audio output: {error:#}"),
                            );
                            format!("audio output changed, but could not be saved: {error:#}")
                        }
                    };
                    if matches!(self.overlay, Overlay::Settings) {
                        self.preferences.notice = Some((self.status.clone(), self.status.contains("could not")));
                    }
                }
                PlayerEvent::OutputChangeFailed { why } => {
                    self.status = why;
                    if matches!(self.overlay, Overlay::Settings) {
                        self.preferences.notice = Some((self.status.clone(), true));
                    }
                }
            }
        }
    }

    /// Resolves a fresh URL for a track whose stream died, and hands it back.
    ///
    /// The cache is bypassed deliberately: the entry it holds for this track is
    /// the URL that just failed. Handing it back was how a track that stopped
    /// once stopped at the same second on every replay for the rest of the
    /// session, and then quietly started working hours later when the
    /// signature expired.
    fn resume_track(&mut self, id: String, from: Duration) {
        let Some((track, _)) = self.listening.as_ref() else {
            // Nothing is playing any more, so there is nothing to carry on.
            let _ = self.player.send(Command::ResumeFailed {
                why: "playback stopped".to_string(),
            });
            return;
        };
        if track.id != id { return; }
        let title = track.label();

        self.status = format!("reconnecting {title} ...");
        let request = Request::Resolve {
            id: id.clone(),
            title,
            bypass_cache: true,
        };
        if self.source.send(request).is_err() {
            let _ = self.player.send(Command::ResumeFailed {
                why: "source worker is not running".to_string(),
            });
            return;
        }
        self.resuming = Some(Resuming { id, from });
    }

    fn apply(&mut self, response: Response) {
        let session_generation = match &response {
            Response::CookiesImported { generation, .. }
            | Response::MusicSignInFailed { generation, .. }
            | Response::SessionRenewed { generation, .. } => Some(*generation),
            _ => None,
        };
        if session_generation.is_some_and(|generation| generation != self.source.session_generation()) {
            return;
        }
        let page_request = match &response {
            Response::Results { request_id, .. }
            | Response::Browsed { request_id, .. }
            | Response::Artist { request_id, .. } => Some(*request_id),
            _ => None,
        };
        // Validate before any shared status, menu or busy state is touched. A
        // late page must be observationally silent, not merely prevented from
        // replacing the rows after it has already closed their menu.
        if page_request.is_some_and(|request_id| !self.accept_page_response(request_id)) {
            return;
        }

        // A contextual menu is a snapshot of the page under it. Responses that
        // replace that page close the menu rather than letting Enter act on a
        // different row than the one still visible in the modal.
        if matches!(
            &response,
            Response::Results { .. }
                | Response::Home { .. }
                | Response::Browsed { .. }
                | Response::Artist { .. }
        ) {
            self.close_page_actions();
        }

        // Nothing here is something the user is waiting on, so none of it may
        // clear the flag a search or a play set. The player page matters as
        // much as the cover does: its panels land on their own threads, and the
        // queue usually arrives while the resolve behind it is still running --
        // clearing the flag there would drop the redraw cadence back to the
        // idle one for the rest of the wait that flag exists to cover.
        //
        // A resolve is excluded for a different reason: a play never sets this
        // flag in the first place -- it tracks `pending` instead, for the
        // reason `play_track` gives -- so there is nothing here for a resolve
        // to be clearing. What it would clear is some *other* request's flag,
        // still in flight, and `awaiting` would then stop reporting a wait that
        // is still going on.
        if !matches!(
            response,
            Response::Rating { .. }
                | Response::Playlists { .. }
                | Response::PlaylistSaved { .. }
                | Response::Cover { .. }
                | Response::Art { .. }
                | Response::Results { .. }
                | Response::Browsed { .. }
                | Response::Artist { .. }
                | Response::Prefetched { .. }
                | Response::Resolved { .. }
                | Response::Replacement { .. }
                | Response::Watch { .. }
                | Response::MoreQueue { .. }
                | Response::Lyrics { .. }
                | Response::Related { .. }
                | Response::Comments { .. }
                | Response::Home { .. }
                | Response::HomeFailed { .. }
                | Response::CookiesImported { .. }
                | Response::MusicSignInFailed { .. }
                | Response::SessionRenewed { .. }
        ) {
            self.busy = false;
        }

        match response {
            Response::Rating { request_id, video_id, changed, result } => self.apply_rating(request_id, &video_id, changed, result),
            Response::Playlists { request_id, choices } => self.apply_playlists(request_id, choices),
            Response::PlaylistSaved { request_id, result } => self.apply_playlist_saved(request_id, result),
            Response::Results { tracks, .. } => {
                let tracks = match tracks {
                    Ok(tracks) => tracks,
                    Err(reason) => {
                        self.report(reason);
                        return;
                    }
                };
                if let Some(origin) = self.search_page.take() {
                    self.push_page(origin);
                }
                self.status = if tracks.is_empty() {
                    "no results".to_string()
                } else {
                    format!("{} results", tracks.len())
                };
                self.browsing = None;
                self.browsing_endpoint = None;
                self.collection = None;
                self.collection_error = None;
                self.view = View::Tracks;
                self.results = tracks.iter().filter_map(|item| item.track.clone()).collect();
                self.search_items = tracks;
                self.selected = 0;
                self.offset = 0;
                // Start the debounce on the top hit: it is what Enter plays
                // most of the time, so it is the one worth having warm.
                self.selection_settled = Some(Instant::now());
                // Results are the point of a search, including an honest empty
                // result. Ending the edit here makes one Back restore its origin.
                self.mode = Mode::Browse;
            }
            Response::Home {
                generation,
                shelves,
            } => {
                if generation != self.home_generation {
                    return;
                }
                self.home_attempts = self.home_attempts.saturating_sub(1);
                self.home_pending = self.home_attempts != 0;
                self.apply_home(shelves);
            }
            Response::HomeFailed { generation } => {
                if generation != self.home_generation {
                    return;
                }
                self.home_attempts = self.home_attempts.saturating_sub(1);
                self.finish_home_attempts();
            }
            Response::HomeMore { generation, shelves } => {
                if generation != self.home_generation { return; }
                for shelf in shelves {
                    if self.home.len() >= 24 { break; }
                    if self.home.iter().any(|existing| existing.title == shelf.title) { continue; }
                    self.home.push(shelf);
                    self.home_scroll.push(0);
                }
            }
            Response::CookiesImported { browser, .. } => {
                self.clear_account_actions();
                // The page on screen was built without a session. There is a
                // better one available now, so it is asked for again -- this is
                // the whole point of doing the import in the background rather
                // than holding the first frame back behind it.
                self.status = format!("read your YouTube session from {browser}");
                self.music_signing_in = false;
                self.automatic_sign_in = false;
                self.sign_in_prompted = false;
                self.session_renewal.succeeded();
                if matches!(self.overlay, Overlay::SignIn(SignIn::Music { .. })) {
                    self.overlay = Overlay::None;
                }
                // A valid session is the missing half of any reports queued
                // while signed out or while the previous cookie was stale.
                let _ = self.source.send(Request::RetryReports);
                self.request_rating();
                self.request_home();
            }
            Response::SessionRenewed { result, .. } => {
                self.music_signing_in = false;
                match result {
                    Ok(()) => {
                        self.sign_in_prompted = false;
                        self.session_renewal.succeeded();
                        crate::diagnostics::info("auth", "saved Music session renewed automatically");
                        let _ = self.source.send(Request::RetryReports);
                        if self.now.as_ref().is_none_or(|now| !now.like_pending) { self.request_rating(); }
                    }
                    Err(crate::session::RenewalFailure::SignInRequired) => {
                        if !self.sign_in_prompted {
                            self.sign_in_prompted = true;
                            self.automatic_sign_in = true;
                            self.music_signing_in = true;
                            self.status = "Google needs verification—finish in the sign-in window.".into();
                            if self.source.send(Request::MusicSignIn { recover: true }).is_err() {
                                self.music_signing_in = false;
                                self.automatic_sign_in = false;
                            }
                        }
                    }
                    Err(crate::session::RenewalFailure::Temporary(message)) => {
                        crate::diagnostics::warn("auth", &format!("automatic renewal deferred: {message}"));
                    }
                }
            }
            Response::MusicSignInFailed { message: msg, .. } => {
                crate::diagnostics::error("auth", "YouTube Music sign-in failed");
                self.music_signing_in = false;
                if self.automatic_sign_in {
                    self.automatic_sign_in = false;
                    self.status = "Google sign-in was not completed; automatic recovery will keep checking.".into();
                    return;
                }
                self.status = msg.clone();
                if matches!(self.overlay, Overlay::Settings) {
                    self.preferences.notice = Some((msg, true));
                } else {
                    self.menu = None;
                    self.overlay = Overlay::SignIn(SignIn::Failed { reason: msg });
                }
            }
            Response::Browsed {
                title,
                endpoint,
                page,
                ..
            } => {
                if self.browsing_endpoint.as_ref() != Some(&endpoint) {
                    return;
                }
                let page = match *page {
                    Ok(page) => page,
                    Err(reason) => {
                        self.collection_error = Some(reason.clone());
                        self.report(reason);
                        return;
                    }
                };
                let tracks = page.tracks;
                self.collection = Some(page.details);
                self.collection_error = None;
                if self.view == View::Tracks {
                    self.status = format!("{} tracks in {title}", tracks.len());
                }
                self.results = tracks;
                self.selected = 0;
                self.offset = 0;
                self.browsing = Some(title);
                self.browsing_endpoint = Some(endpoint);
                self.selection_settled = Some(Instant::now());
            }
            Response::Artist {
                browse_id, page, ..
            } => {
                let Some(artist) = self.artist.as_mut() else {
                    return;
                };
                if artist.requested.endpoint.browse_id != browse_id {
                    return;
                }
                artist.set_page(*page);
                let status = match &artist.page {
                    Panel::Ready(page) => format!(
                        "{} top songs and {} sections",
                        page.top_songs.len(),
                        page.shelves.len()
                    ),
                    Panel::Empty(reason) => reason.clone(),
                    Panel::Idle | Panel::Loading => String::new(),
                };
                if self.view == View::Artist {
                    self.status = status;
                }
                self.selection_settled = Some(Instant::now());
            }
            Response::Resolved { id, title, stream } => self.apply_resolved(&id, title, stream),
            Response::Replacement { id, stream } => {
                if self
                    .listening
                    .as_ref()
                    .is_some_and(|(track, _)| track.id == id)
                    && let Ok(stream) = stream
                {
                    let command = match self.resuming.take_if(|resuming| resuming.id == id) {
                        Some(resuming) => {
                            if let Some((track, _)) = self.listening.as_ref() {
                                self.status = format!("playing {}", track.label());
                            }
                            Command::Resume {
                                url: stream.url,
                                format: stream.format,
                                from: resuming.from,
                            }
                        }
                        None => Command::ArmReplacement {
                            id,
                            url: stream.url,
                            format: stream.format,
                        },
                    };
                    let _ = self.player.send(command);
                }
            }
            Response::Watch { video_id, watch } => self.apply_watch(&video_id, *watch),
            Response::MoreQueue { epoch, page } => self.apply_more_queue(epoch, page),
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
            Response::Art { key, art } => self.art.store(key, art),
            Response::Cover { id, art } => {
                // Stale unless it is still the track we asked about.
                if self.cover_id.as_deref() == Some(id.as_str()) {
                    self.cover = art;
                }
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
        // A recovery, not a choice. Checked first because none of what follows
        // applies to it: the track is already playing, already named and
        // already on screen, and the only thing being replaced is the URL the
        // audio comes from. Running it through the path below would restart the
        // song from the beginning under its own title.
        if self.resuming.as_ref().is_some_and(|r| r.id == id) {
            let from = self.resuming.take().expect("just matched").from;
            let cmd = match stream {
                Ok(stream) => {
                    self.status = format!("playing {title}");
                    Command::Resume {
                        url: stream.url,
                        format: stream.format,
                        from,
                    }
                }
                // Nothing left to try. Reported rather than dropped: the player
                // is parked in `Buffering` waiting for this, and silence here
                // would leave it there naming a track it is never going to
                // play another second of.
                Err(why) => {
                    let summary = self.record_playback_failure(&why);
                    Command::ResumeFailed { why: summary }
                }
            };
            let _ = self.player.send(cmd);
            return;
        }

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
                    format: stream.format,
                    title,
                    id: id.to_string(),
                });
                return;
            }
            Err(why) => why,
        };

        // A queue that cannot resolve its next track steps over it rather than
        // falling silent -- YouTube's radio offers plenty that this program
        // cannot play, and one of them must not end the session. Bounded, and
        // now that the queue pages itself there is no end to reach that would
        // stop it otherwise: see [`MAX_AUTO_SKIPS`].
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
        self.record_playback_failure(&why);
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
        // Minted before the borrow below, which holds the whole of `self`.
        // Unconditionally, because a number spent on a response that turns out
        // to carry no new queue costs nothing: all this has to do is differ
        // from the epoch of the queue it replaces.
        self.queue_epoch += 1;
        let epoch = self.queue_epoch;

        let Some(now) = self.for_track(video_id) else {
            return;
        };
        // Set whether or not it succeeded: this is the round trip the Lyrics
        // tab waits on, and what it does next depends only on there having
        // been one.
        now.watched = true;

        let watch = match watch {
            Ok(watch) => watch,
            Err(why) => {
                // No queue is not an error worth a status line: the track is
                // playing, and the panels say so for themselves. Related's
                // browse id came from this response, so it is never coming --
                // settled here rather than left to say "loading" for the rest
                // of the track.
                now.playing = now.queue.iter().position(|track| track.id == video_id);
                now.related = Panel::Empty(why.clone());
                // Lyrics are not settled with it: LRCLIB needs only the artist
                // and title, and both arrived with the track rather than in
                // this response. Left idle, the fetch starts on the next tick.
                if now.lyrics_query().is_none() {
                    now.lyrics = Panel::Empty(why);
                }
                return;
            }
        };

        now.lyrics_id = watch.lyrics_id;
        now.related_id = watch.related_id;
        if let Some((title, endpoint)) = watch.album_route {
            now.album = Some(title.clone());
            now.album_route = Some((title, endpoint));
        }

        match now.queue.iter().position(|track| track.id == video_id) {
            // The queue carried over, so this response describes a queue we are
            // already inside. Its continuation is deliberately not adopted: the
            // token already held pages the radio the user has been listening
            // to, and this one would page a fresh radio seeded on the track
            // that happens to be playing now.
            Some(index) => now.playing = Some(index),
            None => {
                now.playing = watch.queue.iter().position(|track| track.id == video_id);
                now.queue_title = watch.queue_title;
                now.queue = watch.queue;
                // A different queue, so everything the old one had learned
                // about paging itself is about somebody else's radio now.
                now.queue_epoch = epoch;
                now.continuation = watch.continuation;
                now.topping_up = false;
                now.topup_failures = 0;
                now.dropped.clear();
                // Follow the track that is playing rather than leaving the
                // cursor on whatever row a previous queue had under it.
                now.cursor[Tab::UpNext.index()] = now.playing.unwrap_or(0);
            }
        }

        // Cards can omit duration, while pasted URLs initially carry no useful
        // metadata at all. The queue has the canonical row for both.
        let canonical = now.hydrate_from_queue();

        // A tab YouTube did not offer is one that will never be fetched, so it
        // is settled here rather than left waiting. Lyrics are checked only
        // after queue hydration because pasted URLs learn the artist and title
        // from that row, which can make LRCLIB a valid fallback.
        if now.lyrics_id.is_none() && now.lyrics_query().is_none() {
            now.lyrics = Panel::Empty("no lyrics are published for this track".to_string());
        }
        if now.related_id.is_none() {
            now.related = Panel::Empty("nothing related came back for this track".to_string());
        }

        // Warm the track after this one while the current one plays. This is
        // the whole reason the queue is worth having early: an automatic
        // advance that hits the URL cache starts in milliseconds, where a cold
        // one spends seconds of silence between tracks.
        if let Some(next) = now.next_in_queue().map(|track| track.id.clone()) {
            let _ = self.source.send(Request::Prefetch { id: next });
        }

        // Listening history and recommendations must use the same canonical
        // metadata the player page now shows, not a pasted URL and blank artist.
        if let Some(canonical) = canonical
            && let Some((listening, _)) = self.listening.as_mut()
            && listening.id == canonical.id
        {
            *listening = canonical;
        }

        // The canonical row may have made an artist action available, and the
        // queue selection can have moved. A snapshot opened before that answer
        // must not invoke the old set of actions.
        self.close_page_actions();

        // A queue short enough to need topping up the moment it lands -- the
        // tail of a playlist, or a radio that answered with one page. Asking
        // here as well as on every play is what covers it: `play_track` runs
        // before this response and saw a queue that was still empty.
        self.top_up_queue();
    }

    /// Asks for the next page of the queue, if one is wanted.
    ///
    /// Called on every play and whenever a queue arrives, and cheap to call
    /// when there is nothing to do -- which is most of the time.
    ///
    /// Two ways to answer, tried in that order. YouTube's own continuation is
    /// always preferred: it is the station the user chose, and it is one round
    /// trip. Only once that has nothing left does the journal build a new
    /// station -- which is the difference between a queue that ends when the
    /// radio does and one that plays for as long as anybody wants it to.
    fn top_up_queue(&mut self) {
        let Some(now) = self.now.as_mut() else {
            return;
        };
        // Nothing is playing inside this queue, so nothing is walking towards
        // its end and a page appended here would be tracks nobody reaches.
        // `advance` declines to move for the same reason. This covers the
        // empty queue before the first watch response too, which must not be
        // seeded from the journal on top of the station already on its way.
        if now.playing.is_none() || now.repeat == RepeatMode::All {
            return;
        }
        // Still deep enough, already asking, or given up asking.
        if now.remaining() >= QUEUE_LOW
            || now.topping_up
            || now.topup_failures >= MAX_TOPUP_FAILURES
        {
            return;
        }

        let epoch = now.queue_epoch;
        let request = match now.continuation.clone() {
            Some(token) => Request::MoreQueue { epoch, token },
            None => {
                // Stepped after being read, so the first fallback of a session
                // is built from the best-scoring seed rather than the second.
                // Stepped per attempt rather than per queue, so a station that
                // turns out to be a dead end is not the one built again next
                // time -- which is why this is kept on `App`, where it outlives
                // any one queue.
                let rotation = self.seed_rotation;
                self.seed_rotation += 1;
                Request::SeedQueue { epoch, rotation }
            }
        };

        // Re-borrowed: minting the rotation above needed `self`, which ended
        // the borrow this was taken through.
        if let Some(now) = self.now.as_mut() {
            now.topping_up = true;
        }
        if self.source.send(request).is_err()
            && let Some(now) = self.now.as_mut()
        {
            // Nothing is coming, so the flag would otherwise stand for a
            // request in flight for the rest of the session and stop the queue
            // ever asking again.
            now.topping_up = false;
        }
    }

    /// Takes a page of tracks onto the end of the queue it was asked for.
    ///
    /// Nothing here reaches the status bar. The user did not ask for this and
    /// is listening to music; a queue that quietly stops growing ends exactly
    /// as it did before any of this existed, which is a queue that runs out.
    fn apply_more_queue(&mut self, epoch: u64, page: Result<QueuePage, String>) {
        // The queue that asked has since been replaced -- the user played
        // something from outside it. There is nothing to apply this to, and the
        // queue that took its place has its own token.
        let Some(now) = self.now.as_mut().filter(|now| now.queue_epoch == epoch) else {
            return;
        };
        now.topping_up = false;

        let page = match page {
            Ok(page) => page,
            Err(_) => {
                // Whatever was going to be tried is still going to be tried:
                // a token that failed is kept for the next play, and a journal
                // that could not build a station will be asked again for a
                // different one. One failure is a blip. A run of them is
                // YouTube declining to page this queue, or a journal with
                // nothing left to seed from, and [`MAX_TOPUP_FAILURES`] is what
                // stops either becoming a request per track for the session.
                now.topup_failures += 1;
                return;
            }
        };

        if now.take_page(page) == 0 {
            return;
        }

        // The queue may have been down to its last track when this landed, in
        // which case the track now after it has never been warmed -- and it is
        // about to be the one playing. Cheap when it was already prefetched:
        // the worker answers a warm id out of its cache.
        if let Some(next) = now.next_in_queue().map(|track| track.id.clone()) {
            let _ = self.source.send(Request::Prefetch { id: next });
        }

        // The queue ran dry before this page arrived: the last track ended, the
        // advance found nothing to move to, and the player has been sitting in
        // silence since. There is something to play now, so it starts -- which
        // is what makes a queue that ran out mid-session recover on its own
        // rather than needing the user to press a key.
        //
        // A stop is not this case and cannot reach here: it clears the page,
        // and there is nothing left for a continuation to be applied to. A
        // paused track is not it either -- `Paused` is its own state.
        if self.pending.is_none() && self.snapshot().state == PlayState::Idle {
            self.advance(1, true);
        }
    }

    /// Plays a track and normally opens the player page on it.
    ///
    /// The single path every play goes through -- a result row, a card, the
    /// queue, or the track that follows the one that just ended -- so that
    /// nothing can start playing without the page around it being replaced to
    /// match.
    ///
    /// `auto` marks a play the queue started rather than the user; only those
    /// are allowed to step over a track that will not resolve.
    fn play_track(&mut self, track: Track, auto: bool) {
        self.start_track(track, auto, false);
    }

    fn start_track(&mut self, track: Track, auto: bool, bypass_cache: bool) {
        // A page whose response can still replace its contents must remain live,
        // not be cloned into Player's return slot. Queue advancement is allowed
        // to continue behind it without changing the visible route.
        let keep_page =
            self.view != View::Playing && (self.mode == Mode::Editing || self.snapshot_blocked());
        // Automatic queue advances can happen while a contextual menu is open.
        // Its actions belong to the track it was opened over, not its successor.
        self.close_page_actions();
        // Before anything else touches the player: whatever was playing is over
        // as of this call, and this is the last moment its position is still
        // readable. Every play in the program funnels through here, so this
        // covers a skip, a queue advance and a fresh choice alike.
        self.finish_listening();
        self.listening = Some((track.clone(), Listening::new()));
        self.playback_error = None;
        self.auto = auto;
        // Whatever the previous track was waiting on, it is not waiting any
        // more. Left set, a recovery URL arriving for the song just left would
        // be applied to the one that replaced it.
        self.resuming = None;
        // Only from a list; the player page is not somewhere to go back to.
        if self.view != View::Playing && !keep_page {
            self.player_back = self.current_page();
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
        let repeat = self.now.as_ref().map_or(RepeatMode::Off, |now| now.repeat);
        let mut page = NowPlaying::new(&track);
        page.tab = tab;
        page.repeat = repeat;
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
            // Everything the queue knows about paging itself travels with it,
            // or the endless queue would end on the first track: the token is
            // what buys the next page, and the epoch is what proves a page that
            // arrives two tracks later still belongs here.
            page.queue_epoch = old.queue_epoch;
            page.continuation = old.continuation;
            page.topping_up = old.topping_up;
            page.topup_failures = old.topup_failures;
            page.dropped = old.dropped;
            // Now that the queue has moved forward, whatever has fallen out of
            // the window behind it can go. This is the only place the queue
            // grows a position, so it is the only place that has to shrink.
            if page.repeat != RepeatMode::All {
                page.trim();
            }
        }
        self.now = Some(page);
        if !keep_page {
            self.view = View::Playing;
        }

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
            // A track the user chose. The cache is exactly what should answer
            // it when it can -- that is the difference between pressing Enter
            // on a replay and waiting three seconds for yt-dlp again.
            bypass_cache,
        };
        if self.source.send(request).is_err() {
            self.pending = None;
            let _ = self.player.send(Command::Stop);
            self.playback_error = Some("playback service is unavailable".to_string());
            self.status = playback_failure_status("playback service is unavailable");
            return;
        }
        // Both behind the resolve, which is the order that matters: it is the
        // one producing audio, and these are decoration around it.
        self.request_rating();
        self.request_cover(track.id.clone());
        let _ = self.source.send(Request::Watch { video_id: track.id });
        // A tab the user is already on has to be fetched now; the switch that
        // would otherwise have done it is not going to happen again.
        self.request_tab();
        // Every play moves the queue forward by one, so every play is a place
        // the tail can have got short enough to want another page. This is the
        // one funnel they all pass through, which is why the check lives here
        // rather than beside the advance that started most of them.
        self.top_up_queue();
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
            // Waits on the watch response rather than on the browse id it
            // carries: a track with no lyrics page is not a track with no
            // lyrics, it is the one LRCLIB is there to answer for. Until the
            // response lands there is no telling which of the two this is, so
            // the panel is left idle and `tick_page` opens it again.
            Tab::Lyrics if now.lyrics.is_idle() => {
                if !now.watched {
                    return;
                }
                now.lyrics = Panel::Loading;
                Request::Lyrics {
                    video_id,
                    browse_id: now.lyrics_id.clone(),
                    query: now.lyrics_query(),
                }
            }
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
            Tab::Lyrics => now.lyrics.is_idle() && now.watched,
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
        if self.playback_error.is_none()
            && self.now.is_some()
            && self.pending.is_none()
            && snap.state == PlayState::Idle
            && let Some(error) = snap.error.as_deref()
        {
            self.record_playback_failure(error);
        }
        let ended = self.last_state == PlayState::Playing
            && snap.state == PlayState::Idle
            && snap.error.is_none();
        self.last_state = snap.state;

        // Sampled every frame rather than read once at the end, because at the
        // end there is nothing to read: an ended track reports position zero.
        if let Some((track, listening)) = self.listening.as_mut() {
            listening.observe(snap.position, snap.state == PlayState::Playing, Instant::now());
            if !listening.reported && listening.heard >= Duration::from_secs(30)
                && journal::record_pending(listening.report(&track.id))
            {
                listening.reported = true;
                listening.checkpoint_sent();
                let _ = self.source.send(Request::RetryReports);
            }
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

    /// Offers Discord the card that should be showing. Called once per frame in
    /// both faces of the program -- the card is the one thing about a
    /// backgrounded player that other people can see, so it must not stop
    /// updating when the window goes away.
    ///
    /// Cheap on every tick that changes nothing, which is nearly all of them:
    /// [`Presence::publish`] compares before it sends, and nothing here touches
    /// a socket.
    pub fn tick_presence(&mut self) {
        let activity = self.activity();
        self.presence.publish(activity);
    }

    /// What the Discord card should say right now, or `None` for no card.
    ///
    /// Built fresh each tick rather than at the points that change it. There
    /// are a dozen of those -- a play, a stop, a pause, a seek, a queue
    /// advance, a stream recovered onto a new URL -- and one of them being
    /// forgotten would leave a stale track on the user's profile long after
    /// they had stopped listening to it. Deriving from the same snapshot the
    /// status bar draws from makes that class of bug unrepresentable.
    fn activity(&self) -> Option<Activity> {
        let now = self.now.as_ref()?;
        let snap = self.snapshot();
        // Idle is a track that ended or was stopped, and there is nothing to
        // say about either. `now` outlives it by a frame or two while the queue
        // works out what is next, which is exactly the window this closes.
        if snap.state == PlayState::Idle {
            return None;
        }

        let paused = snap.state == PlayState::Paused;
        // No clock while paused -- Discord cannot hold a bar still, see
        // `Activity::clock` -- and none while buffering either, where the
        // position is zero and a bar would jump backwards the moment audio
        // started.
        let clock = (!paused && snap.state != PlayState::Buffering).then(|| {
            let start_ms =
                crate::discord::now_ms().saturating_sub(snap.position.as_millis() as u64);
            Clock {
                start_ms,
                // A livestream has no length, so it gets a clock that counts up
                // rather than one that counts down to an end it will not reach.
                end_ms: now.duration.map(|d| start_ms + d.as_millis() as u64),
            }
        });

        Some(Activity {
            video_id: now.video_id.clone(),
            title: now.title.clone(),
            // Dropped rather than shown as "unknown", on the same rule as
            // [`Track::label`]: a source that could not name an artist has not
            // named one, and a blank line reads better on a public profile than
            // a confident wrong answer.
            artist: match now.artist.as_str() {
                UNKNOWN_ARTIST => String::new(),
                artist => artist.to_string(),
            },
            album: now.album.clone(),
            paused,
            clock,
        })
    }

    /// Turns the Discord card on or off, and remembers which.
    ///
    /// Written to disk rather than kept for the session: this decides whether
    /// what someone listens to is visible to everyone they share a server with,
    /// and a privacy switch that quietly flips back on at the next launch is
    /// not one.
    fn toggle_presence(&mut self) {
        let enabled = self.presence.toggle();
        self.status = self.presence.status();
        // Best effort. A config directory that cannot be written costs the user
        // the setting surviving a restart, which is not worth refusing the
        // keypress over -- and the status line has already said where it landed.
        if let Err(error) = config::Presence::save(enabled) {
            crate::diagnostics::error("config", &format!("could not save presence: {error:#}"));
            self.status = "could not save Discord preference; changed for this session only".to_string();
        }
    }

    fn toggle_start_in_tray(&mut self) {
        if !cfg!(windows) {
            self.status = "start in tray is only available on Windows".to_string();
            return;
        }
        let enabled = !self.start_in_tray;
        let settings = config::Settings {
            start_in_tray: enabled,
            icon_theme: self.icon_theme,
            cover_style: self.cover_style,
            image_renderer: self.image_renderer,
            volume: self.snapshot().volume,
            output_device: self.output_device.clone(),
        };
        match settings.save() {
            Ok(()) => {
                self.start_in_tray = enabled;
                self.status = if enabled {
                    "MTUI will keep a notification-area icon".to_string()
                } else {
                    "MTUI will show a tray icon only while backgrounded".to_string()
                };
            }
            Err(err) => {
                crate::diagnostics::error("config", &format!("could not save settings: {err:#}"));
                self.status = format!("could not save settings: {err:#}");
            }
        }
    }

    fn cycle_icon_theme(&mut self, forward: bool) {
        let theme = if forward {
            self.icon_theme.next()
        } else {
            self.icon_theme.previous()
        };
        self.set_icon_theme(theme);
    }

    fn set_icon_theme(&mut self, theme: IconTheme) {
        let settings = config::Settings {
            start_in_tray: self.start_in_tray,
            icon_theme: theme,
            cover_style: self.cover_style,
            image_renderer: self.image_renderer,
            volume: self.snapshot().volume,
            output_device: self.output_device.clone(),
        };
        match settings.save() {
            Ok(()) => {
                self.icon_theme = theme;
                self.status = format!("app icon set to {}", theme.label());
            }
            Err(err) => {
                crate::diagnostics::error("config", &format!("could not save settings: {err:#}"));
                self.status = format!("could not save settings: {err:#}");
            }
        }
    }

    fn cycle_cover_style(&mut self, forward: bool) {
        let style = if forward {
            self.cover_style.next()
        } else {
            self.cover_style.previous()
        };
        self.set_cover_style(style);
    }

    fn set_cover_style(&mut self, style: CoverStyle) {
        let settings = config::Settings {
            start_in_tray: self.start_in_tray,
            icon_theme: self.icon_theme,
            cover_style: style,
            image_renderer: self.image_renderer,
            volume: self.snapshot().volume,
            output_device: self.output_device.clone(),
        };
        match settings.save() {
            Ok(()) => {
                self.cover_style = style;
                self.images.clear();
                self.status = format!("song covers set to {}", style.label());
            }
            Err(err) => {
                crate::diagnostics::error("config", &format!("could not save settings: {err:#}"));
                self.status = format!("could not save settings: {err:#}");
            }
        }
    }

    fn cycle_image_renderer(&mut self, forward: bool) {
        let renderer = if forward {
            self.image_renderer.next()
        } else {
            self.image_renderer.previous()
        };
        self.set_image_renderer(renderer);
    }

    fn set_image_renderer(&mut self, renderer: ImageRenderer) {
        let settings = config::Settings {
            start_in_tray: self.start_in_tray,
            icon_theme: self.icon_theme,
            cover_style: self.cover_style,
            image_renderer: renderer,
            volume: self.snapshot().volume,
            output_device: self.output_device.clone(),
        };
        match settings.save() {
            Ok(()) => {
                self.image_renderer = renderer;
                self.images.clear();
                self.status = format!("image renderer set to {}", renderer.label());
            }
            Err(err) => {
                crate::diagnostics::error("config", &format!("could not save settings: {err:#}"));
                self.status = format!("could not save settings: {err:#}");
            }
        }
    }

    fn refresh_output_devices(&mut self) {
        match crate::player::available_output_devices() {
            Ok(devices) => self.output_devices = devices,
            Err(error) => crate::diagnostics::warn(
                "player",
                &format!("could not refresh audio outputs: {error:#}"),
            ),
        }
    }

    fn cycle_audio_output(&mut self, forward: bool) {
        self.refresh_output_devices();
        let next = next_output_device(self.output_device.as_deref(), &self.output_devices, forward);
        self.set_audio_output(next);
    }

    fn set_audio_output(&mut self, next: Option<String>) {
        if next.as_deref() == self.output_device.as_deref() {
            self.status = "this audio output is already selected".to_string();
            return;
        }
        let label = next
            .as_deref()
            .and_then(|id| self.output_devices.iter().find(|output| output.id == id))
            .map(|output| output.name.as_str())
            .unwrap_or("System default")
            .to_string();
        match self.player.send(Command::SetOutput(next)) {
            Ok(()) => self.status = format!("switching audio output to {label} ..."),
            Err(error) => self.status = format!("could not switch audio output: {error:#}"),
        }
    }

    fn toggle_pause(&mut self) {
        let _ = self.player.send(Command::TogglePause);
    }

    fn retry_current_track(&mut self) {
        let snapshot = self.snapshot();
        if !self.playback_failed(&snapshot) {
            return;
        }
        let Some(track) = self.listening.as_ref().map(|(track, _)| track.clone()) else {
            return;
        };
        self.start_track(track, false, true);
    }

    fn toggle_cover_size(&mut self) {
        self.cover_size = self.cover_size.toggled();
    }

    fn remove_queue_selection(&mut self) {
        let Some(removed) = self
            .now
            .as_mut()
            .and_then(NowPlaying::remove_selected_upcoming)
        else {
            return;
        };
        self.status = format!("removed {} from the queue", removed.label());
    }

    fn move_queue_selection(&mut self, delta: isize) {
        let moved = self
            .now
            .as_mut()
            .is_some_and(|now| now.move_selected_upcoming(delta));
        if !moved {
            return;
        }
        self.prefetch_queue_next();
        self.status = if delta < 0 {
            "moved queue track up".to_string()
        } else {
            "moved queue track down".to_string()
        };
    }

    fn clear_upcoming_queue(&mut self) {
        let removed = self.now.as_mut().map_or(0, NowPlaying::clear_upcoming);
        if removed == 0 {
            return;
        }
        // A continuation already in flight belongs to the queue before the
        // explicit clear. Minting a new epoch makes its late answer harmless.
        self.queue_epoch = self.queue_epoch.wrapping_add(1);
        if let Some(now) = self.now.as_mut() {
            now.queue_epoch = self.queue_epoch;
            now.continuation = None;
            now.topping_up = false;
            now.topup_failures = MAX_TOPUP_FAILURES;
        }
        self.status = format!("cleared {removed} upcoming tracks");
    }

    fn shuffle_upcoming_queue(&mut self) {
        let shuffled = self.now.as_mut().map_or(0, NowPlaying::shuffle_upcoming);
        if shuffled == 0 {
            return;
        }
        self.prefetch_queue_next();
        self.status = format!("shuffled {shuffled} upcoming tracks");
    }

    fn prefetch_queue_next(&self) {
        let next = self
            .now
            .as_ref()
            .and_then(NowPlaying::next_in_queue)
            .map(|track| track.id.clone());
        if let Some(id) = next {
            let _ = self.source.send(Request::Prefetch { id });
        }
    }

    fn cycle_repeat(&mut self) {
        let Some(repeat) = self.now.as_ref().map(|now| now.repeat.next()) else {
            return;
        };
        if repeat == RepeatMode::All {
            // Freeze this bounded window. A continuation already on its way is
            // invalidated by the new epoch; its token remains available if
            // repeat is later switched off.
            self.queue_epoch = self.queue_epoch.wrapping_add(1);
        }
        if let Some(now) = self.now.as_mut() {
            now.repeat = repeat;
            if repeat == RepeatMode::All {
                now.queue_epoch = self.queue_epoch;
                now.topping_up = false;
            } else if now.topup_failures >= MAX_TOPUP_FAILURES {
                now.topup_failures = 0;
            }
        }
        self.status = format!("repeat {}", repeat.label());
        if repeat != RepeatMode::All {
            self.top_up_queue();
        }
    }

    /// Moves `delta` tracks through the queue and plays what it lands on.
    ///
    /// Silent at either end: there is nothing before the first track, and a
    /// queue with nothing after the last one is not a failure either. Reaching
    /// that end is now rare rather than routine -- the queue tops itself up
    /// several tracks before it gets there -- so the two cases left are a queue
    /// still waiting on a page and one that has genuinely nothing left to
    /// offer, which say different things and are told apart below.
    ///
    /// `auto` distinguishes the queue advancing itself from `n` and `p`. Only
    /// the former steps over a track that will not resolve -- a user who
    /// pressed a key is owed the reason it did nothing.
    fn advance(&mut self, delta: isize, auto: bool) {
        let Some(now) = self.now.as_ref() else {
            return;
        };
        let Some(index) = now.advanced_index(delta, auto) else {
            if delta > 0 {
                // A page is on its way, so this is not the end -- only the gap
                // between running out and hearing back. `apply_more_queue`
                // starts playing again when it lands, so the user is told to
                // expect that rather than told the session is over.
                self.status = if now.topping_up {
                    "waiting for more of the queue ...".to_string()
                } else {
                    "end of the queue".to_string()
                };
            }
            return;
        };
        let Some(track) = now.queue.get(index).cloned() else {
            return;
        };

        self.play_track(track, auto);
    }

    /// Shows a failure wherever it will actually be read: the status bar for a
    /// one-liner, an overlay for anything with instructions in it.
    fn report(&mut self, msg: String) {
        crate::diagnostics::error("source", &msg);
        if msg.contains('\n') {
            self.status = "press Esc to dismiss".to_string();
            self.menu = None;
            self.overlay = Overlay::Message { body: msg };
        } else {
            self.status = msg;
        }
    }

    fn record_playback_failure(&mut self, why: &str) -> String {
        crate::diagnostics::error("playback", why);
        let summary = playback_failure_summary(why).to_string();
        self.playback_error = Some(summary.clone());
        self.status = playback_failure_status(&summary);
        summary
    }

    /// Hands the finished play to the worker, which journals it and -- with a
    /// cookie saved -- reports it to YouTube.
    ///
    /// Idempotent by construction: the track is taken, so the several paths that
    /// can end a play (the queue advancing, a new choice, or a stop) may
    /// all call this and only the first does anything.
    ///
    /// Nothing is reported for a track that never produced sound, which is what
    /// keeps a resolve that failed out of the history as a play that happened.
    fn finish_listening(&mut self) {
        let Some((track, listening)) = self.listening.take() else {
            return;
        };
        if listening.heard.is_zero() {
            return;
        }
        journal::record_final(&track, listening.report(&track.id));
        let _ = self.source.send(Request::RetryReports);
    }

    /// Writes the play in progress straight to the journal, for the way out.
    ///
    /// The durable append uses the same nonce as the in-song checkpoint.
    /// Network delivery resumes on the next launch without delaying shutdown.
    pub fn flush_listening(&mut self) {
        let Some((track, listening)) = self.listening.take() else {
            return;
        };
        if !listening.heard.is_zero() {
            journal::record_final(&track, listening.report(&track.id));
        }
    }

    /// Asks for the landing page.
    ///
    /// Not `busy`: this runs at launch, and a busy flag set before the first
    /// frame would hold off the prefetch and spin the event loop at the faster
    /// tick for as long as YouTube took to answer. [`Self::home_pending`] is
    /// what stands in for it, and says only what the pane needs -- the
    /// difference between "loading" and "there is no feed".
    fn request_home(&mut self) {
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

    fn refresh_home(&mut self) {
        if self.home_pending {
            return;
        }
        self.status = "refreshing the home feed ...".to_string();
        self.request_home();
    }

    fn finish_home_attempts(&mut self) {
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

    fn apply_home(&mut self, shelves: Vec<Shelf>) {
        let focused = self.home.get(self.home_shelf).and_then(|shelf| {
            shelf
                .cards
                .get(self.home_card)
                .map(|card| (shelf.title.clone(), card.art_key().to_string()))
        });
        self.status = if shelves.is_empty() {
            "nothing came back from YouTube Music".to_string()
        } else {
            "Enter to play, / to search".to_string()
        };
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
        self.selection_settled = Some(Instant::now());
    }

    /// Asks for the artwork of the cards the renderer has just drawn.
    ///
    /// Driven from the render pass rather than from the feed arriving, because
    /// "which cards are on screen" is a fact about the window and the scroll
    /// position, and both are things only the renderer knows. The cache filters
    /// out everything already held or already asked for, so this is a no-op on
    /// all but the first frame after the view moves.
    pub fn want_art(&mut self, cards: Vec<(String, Option<String>)>) {
        self.source.visible_art(cards.iter().filter(|(_,url)|url.is_some()).map(|(key,_)|key.clone()));
        let mut urls: std::collections::HashMap<_, _> = cards.iter()
            .filter_map(|(key, url)| Some((key.clone(), url.clone()?))).collect();
        let requests = self.art.want_visible(cards.iter()
            .filter(|(_, url)| url.is_some()).map(|(key, _)| key.as_str()));
        for key in requests {
            if let Some(url) = urls.remove(&key) {
                let _ = self.source.send(Request::Art { key, url });
            }
        }
    }

    /// Speculatively resolves the selected track once the selection has sat
    /// still for [`PREFETCH_IDLE`]. Called once per frame.
    ///
    /// This removes the pooled player-API round trip from Enter when possible.
    /// Difficult tracks are deliberately not sent through yt-dlp here: a child
    /// process already running cannot be superseded by a later selection.
    pub fn tick_prefetch(&mut self) {
        // Never compete for the serial worker with a request the user is
        // waiting on, and never stack two speculative resolves.
        if self.awaiting() || self.prefetching.is_some() {
            return;
        }
        // Nothing playable is under the cursor while a modal has it, so there
        // is nothing worth warming.
        if self.overlay.is_open() || self.menu.is_some() {
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
        if self
            .source
            .send(Request::Prefetch { id: id.clone() })
            .is_ok()
        {
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
            View::Tracks => Some(self.selected_result_track()?.id.clone()),
            View::Home => match &self.home_card()?.target {
                Target::Play { video_id } => Some(video_id.clone()),
                // Opening one is a round trip of its own, and nothing about
                // which track it lands on is knowable from here.
                Target::Open { .. } | Target::Artist { .. } => None,
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
            View::Artist => {
                let artist = self.artist.as_ref()?;
                artist
                    .selected_track()
                    .map(|track| track.id.clone())
                    .or_else(|| {
                        artist.selected_card().and_then(|card| match &card.target {
                            Target::Play { video_id } => Some(video_id.clone()),
                            Target::Open { .. } | Target::Artist { .. } => None,
                        })
                    })
            }
        }
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

    fn open_menu(&mut self, page: MenuPage) {
        if !self.overlay.is_open() {
            let items = self.items_for_menu(page);
            self.menu = Some(Menu::new(page, items));
        }
    }

    fn items_for_menu(&self, page: MenuPage) -> Vec<MenuItem> {
        match page {
            MenuPage::Root => self.root_menu_items(),
            MenuPage::Account => self.account_menu_items(),
            MenuPage::Help => self.help_menu_items(),
            MenuPage::PageActions => self.page_action_items(),
            MenuPage::PlayerActions => self.player_action_items(),
        }
    }

    fn close_page_actions(&mut self) {
        if matches!(
            self.menu.as_ref().map(|menu| menu.page),
            Some(MenuPage::PageActions)
        ) {
            self.menu = None;
        }
    }

    fn toggle_root_menu(&mut self) {
        match self.menu.as_ref().map(|menu| menu.page) {
            Some(MenuPage::Root) => self.menu = None,
            Some(_) => self.open_menu(MenuPage::Root),
            None => self.open_menu(MenuPage::Root),
        }
    }

    fn handle_menu_key(&mut self, key: KeyEvent) -> Result<()> {
        let len = self.menu_items().len();
        match key.code {
            KeyCode::Char('j') | KeyCode::Down | KeyCode::Tab => {
                if let Some(menu) = self.menu.as_mut() {
                    menu.move_by(1, len);
                }
            }
            KeyCode::Char('k') | KeyCode::Up | KeyCode::BackTab => {
                if let Some(menu) = self.menu.as_mut() {
                    menu.move_by(-1, len);
                }
            }
            KeyCode::Char('g') | KeyCode::Home => {
                if let Some(menu) = self.menu.as_mut() {
                    menu.first();
                }
            }
            KeyCode::Char('G') | KeyCode::End => {
                if let Some(menu) = self.menu.as_mut() {
                    menu.last(len);
                }
            }
            KeyCode::Enter => self.invoke_menu_item(),
            KeyCode::Esc | KeyCode::Char('q') => {
                if matches!(
                    self.menu.as_ref().map(|menu| menu.page),
                    Some(MenuPage::Account | MenuPage::Help)
                ) {
                    self.open_menu(MenuPage::Root);
                } else {
                    self.menu = None;
                }
            }
            _ => {
                let selected = self.menu.as_ref().and_then(|menu| menu.shortcut_index(key));
                if let Some(selected) = selected {
                    if let Some(menu) = self.menu.as_mut() { menu.selected = selected; }
                    self.invoke_menu_item();
                }
            }
        }
        Ok(())
    }

    fn invoke_menu_item(&mut self) {
        let items = self.menu_items();
        let Some(menu) = self.menu.as_mut() else {
            return;
        };
        menu.clamp(items.len());
        let Some(item) = items.get(menu.selected) else {
            return;
        };
        if !item.enabled {
            return;
        }
        let Some(action) = item.action else {
            return;
        };

        self.menu = None;
        match action {
            MenuAction::GoHome => {
                self.go_home();
            }
            MenuAction::BeginSearch => self.begin_search(),
            MenuAction::OpenPlayer => self.open_player(),
            MenuAction::OpenAccount => self.open_menu(MenuPage::Account),
            MenuAction::OpenSettings => {
                self.open_settings();
                self.preferences.return_to_menu = true;
            }
            MenuAction::OpenHelp => self.open_menu(MenuPage::Help),
            #[cfg(windows)]
            MenuAction::Background => self.background(),
            MenuAction::Quit => self.should_quit = true,
            MenuAction::ConnectMusic => self.begin_music_sign_in(false),
            MenuAction::LogOutMusic => self.log_out_music(),
            MenuAction::OpenHomeSelection => self.open_card(),
            MenuAction::StartHomeRadio => self.start_home_radio(),
            MenuAction::PlayHomeNext => self.queue_home_track(true),
            MenuAction::QueueHomeTrack => self.queue_home_track(false),
            MenuAction::PlayHomeShelf => self.play_home_shelf(false),
            MenuAction::ShuffleHomeShelf => self.play_home_shelf(true),
            MenuAction::RefreshHome => self.refresh_home(),
            MenuAction::PlaySelected => self.play_selected(),
            MenuAction::OpenArtist => self.open_context_artist(),
            MenuAction::OpenArtistSelection => self.open_artist_selection(),
            MenuAction::ReloadArtist => self.reload_artist(),
            MenuAction::OpenPageSelection => self.open_page_row(),
            MenuAction::RemoveQueueSelection => self.remove_queue_selection(),
            MenuAction::MoveQueueSelectionUp => self.move_queue_selection(-1),
            MenuAction::MoveQueueSelectionDown => self.move_queue_selection(1),
            MenuAction::ClearUpcomingQueue => self.clear_upcoming_queue(),
            MenuAction::ShuffleUpcomingQueue => self.shuffle_upcoming_queue(),
            MenuAction::CycleRepeat => self.cycle_repeat(),
            MenuAction::FollowLyrics => {
                if let Some(now) = self.now.as_mut() {
                    now.open(Tab::Lyrics);
                }
            }
            MenuAction::TogglePause => self.toggle_pause(),
            MenuAction::ToggleMute => self.toggle_mute(),
            MenuAction::RetryTrack => self.retry_current_track(),
            MenuAction::Next => self.advance(1, false),
            MenuAction::Previous => self.advance(-1, false),
            MenuAction::Stop => self.stop(),
            MenuAction::ToggleCoverSize => self.toggle_cover_size(),
            MenuAction::LikePlaying => self.like_playing(),
            MenuAction::SharePlaying => self.share_playing(),
            MenuAction::SavePlaying => self.save_playing(),
            MenuAction::ChooseOutput => self.choose_output(),
        }
    }

    fn open_settings(&mut self) {
        let return_to_menu = self.menu.is_some();
        self.menu = None;
        self.refresh_output_devices();
        self.preferences.open(return_to_menu);
        self.overlay = Overlay::Settings;
    }

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

        if self.menu.is_some() {
            return self.handle_menu_key(key);
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

        match self.mode {
            Mode::Editing => self.handle_editing_key(key),
            Mode::Browse => match self.view {
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
        match action {
            MouseAction::GoHome => self.go_home(),
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
                    self.selection_settled = Some(Instant::now());
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
                    self.selection_settled = Some(Instant::now());
                }
            }
            MouseAction::PlayTrack(index) if self.view == View::Tracks => {
                if index < self.result_count() {
                    self.selected = index;
                    self.selection_settled = Some(Instant::now());
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
                    self.selection_settled = Some(Instant::now());
                }
            }
            MouseAction::OpenTab(tab) if self.view == View::Playing => {
                if self.now.as_mut().is_some_and(|now| now.open(tab)) {
                    self.request_tab();
                }
            }
            MouseAction::OpenPageRow(index) if self.view == View::Playing => {
                if let Some(now) = self.now.as_mut() {
                    *now.cursor_mut() = index;
                }
                self.open_page_row();
            }
            MouseAction::SelectPageRow(index) if self.view == View::Playing => {
                if let Some(now) = self.now.as_mut() {
                    *now.cursor_mut() = index;
                }
            }
            MouseAction::TogglePlayback => self.toggle_pause(),
            MouseAction::PreviousTrack => self.advance(-1, false),
            MouseAction::NextTrack => self.advance(1, false),
            MouseAction::SeekTo(position) => self.seek_to_fraction(position),
            MouseAction::SetVolume(position) => {
                self.set_volume(pointer_fraction(position) as f32)
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

    fn handle_overlay_key(&mut self, key: KeyEvent) -> Result<()> {
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
            self.selection_settled = Some(Instant::now());
        }
        Ok(())
    }

    /// Enter on a card: play it, or open what it stands for.
    fn open_card(&mut self) {
        let Some(card) = self.home_card().cloned() else {
            return;
        };
        self.activate_card(card);
    }

    /// Starts the ordinary endless station for the selected song. This shares
    /// the same watch request as a normal play; the separate action is an
    /// explicit promise about what will follow the seed, not another fetch or
    /// another queue held in memory.
    fn start_home_radio(&mut self) {
        let Some(track) = self.home_card().and_then(Card::track) else {
            return;
        };
        self.play_track(track, false);
    }

    /// Inserts the selected song into the active queue. The queue owns the
    /// bound and evicts only its farthest-ahead recommendation when full, so
    /// repeated actions cannot turn a long session into a growing heap.
    fn queue_home_track(&mut self, next: bool) {
        let Some(track) = self.home_card().and_then(Card::track) else {
            return;
        };
        let label = track.label();
        let Some(now) = self.now.as_mut().filter(|now| now.playing.is_some()) else {
            self.status = "wait for Up next to finish loading".to_string();
            return;
        };
        if !now.insert_user_track(track.clone(), next) {
            self.status = format!("{} is already in the queue", track.title);
            return;
        }

        if next {
            let _ = self.source.send(Request::Prefetch {
                id: track.id.clone(),
            });
            self.status = format!("playing {label} next");
        } else {
            self.status = format!("added {label} to the queue");
        }
    }

    /// Plays the playable cards in the current shelf as one finite queue. The
    /// cards already hold all metadata needed here; no collection page is
    /// fetched, and the queue is capped at the same fixed ahead window as a
    /// radio page.
    fn play_home_shelf(&mut self, shuffle: bool) {
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
        self.status = if shuffle {
            format!("shuffling {title}")
        } else {
            format!("playing {title}")
        };
    }

    fn activate_card(&mut self, card: Card) {
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

    fn open_browse(&mut self, endpoint: BrowseEndpoint, title: String) {
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
        self.status = format!("opening {title} ...");
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

    fn open_artist(&mut self, artist: ArtistRef) {
        if self.snapshot_blocked() {
            self.status = "wait for the current page to finish loading".to_string();
            return;
        }
        self.push_current_page();
        self.status = format!("opening {} ...", artist.name);
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

    fn reload_artist(&mut self) {
        if self.snapshot_blocked() {
            self.status = "wait for the current artist to finish loading".to_string();
            return;
        }
        let Some(artist) = self.artist.as_ref().map(|page| page.requested.clone()) else {
            return;
        };
        self.status = format!("refreshing {} ...", artist.name);
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

    fn context_artist(&self) -> Option<ArtistRef> {
        match self.view {
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

    fn open_context_artist(&mut self) {
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

    fn open_artist_selection(&mut self) {
        let Some(artist) = self.artist.as_ref() else {
            return;
        };
        if let Some(track) = artist.selected_song().map(|song| song.track.clone()) {
            self.play_track(track, false);
        } else if let Some(card) = artist.selected_card().cloned() {
            self.activate_card(card);
        }
    }

    fn handle_artist_key(&mut self, key: KeyEvent) -> Result<()> {
        let before = self
            .artist
            .as_ref()
            .map(|artist| (artist.section, artist.song, artist.card));

        match key.code {
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
            self.selection_settled = Some(Instant::now());
        }
        Ok(())
    }

    fn artist_first(&mut self) {
        if let Some(artist) = self.artist.as_mut() {
            artist.section = 0;
            artist.song = 0;
            artist.card = 0;
        }
    }

    fn artist_last(&mut self) {
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

    fn move_artist_vertical(&mut self, delta: isize) {
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

    fn move_artist_card(&mut self, delta: isize) {
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
            KeyCode::Char('g') | KeyCode::Home => self.jump_page(0),
            // No length is known here for the text panels, so this asks for a
            // line no panel has and lets the renderer -- which knows how long
            // each of them is -- clamp it to the last one.
            KeyCode::Char('G') | KeyCode::End => self.jump_page(usize::MAX),
            KeyCode::Enter => self.open_page_row(),
            KeyCode::Char('d') if tab == Some(Tab::UpNext) => self.remove_queue_selection(),
            KeyCode::Char('K') if tab == Some(Tab::UpNext) => self.move_queue_selection(-1),
            KeyCode::Char('J') if tab == Some(Tab::UpNext) => self.move_queue_selection(1),
            KeyCode::Char('C') if tab == Some(Tab::UpNext) => self.clear_upcoming_queue(),
            KeyCode::Char('z') if tab == Some(Tab::UpNext) => self.shuffle_upcoming_queue(),
            KeyCode::Char('R') => self.cycle_repeat(),
            KeyCode::Char('r') if self.playback_failed(&self.snapshot()) => {
                self.retry_current_track()
            }
            KeyCode::Char('n') => self.advance(1, false),
            KeyCode::Char('p') => self.advance(-1, false),
            KeyCode::Char(' ') => self.toggle_pause(),
            KeyCode::Char('m') => self.toggle_mute(),
            KeyCode::Char('s') => self.stop(),
            KeyCode::Char('c') => self.toggle_cover_size(),
            KeyCode::Char('+') | KeyCode::Char('=') => self.nudge_volume(VOLUME_STEP),
            KeyCode::Char('-') | KeyCode::Char('_') => self.nudge_volume(-VOLUME_STEP),
            KeyCode::Right => self.seek_relative(5),
            KeyCode::Left => self.seek_relative(-5),
            KeyCode::Char('M') => self.begin_music_sign_in(false),
            KeyCode::Char('B') => self.background(),
            KeyCode::Char('D') => self.toggle_presence(),
            KeyCode::Char('S') => self.open_settings(),
            KeyCode::Char('/') | KeyCode::Char('i') => self.begin_search(),
            // Back to the list the track was started from. The music keeps
            // playing -- leaving the page is not stopping it, and `P` brings it
            // back.
            KeyCode::Esc => self.return_from_player(),
            KeyCode::Char('H') => self.go_home(),
            _ => {}
        }

        // Applied in one place rather than in each arm above, so that every way
        // of changing tabs also starts the fetch behind the new one.
        if let (Some(now), Some(tab)) = (self.now.as_mut(), tab)
            && now.open(tab)
        {
            self.request_tab();
        }
        Ok(())
    }

    /// Moves the cursor of the open tab, when there is a page to move it on.
    fn scroll_page(&mut self, delta: isize) {
        if let Some(now) = self.now.as_mut() {
            now.scroll(delta);
        }
    }

    /// [`Self::scroll_page`] straight to a line, for `g` and `G`.
    fn jump_page(&mut self, to: usize) {
        if let Some(now) = self.now.as_mut() {
            now.jump(to);
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
                self.activate_card((*card).clone());
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
        self.now = None;
        self.auto = false;
        // A resolve may still be in flight; without this it would start playing
        // moments after the user asked for silence. A recovery in flight is the
        // same hazard by the other route.
        self.pending = None;
        self.resuming = None;
        self.playback_error = None;
        // Nothing is playing, so nothing owns the cover pane.
        self.cover = None;
        self.cover_id = None;
        if self.view == View::Playing {
            self.return_from_player();
        }
        self.status = "stopped".to_string();
    }

    fn begin_search(&mut self) {
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
        self.view = View::Tracks;
        self.mode = Mode::Editing;
        self.status = "type to search, Enter to run".to_string();
    }

    fn handle_editing_key(&mut self, key: KeyEvent) -> Result<()> {
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
                self.status = "/ to search, H for Home".to_string();
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
            self.selection_settled = Some(Instant::now());
        }
        Ok(())
    }

    fn move_selection(&mut self, delta: isize) {
        if self.result_count() == 0 {
            return;
        }
        let last = self.result_count() - 1;
        let next = self.selected as isize + delta;
        self.selected = next.clamp(0, last as isize) as usize;
    }

    fn submit_search(&mut self) {
        let query = self.query.trim().to_string();
        if query.is_empty() {
            self.status = "enter something to search for".to_string();
            return;
        }
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

        self.status = format!("searching for {query} ...");
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

    fn selected_result_track(&self) -> Option<&Track> {
        if self.browsing.is_none() && !self.search_items.is_empty() {
            self.search_items.get(self.selected)?.track.as_ref()
        } else { self.results.get(self.selected) }
    }

    fn filter_search(&mut self, filter: crate::source::search::Filter) {
        if self.busy || self.query.trim().is_empty() { return; }
        self.search_filter = filter;
        self.status = format!("Searching {}…", filter.label());
        let request_id = self.begin_page_request(PageRequestKind::Search);
        if self.source.send(Request::Search {
            request_id, query: self.query.clone(), limit: 60.min(MAX_RESULTS), filter,
        }).is_err() { self.cancel_page_request(); }
    }

    fn play_selected(&mut self) {
        if self.browsing.is_some() {
            self.play_collection(self.selected, false);
            return;
        }
        if let Some(item) = self.search_items.get(self.selected).cloned() {
            if let Some(track) = item.track {
                self.play_track(track, false);
            } else {
                self.activate_card(item.card);
            }
            return;
        }
        // Resolving takes seconds and spawns yt-dlp, so it goes to the worker
        // and playback starts when the response arrives -- unless a prefetch
        // already warmed the cache, in which case the round trip is all that is
        // left and saying "resolving" would be a lie the user can see through.
        if let Some(track) = self.results.get(self.selected).cloned() {
            self.play_track(track, false);
        }
    }

    fn reload_collection(&mut self) {
        if self.busy { return; }
        let Some(endpoint) = self.browsing_endpoint.clone() else { return; };
        let title = self.browsing.clone().unwrap_or_default();
        self.collection_error = None;
        self.status = format!("opening {title} ...");
        let request_id = self.begin_page_request(PageRequestKind::Browse);
        if self.source.send(Request::OpenBrowse { request_id, endpoint, title }).is_err() {
            self.cancel_page_request();
            self.collection_error = Some("source worker is not running".to_owned());
        }
    }

    fn play_collection(&mut self, start: usize, shuffle: bool) {
        if self.busy || self.collection_error.is_some() { return; }
        let mut tracks = self.results.clone();
        if shuffle { shuffle_tracks(&mut tracks); }
        let position = if shuffle { 0 } else { start };
        let Some(first) = tracks.get(position).cloned() else { return; };
        let title = self.browsing.clone().unwrap_or_else(|| "Songs".to_owned());
        self.play_track(first, false);
        self.queue_epoch = self.queue_epoch.wrapping_add(1);
        if let Some(now) = self.now.as_mut() {
            now.queue_title = title;
            now.queue = tracks;
            now.queue_epoch = self.queue_epoch;
            now.continuation = None;
            now.topping_up = false;
            now.topup_failures = 0;
            now.dropped.clear();
            now.playing = Some(position);
            now.cursor[Tab::UpNext.index()] = position;
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

    fn return_from_player(&mut self) {
        if let Some(page) = self.player_back.take() {
            self.restore_page(page);
        } else {
            self.view = self.back_to;
            self.mode = Mode::Browse;
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
            TrayCommand::TogglePause => self.toggle_pause(),
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

    fn begin_music_sign_in(&mut self, recover: bool) {
        if self.music_signing_in {
            self.status = "the YouTube Music session import is already pending".to_string();
            return;
        }
        if self.overlay.is_open() && !matches!(self.overlay, Overlay::SignIn(SignIn::Failed { .. }))
        {
            self.status = "close the current panel, then press M to open sign-in".to_string();
            return;
        }
        self.menu = None;

        self.request_music_sign_in(recover);
    }

    fn request_music_sign_in(&mut self, recover: bool) {
        self.automatic_sign_in = false;
        self.music_signing_in = true;
        self.status = if recover {
            "renewing the saved YouTube Music session ...".to_string()
        } else {
            "finish signing in in the YouTube Music window ...".to_string()
        };
        self.overlay = Overlay::SignIn(SignIn::Music {
            started: Instant::now(),
            recovering: recover,
        });
        if self.source.send(Request::MusicSignIn { recover }).is_err() {
            self.music_signing_in = false;
            let reason = "source worker is not running".to_string();
            self.status = reason.clone();
            self.overlay = Overlay::SignIn(SignIn::Failed { reason });
        }
    }

    fn log_out_music(&mut self) {
        self.clear_account_actions();
        self.music_signing_in = false;
        self.automatic_sign_in = false;
        self.sign_in_prompted = false;
        self.menu = None;
        // Clear the worker's in-memory copy even if removing credentials or
        // the durable outbox reports an error below.
        let _ = self.source.send(Request::ClearReports);
        match crate::session::sign_out() {
            Ok(warning) => {
                // Any personalized response already in flight belongs to the
                // session that just ended and must not repopulate Home.
                self.home_generation = self.home_generation.wrapping_add(1);
                self.home_attempts = 0;
                self.home_pending = false;
                self.home.clear();
                self.home_scroll.clear();
                self.home_shelf = 0;
                self.home_card = 0;
                self.home_top = 0;
                self.selection_settled = None;
                self.art = ArtCache::default();
                self.status = warning.map_or_else(
                    || "logged out of YouTube Music -- loading guest Home".to_string(),
                    |warning| format!("logged out; {warning}"),
                );
                self.request_home();
            }
            Err(error) => {
                self.status = format!("could not log out of YouTube Music: {error:#}");
            }
        }
    }

    fn nudge_volume(&mut self, delta: f32) {
        self.set_volume(self.snapshot().volume + delta);
    }

    fn set_volume(&mut self, volume: f32) {
        let volume = volume.clamp(0.0, 2.0);
        if let Err(error) = self.player.send(Command::SetVolume(volume)) {
            self.status = format!("could not change volume: {error:#}");
            return;
        }
        self.muted = volume == 0.0;
        if !self.muted {
            self.volume_before_mute = volume;
        }
        self.volume_save = Some((Instant::now(), volume));
        self.status = if self.muted { "muted".into() } else { format!("volume {:.0}%", volume * 100.0) };
    }

    fn flush_volume(&mut self) {
        let Some((_, volume)) = self.volume_save.take() else { return; };
        let settings = config::Settings { start_in_tray: self.start_in_tray,
            icon_theme: self.icon_theme, cover_style: self.cover_style,
            image_renderer: self.image_renderer, volume, output_device: self.output_device.clone() };
        if let Err(error) = settings.save() {
            crate::diagnostics::error("config", &format!("could not save volume: {error:#}"));
            self.status = "Could not save the volume setting.".into();
        }
    }

    fn toggle_mute(&mut self) {
        let (target, remembered) = mute_transition(
            self.muted,
            self.snapshot().volume,
            self.volume_before_mute,
        );
        if let Err(error) = self.player.send(Command::SetVolume(target)) {
            self.status = format!("could not change mute state: {error:#}");
            return;
        }
        self.volume_before_mute = remembered;
        self.muted = !self.muted;
        self.status = if self.muted {
            "muted -- press m or click mut to restore volume".to_string()
        } else {
            format!("volume {:.0}%", target * 100.0)
        };
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
        if let Some((_, listening)) = self.listening.as_mut() { listening.seeked(); }
    }

    /// Seeks to an absolute point selected on the progress bar.
    fn seek_to_fraction(&mut self, position: u16) {
        let snap = self.snapshot();
        if snap.state == PlayState::Idle {
            return;
        }
        let Some(total) = self.now.as_ref().and_then(|now| now.duration) else {
            return;
        };
        let target = duration_at_pointer(total, position);
        let _ = self.player.send(Command::Seek(target));
        if let Some((_, listening)) = self.listening.as_mut() { listening.seeked(); }
    }
}

fn duration_at_pointer(total: Duration, position: u16) -> Duration {
    total.mul_f64(pointer_fraction(position))
}

fn pointer_fraction(position: u16) -> f64 {
    f64::from(position.min(POINTER_SCALE)) / f64::from(POINTER_SCALE)
}

fn mute_transition(muted: bool, current: f32, remembered: f32) -> (f32, f32) {
    if muted {
        (
            remembered.clamp(VOLUME_STEP, DEFAULT_UNMUTED_VOLUME * 2.0),
            remembered,
        )
    } else {
        (0.0, if current > 0.0 { current } else { remembered })
    }
}

fn playback_failure_summary(why: &str) -> &'static str {
    let why = why.to_ascii_lowercase();
    if ["sign in", "login", "cookie", "authentication", "http 401", "http 403"]
        .iter()
        .any(|needle| why.contains(needle))
    {
        "YouTube session needs attention"
    } else if why.contains("source worker") || why.contains("resolver") {
        "playback service is unavailable"
    } else if ["unavailable", "not available", "private", "restricted", "removed"]
        .iter()
        .any(|needle| why.contains(needle))
    {
        "track is unavailable"
    } else if [
        "timeout",
        "timed out",
        "network",
        "connect",
        "dns",
        "http 5",
        "request failed",
    ]
    .iter()
    .any(|needle| why.contains(needle))
    {
        "network interrupted playback"
    } else if ["decode", "decoder", "codec", "format", "audio stream"]
        .iter()
        .any(|needle| why.contains(needle))
    {
        "audio format could not be played"
    } else {
        "track could not be played"
    }
}

fn playback_failure_status(summary: &str) -> String {
    format!("{summary} — r retry · n skip")
}

fn next_output_device(
    current: Option<&str>,
    outputs: &[OutputDevice],
    forward: bool,
) -> Option<String> {
    let choices = outputs.len() + 1;
    let current = current
        .and_then(|id| outputs.iter().position(|output| output.id == id))
        .map_or(0, |index| index + 1);
    let next = if forward {
        (current + 1) % choices
    } else {
        (current + choices - 1) % choices
    };
    next.checked_sub(1)
        .and_then(|index| outputs.get(index))
        .map(|output| output.id.clone())
}

pub(crate) fn app_menu_items(has_player: bool) -> Vec<MenuItem> {
    let mut items = vec![
        MenuItem::action("Home", Some("H"), true, Some("Go to"), MenuAction::GoHome),
        MenuItem::action("Search", Some("/"), true, None, MenuAction::BeginSearch),
    ];
    items.extend([
        MenuItem::action(
            "Now Playing",
            Some("P"),
            has_player,
            None,
            MenuAction::OpenPlayer,
        ),
        MenuItem::action(
            "Account",
            None,
            true,
            Some("App"),
            MenuAction::OpenAccount,
        ),
        MenuItem::action("Settings", Some("Ctrl+S"), true, None, MenuAction::OpenSettings),
        MenuItem::action("Keyboard shortcuts", Some("?"), true, None, MenuAction::OpenHelp),
    ]);
    #[cfg(windows)]
    items.push(MenuItem::action(
        "Minimize to tray",
        Some("B"),
        has_player,
        Some("Window"),
        MenuAction::Background,
    ));
    items.push(MenuItem::action(
        "Quit",
        Some("Ctrl+C"),
        true,
        None,
        MenuAction::Quit,
    ));
    items
}

pub(crate) fn keyboard_help_items() -> Vec<MenuItem> {
    vec![
        MenuItem::help("Move selection", "j/k, Up/Down", Some("Navigation")),
        MenuItem::help("First or last item", "g/G, Home/End", None),
        MenuItem::help("Open or play", "Enter", None),
        MenuItem::help("Go back", "Esc", None),
        MenuItem::help("Search", "/ or i", Some("Pages")),
        MenuItem::help("Home", "H", None),
        MenuItem::help("Now Playing", "P", None),
        MenuItem::help("Pause or resume", "Space", Some("Playback")),
        MenuItem::help("Next or previous", "n / p", None),
        MenuItem::help("Repeat off / all / one", "R", None),
        MenuItem::help("Player actions", "Ctrl+P", None),
            MenuItem::help("Queue: remove / shuffle", "d / z", None),
            MenuItem::help("Queue: move row", "K / J", None),
            MenuItem::help("Queue: clear upcoming", "C", None),
            MenuItem::help("Seek", "Left/Right", None),
        MenuItem::help("Change volume", "+ / -", None),
        MenuItem::help("Mute or restore volume", "m", None),
        MenuItem::help("Stop", "s", None),
        MenuItem::help("App Menu", "Ctrl+K", Some("Global")),
        MenuItem::help("Page Actions (outside search)", ".", None),
        MenuItem::help("Keyboard Help", "?", None),
        MenuItem::help("Settings", "Ctrl+S", None),
        MenuItem::help("Quit immediately", "Ctrl+C", None),
    ]
}

pub(crate) fn account_menu_items(connected: bool, signing_in: bool) -> Vec<MenuItem> {
    let mut items = vec![MenuItem::action(
        if connected {
            "Refresh YouTube Music session"
        } else {
            "Connect YouTube Music"
        },
        Some("M"),
        !signing_in,
        Some("Account"),
        MenuAction::ConnectMusic,
    )];
    if connected {
        items.push(MenuItem::action(
            "Log out of YouTube Music",
            None,
            true,
            None,
            MenuAction::LogOutMusic,
        ));
    }
    items
}

/// Pure state tests. [`App`] itself is not constructed because it owns live
/// audio and source workers.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "renders fixture UI with a zero-volume audio worker; run with isolated APPDATA"]
    fn preview_search_palette_and_player_links() {
        use crate::source::search::{Filter, Item};
        let player = Player::spawn(0.0, None).unwrap();
        let source = SourceWorker::spawn(crate::source::youtube::YouTube::default()).unwrap();
        let mut app = App::new(player, source, Graphics::blocks(), config::Settings::default());
        let artist = ArtistRef { name: "Evening Artist".into(), endpoint: BrowseEndpoint::new("UCfixture") };
        let track = Track { id: "fixtureSong".into(), title: "A quieter evening".into(), uploader: artist.name.clone(),
            album: Some("Evening Album".into()), artist_ref: Some(artist.clone()), duration: Some(Duration::from_secs(180)) };
        app.search_items = [
            (Filter::Artists, "Evening Artist", Target::Artist { artist: artist.clone() }),
            (Filter::Albums, "Evening Album", Target::Open { endpoint: BrowseEndpoint::new("MPREfixture") }),
            (Filter::Playlists, "After hours", Target::Open { endpoint: BrowseEndpoint::new("VLfixture") }),
            (Filter::Songs, "A quieter evening", Target::Play { video_id: track.id.clone() }),
        ].into_iter().map(|(kind, title, target)| Item {
            card: Card { title: title.into(), subtitle: format!("{} • Evening Artist", kind.label().trim_end_matches('s')),
                art: None, duration: None, artist_ref: Some(artist.clone()), target },
            kind, track: (kind == Filter::Songs).then(|| track.clone()),
        }).collect();
        let keys: Vec<_> = app.search_items.iter().map(|item| item.card.art_key().to_owned()).collect();
        app.art.want_visible(keys.iter().map(String::as_str));
        for (index, key) in keys.into_iter().enumerate() {
            app.art.store(key, Some(Cover::from_rgb(4, 4, [70 + index as u8 * 35, 90, 110].repeat(16))));
        }
        app.results = vec![track.clone()]; app.query = "Evening".into(); app.view = View::Tracks;
        app.cover = Some(Cover::from_rgb(4, 4, [72, 104, 156].repeat(16)));
        app.now = Some(NowPlaying::new(&track));
        app.now.as_mut().unwrap().album_route = Some(("Evening Album".into(), BrowseEndpoint::new("MPREfixture")));
        let mut mouse = crate::ui::MouseMap::default();
        for (width, height) in [(48, 18), (100, 36), (160, 42)] {
            let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
            terminal.draw(|frame| crate::ui::render(frame, &mut app, &mut mouse)).unwrap();
            let buffer = terminal.backend().buffer();
            let text: String = buffer.content.iter().map(|cell| cell.symbol()).collect();
            assert!(text.contains("Artists") && text.contains("Playlists"));
            assert!((0..height).any(|y| (0..width).any(|x| mouse.action_at(x, y) == Some(MouseAction::PlayTrack(1)))));
            crate::ui::preview_buffer(&format!("mixed-search-{width}x{height}"), buffer);
        }
        app.handle_mouse_action(MouseAction::PlayTrack(0)).unwrap();
        assert_eq!(app.view, View::Artist);
        assert!(app.listening.is_none());
        app.go_back();
        assert_eq!(app.search_items.len(), 4);
        app.handle_mouse_action(MouseAction::PlayTrack(1)).unwrap();
        assert_eq!(app.browsing_endpoint.as_ref().unwrap().browse_id, "MPREfixture");
        assert!(app.listening.is_none());
        app.go_back();
        app.view = View::Playing;
        app.now.as_mut().unwrap().tab = Tab::Lyrics;
        app.now.as_mut().unwrap().lyrics = Panel::Ready(Lyrics {
            text: "The evening settles quietly\nA little light across the room\nThe music stays with us".into(),
            source: Some("Synced lyrics".into()), timed: (0..8).map(|index| crate::source::watch::TimedLine {
                start: Duration::from_secs(index * 10), text: format!("A quiet lyric line {index}") }).collect(),
        });
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 36)).unwrap();
        terminal.draw(|frame| crate::ui::render(frame, &mut app, &mut mouse)).unwrap();
        assert!((0..36).any(|y| (0..120).any(|x| mouse.action_at(x, y) == Some(MouseAction::OpenPlayingAlbum))));
        crate::ui::preview_buffer("palette-lyrics-120x36", terminal.backend().buffer());
        app.now.as_mut().unwrap().tab = Tab::UpNext;
        app.now.as_mut().unwrap().queue_title = "Evening Mix".into();
        app.now.as_mut().unwrap().playing = Some(0);
        app.now.as_mut().unwrap().queue = (0..40).map(|index| Track {
            id: format!("fixture{index}"), title: format!("Evening song {index:02} — a longer title"), ..track.clone()
        }).collect();
        for (width, height) in [(48, 18), (64, 20), (100, 36), (160, 42), (216, 50)] {
            let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
            for selected in [0, 20, 39] {
                app.now.as_mut().unwrap().cursor[Tab::UpNext.index()] = selected;
                terminal.draw(|frame| crate::ui::render(frame, &mut app, &mut mouse)).unwrap();
                assert!((0..height).any(|y| (0..width).any(|x| mouse.action_at(x, y) == Some(MouseAction::OpenPageRow(selected)))), "selected song cannot be clicked at {width}x{height}");
                assert!((0..height).any(|y| (0..width).any(|x| mouse.action_at(x, y) == Some(MouseAction::OpenPlayingAlbum))), "album target missing at {width}x{height}");
                if width >= 100 {
                    assert!((0..height).any(|y| (0..width).any(|x| mouse.action_at(x, y) == Some(MouseAction::OpenQueueArtist(selected)))), "queue artist hidden at {width}x{height}");
                }
                crate::ui::preview_buffer(&format!("spaced-queue-{width}x{height}-{selected}"), terminal.backend().buffer());
            }
        }
        app.handle_mouse_action(MouseAction::OpenPlayingArtist).unwrap();
        assert_eq!(app.view, View::Artist);
        assert!(app.listening.is_none());
    }

    #[test]
    #[ignore = "reads saved playlists through clicks and workers; no playback or history writes"]
    fn saved_playlist_clicks_load_tracks_in_the_real_app() {
        let cookies = config::Cookies::available().unwrap().expect("sign in before this check");
        let http = crate::source::http::Http::new().unwrap();
        let cards: Vec<Card> = crate::source::collection::saved_cards(&http, &cookies).unwrap()
            .into_iter().take(4).collect();
        assert!(!cards.is_empty());
        let player = Player::spawn(0.0, None).unwrap();
        let source = SourceWorker::spawn(crate::source::youtube::YouTube::default()).unwrap();
        let mut app = App::new(player, source, Graphics::blocks(), config::Settings::default());
        // Startup Home is independent; invalidate it before providing this
        // fixture shelf so its later response cannot replace the click target.
        app.home_generation += 1;
        app.home_pending = false;
        app.home = vec![Shelf { title: "Saved playlists".into(), cards }];
        for card in 0..app.home[0].cards.len() {
            app.view = View::Home;
            app.handle_mouse_action(MouseAction::OpenHomeCard { shelf: 0, card }).unwrap();
            assert_eq!(app.view, View::Tracks);
            assert!(app.collection.is_some());
            let deadline = Instant::now() + Duration::from_secs(25);
            while app.pending_page_request.is_some() && Instant::now() < deadline {
                app.poll_source();
                std::thread::sleep(Duration::from_millis(25));
            }
            assert!(app.pending_page_request.is_none(), "playlist request timed out");
            assert!(app.collection_error.is_none(), "playlist click failed: {:?}", app.collection_error);
            let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 36)).unwrap();
            let mut mouse = crate::ui::MouseMap::default();
            terminal.draw(|frame| crate::ui::render(frame, &mut app, &mut mouse)).unwrap();
            let rendered: String = terminal.backend().buffer().content.iter().map(|cell| cell.symbol()).collect();
            assert!(rendered.contains("Play") || app.results.is_empty());
            if let Some(track) = app.results.first() {
                assert!(rendered.contains(&track.title.chars().take(12).collect::<String>()));
            }
            println!("Saved playlist click: {} tracks rendered, error=false", app.results.len());
        }
        assert!(app.now.is_none());
        assert!(app.listening.is_none());
    }

    #[test]
    fn displayed_menu_shortcuts_select_the_matching_enabled_command() {
        let menu = Menu::new(MenuPage::Root, app_menu_items(false));
        for (code, modifiers, expected) in [
            (KeyCode::Char('H'), KeyModifiers::SHIFT, MenuAction::GoHome),
            (KeyCode::Char('/'), KeyModifiers::NONE, MenuAction::BeginSearch),
            (KeyCode::Char('?'), KeyModifiers::SHIFT, MenuAction::OpenHelp),
            (KeyCode::Char('s'), KeyModifiers::CONTROL, MenuAction::OpenSettings),
        ] {
            let index = menu.shortcut_index(KeyEvent::new(code, modifiers)).unwrap();
            assert_eq!(menu.items[index].action, Some(expected));
        }
        assert_eq!(menu.shortcut_index(KeyEvent::new(KeyCode::Char('P'), KeyModifiers::SHIFT)), None);
        assert_eq!(menu.shortcut_index(KeyEvent::new(KeyCode::Char('H'), KeyModifiers::ALT)), None);
        let account = Menu::new(MenuPage::Account, account_menu_items(false, false));
        assert_eq!(account.shortcut_index(KeyEvent::new(KeyCode::Char('M'), KeyModifiers::SHIFT)), Some(0));
        let pending = Menu::new(MenuPage::Account, account_menu_items(false, true));
        assert_eq!(pending.shortcut_index(KeyEvent::new(KeyCode::Char('M'), KeyModifiers::SHIFT)), None);
    }

    #[test]
    fn menu_navigation_skips_unavailable_player_commands() {
        let mut menu = Menu::new(MenuPage::Root, app_menu_items(false));
        menu.selected = 1;
        menu.move_by(1, menu.items.len());
        assert_eq!(menu.items[menu.selected].action, Some(MenuAction::OpenAccount));
        menu.move_by(-1, menu.items.len());
        assert_eq!(menu.items[menu.selected].action, Some(MenuAction::BeginSearch));
    }

    #[test]
    fn logout_is_offered_only_for_a_connected_session() {
        let disconnected = account_menu_items(false, false);
        assert_eq!(disconnected.len(), 1);
        assert_eq!(disconnected[0].label, "Connect YouTube Music");

        let connected = account_menu_items(true, false);
        assert_eq!(connected.len(), 2);
        assert_eq!(connected[0].label, "Refresh YouTube Music session");
        assert_eq!(connected[1].label, "Log out of YouTube Music");
        assert!(connected[1].enabled);

        let pending = account_menu_items(true, true);
        assert!(!pending[0].enabled);
        assert!(pending[1].enabled, "logout must remain available during renewal");
    }

    #[test]
    fn menu_cursor_movement_clamps_at_both_ends() {
        let mut menu = Menu::new(MenuPage::Root, Vec::new());

        menu.move_by(-1, 4);
        assert_eq!(menu.selected, 0);
        menu.move_by(2, 4);
        assert_eq!(menu.selected, 2);
        menu.move_by(20, 4);
        assert_eq!(menu.selected, 3);
        menu.move_by(-20, 4);
        assert_eq!(menu.selected, 0);

        menu.selected = usize::MAX;
        menu.clamp(2);
        assert_eq!(menu.selected, 1);
        menu.last(0);
        assert_eq!(menu.selected, 0);
    }

    #[test]
    fn stale_page_responses_do_not_consume_the_active_request() {
        let mut pending = Some((42, PageRequestKind::Artist));

        assert!(!accept_pending_page(&mut pending, 41));
        assert_eq!(pending, Some((42, PageRequestKind::Artist)));
        assert!(accept_pending_page(&mut pending, 42));
        assert_eq!(pending, None);
    }

    #[test]
    fn only_search_requests_allow_the_current_page_to_be_snapshotted() {
        assert!(!page_snapshot_blocked(None, false, false));
        assert!(page_snapshot_blocked(None, true, false));
        assert!(page_snapshot_blocked(None, false, true));
        assert!(!page_snapshot_blocked(
            Some((1, PageRequestKind::Search)),
            true,
            false
        ));
        assert!(page_snapshot_blocked(
            Some((2, PageRequestKind::Browse)),
            true,
            false
        ));
        assert!(page_snapshot_blocked(
            Some((4, PageRequestKind::Artist)),
            true,
            false
        ));
    }

    #[test]
    fn player_return_targets_are_flattened_to_a_real_page() {
        let nested = HistoryEntry::Playing {
            back_to: View::Artist,
            player_back: Some(Box::new(HistoryEntry::Playing {
                back_to: View::Home,
                player_back: Some(Box::new(HistoryEntry::Home {
                    status: "home".to_string(),
                })),
            })),
        };

        let (page, view) = player_return_target(Some(nested), View::Tracks);
        assert_eq!(view, View::Home);
        assert!(matches!(page, Some(HistoryEntry::Home { .. })));
    }

    #[test]
    fn page_history_drops_the_oldest_route_at_its_bound() {
        let mut history = Vec::new();
        for selected in 0..PAGE_HISTORY + 3 {
            push_history(
                &mut history,
                HistoryEntry::Tracks(Box::new(TrackPage {
                    results: Vec::new(),
                    query: String::new(),
                    search_items: Vec::new(),
                    search_filter: crate::source::search::Filter::All,
                    selected,
                    offset: selected,
                    browsing: None,
                    browsing_endpoint: None,
                    collection: None,
                    collection_error: None,
                    status: format!("page {selected}"),
                })),
            );
        }

        assert_eq!(history.len(), PAGE_HISTORY);
        let HistoryEntry::Tracks(oldest) = &history[0] else {
            panic!("the test only inserted track pages");
        };
        assert_eq!(oldest.selected, 3);
    }

    #[test]
    fn artist_selection_belongs_to_exactly_one_section() {
        let artist = ArtistRef {
            name: "Tame Impala".to_string(),
            endpoint: BrowseEndpoint::new("UCGz-artist"),
        };
        let track = Track {
            id: "letithappen".to_string(),
            title: "Let It Happen".to_string(),
            uploader: artist.name.clone(),
            duration: Some(Duration::from_secs(468)),
            album: Some("Currents".to_string()),
            artist_ref: Some(artist.clone()),
        };
        let mut view = ArtistView {
            requested: artist.clone(),
            page: Panel::Ready(ArtistPage {
                artist: artist.clone(),
                audience: None,
                description: None,
                art: None,
                top_songs: vec![ArtistSong { track, plays: None }],
                shelves: vec![Shelf {
                    title: "Fans might also like".to_string(),
                    cards: vec![Card {
                        title: "Metronomy".to_string(),
                        subtitle: "Artist".to_string(),
                        art: None,
                        duration: None,
                        artist_ref: None,
                        target: Target::Artist { artist },
                    }],
                }],
            }),
            section: 0,
            song: 0,
            song_offset: 0,
            card: 0,
            top: 0,
            scroll: vec![0],
        };

        assert!(view.selected_song().is_some());
        assert!(view.selected_card().is_none());
        view.section = 1;
        assert!(view.selected_song().is_none());
        assert_eq!(
            view.selected_card().map(|card| card.title.as_str()),
            Some("Metronomy")
        );
    }

    #[test]
    fn player_tabs_use_the_number_key_order_and_title_case_labels() {
        assert_eq!(
            Tab::ALL,
            [Tab::UpNext, Tab::Lyrics, Tab::Related, Tab::Comments]
        );
        assert_eq!(
            Tab::ALL.map(Tab::label),
            ["Queue", "Lyrics", "Related", "Comments"]
        );
        assert_eq!(Tab::UpNext.shifted(-1), Tab::Comments);
        assert_eq!(Tab::Comments.shifted(1), Tab::UpNext);
    }

    #[test]
    fn pointer_positions_cover_the_whole_track() {
        let total = Duration::from_secs(240);

        assert_eq!(pointer_fraction(0), 0.0);
        assert_eq!(pointer_fraction(POINTER_SCALE / 2), 0.5);
        assert_eq!(pointer_fraction(POINTER_SCALE), 1.0);
        assert_eq!(duration_at_pointer(total, 0), Duration::ZERO);
        assert_eq!(
            duration_at_pointer(total, POINTER_SCALE / 2),
            Duration::from_secs(120)
        );
        assert_eq!(
            duration_at_pointer(total, POINTER_SCALE),
            Duration::from_secs(240)
        );
        assert_eq!(
            duration_at_pointer(total, u16::MAX),
            Duration::from_secs(240),
            "out-of-range input is clamped"
        );
    }

    #[test]
    fn mute_restores_the_last_audible_volume() {
        let (muted, remembered) = mute_transition(false, 0.65, 1.0);
        assert_eq!(muted, 0.0);
        assert_eq!(remembered, 0.65);

        let (restored, remembered) = mute_transition(true, 0.0, remembered);
        assert_eq!(restored, 0.65);
        assert_eq!(remembered, 0.65);

        let (restored_default, _) = mute_transition(true, 0.0, 0.0);
        assert_eq!(restored_default, VOLUME_STEP);
    }

    #[test]
    fn playback_failures_are_reduced_to_actionable_categories() {
        assert_eq!(
            playback_failure_summary("HTTP 403 while reading the signed URL"),
            "YouTube session needs attention"
        );
        assert_eq!(
            playback_failure_summary("Video unavailable: private video"),
            "track is unavailable"
        );
        assert_eq!(
            playback_failure_summary("chunk request timed out"),
            "network interrupted playback"
        );
        assert_eq!(
            playback_failure_summary("decoder rejected the audio codec"),
            "audio format could not be played"
        );
        assert_eq!(
            playback_failure_summary("resolver unavailable"),
            "playback service is unavailable"
        );
        assert_eq!(
            playback_failure_summary("unexpected response"),
            "track could not be played"
        );
        assert_eq!(
            playback_failure_status("track is unavailable"),
            "track is unavailable — r retry · n skip"
        );
    }

    #[test]
    fn a_pasted_url_adopts_the_real_queue_metadata() {
        let artist = ArtistRef {
            name: "Daft Punk".to_string(),
            endpoint: BrowseEndpoint::new("UCdaft"),
        };
        let mut now = NowPlaying::new(&Track {
            id: "JhulBGMA7G4".to_string(),
            title: "https://music.youtube.com/watch?v=JhulBGMA7G4".to_string(),
            uploader: String::new(),
            duration: None,
            album: None,
            artist_ref: None,
        });
        now.queue = vec![Track {
            id: "JhulBGMA7G4".to_string(),
            title: "Harder, Better, Faster, Stronger".to_string(),
            uploader: "Daft Punk".to_string(),
            duration: Some(Duration::from_secs(224)),
            album: Some("Discovery".to_string()),
            artist_ref: Some(artist.clone()),
        }];
        now.playing = Some(0);

        let _ = now.hydrate_from_queue();

        assert_eq!(now.title, "Harder, Better, Faster, Stronger");
        assert_eq!(now.artist, "Daft Punk");
        assert_eq!(now.album.as_deref(), Some("Discovery"));
        assert_eq!(now.duration, Some(Duration::from_secs(224)));
        assert_eq!(now.artist_ref, Some(artist));
        assert!(now.lyrics_query().is_some());
    }

    #[test]
    fn bare_menu_keys_allow_shift_but_not_control_or_alt() {
        assert!(is_bare_character(
            KeyEvent::new(KeyCode::Char('?'), KeyModifiers::SHIFT),
            '?'
        ));
        assert!(!is_bare_character(
            KeyEvent::new(KeyCode::Char('.'), KeyModifiers::CONTROL),
            '.'
        ));
        assert!(!is_bare_character(
            KeyEvent::new(KeyCode::Char('?'), KeyModifiers::ALT),
            '?'
        ));
        assert!(is_ctrl_key(
            KeyEvent::new(
                KeyCode::Char('K'),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT
            ),
            'k'
        ));
    }

    #[test]
    fn a_search_started_on_player_returns_to_the_page_behind_player() {
        assert_eq!(page_behind_search(View::Home, View::Tracks), View::Home);
        assert_eq!(
            page_behind_search(View::Playing, View::Artist),
            View::Artist
        );
    }

    fn playing() -> NowPlaying {
        NowPlaying::new(&Track {
            id: "letithappen".to_string(),
            title: "Let It Happen".to_string(),
            uploader: "Tame Impala".to_string(),
            duration: Some(Duration::from_secs(468)),
            album: Some("Currents".to_string()),
            artist_ref: None,
        })
    }

    #[test]
    fn a_new_page_follows_the_singer_until_told_otherwise() {
        assert!(
            playing().follow_lyrics,
            "a track just started is one nobody has scrolled yet"
        );
    }

    #[test]
    fn scrolling_the_lyrics_hands_them_back_to_the_user() {
        let mut now = playing();
        now.open(Tab::Lyrics);

        now.scroll(3);
        assert_eq!(now.cursor(), 3);
        assert!(
            !now.follow_lyrics,
            "a panel that yanks itself back to the singer cannot be read ahead in"
        );

        // Bounded below only: the far end needs the wrapped length, which is
        // the renderer's to know.
        now.scroll(-10);
        assert_eq!(now.cursor(), 0);
    }

    #[test]
    fn jumping_to_an_end_hands_them_back_too() {
        let mut now = playing();
        now.open(Tab::Lyrics);

        // What `G` asks for: a line no panel has, for the renderer to clamp.
        now.jump(usize::MAX);
        assert_eq!(now.cursor(), usize::MAX);
        assert!(!now.follow_lyrics);

        now.follow_lyrics = true;
        now.jump(0);
        assert!(!now.follow_lyrics, "`g` is a scroll like any other");
    }

    #[test]
    fn opening_the_lyrics_tab_again_is_how_you_get_back_to_the_song() {
        let mut now = playing();
        assert!(now.open(Tab::Lyrics), "that was a change of tab");
        now.scroll(20);
        assert!(!now.follow_lyrics);

        // Pressing `2` while already on Lyrics: not a change of tab, so no
        // fetch is started, but it is the gesture that means "show me where we
        // are" -- and it needs no key of its own.
        assert!(!now.open(Tab::Lyrics), "the tab did not change");
        assert!(now.follow_lyrics, "the panel should be following again");
        assert_eq!(now.cursor(), 20, "the cursor is the renderer's to move");
    }

    #[test]
    fn the_other_tabs_leave_the_lyrics_alone() {
        let mut now = playing();
        now.open(Tab::Lyrics);
        now.scroll(20);

        // Walking away and back through another tab does re-arm it, because
        // arriving on Lyrics is the gesture; walking *past* it does not.
        assert!(now.open(Tab::Comments));
        assert!(!now.follow_lyrics);
        now.scroll(5);
        assert_eq!(now.cursor(), 5, "each tab keeps its own cursor");
        assert_eq!(
            now.cursor[Tab::Lyrics.index()],
            20,
            "the lyrics cursor is where it was left"
        );
    }

    /// One numbered track, so a queue and the pages appended to it can be
    /// built out of ids that never accidentally collide.
    fn numbered(i: usize) -> Track {
        Track {
            id: i.to_string(),
            title: format!("track {i}"),
            uploader: "Tame Impala".to_string(),
            duration: Some(Duration::from_secs(180)),
            album: None,
            artist_ref: None,
        }
    }

    /// A queue of `count` tracks, ids "0", "1", ...
    fn queued(count: usize) -> Vec<Track> {
        (0..count).map(numbered).collect()
    }

    #[test]
    fn a_page_of_tracks_lands_on_the_end_of_the_queue() {
        let mut now = playing();
        now.queue = queued(3);
        now.playing = Some(0);

        assert_eq!(now.remaining(), 2);
        assert_eq!(now.absorb(queued(6).split_off(3)), 3);
        assert_eq!(now.queue.len(), 6);
        assert_eq!(
            now.remaining(),
            5,
            "the new tracks are ahead of the playing one"
        );
        assert_eq!(now.playing, Some(0), "appending must not move the cursor");
    }

    /// A radio repeats over hours. An endless queue that re-appends what it just
    /// played is a loop, which is worse than a queue that ends honestly.
    #[test]
    fn a_page_of_tracks_already_held_is_not_appended_twice() {
        let mut now = playing();
        now.queue = queued(3);
        now.playing = Some(0);

        assert_eq!(now.absorb(queued(3)), 0, "every track was already held");
        assert_eq!(now.queue.len(), 3);

        // Half repeats, half new: only the new half lands.
        let mut page = queued(5);
        page.drain(..2);
        assert_eq!(now.absorb(page), 2);
        assert_eq!(now.queue.len(), 5);
    }

    #[test]
    fn explicit_queue_actions_share_the_radio_memory_bound() {
        let mut now = playing();
        now.queue = queued(QUEUE_BEHIND + 1 + QUEUE_AHEAD);
        now.playing = Some(QUEUE_BEHIND);
        let old_tail = now.queue.last().unwrap().id.clone();

        assert!(now.insert_user_track(numbered(999), true));

        assert_eq!(now.queue.len(), QUEUE_BEHIND + 1 + QUEUE_AHEAD);
        assert_eq!(now.queue[QUEUE_BEHIND + 1].id, "999");
        assert!(
            now.queue.iter().all(|track| track.id != old_tail),
            "the farthest recommendation should yield to the user choice"
        );
    }

    #[test]
    fn adding_to_queue_uses_the_tail_and_rejects_duplicates() {
        let mut now = playing();
        now.queue = queued(4);
        now.playing = Some(0);

        assert!(now.insert_user_track(numbered(9), false));
        assert_eq!(now.queue.last().unwrap().id, "9");
        assert!(!now.insert_user_track(numbered(9), true));
        assert_eq!(now.queue.iter().filter(|track| track.id == "9").count(), 1);
    }

    #[test]
    fn queue_edits_apply_only_after_the_playing_track() {
        let mut now = playing();
        now.queue = queued(6);
        now.playing = Some(2);

        now.cursor[Tab::UpNext.index()] = 2;
        assert!(now.remove_selected_upcoming().is_none());
        assert!(!now.move_selected_upcoming(1));

        now.cursor[Tab::UpNext.index()] = 4;
        assert_eq!(now.remove_selected_upcoming().unwrap().id, "4");
        assert_eq!(
            now.queue
                .iter()
                .map(|track| track.id.as_str())
                .collect::<Vec<_>>(),
            vec!["0", "1", "2", "3", "5"]
        );

        assert!(now.move_selected_upcoming(-1));
        assert_eq!(now.cursor(), 3);
        assert_eq!(now.queue[3].id, "5");
        assert!(
            !now.move_selected_upcoming(-1),
            "an upcoming row cannot cross the current track"
        );
    }

    #[test]
    fn clearing_upcoming_keeps_history_and_the_current_track() {
        let mut now = playing();
        now.queue = queued(8);
        now.playing = Some(3);
        now.cursor[Tab::UpNext.index()] = 7;

        assert_eq!(now.clear_upcoming(), 4);
        assert_eq!(
            now.queue
                .iter()
                .map(|track| track.id.as_str())
                .collect::<Vec<_>>(),
            vec!["0", "1", "2", "3"]
        );
        assert_eq!(now.playing, Some(3));
        assert_eq!(now.cursor(), 3);
    }

    #[test]
    fn shuffling_upcoming_keeps_current_and_selected_tracks() {
        let mut now = playing();
        now.queue = queued(12);
        now.playing = Some(3);
        now.cursor[Tab::UpNext.index()] = 8;

        let before = now
            .queue
            .iter()
            .map(|track| track.id.clone())
            .collect::<Vec<_>>();
        assert_eq!(now.shuffle_upcoming(), 8);
        assert_eq!(
            now.queue[..=3]
                .iter()
                .map(|track| track.id.as_str())
                .collect::<Vec<_>>(),
            vec!["0", "1", "2", "3"]
        );
        assert_eq!(now.queue[now.cursor()].id, "8");

        let mut after = now
            .queue
            .iter()
            .map(|track| track.id.clone())
            .collect::<Vec<_>>();
        let mut before = before;
        before.sort();
        after.sort();
        assert_eq!(after, before);
    }

    #[test]
    fn repeat_modes_cycle_in_the_order_shown_to_the_user() {
        let mut repeat = RepeatMode::Off;
        repeat = repeat.next();
        assert_eq!((repeat, repeat.label()), (RepeatMode::All, "all"));
        repeat = repeat.next();
        assert_eq!((repeat, repeat.label()), (RepeatMode::One, "one"));
        assert_eq!(repeat.next(), RepeatMode::Off);
    }

    #[test]
    fn repeat_modes_change_only_automatic_queue_edges() {
        let mut now = playing();
        now.queue = queued(4);
        now.playing = Some(3);

        now.repeat = RepeatMode::One;
        assert_eq!(now.advanced_index(1, true), Some(3));
        assert_eq!(now.advanced_index(-1, false), Some(2));
        assert_eq!(now.advanced_index(1, false), None);

        now.repeat = RepeatMode::All;
        assert_eq!(now.advanced_index(1, true), Some(0));
        now.playing = Some(0);
        assert_eq!(now.advanced_index(-1, false), Some(3));

        now.topping_up = true;
        now.playing = Some(3);
        assert_eq!(
            now.advanced_index(1, true),
            None,
            "a late page should land before repeat-all wraps"
        );
    }

    #[test]
    fn a_queue_still_loading_does_not_accept_an_orphaned_track() {
        let mut now = playing();
        now.queue = queued(2);

        assert!(!now.insert_user_track(numbered(9), true));
        assert_eq!(
            now.queue
                .iter()
                .map(|track| track.id.as_str())
                .collect::<Vec<_>>(),
            vec!["0", "1"]
        );
    }

    #[test]
    fn shelf_shuffle_keeps_exactly_the_same_bounded_tracks() {
        let mut tracks = queued(QUEUE_AHEAD + 1);
        let mut ids: Vec<String> = tracks.iter().map(|track| track.id.clone()).collect();

        shuffle_tracks(&mut tracks);

        let mut shuffled: Vec<String> = tracks.into_iter().map(|track| track.id).collect();
        ids.sort();
        shuffled.sort();
        assert_eq!(shuffled, ids);
    }

    /// The window is what keeps an endless queue from being an endless `Vec`.
    /// Both indices have to move with it, or the queue silently jumps.
    #[test]
    fn the_queue_trims_behind_the_playing_track() {
        let mut now = playing();
        now.queue = queued(QUEUE_BEHIND + 10);
        now.playing = Some(QUEUE_BEHIND + 5);
        now.cursor[Tab::UpNext.index()] = QUEUE_BEHIND + 5;

        now.trim();

        assert_eq!(now.queue.len(), QUEUE_BEHIND + 5);
        assert_eq!(now.playing, Some(QUEUE_BEHIND));
        assert_eq!(now.cursor[Tab::UpNext.index()], QUEUE_BEHIND);
        assert_eq!(
            now.queue[QUEUE_BEHIND].id, "25",
            "the playing track is still the one under the index"
        );
        assert_eq!(now.remaining(), 4, "and what was ahead is untouched");
    }

    /// Trimming is what makes a track eligible to be offered again, so what
    /// leaves the window has to be remembered or the radio simply re-queues it.
    #[test]
    fn a_track_trimmed_away_is_not_offered_back() {
        let mut now = playing();
        now.queue = queued(QUEUE_BEHIND + 2);
        now.playing = Some(QUEUE_BEHIND + 1);
        now.trim();

        assert!(now.dropped.contains(&"0".to_string()));
        assert_eq!(now.absorb(queued(1)), 0, "track 0 has already been played");
        assert_eq!(now.queue.len(), QUEUE_BEHIND + 1);
    }

    #[test]
    fn a_short_queue_is_left_alone_by_the_trim() {
        let mut now = playing();
        now.queue = queued(5);
        now.playing = Some(4);
        now.cursor[Tab::UpNext.index()] = 4;

        now.trim();

        assert_eq!(
            now.queue.len(),
            5,
            "nothing has fallen out of the window yet"
        );
        assert_eq!(now.playing, Some(4));
        assert!(now.dropped.is_empty());
    }

    /// The whole claim of an endless queue: it can be played through for as
    /// long as anyone likes without growing. Walked here the way a session
    /// walks it -- top up when the tail runs low, advance one, trim behind --
    /// for far more tracks than either bound holds.
    #[test]
    fn playing_through_an_endless_queue_does_not_grow_it() {
        let mut now = playing();
        now.queue = queued(1);
        now.playing = Some(0);

        let mut next = 1;
        for _ in 0..(QUEUE_MEMORY * 4) {
            if now.remaining() < QUEUE_LOW {
                // A page, as a continuation delivers one.
                let page: Vec<Track> = (next..next + 25).map(numbered).collect();
                next += 25;
                assert!(now.absorb(page) > 0, "a page of new tracks must land");
            }
            now.playing = Some(now.playing.unwrap() + 1);
            now.trim();
        }

        assert!(
            now.dropped.len() <= QUEUE_MEMORY,
            "remembered {} ids, over the {QUEUE_MEMORY} bound",
            now.dropped.len()
        );
        assert!(
            now.queue.len() <= QUEUE_BEHIND + 1 + QUEUE_AHEAD,
            "held {} tracks, wider than the window",
            now.queue.len()
        );
        assert_eq!(
            now.playing,
            Some(QUEUE_BEHIND),
            "the window settles with the playing track a fixed way into it"
        );
    }

    /// Nothing playing inside the queue is not the same as a queue with plenty
    /// left: reporting its length would top up a queue standing still.
    #[test]
    fn a_queue_nothing_is_playing_in_has_nothing_remaining() {
        let mut now = playing();
        now.queue = queued(30);
        now.playing = None;

        assert_eq!(now.remaining(), 0);
    }

    #[test]
    fn audio_outputs_cycle_through_the_system_default() {
        let outputs = vec![
            OutputDevice {
                id: "speakers".to_string(),
                name: "Speakers".to_string(),
            },
            OutputDevice {
                id: "headphones".to_string(),
                name: "Headphones".to_string(),
            },
        ];

        assert_eq!(
            next_output_device(None, &outputs, true).as_deref(),
            Some("speakers")
        );
        assert_eq!(
            next_output_device(Some("speakers"), &outputs, true).as_deref(),
            Some("headphones")
        );
        assert_eq!(next_output_device(Some("headphones"), &outputs, true), None);
        assert_eq!(
            next_output_device(None, &outputs, false).as_deref(),
            Some("headphones")
        );
        // A disconnected saved device starts cycling from the usable default.
        assert_eq!(
            next_output_device(Some("missing"), &outputs, true).as_deref(),
            Some("speakers")
        );
    }

    /// A continuation that carried the queue forward is trusted about where the
    /// next page lives, and leaves the station's name alone.
    #[test]
    fn a_page_that_carried_the_queue_forward_is_paged_again() {
        let mut now = playing();
        now.queue = queued(3);
        now.playing = Some(0);
        now.queue_title = "Let It Happen Mix".to_string();
        now.continuation = Some("FIRST".to_string());
        now.topup_failures = 2;

        let added = now.take_page(QueuePage {
            tracks: queued(8).split_off(3),
            continuation: Some("SECOND".to_string()),
            title: None,
        });

        assert_eq!(added, 5);
        assert_eq!(now.continuation.as_deref(), Some("SECOND"));
        assert_eq!(
            now.topup_failures, 0,
            "a page that worked clears what earlier failures had counted up"
        );
        assert_eq!(
            now.queue_title, "Let It Happen Mix",
            "more of the same station must not rename it"
        );
    }

    /// A page whose every track is already held teaches the queue nothing, and
    /// the token that bought it will buy the same page again. Giving it up is
    /// what lets the next top-up fall through to the journal instead of
    /// spending a round trip per track re-learning the same thing.
    #[test]
    fn a_page_of_nothing_new_gives_up_the_token() {
        let mut now = playing();
        now.queue = queued(3);
        now.playing = Some(0);
        now.continuation = Some("TOKEN".to_string());

        let added = now.take_page(QueuePage {
            tracks: queued(3),
            continuation: Some("WOULD_BUY_THE_SAME_AGAIN".to_string()),
            title: None,
        });

        assert_eq!(added, 0);
        assert_eq!(now.queue.len(), 3, "nothing was appended");
        assert!(
            now.continuation.is_none(),
            "the offered token would buy this same page again"
        );
        assert_eq!(
            now.topup_failures, 1,
            "a station repeating itself has to count against the budget too"
        );
    }

    /// The handover to the journal: a station built locally is a different
    /// station, and the panel has to say so rather than keep the old name over
    /// entirely different music.
    #[test]
    fn a_seeded_station_renames_the_queue_and_is_paged_from_there() {
        let mut now = playing();
        now.queue = queued(2);
        now.playing = Some(1);
        now.queue_title = "Let It Happen Mix".to_string();
        // Tier one has already given up: this is the fallback landing.
        now.continuation = None;
        now.topup_failures = 1;

        let added = now.take_page(QueuePage {
            tracks: queued(20).split_off(2),
            continuation: Some("STATION_PAGE_TWO".to_string()),
            title: Some("Station from Currents".to_string()),
        });

        assert_eq!(added, 18);
        assert_eq!(now.queue_title, "Station from Currents");
        assert_eq!(
            now.continuation.as_deref(),
            Some("STATION_PAGE_TWO"),
            "the new station is YouTube's to page from here on"
        );
        assert_eq!(now.topup_failures, 0);
    }

    /// The window has a ceiling as well as a floor.
    #[test]
    fn the_queue_does_not_grow_past_the_window() {
        let mut now = playing();
        now.queue = queued(2);
        now.playing = Some(0);

        let page: Vec<Track> = queued(QUEUE_AHEAD + 20).split_off(2);
        now.absorb(page);

        assert_eq!(now.queue.len(), QUEUE_AHEAD + 1);
        assert_eq!(now.remaining(), QUEUE_AHEAD);
    }

    /// A top-up arrives below the low-water mark and may contain sixty rows.
    /// The continuation it carries starts after all sixty, so all must fit or
    /// the omitted tail can never be requested again.
    #[test]
    fn a_complete_continuation_page_fits_at_the_low_water_mark() {
        let mut now = playing();
        now.queue = queued(QUEUE_LOW);
        now.playing = Some(0);

        let page: Vec<Track> = (QUEUE_LOW..QUEUE_LOW + 60).map(numbered).collect();
        assert_eq!(now.absorb(page), 60);
        assert_eq!(now.remaining(), QUEUE_LOW - 1 + 60);
    }
}
