//! Background worker for source operations.
//!
//! Every yt-dlp call blocks for seconds. Running one on the UI thread would
//! freeze rendering and input for the whole duration, so all of them happen
//! here and results are collected without blocking.
//!
//! yt-dlp work is intentionally serial: concurrent invocations would multiply
//! the ~80 MB transient cost, which is exactly the spike the whole design
//! exists to contain. Covers get a second thread of their own -- they are one
//! HTTPS GET with no subprocess, so they carry none of that cost, and leaving
//! them in the same queue would let a slow thumbnail sit in front of the
//! resolve that actually produces audio.
//!
//! Library calls get a third thread by the same argument: they are pure HTTPS
//! against Google's API, and queueing one behind a four-second resolve would
//! make browsing a playlist feel broken. Signing in gets a fourth, transient
//! one, because it blocks for as long as the user takes to approve a code in a
//! browser -- minutes, potentially -- and no shared queue can absorb that.
//!
//! The player page gets a fifth. Its panels are fetched while music is already
//! playing and nobody is waiting on them, so they must not be able to delay
//! anything that somebody is: a comment section costs two round trips, and
//! sharing the library's queue would put it in front of the playlist the user
//! just asked to open.
//!
//! The landing page's card artwork gets a sixth, which is the same argument at
//! a different scale. One cover is one request per track; a screenful of cards
//! is a dozen at once, and a dozen pictures of things the user has not chosen
//! must not be able to queue in front of the picture of the thing they are
//! listening to. It is also the one thread that keeps a connection open between
//! requests -- see [`cover::ArtFetcher`].

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, bail};

use super::auth::{self, Http};
use super::cover::{self, Cover};
use super::home::{self, Shelf};
use super::innertube::InnerTube;
use super::journal::{Journal, Play};
use super::library::{self, Library, Playlist};
use super::{StreamUrl, Track};
use super::{browser, lrclib, sapisid, stats, watch};
use crate::config::{Cookies, Credentials, Import, Tokens};
use crate::source::youtube::YouTube;
use mtui_resolver::{PlaybackSession, ResolveRequest, Resolver};

/// Ceiling on how much of a playlist is loaded.
///
/// Bounded for the same reason [`crate::source::youtube::MAX_RESULTS`] is: a
/// long session must not be able to grow the heap without limit. Beyond this,
/// each further fifty rows is two more round trips for lines nobody scrolls to.
const MAX_LIBRARY_TRACKS: usize = 200;

/// Likes read to build the shelves on the landing page.
///
/// Two pages of the Data API, so two of its 10,000 daily quota units. Deep
/// enough that the oldest of them are genuinely older than the newest -- which
/// is the whole of what separates "Forgotten favorites" from "Listen again" --
/// and shallow enough not to spend a page request per shelf card.
const MAX_PERSONAL_LIKES: usize = 100;

pub enum Request {
    Search {
        query: String,
        limit: usize,
    },
    /// `title` is carried through so the UI can label playback without
    /// re-looking-up the track when the response arrives.
    Resolve {
        id: String,
        title: String,
        /// Set when recovering a track whose stream died. The cached URL is
        /// then the one that just failed, so answering from cache would hand
        /// back exactly what is being recovered from.
        bypass_cache: bool,
    },
    /// Resolve through the fast player API before the user has committed to
    /// anything, so Enter can use a warm URL without speculative yt-dlp work.
    Prefetch {
        id: String,
    },
    /// Thumbnail for a track. Runs on its own thread; see the module header.
    Cover {
        id: String,
    },
    /// Artwork for one card of the landing page.
    ///
    /// A sixth thread rather than the cover one, for the reason that gave the
    /// cover its own: opening the landing page asks for a dozen of these at
    /// once, and a burst of decoration must not be able to sit in front of the
    /// picture of the track that is actually playing.
    ///
    /// `key` is what the answer is filed under and `url` is what to fetch --
    /// two fields because a card's artwork lives at an opaque CDN address that
    /// nothing can be derived from. See [`crate::source::home::Card::art_key`].
    Art {
        key: String,
        url: String,
    },
    /// The landing page: YouTube Music's own home feed. Runs on the library
    /// thread, which is where the cookie and the session both live.
    Home,
    /// The shelves built from this account's listening, which arrive after the
    /// feed rather than with it -- see [`Response::PersonalShelves`].
    ///
    /// `rotation` steps the radio seeds along, so asking again gives a
    /// different page rather than the one already on screen.
    PersonalShelves {
        rotation: usize,
    },
    /// Reads a YouTube session out of whichever browser has one.
    ///
    /// The cookie is the only credential InnerTube accepts, and having the user
    /// fetch one by hand every few weeks is the thing this avoids. `force`
    /// re-reads even when there is already a usable cookie, which is what makes
    /// this a recovery for one that has expired as well as a first-run step.
    ImportCookies {
        force: bool,
    },
    /// A finished play: how far the user actually got through a track.
    ///
    /// Journalled locally, which is what the shelves above are ranked from, and
    /// -- when a cookie allows it -- reported to YouTube so the same play shows
    /// up in their history everywhere else. Answers with nothing on success;
    /// the user did not ask for this and there is nothing to tell them.
    ReportPlay {
        track: Track,
        listened: Duration,
    },
    /// Tracks behind a card that browses rather than plays -- an album, a
    /// playlist or an artist. `title` is carried through to label the list.
    OpenBrowse {
        browse_id: String,
        title: String,
    },
    /// The queue a track plays inside, asked for the moment it starts. This is
    /// what decides what plays when it ends, so it is the one part of the
    /// player page fetched without being asked for.
    Watch {
        video_id: String,
    },
    /// The next page of the queue, asked for while there is still some left to
    /// play. This is what keeps "Up next" from ever running out.
    ///
    /// Identified by `epoch` rather than by a video id, unlike every other
    /// request here. A queue outlives the track it was fetched for -- advancing
    /// through a radio keeps one queue across many tracks -- so a video id
    /// would discard a page that arrived one track late even though it belongs
    /// to the very queue that asked for it. The epoch changes only when the
    /// queue itself is replaced, which is exactly the case where a page in
    /// flight has become stale.
    MoreQueue {
        epoch: u64,
        token: String,
    },
    /// A fresh station for a queue that has run out of pages, built from the
    /// play journal rather than from anything YouTube handed back.
    ///
    /// Answered with [`Response::MoreQueue`], like the request above: from the
    /// queue's point of view a page of tracks is a page of tracks, and where it
    /// was found is this thread's business. `rotation` steps the seed along so
    /// a second attempt builds a different station.
    SeedQueue {
        epoch: u64,
        rotation: usize,
    },
    /// One panel of the player page, fetched when the user opens that tab. The
    /// video id travels with each so a response that arrives after the user has
    /// skipped on can be discarded; the browse id is what [`Request::Watch`]
    /// handed back.
    ///
    /// Lyrics take theirs as an `Option`, unlike the panels below: a track with
    /// no lyrics page on YouTube is exactly the track [`lrclib`] exists to
    /// answer for, so "there is no browse id" has to reach the worker rather
    /// than stopping the request. `query` is what LRCLIB is asked with, and is
    /// `None` when the track is too thinly named to ask about at all.
    Lyrics {
        video_id: String,
        browse_id: Option<String>,
        query: Option<lrclib::Query>,
    },
    Related {
        video_id: String,
        browse_id: String,
    },
    Comments {
        video_id: String,
    },
    /// Begin the Google device flow. Answers with [`Response::DeviceCode`]
    /// almost immediately, then with [`Response::SignedIn`] whenever the user
    /// gets round to approving it.
    SignIn,
    /// Forget the stored tokens. Local only -- it does not revoke the grant.
    SignOut,
    /// The signed-in user's playlists.
    Playlists,
    /// Contents of one playlist. `title` is carried through so the UI can label
    /// the list without holding the playlist it came from.
    OpenPlaylist {
        id: String,
        title: String,
    },
    AddToPlaylist {
        playlist_id: String,
        /// For the confirmation message, which is the only feedback a write
        /// gets -- the row it affects is not on screen.
        playlist_title: String,
        video_id: String,
    },
    /// Removes one row from the playlist currently open. Takes the playlist
    /// *item* id, not the video id; see [`Track::playlist_item_id`].
    RemoveFromPlaylist {
        playlist_item_id: String,
        title: String,
    },
    /// Likes or unlikes, whichever the current rating is not.
    ToggleLike {
        video_id: String,
        title: String,
    },
    Shutdown,
}

pub enum Response {
    Results(Vec<Track>),
    /// The landing page.
    Home(Vec<Shelf>),
    /// A browser session was read successfully. Names the browser it came from,
    /// because that is the part worth telling the user: it is the difference
    /// between "it worked" and "it worked, and here is what it read".
    ///
    /// The UI answers this by asking for the landing page again -- the page on
    /// screen was built without a cookie, and there is now a better one to be
    /// had.
    CookiesImported(String),
    /// Shelves built from the user's liked songs, to go in front of the feed.
    ///
    /// A second response rather than part of [`Self::Home`] because it costs
    /// three more round trips: sending them together would hold a landing page
    /// that is ready back behind one that is not. Empty when signed out, or
    /// when a cookie already bought the real shelves.
    PersonalShelves(Vec<Shelf>),
    /// Contents of something opened from the landing page. Distinct from
    /// [`Self::PlaylistTracks`] because these rows are not the user's: they
    /// belong to a YouTube Music album or a stranger's playlist, so there is
    /// nothing here that removing a row could apply to.
    Browsed {
        title: String,
        tracks: Vec<Track>,
    },
    /// A resolve finished, whichever way it went.
    ///
    /// `id` comes back so a response for a track the user has already moved on
    /// from is dropped rather than played under the title of the one they are
    /// now looking at. That is the single most visible way a serial worker can
    /// lie about what is playing, and the id is the whole of what prevents it.
    ///
    /// The failure travels here rather than as [`Self::Failed`] for the same
    /// reason: only a failure that can be tied to a track can be told from one
    /// belonging to some library call that happened to finish at the same time.
    Resolved {
        id: String,
        title: String,
        stream: Result<StreamUrl, String>,
    },
    /// The player page's queue. `video_id` comes back so a queue for a track
    /// the user has already skipped past is dropped rather than shown.
    ///
    /// Every panel below follows the same rule, and every one of them reports
    /// failure as an empty panel: these are decoration around music that is
    /// already playing, and replacing the status line with "lyrics failed"
    /// would be trading something the user is using for something they are not.
    Watch {
        video_id: String,
        watch: Box<Result<crate::source::watch::Watch, String>>,
    },
    /// More queue, for the queue that asked. See [`Request::MoreQueue`] on why
    /// this is matched by epoch rather than by video id.
    MoreQueue {
        epoch: u64,
        page: Result<crate::source::watch::QueuePage, String>,
    },
    Lyrics {
        video_id: String,
        lyrics: Result<crate::source::watch::Lyrics, String>,
    },
    Related {
        video_id: String,
        related: Result<Vec<Shelf>, String>,
    },
    Comments {
        video_id: String,
        comments: Box<Result<crate::source::watch::Comments, String>>,
    },
    /// A speculative resolve finished. `ready` is false when it failed --
    /// deliberately not a `Failed`, since the user never asked for this and can
    /// do nothing about it; the error resurfaces if they do play the track.
    Prefetched {
        id: String,
        ready: bool,
    },
    /// `art` is `None` when the fetch or decode failed. The id comes back so a
    /// response that arrives after the user has moved on can be discarded.
    Cover {
        id: String,
        art: Option<Cover>,
    },
    /// One card's artwork. `art` is `None` when there was no usable picture,
    /// which the cache records as firmly as a success -- see
    /// [`crate::art::ArtCache::store`].
    Art {
        key: String,
        art: Option<Cover>,
    },
    /// The code the user has to approve, and where to approve it. Sent as soon
    /// as Google issues it, well before the sign-in finishes.
    DeviceCode {
        user_code: String,
        url: String,
        /// Seconds Google will keep accepting this code. Sent so the panel can
        /// count down: the user has to leave the terminal to approve it, and
        /// "this stopped working four minutes ago" is worth knowing before
        /// typing the code rather than after.
        expires_in: u64,
    },
    SignedIn,
    SignedOut,
    /// The device flow ended without a session. Separate from [`Self::Failed`]
    /// so the UI can keep it in the sign-in panel, where the user is already
    /// looking and where the retry key lives, instead of the status bar.
    SignInFailed(String),
    /// A library request arrived with no session. The UI turns this into a
    /// sign-in rather than an error, since asking for the library is a clear
    /// enough statement of intent.
    NeedsSignIn,
    Playlists(Vec<Playlist>),
    /// Contents of an opened playlist. `id` and `title` come back so the UI can
    /// label the list and know which playlist a later removal applies to.
    PlaylistTracks {
        id: String,
        title: String,
        tracks: Vec<Track>,
    },
    /// A row was removed from the open playlist. The id lets the UI drop it
    /// locally rather than re-fetching the whole playlist to lose one line.
    Removed {
        playlist_item_id: String,
        title: String,
    },
    Liked {
        title: String,
        liked: bool,
    },
    /// A write succeeded. Carries the message to show, already formatted.
    Done(String),
    /// Human-readable failure, already formatted for display.
    Failed(String),
}

/// Handle to the worker threads. Both answer into one response channel, so the
/// UI drains a single queue regardless of which thread did the work.
pub struct SourceWorker {
    tx: Sender<Request>,
    cover_tx: Sender<Request>,
    library_tx: Sender<Request>,
    page_tx: Sender<Request>,
    art_tx: Sender<Request>,
    /// Kept so a sign-in thread can be handed somewhere to answer. Sign-in is
    /// spawned on demand rather than kept resident: it runs at most once a
    /// session and would otherwise be a thread asleep for the whole run.
    res_tx: Sender<Response>,
    rx: Receiver<Response>,
    /// How many resolves have been asked for. The source thread counts the ones
    /// it has taken off the queue against this, which is what lets it tell a
    /// resolve the user is waiting on from one they have already skipped past --
    /// see [`run`]. Shared rather than passed in the request because the count
    /// has to be readable *while* a request sits in the queue.
    resolves: Arc<AtomicU64>,
    handle: Option<thread::JoinHandle<()>>,
}

impl SourceWorker {
    pub fn spawn(yt: YouTube) -> Result<Self> {
        let (req_tx, req_rx) = channel::<Request>();
        let (cover_req_tx, cover_req_rx) = channel::<Request>();
        let (library_req_tx, library_req_rx) = channel::<Request>();
        let (page_req_tx, page_req_rx) = channel::<Request>();
        let (art_req_tx, art_req_rx) = channel::<Request>();
        let (res_tx, res_rx) = channel::<Response>();
        let resolves = Arc::new(AtomicU64::new(0));
        let thread_resolves = Arc::clone(&resolves);
        let cover_res_tx = res_tx.clone();
        let library_res_tx = res_tx.clone();
        let page_res_tx = res_tx.clone();
        let art_res_tx = res_tx.clone();
        let spawn_res_tx = res_tx.clone();

        // Cloned rather than moved: it is a path, and the library thread runs
        // the same binary to read browser cookies.
        let library_yt = yt.clone();

        let handle = thread::Builder::new()
            .name("mtui-source".to_string())
            .spawn(move || run(yt, req_rx, res_tx, &thread_resolves))
            .context("failed to spawn source worker")?;

        // Both deliberately detached -- see `Drop`.
        thread::Builder::new()
            .name("mtui-cover".to_string())
            .spawn(move || run_covers(cover_req_rx, cover_res_tx))
            .context("failed to spawn cover worker")?;

        thread::Builder::new()
            .name("mtui-library".to_string())
            .spawn(move || run_library(library_yt, library_req_rx, library_res_tx))
            .context("failed to spawn library worker")?;

        thread::Builder::new()
            .name("mtui-page".to_string())
            .spawn(move || run_pages(page_req_rx, page_res_tx))
            .context("failed to spawn player page worker")?;

        thread::Builder::new()
            .name("mtui-art".to_string())
            .spawn(move || run_art(&art_req_rx, &art_res_tx))
            .context("failed to spawn artwork worker")?;

        Ok(Self {
            tx: req_tx,
            cover_tx: cover_req_tx,
            library_tx: library_req_tx,
            page_tx: page_req_tx,
            art_tx: art_req_tx,
            res_tx: spawn_res_tx,
            rx: res_rx,
            resolves,
            handle: Some(handle),
        })
    }

    pub fn send(&self, req: Request) -> Result<()> {
        // Counted before the request is queued, so that a resolve already
        // sitting in the queue can see that a newer one exists behind it. That
        // order is the whole point, and it is why the count has to be given
        // back when the queueing then fails: a resolve counted but never queued
        // leaves `asked` permanently ahead of what the source thread can take,
        // and every later resolve reads as superseded and is dropped in
        // silence. Only reachable once that thread is already gone -- but "the
        // worker died and now nothing plays" is a far worse way to find out
        // than the error this returns.
        let counted = matches!(req, Request::Resolve { .. });
        if counted {
            self.resolves.fetch_add(1, Ordering::SeqCst);
        }
        let sent = self.route(req);
        if counted && sent.is_err() {
            self.resolves.fetch_sub(1, Ordering::SeqCst);
        }
        sent
    }

    /// Hands one request to the thread that runs it.
    ///
    /// Split out of [`Self::send`] so the resolve count above has a single
    /// place to be undone, rather than one per routing arm.
    fn route(&self, req: Request) -> Result<()> {
        match req {
            Request::Cover { .. } => self.cover_tx.send(req).context("cover worker is gone"),
            Request::Art { .. } => self.art_tx.send(req).context("artwork worker is gone"),
            // Not queued anywhere: this one blocks for as long as the user
            // takes to approve a code, so it gets a thread that exists only for
            // the duration of the flow.
            Request::SignIn => {
                spawn_sign_in(self.res_tx.clone());
                Ok(())
            }
            Request::Watch { .. }
            | Request::MoreQueue { .. }
            | Request::SeedQueue { .. }
            | Request::Lyrics { .. }
            | Request::Related { .. }
            | Request::Comments { .. } => {
                self.page_tx.send(req).context("player page worker is gone")
            }
            Request::SignOut
            | Request::Home
            | Request::PersonalShelves { .. }
            | Request::ReportPlay { .. }
            | Request::ImportCookies { .. }
            | Request::OpenBrowse { .. }
            | Request::Playlists
            | Request::OpenPlaylist { .. }
            | Request::AddToPlaylist { .. }
            | Request::RemoveFromPlaylist { .. }
            | Request::ToggleLike { .. } => {
                self.library_tx.send(req).context("library worker is gone")
            }
            _ => self.tx.send(req).context("source worker is gone"),
        }
    }

    /// Non-blocking poll, called once per UI frame. `None` means nothing new.
    ///
    /// A disconnected channel reads the same as an empty one on purpose: every
    /// worker is detached except the source thread, so a dead one must leave
    /// the UI drawing and playing rather than wedging it. Spelled out rather
    /// than written as `.ok()` -- which is what the allow below is for -- so
    /// that treating the two the same is visibly a decision.
    #[allow(clippy::manual_ok_err)]
    pub fn poll(&self) -> Option<Response> {
        match self.rx.try_recv() {
            Ok(res) => Some(res),
            Err(TryRecvError::Empty | TryRecvError::Disconnected) => None,
        }
    }
}

/// Names a request, for the asserts that catch a misrouted one.
///
/// Only ever reached from a `debug_assert!` that has already failed, so what it
/// is worth is naming *which* request went to the wrong thread -- "a request"
/// would leave whoever hit it re-reading [`SourceWorker::send`] to guess.
fn name(req: &Request) -> &'static str {
    match req {
        Request::Search { .. } => "Search",
        Request::Resolve { .. } => "Resolve",
        Request::Prefetch { .. } => "Prefetch",
        Request::Cover { .. } => "Cover",
        Request::Art { .. } => "Art",
        Request::Home => "Home",
        Request::PersonalShelves { .. } => "PersonalShelves",
        Request::ImportCookies { .. } => "ImportCookies",
        Request::ReportPlay { .. } => "ReportPlay",
        Request::OpenBrowse { .. } => "OpenBrowse",
        Request::Watch { .. } => "Watch",
        Request::MoreQueue { .. } => "MoreQueue",
        Request::SeedQueue { .. } => "SeedQueue",
        Request::Lyrics { .. } => "Lyrics",
        Request::Related { .. } => "Related",
        Request::Comments { .. } => "Comments",
        Request::SignIn => "SignIn",
        Request::SignOut => "SignOut",
        Request::Playlists => "Playlists",
        Request::OpenPlaylist { .. } => "OpenPlaylist",
        Request::AddToPlaylist { .. } => "AddToPlaylist",
        Request::RemoveFromPlaylist { .. } => "RemoveFromPlaylist",
        Request::ToggleLike { .. } => "ToggleLike",
        Request::Shutdown => "Shutdown",
    }
}

impl Drop for SourceWorker {
    fn drop(&mut self) {
        let _ = self.tx.send(Request::Shutdown);
        let _ = self.cover_tx.send(Request::Shutdown);
        let _ = self.library_tx.send(Request::Shutdown);
        let _ = self.page_tx.send(Request::Shutdown);
        // Only the source thread is joined. The cover thread may be most of a
        // ten-second timeout into a fetch, and making the user wait that out to
        // quit -- for a picture that is already off screen -- would be absurd.
        // The library, page and sign-in threads are the same case, and the
        // sign-in one may be asleep between polls for another five seconds on
        // top. None of them owns a subprocess, so letting the process exit
        // under them is safe.
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// The source thread: searches and resolves, one at a time.
///
/// `asked` is how many resolves the UI has sent. Counting the ones taken off
/// the queue against it is what makes skipping through tracks responsive: a
/// resolve costs seconds, so a user who presses Enter four times would
/// otherwise wait out all four before hearing the fourth. Only the last of a
/// run is worth doing, and a speculative prefetch queued in front of one is
/// worth nothing at all.
fn run(yt: YouTube, rx: Receiver<Request>, tx: Sender<Response>, asked: &AtomicU64) {
    // Owned outright by this thread, so its bounded cache and pooled playback
    // client need no locks. Search keeps the older shared client below because
    // it also serves the home and queue parsers.
    let mut resolver = match Resolver::new(yt.bin()) {
        Ok(resolver) => resolver,
        Err(error) => {
            let _ = tx.send(Response::Failed(error.to_string()));
            return;
        }
    };
    resolver.set_js_runtime(yt.js_runtime().map(str::to_string));
    // Resolves taken off the queue. Behind `asked` exactly when the queue holds
    // one the user asked for more recently than the one in hand.
    let mut taken = 0u64;
    // Built once so its connection pool and TLS session outlive a single track.
    // `None` means the fast path is simply unavailable and every resolve goes
    // to yt-dlp -- slower, but no less correct.
    let tube = InnerTube::new().ok();

    while let Ok(req) = rx.recv() {
        let response = match req {
            Request::Search { query, limit } => match search(&yt, tube.as_ref(), &query, limit) {
                Ok(tracks) => Response::Results(tracks),
                Err(e) => Response::Failed(format!("{e:#}")),
            },
            Request::Resolve {
                id,
                title,
                bypass_cache,
            } => {
                resolver.set_session(playback_session());
                taken += 1;
                // A newer play is already queued behind this one, so nobody is
                // waiting on it any more. Spending seconds of yt-dlp on it
                // would only push the track the user is actually waiting for
                // that much further out. Answered with silence rather than a
                // failure: the request that superseded it is the one that owes
                // the UI an answer.
                if taken < asked.load(Ordering::SeqCst) {
                    continue;
                }
                let stream = resolver
                    .resolve(ResolveRequest {
                        video_id: &id,
                        bypass_cache,
                    })
                    .map_err(|error| error.to_string());
                Response::Resolved { id, title, stream }
            }
            Request::Prefetch { id } => {
                resolver.set_session(playback_session());
                // Nothing speculative may run in front of a play. The cache is
                // still worth consulting -- it is a scan of 32 entries -- so an
                // already-warm track is still reported as warm.
                let ready = if taken < asked.load(Ordering::SeqCst) {
                    resolver.is_cached(&id)
                } else {
                    resolver.prefetch_fast(&id)
                };
                Response::Prefetched { id, ready }
            }
            Request::Shutdown => break,
            // Routed to the cover, library, page or sign-in threads by `send`.
            // Reaching here would mean that routing and this match had drifted
            // apart -- ignored rather than answered, since replying to a
            // request this thread never ran would be worse than silence. The
            // assert makes that drift fail a test run instead of costing the
            // user a request that is answered by nobody.
            other => {
                debug_assert!(false, "{} was routed to the source thread", name(&other));
                continue;
            }
        };

        // A send error means the UI is already gone; stop rather than
        // continuing to run expensive subprocesses for nobody.
        if tx.send(response).is_err() {
            break;
        }
    }
}

/// Searches for songs, preferring YouTube Music to YouTube at large.
///
/// The difference is what comes back, not just how fast. Plain `ytsearch`
/// returns reaction videos, hour-long mix compilations, full concert uploads
/// and duplicate reuploads alongside the actual songs; the music corpus returns
/// songs with a real artist and album. yt-dlp still answers when the fast path
/// cannot, so an unrecognised response shape degrades to the old results rather
/// than to no results.
fn search(yt: &YouTube, tube: Option<&InnerTube>, query: &str, limit: usize) -> Result<Vec<Track>> {
    match tube.and_then(|t| t.search(query, limit).ok()) {
        Some(tracks) => Ok(tracks),
        None => yt.search(query, limit),
    }
}

/// The account session available to playback right now. Read immediately before
/// resolving so a browser import, manual cookie change, or stale-session removal
/// takes effect without copying credentials into worker requests.
fn playback_session() -> Option<PlaybackSession> {
    Cookies::available().ok().flatten().map(|cookies| {
        PlaybackSession::new(cookies.header().to_string(), cookies.sapisid().to_string())
    })
}

/// Runs the Google device flow to completion on a thread of its own.
///
/// Detached, and short-lived by construction: it ends when the user approves
/// the code, declines it, or lets it expire. Nothing joins it, because the only
/// thing it holds is a socket.
fn spawn_sign_in(tx: Sender<Response>) {
    // Cloned before the move so there is still a way to report the one failure
    // the thread itself cannot: not starting at all. Otherwise the user presses
    // a key and watches nothing happen.
    let report = tx.clone();
    let spawned = thread::Builder::new()
        .name("mtui-signin".to_string())
        .spawn(move || {
            let result = sign_in(&tx);
            let _ = match result {
                Ok(()) => tx.send(Response::SignedIn),
                Err(e) => tx.send(Response::SignInFailed(format!("{e:#}"))),
            };
        });

    if spawned.is_err() {
        let _ = report.send(Response::SignInFailed(
            "could not start the sign-in thread".to_string(),
        ));
    }
}

fn sign_in(tx: &Sender<Response>) -> Result<()> {
    let http = Http::new()?;
    let creds = Credentials::load()?;
    let code = auth::start(&http, &creds)?;

    // Sent before the wait, not after: this is the whole point of the device
    // flow, and the user cannot approve a code they have not been shown.
    let _ = tx.send(Response::DeviceCode {
        user_code: code.user_code.clone(),
        url: code.verification_url.clone(),
        expires_in: code.expires_in().as_secs(),
    });

    let tokens = auth::wait_for_approval(&http, &creds, &code)?;
    tokens.save()
}

fn run_library(yt: YouTube, rx: Receiver<Request>, tx: Sender<Response>) {
    // Built once so the connection pool and TLS session outlive a single call,
    // for the same reason `InnerTube` holds its own.
    let mut library = Library::new();
    // Read once at launch and owned by this thread alone, which is what lets it
    // be appended to without a lock: every play is reported here, and every
    // shelf is built here from what it holds.
    let mut journal = Journal::load();

    while let Ok(req) = rx.recv() {
        if matches!(req, Request::Shutdown) {
            break;
        }

        let library = match library.as_mut() {
            Ok(library) => library,
            // Nothing here can work without it, so every request gets the same
            // honest answer rather than the thread dying silently.
            Err(e) => {
                if tx.send(Response::Failed(format!("{e:#}"))).is_err() {
                    break;
                }
                continue;
            }
        };

        // The sign-in thread writes tokens to disk rather than passing them
        // across a channel, so a library that believes it is signed out
        // re-reads before refusing. This is what makes the first request after
        // a sign-in succeed without any explicit handshake.
        if !library.is_signed_in() {
            let _ = library.reload();
        }

        let response = match handle_library(library, &mut journal, &yt, req) {
            Ok(Some(response)) => response,
            // Nothing to report: a shutdown, or a request that answered itself.
            Ok(None) => continue,
            Err(e) => Response::Failed(format!("{e:#}")),
        };

        if tx.send(response).is_err() {
            break;
        }
    }
}

fn handle_library(
    library: &mut Library,
    journal: &mut Journal,
    yt: &YouTube,
    req: Request,
) -> Result<Option<Response>> {
    // Most of what follows needs a session, so the check is made once here
    // rather than repeated in each arm. The UI reads this as "start the sign-in
    // flow", not as an error -- which is exactly why the home feed is exempt:
    // it is fetched on launch, before the user has asked for anything, and
    // answering it with a sign-in nobody requested would be an ambush.
    let exempt = matches!(
        req,
        Request::SignOut
            | Request::Home
            | Request::PersonalShelves { .. }
            // Journalling needs no session at all, and reporting needs a cookie
            // rather than a sign-in. Demanding one would mean a user who never
            // signs in gets no shelves built from their own listening, which is
            // the one part of this that works without Google's permission.
            | Request::ReportPlay { .. }
            // Reading a browser is not a Google call at all.
            | Request::ImportCookies { .. }
            | Request::OpenBrowse { .. }
    );
    if !library.is_signed_in() && !exempt {
        return Ok(Some(Response::NeedsSignIn));
    }

    let response = match req {
        Request::Home => {
            // Deliberately tolerant: a cookie file the user has mistyped is
            // worth telling them about, but not at the price of the landing
            // page, which loads perfectly well without one. The request queued
            // right behind this one is the one that reports it.
            let cookies = Cookies::available().ok().flatten();
            Response::Home(home::fetch(library.http(), cookies.as_ref())?)
        }
        Request::PersonalShelves { rotation } => {
            // Where a broken cookie file is reported, by the `?`. By the time
            // this runs the landing page is already on screen, so the message
            // lands in the status bar next to a working program rather than
            // instead of one.
            let cookies = Cookies::available()?;
            // With a cookie the feed already carries the real shelves, built by
            // YouTube from listening history -- including, once reporting has
            // been running, the plays that happened here. The community shelf
            // is still useful: it turns MTUI's own current listening into public
            // playlists rather than duplicating YouTube's song shelves.
            let community = home::community_playlists(library.http(), journal, rotation);
            if cookies.is_some() {
                return Ok(Some(Response::PersonalShelves(
                    community.into_iter().collect(),
                )));
            }
            // Signed out the like list is simply empty, which the shelf builder
            // handles: with a journal behind it there is still a page to build,
            // and without one there is nothing to build from either way.
            let likes = if library.is_signed_in() {
                library.tracks(library::LIKED_ID, MAX_PERSONAL_LIKES)?
            } else {
                Vec::new()
            };
            let mut shelves = home::personal(library.http(), &likes, journal, rotation);
            if let Some(community) = community {
                // The UI applies the final cross-feed section order after this
                // local shelf is merged with YouTube's own shelves.
                shelves.insert(2.min(shelves.len()), community);
            }
            Response::PersonalShelves(shelves)
        }
        Request::ImportCookies { force } => {
            // Nothing to do when a session is already in hand. `force` is what
            // the recovery path sets, after YouTube has refused the one we had.
            if !force && Cookies::available()?.is_some() {
                return Ok(None);
            }

            // Straight back to whichever browser worked last time, when there
            // is one: probing the whole list costs a process spawn each, and
            // yt-dlp's start-up is the dominant cost in this program.
            let last = Import::load().map(|import| import.browser);
            let (browser, cookies) = match last {
                Some(browser) => match browser::extract(yt, &browser) {
                    Ok(cookies) => (browser, cookies),
                    // It stopped working -- the user switched browsers, or
                    // signed out of that one. Worth the full sweep rather than
                    // reporting a failure that a different browser would answer.
                    Err(_) => browser::detect(yt)?,
                },
                None => browser::detect(yt)?,
            };

            Import {
                browser: browser.clone(),
                header: cookies.header().to_string(),
                at: sapisid::unix_now(),
            }
            .save()?;
            Response::CookiesImported(browser)
        }
        Request::ReportPlay { track, listened } => {
            // Local first, and unconditionally: this is the half that needs no
            // account, no cookie and no network, and it is what MTUI's own
            // shelves are ranked from. A play too short to mean anything is
            // dropped here rather than tested for twice.
            if !journal.record(Play::new(&track, listened)) {
                return Ok(None);
            }

            // Upstream second, and only with a cookie -- there is no other way
            // to attribute a play to an account, for the reason
            // `crate::source::sapisid` documents.
            if let Some(cookies) = Cookies::available().ok().flatten() {
                // Swallowed on purpose. The user did not ask for this, the
                // music already played, and the local journal already has it;
                // replacing the status line with a tracking-endpoint failure
                // would be spending something they are using on something they
                // are not.
                //
                // Except for one failure, which is worth acting on rather than
                // reporting: a player response with no tracking in it means
                // YouTube did not recognise the session. An imported cookie
                // that has expired is exactly what that looks like, and the
                // fix -- read the browser again -- needs no user involvement.
                // Forgetting it here is what makes the next launch do so.
                if let Err(why) = stats::report(library.http(), &cookies, &track.id, listened)
                    && format!("{why:#}").contains(stats::STALE)
                {
                    let _ = Import::forget();
                }
            }
            return Ok(None);
        }
        Request::OpenBrowse { browse_id, title } => Response::Browsed {
            tracks: home::tracks(library.http(), &browse_id)?,
            title,
        },
        Request::SignOut => {
            library.sign_out();
            Tokens::forget()?;
            Response::SignedOut
        }
        Request::Playlists => Response::Playlists(library.playlists()?),
        Request::OpenPlaylist { id, title } => Response::PlaylistTracks {
            tracks: library.tracks(&id, MAX_LIBRARY_TRACKS)?,
            id,
            title,
        },
        Request::AddToPlaylist {
            playlist_id,
            playlist_title,
            video_id,
        } => {
            library.add(&playlist_id, &video_id)?;
            Response::Done(format!("added to {playlist_title}"))
        }
        Request::RemoveFromPlaylist {
            playlist_item_id,
            title,
        } => {
            library.remove(&playlist_item_id)?;
            Response::Removed {
                playlist_item_id,
                title,
            }
        }
        Request::ToggleLike { video_id, title } => {
            // Read before write, so one key is a real toggle rather than two
            // keys the user has to keep straight. The read costs 1 quota unit
            // against the write's 50.
            let liked = !library.is_liked(&video_id)?;
            library.set_like(&video_id, liked)?;
            Response::Liked { title, liked }
        }
        // Routed elsewhere by `send`; reaching here would mean that routing and
        // this match had drifted apart.
        other => {
            debug_assert!(false, "{} was routed to the library thread", name(&other));
            return Ok(None);
        }
    };

    Ok(Some(response))
}

/// Fetches the panels of the player page.
///
/// Every failure is carried in the response rather than raised as a
/// [`Response::Failed`]: these arrive beside music that is already playing, and
/// a lyrics fetch that came back empty is a fact about the track, not an error
/// worth taking the status line away from what is playing.
fn run_pages(rx: Receiver<Request>, tx: Sender<Response>) {
    // Built once, so the connection pool and TLS session survive between
    // tracks -- the same argument `Library` and `InnerTube` make for theirs.
    // Without it there is nothing this thread can do, so it stops rather than
    // answering every request with the same failure.
    let Ok(http) = Http::new() else {
        return;
    };

    while let Ok(req) = rx.recv() {
        let response = match req {
            Request::Watch { video_id } => Response::Watch {
                watch: Box::new(watch::fetch(&http, &video_id).map_err(|e| format!("{e:#}"))),
                video_id,
            },
            Request::MoreQueue { epoch, token } => Response::MoreQueue {
                page: watch::continue_queue(&http, &token).map_err(|e| format!("{e:#}")),
                epoch,
            },
            Request::SeedQueue { epoch, rotation } => Response::MoreQueue {
                // Read fresh rather than held: this thread runs one of these
                // every couple of hours at most, and the authoritative journal
                // belongs to the library thread, which is appending to it as
                // plays finish. A copy kept here would be the state of somebody
                // else's listening at the moment this thread started.
                //
                // Safe to read while that thread appends, by the journal's own
                // design: a record is one line written in one call, and a line
                // that will not parse is skipped rather than fatal.
                page: watch::seeded_page(&http, &Journal::load(), rotation)
                    .map_err(|e| format!("{e:#}")),
                epoch,
            },
            Request::Lyrics {
                video_id,
                browse_id,
                query,
            } => Response::Lyrics {
                lyrics: lyrics(&http, browse_id.as_deref(), query.as_ref())
                    .map_err(|e| format!("{e:#}")),
                video_id,
            },
            Request::Related {
                video_id,
                browse_id,
            } => Response::Related {
                related: watch::related(&http, &browse_id).map_err(|e| format!("{e:#}")),
                video_id,
            },
            Request::Comments { video_id } => Response::Comments {
                comments: Box::new(watch::comments(&http, &video_id).map_err(|e| format!("{e:#}"))),
                video_id,
            },
            Request::Shutdown => break,
            // Routed elsewhere by `send`; reaching here would mean that routing
            // and this match had drifted apart.
            other => {
                debug_assert!(false, "{} was routed to the page thread", name(&other));
                continue;
            }
        };

        if tx.send(response).is_err() {
            break;
        }
    }
}

/// The best lyrics available for one track, from either of the two sources.
///
/// YouTube is asked first and its words are always preferred: they are the ones
/// matched to the recording that is actually playing. What it often does not
/// have is *timings* -- it publishes those only for its own catalogue -- and
/// without them the panel is a wall of text with nothing to follow. LRCLIB is
/// asked only for what is missing, in one of two ways:
///
/// - YouTube gave words but no timings: keep its words, lay LRCLIB's timings
///   over them, and say so in the credit.
/// - YouTube has no lyrics page at all, or the fetch failed: LRCLIB is the only
///   chance, and its own words are better than an empty tab.
///
/// Whichever way it goes, LRCLIB failing is never what the user is told about:
/// the error surfaced is YouTube's, because that is the source the tab is
/// nominally showing.
fn lyrics(
    http: &Http,
    browse_id: Option<&str>,
    query: Option<&lrclib::Query>,
) -> Result<watch::Lyrics> {
    let found = browse_id.map(|browse_id| watch::lyrics(http, browse_id));

    match found {
        // Timed by YouTube itself. Nothing to add, and a second round trip
        // would buy a worse answer than the one in hand.
        Some(Ok(lyrics)) if !lyrics.timed.is_empty() => Ok(lyrics),

        // Words but no timings -- a cover, a live take, most of what is not in
        // YouTube's own catalogue.
        Some(Ok(mut lyrics)) => {
            if let Some(timed) = query.and_then(|query| lrclib::timed(http, query)) {
                lyrics.timed = timed;
                // The words on screen are still YouTube's; what changed is that
                // they now scroll. Crediting both is the honest version, and
                // the panel has one line to say it in.
                lyrics.source = Some(match lyrics.source {
                    Some(source) => format!("{source} • Timings: LRCLIB"),
                    None => "Timings: LRCLIB".to_string(),
                });
            }
            Ok(lyrics)
        }

        // No lyrics page, or YouTube would not answer for one.
        other => {
            if let Some(timed) = query.and_then(|query| lrclib::timed(http, query)) {
                return Ok(watch::Lyrics::from_timed(timed, Some("Source: LRCLIB")));
            }
            match other {
                Some(Err(e)) => Err(e),
                _ => bail!("no lyrics are published for this track"),
            }
        }
    }
}

fn run_covers(rx: Receiver<Request>, tx: Sender<Response>) {
    while let Ok(req) = rx.recv() {
        let id = match req {
            Request::Cover { id } => id,
            Request::Shutdown => break,
            // Routed elsewhere by `send`; reaching here would mean that routing
            // and this match had drifted apart. Skipped rather than taken as a
            // reason to stop -- this loop used to match `Cover` alone, so a
            // misrouted request ended it, and since nothing joins this thread
            // the whole session would silently go without covers.
            other => {
                debug_assert!(false, "{} was routed to the cover thread", name(&other));
                continue;
            }
        };

        // Deliberately not reported as `Failed`: the status line is showing
        // what is playing, and a missing picture is not worth replacing that
        // with an error the user can do nothing about. The UI renders no cover
        // pane and says nothing.
        let response = Response::Cover {
            art: cover::fetch(&id).ok(),
            id,
        };
        if tx.send(response).is_err() {
            break;
        }
    }
}

/// Card artwork for the landing page, one tile at a time.
///
/// Serial like every other worker here, and that is the right shape even though
/// a dozen arrive at once: these are small pictures off a CDN that keeps the
/// connection open, so they come back in a few hundred milliseconds all told,
/// and the cards fill in as they land rather than all at the end. Fanning them
/// out across threads would buy a fraction of a second and cost a connection
/// pool per thread.
///
/// A fetcher that will not start is not fatal. It means this session draws
/// cards without pictures, which is what the renderer does anyway until they
/// arrive -- so the thread answers every request with `None` rather than
/// exiting, which would silently strand the requests in the channel.
fn run_art(rx: &Receiver<Request>, tx: &Sender<Response>) {
    let fetcher = cover::ArtFetcher::new();
    if let Err(e) = &fetcher {
        debug_assert!(false, "could not start the artwork fetcher: {e:#}");
    }

    while let Ok(req) = rx.recv() {
        let (key, url) = match req {
            Request::Art { key, url } => (key, url),
            Request::Shutdown => break,
            other => {
                debug_assert!(false, "{} was routed to the art thread", name(&other));
                continue;
            }
        };

        // Silent on failure, like the cover thread and for the same reason: a
        // card without its sleeve is a card, and there is nothing the user
        // could do with the news that a CDN said 404.
        let art = fetcher
            .as_ref()
            .ok()
            .and_then(|fetcher| fetcher.fetch(&url, crate::art::EDGE).ok());
        if tx.send(Response::Art { key, art }).is_err() {
            break;
        }
    }
}
