//! Current-track state and bounded queue operations, without service dispatch.

use std::collections::VecDeque;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::source::{ArtistRef, BrowseEndpoint, Track, lrclib};
use crate::source::home::{Card, Shelf};
use crate::source::watch::{Comments, Lyrics, QueuePage};
use crate::source::youtube::extract_video_id;

/// Tracks kept behind the one playing when the queue is trimmed.
///
/// A radio queue has no end, so it cannot simply be accumulated: a session left
/// running would grow the heap by a page every hour and a half, which is the
/// one thing this program is built not to do. What is kept instead is a window
/// around the playing track -- deep enough behind that `p` still walks back
/// through an evening's listening, and shallow enough that the queue costs a
/// few kilobytes however long the session runs.
pub(super) const QUEUE_BEHIND: usize = 20;

/// Tracks kept ahead of the one playing. Large enough for the five-track
/// low-water mark plus a complete 60-row continuation page: advancing the
/// continuation token after retaining only part of that page loses its tail.
pub(super) const QUEUE_AHEAD: usize = 65;

/// Ids remembered after they leave the window, so a radio that comes back round
/// to a track does not replay it.
///
/// A radio legitimately repeats over hours; an endless queue that loops the
/// same eight songs is worse than one that ends honestly. Bounded because this
/// outlives everything else here: at eleven characters an id, this is a couple
/// of kilobytes.
pub(super) const QUEUE_MEMORY: usize = 200;

/// The sidebar's visible tab, or the expanded player's keyboard-focused panel.
/// Expanded playback retains a separate detail tab above the lyrics.
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

    pub(super) fn next(self) -> Self {
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
    pub(super) fn shifted(self, delta: isize) -> Self {
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

impl<T> Panel<T> {
    /// True when this panel has not been asked for yet, which is the only state
    /// a tab switch should start a fetch from -- otherwise opening the same tab
    /// twice would fetch it twice.
    pub(super) fn is_idle(&self) -> bool {
        matches!(self, Self::Idle)
    }
}

/// What is playing, and everything the player page shows around it.
///
/// Held whole rather than as loose fields on [`super::App`] so that starting a track
/// replaces it in one move: every panel here belongs to one video id, and a
/// half-replaced page would show one track's lyrics under another's title.
pub struct NowPlaying {
    pub video_id: String,
    pub title: String,
    pub artist: String,
    pub artist_ref: Option<ArtistRef>,
    pub album: Option<String>,
    pub album_route: Option<(String, BrowseEndpoint)>,
    pub artwork_url: Option<String>,
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
    /// Explicit artist lists finish at their last song rather than starting radio.
    pub finite_queue: bool,
    /// Keep rows still while selecting and reordering; scroll only at an edge.
    pub queue_offset: usize,
    /// Identifies the queue itself, as opposed to the track playing inside it.
    ///
    /// A queue outlives the page around it: advancing through a radio replaces
    /// this whole struct on every track while carrying the same queue across,
    /// so `video_id` cannot say whether a page of tracks still belongs here.
    /// This changes only when the queue is genuinely replaced, which is exactly
    /// when a top-up in flight has become stale. See [`crate::source::worker::Request::MoreQueue`].
    pub(super) queue_epoch: u64,
    /// What to redeem for the next page of the queue.
    ///
    /// `None` in three different cases the queue does not need to tell apart:
    /// no queue yet, a finite queue that has run out, and a queue that has
    /// given up paging itself. All three mean the same thing here -- there is
    /// nothing more to ask for -- and the queue simply ends as it always did.
    pub(super) continuation: Option<String>,
    /// Set while a page is in flight, so the low-water check fires once rather
    /// than on every track for as long as the round trip takes.
    pub(super) topping_up: bool,
    /// Consecutive failed top-ups. See [`super::playback::MAX_TOPUP_FAILURES`].
    pub(super) topup_failures: u8,
    /// Ids that have been in this queue and have since left the window.
    ///
    /// Oldest first, bounded at [`QUEUE_MEMORY`]. What stops a radio that comes
    /// back round to a track from queueing it again behind itself.
    pub(super) dropped: VecDeque<String>,
    /// Where in `queue` the playing track is. `None` until the queue lands, or
    /// when what is playing is not in it.
    pub playing: Option<usize>,
    pub liked: Option<bool>,
    pub like_pending: bool,
    pub(super) rating_request: u64,
    pub repeat: RepeatMode,
    pub tab: Tab,
    /// Last non-lyrics panel, retained while lyrics have keyboard focus.
    pub detail_tab: Tab,
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
    /// An explicit reading view; ordinary playback keeps the large artwork.
    pub lyrics_expanded: bool,
    pub lyrics: Panel<Lyrics>,
    pub(super) lyrics_attempts: u8,
    pub(super) lyrics_retry_at: Option<Instant>,
    pub comments: Panel<Comments>,
    pub related: Panel<Vec<Shelf>>,
    /// Browse ids from the watch response, held until the tab they belong to is
    /// opened. `None` means the track has no such tab -- most videos have no
    /// lyrics, and that is not a failure.
    pub(super) lyrics_id: Option<String>,
    pub(super) related_id: Option<String>,
    /// Whether the watch response has come back, successfully or not.
    ///
    /// Distinct from `lyrics_id.is_some()`, which it used to be read as: a
    /// track with no lyrics page is still worth asking LRCLIB about, and only
    /// this says the difference between "there is no page" and "we have not
    /// heard yet".
    pub(super) watched: bool,
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
            artwork_url: None,
            duration: track.duration,
            queue_title: String::new(),
            queue: Vec::new(),
            finite_queue: false,
            queue_offset: 0,
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
            detail_tab: Tab::UpNext,
            cursor: [0; Tab::ALL.len()],
            follow_lyrics: true,
            lyrics_expanded: false,
            lyrics: Panel::Idle,
            lyrics_attempts: 0,
            lyrics_retry_at: None,
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
    pub(super) fn lyrics_query(&self) -> Option<lrclib::Query> {
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

    pub(super) fn cursor_mut(&mut self) -> &mut usize {
        let index = self.tab.index();
        &mut self.cursor[index]
    }

    /// Moves the open tab's cursor, and hands the lyrics panel back to the
    /// user: they have said where they want to be looking, and a panel that
    /// yanks itself back to the singer is one they cannot read ahead in.
    ///
    /// Only the near end is clamped here. The far end is the renderer's, which
    /// is what knows how long each panel is once it has wrapped it.
    pub(super) fn scroll(&mut self, delta: isize) {
        if self.tab == Tab::Lyrics { self.follow_lyrics = false; }
        let cursor = self.cursor_mut();
        *cursor = cursor.saturating_add_signed(delta);
    }

    /// [`Self::scroll`] straight to a line, for `g` and `G`.
    pub(super) fn jump(&mut self, to: usize) {
        if self.tab == Tab::Lyrics { self.follow_lyrics = false; }
        *self.cursor_mut() = to;
    }

    /// Opens a tab, and reports whether that was a change.
    ///
    /// Landing on Lyrics is what asks for the line being sung, whether or not
    /// the tab changed: pressing `2` while already there is how a user who has
    /// scrolled off says "back to the song", and it needs no key of its own.
    /// The caller uses the return to decide whether to start a fetch.
    pub(super) fn open(&mut self, tab: Tab) -> bool {
        if tab == Tab::Lyrics {
            self.follow_lyrics = true;
        }
        let changed = self.tab != tab;
        self.tab = tab;
        if tab != Tab::Lyrics { self.detail_tab = tab; }
        changed
    }

    /// Replaces the deliberately thin metadata used for a pasted URL or bare
    /// id with the real row from the queue once the watch response arrives.
    pub(super) fn hydrate_from_queue(&mut self) -> Option<Track> {
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
    pub(super) fn next_in_queue(&self) -> Option<&Track> {
        self.queue.get(self.playing? + 1)
    }

    pub(super) fn advanced_index(&self, delta: isize, auto: bool) -> Option<usize> {
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
    pub(super) fn insert_user_track(&mut self, track: Track, next: bool) -> bool {
        let Some(playing) = self.playing else {
            return false;
        };
        if self.queue.iter().any(|held| held.id == track.id) {
            return false;
        }

        let ceiling = if self.finite_queue { crate::source::collection::MAX_TRACKS }
            else { playing + 1 + QUEUE_AHEAD };
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
    pub(super) fn remove_selected_upcoming(&mut self) -> Option<Track> {
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
    pub(super) fn move_selected_upcoming(&mut self, delta: isize) -> bool {
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
        let track = self.queue.remove(selected);
        self.queue.insert(moved, track);
        self.cursor[Tab::UpNext.index()] = moved;
        true
    }

    pub(super) fn clear_upcoming(&mut self) -> usize {
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
    pub(super) fn shuffle_upcoming(&mut self) -> usize {
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
    pub(super) fn remaining(&self) -> usize {
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
    pub(super) fn trim(&mut self) {
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
        self.queue_offset = self.queue_offset.saturating_sub(excess);
    }

    /// Appends a page of tracks, and reports how many were new.
    ///
    /// Anything already in the window or recently dropped out of it is
    /// discarded: a radio repeats over hours, and a queue that grows by
    /// re-appending what it just played is a loop rather than an endless queue.
    /// The count is what tells the caller a page of nothing but repeats has
    /// arrived -- the radio has run out of material, and the queue should stop
    /// asking rather than spend a round trip per track discovering it again.
    pub(super) fn absorb(&mut self, tracks: Vec<Track>) -> usize {
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
    pub(super) fn take_page(&mut self, page: QueuePage) -> usize {
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

/// Fisher-Yates with a tiny local xorshift state. Pulling a random-number
/// crate into the player solely to reorder at most 66 already-held tracks
/// would cost more code and dependency surface than the feature warrants.
pub(super) fn shuffle_tracks(tracks: &mut [Track]) {
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
