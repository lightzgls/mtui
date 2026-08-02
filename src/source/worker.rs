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

use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};
use std::thread;

use anyhow::{Context, Result};

use super::auth::{self, Http};
use super::cover::{self, Cover};
use super::innertube::InnerTube;
use super::library::{Library, Playlist};
use super::{StreamUrl, Track, UrlCache};
use crate::config::{Credentials, Tokens};
use crate::source::youtube::YouTube;

/// Ceiling on how much of a playlist is loaded.
///
/// Bounded for the same reason [`crate::source::youtube::MAX_RESULTS`] is: a
/// long session must not be able to grow the heap without limit. Beyond this,
/// each further fifty rows is two more round trips for lines nobody scrolls to.
const MAX_LIBRARY_TRACKS: usize = 200;

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
    },
    /// Resolve into the cache before the user has committed to anything, so the
    /// Enter that follows costs a channel round trip instead of a process spawn.
    Prefetch {
        id: String,
    },
    /// Thumbnail for a track. Runs on its own thread; see the module header.
    Cover {
        id: String,
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
    Resolved {
        stream: StreamUrl,
        title: String,
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
    /// Kept so a sign-in thread can be handed somewhere to answer. Sign-in is
    /// spawned on demand rather than kept resident: it runs at most once a
    /// session and would otherwise be a thread asleep for the whole run.
    res_tx: Sender<Response>,
    rx: Receiver<Response>,
    handle: Option<thread::JoinHandle<()>>,
}

impl SourceWorker {
    pub fn spawn(yt: YouTube) -> Result<Self> {
        let (req_tx, req_rx) = channel::<Request>();
        let (cover_req_tx, cover_req_rx) = channel::<Request>();
        let (library_req_tx, library_req_rx) = channel::<Request>();
        let (res_tx, res_rx) = channel::<Response>();
        let cover_res_tx = res_tx.clone();
        let library_res_tx = res_tx.clone();
        let spawn_res_tx = res_tx.clone();

        let handle = thread::Builder::new()
            .name("mtui-source".to_string())
            .spawn(move || run(yt, req_rx, res_tx))
            .context("failed to spawn source worker")?;

        // Both deliberately detached -- see `Drop`.
        thread::Builder::new()
            .name("mtui-cover".to_string())
            .spawn(move || run_covers(cover_req_rx, cover_res_tx))
            .context("failed to spawn cover worker")?;

        thread::Builder::new()
            .name("mtui-library".to_string())
            .spawn(move || run_library(library_req_rx, library_res_tx))
            .context("failed to spawn library worker")?;

        Ok(Self {
            tx: req_tx,
            cover_tx: cover_req_tx,
            library_tx: library_req_tx,
            res_tx: spawn_res_tx,
            rx: res_rx,
            handle: Some(handle),
        })
    }

    pub fn send(&self, req: Request) -> Result<()> {
        match req {
            Request::Cover { .. } => self.cover_tx.send(req).context("cover worker is gone"),
            // Not queued anywhere: this one blocks for as long as the user
            // takes to approve a code, so it gets a thread that exists only for
            // the duration of the flow.
            Request::SignIn => {
                spawn_sign_in(self.res_tx.clone());
                Ok(())
            }
            Request::SignOut
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
    pub fn poll(&self) -> Option<Response> {
        match self.rx.try_recv() {
            Ok(res) => Some(res),
            Err(TryRecvError::Empty | TryRecvError::Disconnected) => None,
        }
    }
}

impl Drop for SourceWorker {
    fn drop(&mut self) {
        let _ = self.tx.send(Request::Shutdown);
        let _ = self.cover_tx.send(Request::Shutdown);
        let _ = self.library_tx.send(Request::Shutdown);
        // Only the source thread is joined. The cover thread may be most of a
        // ten-second timeout into a fetch, and making the user wait that out to
        // quit -- for a picture that is already off screen -- would be absurd.
        // The library and sign-in threads are the same case, and the sign-in
        // one may be asleep between polls for another five seconds on top.
        // None of them owns a subprocess, so letting the process exit under
        // them is safe.
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

fn run(yt: YouTube, rx: Receiver<Request>, tx: Sender<Response>) {
    // Owned outright by this thread, so it needs no lock: every resolve in the
    // program funnels through here.
    let mut cache = UrlCache::new();
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
            Request::Resolve { id, title } => match resolve(&yt, tube.as_ref(), &mut cache, &id) {
                Ok(stream) => Response::Resolved { stream, title },
                Err(e) => Response::Failed(format!("{e:#}")),
            },
            Request::Prefetch { id } => {
                let ready = resolve(&yt, tube.as_ref(), &mut cache, &id).is_ok();
                Response::Prefetched { id, ready }
            }
            Request::Shutdown => break,
            // Routed to the cover, library or sign-in threads by `send`.
            // Reaching here would mean that routing and this match had drifted
            // apart -- ignored rather than answered, since replying to a
            // request this thread never ran would be worse than silence.
            Request::Cover { .. }
            | Request::SignIn
            | Request::SignOut
            | Request::Playlists
            | Request::OpenPlaylist { .. }
            | Request::AddToPlaylist { .. }
            | Request::RemoveFromPlaylist { .. }
            | Request::ToggleLike { .. } => continue,
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

/// Resolves `id` by the cheapest route that works, filling the cache.
///
/// Three tiers, cheapest first: the cache (free), YouTube's player API
/// (~0.2 s), then yt-dlp (~4 s). The cache is what keeps a replay -- or a play
/// the user was prefetched into -- from paying anything at all.
///
/// A player API failure is swallowed rather than reported. It is expected for
/// age-gated and region-locked videos, the user did not ask for that path
/// specifically, and yt-dlp is about to answer the same question properly. If
/// yt-dlp also fails, *its* error is the one worth showing.
fn resolve(
    yt: &YouTube,
    tube: Option<&InnerTube>,
    cache: &mut UrlCache,
    id: &str,
) -> Result<StreamUrl> {
    if let Some(hit) = cache.get(id) {
        return Ok(hit);
    }

    let stream = match tube.and_then(|t| t.resolve(id).ok()) {
        Some(fast) => fast,
        None => yt.resolve(id)?,
    };
    cache.insert(id.to_string(), stream.clone());
    Ok(stream)
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

fn run_library(rx: Receiver<Request>, tx: Sender<Response>) {
    // Built once so the connection pool and TLS session outlive a single call,
    // for the same reason `InnerTube` holds its own.
    let mut library = Library::new();

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

        let response = match handle_library(library, req) {
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

fn handle_library(library: &mut Library, req: Request) -> Result<Option<Response>> {
    // Everything below this point needs a session, so the check is made once
    // here rather than repeated in each arm. The UI reads this as "start the
    // sign-in flow", not as an error.
    if !library.is_signed_in() && !matches!(req, Request::SignOut) {
        return Ok(Some(Response::NeedsSignIn));
    }

    let response = match req {
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
        _ => return Ok(None),
    };

    Ok(Some(response))
}

fn run_covers(rx: Receiver<Request>, tx: Sender<Response>) {
    while let Ok(Request::Cover { id }) = rx.recv() {
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
