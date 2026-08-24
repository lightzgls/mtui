//! Audio playback, owned by a single dedicated thread.
//!
//! The UI never touches rodio or the decoder. It sends [`Command`]s down a
//! channel and reads an atomically-published [`Snapshot`]; the player thread
//! owns everything else. That keeps the audio path free of locks the UI could
//! contend on, and keeps redraws from ever blocking on I/O.

pub mod backend;
pub mod chunked;

use std::collections::VecDeque;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use mtui_resolver::AudioFormat;
use rodio::Source;

use chunked::{StreamFault, StreamLink};

/// How short of the end a drained source is still a finished track.
///
/// The container's duration and what the decoder actually yields never agree to
/// the sample: AAC's encoder delay and the final partial frame cost a fraction
/// of a second, and a livestream's duration is a guess. Anything inside this is
/// the track ending, not the stream dying.
const END_GRACE: Duration = Duration::from_secs(3);

/// Rebuilds allowed before a track is given up on.
///
/// Spent only on stalls that produce no playback: [`Track::note_progress`]
/// returns the budget once a rebuilt stream has played for [`PROGRESS`], so a
/// long track that hiccups every few minutes never runs out, while a stream
/// that dies the moment it opens stops after three attempts instead of
/// spinning on the network forever.
const MAX_REBUILDS: u8 = 3;

/// Playback from a rebuilt stream that counts as recovered.
const PROGRESS: Duration = Duration::from_secs(5);

/// Instructions from the UI to the player thread.
#[derive(Debug, Clone)]
pub enum Command {
    /// A different track is being loaded: stop, and say so.
    ///
    /// Sent the moment the user chooses something, which is seconds before the
    /// [`Self::Play`] that follows it -- resolving a URL spawns yt-dlp. Without
    /// this the previous song keeps playing under the new one's title and
    /// progress bar for the whole of that wait, which is the player telling the
    /// user two different things about what they are listening to.
    Load {
        title: String,
    },
    /// Stream and play a resolved URL, replacing whatever is playing.
    ///
    /// `id` is carried so that a stream which dies mid-track can be recovered
    /// against a freshly resolved URL rather than the one that just failed --
    /// see [`PlayerEvent::NeedsUrl`].
    Play {
        url: String,
        format: AudioFormat,
        title: String,
        id: String,
    },
    /// Carry on the current track from `from`, against a newly resolved URL.
    ///
    /// The answer to [`PlayerEvent::NeedsUrl`]. Unlike [`Self::Play`] this does
    /// not reset the title, the position, or the rebuild budget: it is the same
    /// track continuing, not a new one starting.
    Resume {
        url: String,
        format: AudioFormat,
        from: Duration,
    },
    /// A verified whole-file stream resolved while the fast native URL is
    /// already playing. Same-format URLs are armed directly on the downloader;
    /// other formats are held for a track-time rebuild at the cap.
    ArmReplacement {
        id: String,
        url: String,
        format: AudioFormat,
    },
    /// A requested URL could not be produced, so the track cannot go on.
    ///
    /// Sent instead of leaving the player parked in `Buffering` forever, which
    /// is what a silent failure to answer [`PlayerEvent::NeedsUrl`] would mean.
    ResumeFailed {
        why: String,
    },
    TogglePause,
    Stop,
    Seek(Duration),
    /// Clamped to 0.0..=2.0 by the player thread.
    SetVolume(f32),
    Shutdown,
}

/// Something the player needs from the app, which owns the source worker.
///
/// The player thread deliberately cannot resolve a URL itself -- that means
/// yt-dlp, a cache and a network client, none of which belong next to the audio
/// callback. So it asks, the same way the UI asks it to play.
#[derive(Debug, Clone)]
pub enum PlayerEvent {
    /// The stream stopped being served and a fresh URL is needed to carry on
    /// from `from`.
    ///
    /// The URL the track was started with is not reusable here, and reusing it
    /// is what the recovery path used to do: three rebuilds against the very
    /// URL that had just refused, all failing at the same byte within seconds
    /// of each other, and the track given up on.
    ///
    /// Answering this is the app's job because resolving properly means the
    /// whole cascade, cap check included -- a cheaper answer can hand back a URL
    /// with the same ceiling as the one being recovered from.
    NeedsUrl { id: String, from: Duration },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PlayState {
    #[default]
    Idle,
    /// Between `Play` and the first decoded sample: connecting and filling the
    /// ring buffer. Worth showing, since it is the one user-visible latency.
    Buffering,
    Playing,
    Paused,
}

/// What the UI renders. Cheap to clone; replaced wholesale on each change.
#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub state: PlayState,
    pub title: String,
    pub position: Duration,
    pub volume: f32,
    /// Last playback error, shown as a transient message. Cleared on next play.
    pub error: Option<String>,
}

/// Handle to the player thread. Dropping it stops playback and joins the thread.
pub struct Player {
    tx: Sender<Command>,
    /// Requests from the player thread back to the app. See [`PlayerEvent`].
    events: Receiver<PlayerEvent>,
    snapshot: Arc<Mutex<Snapshot>>,
    handle: Option<thread::JoinHandle<()>>,
}

impl Player {
    /// Spawns the player thread and its private tokio runtime.
    pub fn spawn() -> Result<Self> {
        let (tx, rx) = channel();
        let (events_tx, events) = channel();
        let snapshot = Arc::new(Mutex::new(Snapshot {
            volume: 1.0,
            ..Default::default()
        }));

        let thread_snapshot = Arc::clone(&snapshot);
        let handle = thread::Builder::new()
            .name("mtui-player".to_string())
            .spawn(move || run(rx, events_tx, thread_snapshot))
            .context("failed to spawn player thread")?;

        Ok(Self {
            tx,
            events,
            snapshot,
            handle: Some(handle),
        })
    }

    /// Sends a command. Fails only if the player thread has died.
    pub fn send(&self, cmd: Command) -> Result<()> {
        self.tx.send(cmd).context("player thread is gone")
    }

    /// Takes the next thing the player is waiting on, if there is one.
    ///
    /// Never blocks: called from the app's event loop beside the source
    /// worker's own responses.
    pub fn poll_event(&self) -> Option<PlayerEvent> {
        self.events.try_recv().ok()
    }

    /// Current state for rendering. Never blocks meaningfully -- the lock is
    /// only ever held for a struct copy.
    pub fn snapshot(&self) -> Snapshot {
        self.snapshot
            .lock()
            .expect("snapshot mutex poisoned")
            .clone()
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        let _ = self.tx.send(Command::Shutdown);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// The track the player thread is on, and everything needed to rebuild its
/// stream from a given point.
///
/// This exists because rodio cannot tell us *why* a source stopped. A decoder
/// whose next read fails is dropped exactly like one that reached the end of
/// the file, so a network stall, a seek the ring buffer could not serve, and a
/// finished song all arrive as the same silent empty queue. Knowing how long
/// the track is, and how far it got, is what separates them.
struct Track {
    url: String,
    /// Representation behind `url`. Byte offsets are interchangeable only when
    /// this identity matches a refreshed URL.
    format: AudioFormat,
    /// The video this is, so a dead stream can be re-resolved rather than
    /// reopened at the URL that just refused us.
    id: String,
    /// What the current stream's downloader hit, if anything. This is the one
    /// piece of hard evidence in the whole decision below -- everything else is
    /// inference from the clock. It is also how a fresh URL is handed down to a
    /// download that has been refused. See [`chunked::StreamLink`].
    link: StreamLink,
    /// Length according to the container. `None` for a livestream, which has no
    /// end to fall short of -- so a drained source is taken at its word unless
    /// the downloader recorded a reason not to.
    total: Option<Duration>,
    /// Track time the current rodio source begins at. Non-zero once a stream
    /// has been rebuilt: rodio's clock restarts with every source it is given.
    offset: Duration,
    /// Last position published, and where a rebuild picks up.
    position: Duration,
    /// Where a seek was aimed, until the stream is seen to have played on past
    /// it. `None` the rest of the time.
    ///
    /// A seek is answered before it is carried out, and answered `Ok` either
    /// way: rodio reports the position it was asked for, and the decoder only
    /// discovers whether the stream could follow when it reads the next packet.
    /// If it could not, the source ends the way a finished track does -- so
    /// without this the recovery below would pick up at the position the dead
    /// decoder was left reporting, which is the target it never reached, or
    /// call the track finished and move on to the next song.
    seeking_to: Option<Duration>,
    rebuilds: u8,
    replacement: Option<(String, AudioFormat)>,
}

/// Why the queue went empty, decided before anything is touched so the caller
/// is free to replace the track it was read from.
enum Ending {
    /// The track ran out, which is the ordinary case.
    Finished,
    /// The stream died this far in. Worth another attempt.
    Stalled(Duration),
    /// The stream died and the rebuild budget is spent.
    GaveUp {
        at: Duration,
        /// `None` for a livestream, which never had a length to fall short of.
        total: Option<Duration>,
        /// What the downloader hit, when it recorded anything. Carried so the
        /// message can name a cause instead of only a position.
        fault: Option<StreamFault>,
    },
}

impl Track {
    fn arm_replacement(&mut self, url: String, format: AudioFormat) {
        if same_representation(self.format, format) {
            if self.link.supply_url(&url) {
                self.url = url;
            }
        } else {
            self.replacement = Some((url, format));
        }
    }

    /// Returns the budget once the current stream has played for [`PROGRESS`].
    ///
    /// Progress is measured from where this stream started rather than from the
    /// start of the track, so it means "this rebuild worked", which is the only
    /// thing the budget is protecting against.
    fn note_progress(&mut self) {
        if self.position >= self.offset + PROGRESS {
            self.rebuilds = 0;
        }
    }

    /// Records a seek to `target` as outstanding.
    ///
    /// Not past the end, where a seek is answered by saturating at it: there is
    /// nothing left for the stream to have failed to reach, and holding a
    /// target it can never play past would answer the last seek of a track with
    /// a stream reopened only to find it over.
    fn aim_at(&mut self, target: Duration) {
        self.seeking_to = match self.total {
            Some(total) if target + END_GRACE >= total => None,
            _ => Some(target),
        };
    }

    /// Records where the stream has played to, and with it whether a seek has
    /// been reached.
    ///
    /// A seek is believed once the stream has played on *past* it. Until then
    /// rodio is only repeating the position it was asked to seek to, which a
    /// stream that could not follow it never reached.
    fn played_to(&mut self, position: Duration) {
        self.position = position;
        if self.seeking_to.is_some_and(|target| position > target) {
            self.seeking_to = None;
        }
    }

    fn ending(&self) -> Ending {
        // Where a rebuild picks up. A source that drained with a seek still
        // outstanding never played from where it was sent, so recovery aims at
        // the target rather than at the position it was left reporting.
        let from = self.seeking_to.unwrap_or(self.position);
        let fault = self.link.fault();

        // The end-of-track tests give way while a seek is outstanding: a drain
        // there is the seek having failed, however near the end of the track it
        // landed, and reading it as Finished would answer a rewind by playing
        // the next song.
        if self.seeking_to.is_none() {
            match self.total {
                // The music played. A refusal on the last chunk is real but no
                // longer interesting -- rebuilding here would replay the last
                // few seconds only to arrive at the same end.
                Some(total) if self.position + END_GRACE >= total => return Ending::Finished,
                // Fell short of a stated length: a stall, as it always was.
                Some(_) => {}
                // No stated length, and the downloader recorded nothing wrong.
                // A livestream has no end to have fallen short of, so a drain
                // is taken at its word.
                None if fault.is_none() => return Ending::Finished,
                // No stated length, but the stream died of something nameable.
                // This is the case that used to advance silently to the next
                // song: a fragmented container states no duration, so every
                // mid-track refusal on one was read as a track that ended.
                None => {}
            }
        }

        if self.rebuilds >= MAX_REBUILDS {
            Ending::GaveUp {
                at: from,
                total: self.total,
                fault,
            }
        } else {
            Ending::Stalled(from)
        }
    }
}

/// `M:SS`, for the one place the player has to name a position itself.
fn clock(d: Duration) -> String {
    let secs = d.as_secs();
    format!("{}:{:02}", secs / 60, secs % 60)
}

/// Player thread body. Owns the output device, the decoder, and the runtime.
fn run(rx: Receiver<Command>, events: Sender<PlayerEvent>, snapshot: Arc<Mutex<Snapshot>>) {
    // One worker thread, dedicated to stream-download's downloader task.
    //
    // This must be a multi-thread runtime even though there is only ever one
    // task. The decoder reads synchronously from *this* thread while the
    // downloader needs to keep filling the ring buffer; a current-thread
    // runtime only polls spawned tasks inside `block_on`, so the first read
    // after `block_on` returns would block forever with the downloader parked.
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            set_error(&snapshot, format!("could not start async runtime: {e}"));
            return;
        }
    };

    // Opening the device can fail on a machine with no audio output at all.
    // Report it and idle rather than killing the app.
    let mut device = match rodio::DeviceSinkBuilder::open_default_sink() {
        Ok(d) => d,
        Err(e) => {
            set_error(&snapshot, format!("no audio output device: {e}"));
            return;
        }
    };
    // rodio otherwise prints a notice to stderr when the sink drops, which
    // would scribble over the alternate screen on exit.
    device.log_on_drop(false);
    let player = rodio::Player::connect_new(device.mixer());

    // Wake at least every TICK even with no commands pending, so the published
    // position stays live and end-of-track is noticed. A purely command-driven
    // loop would freeze the clock the moment the queue went quiet.
    const TICK: Duration = Duration::from_millis(250);

    // Mirrored rather than read back out of the snapshot, so the tick below can
    // branch on the state without taking the lock the UI is reading through.
    let mut state = PlayState::Idle;
    let mut track: Option<Track> = None;
    // Commands pulled off the channel ahead of time, which is how opening a
    // stream can notice that it has been superseded. Drained before the channel
    // is read again, so nothing here waits longer than it would have.
    let mut queued: VecDeque<Command> = VecDeque::new();
    // Whether a fresh URL has already been asked for and not yet answered, so a
    // downloader waiting ten seconds is not asked about forty times over.
    let mut asked_for_url = false;

    loop {
        let cmd = match queued.pop_front() {
            Some(cmd) => Some(cmd),
            None => match rx.recv_timeout(TICK) {
                Ok(cmd) => Some(cmd),
                Err(RecvTimeoutError::Timeout) => None,
                // The Player handle was dropped; nothing can command us again.
                Err(RecvTimeoutError::Disconnected) => break,
            },
        };

        if let Some(cmd) = cmd {
            match cmd {
                Command::Load { title } => {
                    player.stop();
                    track = None;
                    state = PlayState::Buffering;
                    update(&snapshot, |s| {
                        s.state = PlayState::Buffering;
                        s.title = title;
                        s.position = Duration::ZERO;
                        s.error = None;
                    });
                }
                Command::Play {
                    url,
                    format,
                    title,
                    id,
                } => {
                    update(&snapshot, |s| {
                        s.state = PlayState::Buffering;
                        s.title = title.clone();
                        s.position = Duration::ZERO;
                        s.error = None;
                    });
                    // The loop's own copy is left alone until this settles: the
                    // open below blocks, and nothing can read it in the
                    // meantime. What the UI renders is the snapshot above.
                    match open_stream(&runtime, &url) {
                        Ok((decoder, total, link)) => {
                            // Opening blocks this thread for a second or more,
                            // and whatever arrived during it is newer than this
                            // track. Starting it now would mean a burst of the
                            // song the user just left before the one they chose
                            // -- so the stream is dropped unheard.
                            if drain(&rx, &mut queued) {
                                continue;
                            }
                            play_source(&player, decoder, Duration::ZERO);
                            track = Some(Track {
                                url,
                                format,
                                id,
                                link,
                                total,
                                offset: Duration::ZERO,
                                position: Duration::ZERO,
                                seeking_to: None,
                                rebuilds: 0,
                                replacement: None,
                            });
                            state = set_state(&snapshot, PlayState::Playing);
                        }
                        Err(e) => {
                            player.stop();
                            track = None;
                            state = PlayState::Idle;
                            set_error(&snapshot, format!("{e:#}"));
                        }
                    }
                }
                Command::Resume { url, format, from } => {
                    // Nothing to carry on: the track was stopped or replaced
                    // while the app was resolving. The URL just fetched is
                    // simply dropped -- starting it would play a song the user
                    // has already left.
                    let Some(cur) = track.as_mut() else {
                        continue;
                    };

                    // A background replacement may have answered the same
                    // refusal while this resolve was still in flight. Once the
                    // downloader is healthy again, this response carries a
                    // stale `from`; rebuilding from it would replay the start
                    // of the song several seconds into playback.
                    if resume_is_stale(state, cur.link.wants_url().is_some()) {
                        continue;
                    }
                    cur.url = url;

                    // The stream is still running and only wants a signature
                    // that is still honoured. Handing the URL straight down to
                    // the chunk loop is the cheap path: it re-asks for the byte
                    // range it was refused, and the music never stops.
                    //
                    // Rebuilding instead would restart the decoder and wind it
                    // forward from byte zero to get back to where it already
                    // was -- audible, and pointless when the bytes are simply
                    // waiting behind a URL that has been replaced.
                    if same_representation(cur.format, format)
                        && cur.link.wants_url().is_some()
                        && cur.link.supply_url(&cur.url)
                    {
                        continue;
                    }

                    // A byte offset in one AAC representation is unrelated to
                    // the same offset in another. End the downloader's wait and
                    // rebuild from track time instead of splicing the files.
                    cur.link.decline();
                    cur.format = format;
                    cur.replacement = None;

                    let url = cur.url.clone();
                    match start_stream(&runtime, &player, &url, from) {
                        Ok((total, link)) => {
                            // The fresh stream reports its own faults, and its
                            // own length -- a re-resolve can land on a
                            // different itag, and the old figure would then be
                            // the wrong thing to measure the end against.
                            if let Some(cur) = track.as_mut() {
                                cur.link = link;
                                if total.is_some() {
                                    cur.total = total;
                                }
                            }
                            state = set_state(&snapshot, PlayState::Playing);
                        }
                        Err(e) => {
                            player.stop();
                            track = None;
                            state = PlayState::Idle;
                            set_error(
                                &snapshot,
                                format!("stream stopped at {}: {e:#}", clock(from)),
                            );
                        }
                    }
                }
                Command::ArmReplacement { id, url, format } => {
                    let Some(cur) = track.as_mut().filter(|cur| cur.id == id) else {
                        continue;
                    };
                    cur.arm_replacement(url, format);
                }
                Command::ResumeFailed { why } => {
                    // Let a downloader still waiting stop waiting. Without
                    // this it sits out its whole timeout for an answer that
                    // has already come back empty.
                    if let Some(cur) = track.as_ref() {
                        cur.link.decline();
                    }
                    player.stop();
                    track = None;
                    state = PlayState::Idle;
                    set_error(&snapshot, why);
                }
                Command::TogglePause => {
                    if player.is_paused() {
                        player.play();
                        state = set_state(&snapshot, PlayState::Playing);
                    } else {
                        player.pause();
                        state = set_state(&snapshot, PlayState::Paused);
                    }
                }
                Command::Stop => {
                    player.stop();
                    track = None;
                    state = PlayState::Idle;
                    update(&snapshot, |s| {
                        s.state = PlayState::Idle;
                        s.position = Duration::ZERO;
                        s.title.clear();
                    });
                }
                Command::Seek(target) => {
                    // rodio's clock is relative to the source it was given,
                    // which after a rebuild does not begin at the track's
                    // start. Everywhere else here speaks in track time.
                    // Aimed before the attempt, because a seek that kills the
                    // source has to be recovered at the target and by then this
                    // is the only thing that remembers it.
                    let aimed = track.as_mut().map(|cur| {
                        let seek = (cur.offset, target < cur.position);
                        cur.aim_at(target);
                        seek
                    });
                    if let Some((offset, backwards)) = aimed {
                        match target.checked_sub(offset) {
                            // Forwards, symphonia reads ahead and discards,
                            // which lands exactly.
                            //
                            // The position is left to the tick below rather
                            // than written from the target here: rodio answers
                            // with the position it was asked for whether or not
                            // the stream could follow, and a clock running
                            // ahead of the audio takes the lyrics with it.
                            Some(within) if !backwards && player.try_seek(within).is_ok() => {}
                            // Backwards, or before this source begins: only a
                            // fresh stream can reach it.
                            //
                            // A rewind is never attempted in place, however
                            // little of it there is. The bytes are usually
                            // still in the ring buffer, but the decoder has
                            // been told it cannot seek in that buffer -- so it
                            // answers by moving its own sample pointer,
                            // reports success, and fails the next read, which
                            // rodio reads as a source that ended. That is the
                            // silent path that left the clock, and the lyrics
                            // marked off it, describing a rewind the audio
                            // never made. Winding a fresh stream forward costs
                            // the time it takes to open one and lands where it
                            // was sent.
                            _ => {
                                state = rebuild(&runtime, &player, &snapshot, &mut track, target);
                            }
                        }
                    }
                }
                Command::SetVolume(v) => {
                    let v = v.clamp(0.0, 2.0);
                    player.set_volume(v);
                    update(&snapshot, |s| s.volume = v);
                }
                Command::Shutdown => break,
            }
        }

        // Runs on every tick as well as after every command.
        let drained = player.empty();
        if let Some(cur) = track.as_mut() {
            if !drained {
                cur.played_to(cur.offset + player.get_pos());
            }
            cur.note_progress();
            let position = cur.position;
            update(&snapshot, |s| s.position = position);
        }

        // A download that has been refused mid-stream, asking for a signature
        // that is still honoured. Answered while the audio keeps
        // playing: the ring buffer holds about thirty seconds and a resolve
        // costs a fraction of that, so the swap is inaudible and nothing above
        // the chunk loop ever learns it happened.
        //
        // Checked before the drain test below, because catching it here is what
        // stops it ever becoming a drain.
        let armed = track
            .as_mut()
            .and_then(|cur| cur.link.wants_url().and_then(|_| cur.replacement.take()));
        if let Some((url, format)) = armed {
            let paused = state == PlayState::Paused;
            match replace_running_stream(&runtime, &player, &mut track, url, format, paused) {
                Ok(()) => {
                    let restored = if paused {
                        PlayState::Paused
                    } else {
                        PlayState::Playing
                    };
                    state = set_state(&snapshot, restored);
                }
                Err(error) => crate::diagnostics::warn(
                    "player",
                    &format!("armed stream replacement failed: {error:#}"),
                ),
            }
        } else if let Some(cur) = track.as_ref()
            && !asked_for_url
            && cur.link.wants_url().is_some()
        {
            asked_for_url = events
                .send(PlayerEvent::NeedsUrl {
                    id: cur.id.clone(),
                    from: cur.position,
                })
                .is_ok();
            if !asked_for_url {
                // Nobody can answer, so let the downloader stop waiting and
                // fail in the open rather than sitting out its whole timeout.
                cur.link.decline();
            }
        }
        // The request was answered, or the downloader gave up on it. Either way
        // the next refusal is a new question.
        if track
            .as_ref()
            .is_none_or(|cur| cur.link.wants_url().is_none())
        {
            asked_for_url = false;
        }

        // rodio drains its queue when a source ends -- and it ends a source
        // whose next read failed exactly as it ends a finished one, with no
        // error anywhere. `empty()` is also true while idle, so only a state
        // that was actually running can mean anything by it.
        if !drained || state != PlayState::Playing {
            continue;
        }
        match track.as_ref().map_or(Ending::Finished, Track::ending) {
            Ending::Finished => {
                track = None;
                state = set_state(&snapshot, PlayState::Idle);
            }
            Ending::Stalled(from) => {
                // Only an automatic retry spends the budget. A rebuild the user
                // asked for by seeking is not the failure it guards against.
                if let Some(cur) = track.as_mut() {
                    cur.rebuilds += 1;
                }
                // Asks the app for a fresh URL rather than reopening the one
                // that just died. Reopening was the whole reason this path
                // could not recover: a URL googlevideo has stopped serving past
                // some byte refuses the same byte every time, so all three
                // attempts died at the same place within a few seconds of each
                // other and the budget was gone before the network was.
                state = request_url(&events, &snapshot, &mut track, from);
            }
            Ending::GaveUp { at, total, fault } => {
                track = None;
                state = PlayState::Idle;
                // Silence with a full progress bar is the worst outcome here:
                // it tells the user their music stopped and nothing else.
                set_error(&snapshot, stopped_message(at, total, fault));
            }
        }
    }

    player.stop();
}

/// Whether byte offsets in two resolved URLs name the same media bytes.
/// Unknown identities are rebuilt by track time; guessing here can splice two
/// different AAC encodes at the same numeric byte offset.
fn same_representation(current: AudioFormat, fresh: AudioFormat) -> bool {
    current.itag.is_some() && current == fresh
}

fn resume_is_stale(state: PlayState, downloader_waiting: bool) -> bool {
    state == PlayState::Playing && !downloader_waiting
}

/// Opens a different representation while the old buffered source keeps
/// playing, then commits at the position reached after the open completed.
fn replace_running_stream(
    runtime: &tokio::runtime::Runtime,
    player: &rodio::Player,
    track: &mut Option<Track>,
    url: String,
    format: AudioFormat,
    paused: bool,
) -> Result<()> {
    let (decoder, total, link) = open_stream(runtime, &url)?;
    let Some(cur) = track.as_mut() else {
        return Ok(());
    };
    let from = cur.offset + player.get_pos();
    cur.link.decline();
    play_source(player, decoder, from);
    if paused {
        player.pause();
    }
    cur.url = url;
    cur.format = format;
    cur.link = link;
    if total.is_some() {
        cur.total = total;
    }
    cur.offset = from;
    cur.position = from;
    cur.seeking_to = None;
    Ok(())
}

/// Names where playback stopped, and why when the reason was recorded.
///
/// The position alone was all this could ever say before, because by the time
/// the player noticed silence the reason had been discarded three layers down.
/// "stopped at 0:31 of 3:44" tells the user what they can already hear;
/// "(HTTP 403 at 1.0 MiB)" tells them, and a bug report, which of several very
/// different faults it was.
fn stopped_message(at: Duration, total: Option<Duration>, fault: Option<StreamFault>) -> String {
    let mut msg = match total {
        Some(total) => format!("stream stopped at {} of {}", clock(at), clock(total)),
        None => format!("stream stopped at {}", clock(at)),
    };
    if let Some(fault) = fault {
        msg.push_str(&format!(" ({fault})"));
    }
    msg
}

/// Asks the app for a fresh URL for the current track, to carry on from `from`.
///
/// The counterpart to [`rebuild`], and the difference between them is the whole
/// fix for tracks that stopped a few seconds in. A rebuild reopens the URL in
/// hand, which is right when the stream is healthy and the user simply wants to
/// be somewhere else in it. It is exactly wrong when the stream *died*: the
/// commonest cause is googlevideo refusing to serve past some byte on that
/// particular signed URL, and reopening it walks into the same refusal at the
/// same offset every time.
///
/// So this hands the problem to the app, which owns the resolver and the cache,
/// and parks in `Buffering` until [`Command::Resume`] arrives.
fn request_url(
    events: &Sender<PlayerEvent>,
    snapshot: &Arc<Mutex<Snapshot>>,
    track: &mut Option<Track>,
    from: Duration,
) -> PlayState {
    let Some(cur) = track.as_mut() else {
        return PlayState::Idle;
    };
    cur.offset = from;
    cur.position = from;
    // A fresh stream wound forward lands where it was told to by construction,
    // so nothing is left outstanding for the recovery above to aim at.
    cur.seeking_to = None;
    let id = cur.id.clone();

    update(snapshot, |s| {
        s.state = PlayState::Buffering;
        s.position = from;
    });

    if events.send(PlayerEvent::NeedsUrl { id, from }).is_err() {
        // The app is gone, so nothing is ever going to answer. Better to stop
        // than to sit in `Buffering` forever.
        *track = None;
        set_error(snapshot, format!("stream stopped at {}", clock(from)));
        return PlayState::Idle;
    }

    PlayState::Buffering
}

/// Reopens the current track's stream so it plays from `from`, and reports the
/// state to settle in.
///
/// Used for a seek the user asked for, where the URL in hand is known good --
/// the stream was playing a moment ago. A stream that *died* is recovered
/// through [`request_url`] instead, which replaces the URL rather than trusting
/// it. A fresh stream can be wound forward to any point in the track, so that
/// is what a rebuild is.
fn rebuild(
    runtime: &tokio::runtime::Runtime,
    player: &rodio::Player,
    snapshot: &Arc<Mutex<Snapshot>>,
    track: &mut Option<Track>,
    from: Duration,
) -> PlayState {
    let Some(cur) = track.as_mut() else {
        return PlayState::Idle;
    };
    cur.offset = from;
    cur.position = from;
    // A fresh stream wound forward lands where it was told to by construction,
    // so nothing is left outstanding for the recovery above to aim at.
    cur.seeking_to = None;
    let url = cur.url.clone();

    update(snapshot, |s| {
        s.state = PlayState::Buffering;
        s.position = from;
    });

    match start_stream(runtime, player, &url, from) {
        Ok((_, link)) => {
            // The old stream's log described a stream that no longer exists.
            // Leaving it in place would let a fault from before the seek decide
            // how the *next* drain is read.
            if let Some(cur) = track.as_mut() {
                cur.link = link;
            }
            set_state(snapshot, PlayState::Playing)
        }
        Err(e) => {
            player.stop();
            *track = None;
            set_error(
                snapshot,
                format!("stream stopped at {}: {e:#}", clock(from)),
            );
            PlayState::Idle
        }
    }
}

/// Takes everything waiting on the channel, reporting whether any of it makes
/// the stream just opened not worth starting.
///
/// Only the commands that replace what is playing count. A volume change or a
/// pause arriving during an open says nothing about which track the user wants;
/// they are queued and applied to the track once it starts, as they would have
/// been anyway.
fn drain(rx: &Receiver<Command>, queued: &mut VecDeque<Command>) -> bool {
    let mut superseded = false;
    while let Ok(cmd) = rx.try_recv() {
        superseded |= matches!(
            cmd,
            Command::Load { .. } | Command::Play { .. } | Command::Stop | Command::Shutdown
        );
        queued.push_back(cmd);
    }
    superseded
}

/// Opens the network stream and decodes its header, without touching playback.
///
/// Separate from [`play_source`] because this is the part that blocks -- a
/// second or more of network -- and what the user wants can change inside it.
/// Nothing audible happens until the caller commits.
///
/// Returns the track's length when the container states one, read here because
/// it is the only place it can be had: it is what tells a track that ended from
/// a stream that died.
///
/// How the stream is handed to symphonia -- and why it is left unable to seek
/// backwards in it -- is [`backend::decoder`].
fn open_stream(
    runtime: &tokio::runtime::Runtime,
    url: &str,
) -> Result<(
    rodio::Decoder<backend::AudioStream>,
    Option<Duration>,
    StreamLink,
)> {
    let started = Instant::now();
    let opened = runtime.block_on(backend::open(url));
    let elapsed = started.elapsed().as_millis();
    crate::diagnostics::info(
        "player",
        &format!(
            "stream prefetch {} in {elapsed} ms",
            if opened.is_ok() {
                "completed"
            } else {
                "failed"
            }
        ),
    );
    let (stream, link) = opened?;

    let started = Instant::now();
    let decoded = backend::decoder(stream);
    let elapsed = started.elapsed().as_millis();
    crate::diagnostics::info(
        "player",
        &format!(
            "decoder initialization {} in {elapsed} ms",
            if decoded.is_ok() {
                "completed"
            } else {
                "failed"
            }
        ),
    );
    let decoder = decoded?;
    let total = decoder.total_duration();
    Ok((decoder, total, link))
}

/// Hands an opened stream to rodio, starting `skip` into the track.
fn play_source(
    player: &rodio::Player,
    decoder: rodio::Decoder<backend::AudioStream>,
    skip: Duration,
) {
    // Replace whatever was playing; rodio queues appended sources otherwise.
    player.stop();
    if skip.is_zero() {
        player.append(decoder);
    } else {
        // `skip_duration` decodes and discards on this thread, so the player
        // takes no commands until it lands. It is bounded by how fast the
        // chunked downloader can feed it, which is far above playback rate --
        // winding to the middle of a track costs about a second.
        player.append(decoder.skip_duration(skip));
    }
    player.play();
}

/// Opens and starts in one step, for the rebuild path -- which is recovering a
/// track that is already playing rather than starting one the user chose, so
/// there is nothing for a newer choice to supersede. A choice that does arrive
/// stops it through the [`Command::Load`] queued behind this.
fn start_stream(
    runtime: &tokio::runtime::Runtime,
    player: &rodio::Player,
    url: &str,
    skip: Duration,
) -> Result<(Option<Duration>, StreamLink)> {
    let (decoder, total, link) = open_stream(runtime, url)?;
    play_source(player, decoder, skip);
    Ok((total, link))
}

/// Publishes a state and hands it back, so the loop's copy and the snapshot
/// cannot drift apart.
fn set_state(snapshot: &Arc<Mutex<Snapshot>>, state: PlayState) -> PlayState {
    update(snapshot, |s| s.state = state);
    state
}

fn update(snapshot: &Arc<Mutex<Snapshot>>, f: impl FnOnce(&mut Snapshot)) {
    if let Ok(mut s) = snapshot.lock() {
        f(&mut s);
    }
}

fn set_error(snapshot: &Arc<Mutex<Snapshot>>, msg: String) {
    crate::diagnostics::error("player", &msg);
    update(snapshot, |s| {
        s.state = PlayState::Idle;
        s.error = Some(msg);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track(position: u64, total: Option<u64>) -> Track {
        Track {
            url: "https://example.com/a.m4a".into(),
            format: AudioFormat { itag: Some(140) },
            id: "dQw4w9WgXcQ".into(),
            link: StreamLink::default(),
            total: total.map(Duration::from_secs),
            offset: Duration::ZERO,
            position: Duration::from_secs(position),
            seeking_to: None,
            rebuilds: 0,
            replacement: None,
        }
    }

    #[test]
    fn only_the_same_known_format_can_resume_at_a_byte_offset() {
        let known = AudioFormat { itag: Some(141) };
        assert!(same_representation(known, known));
        assert!(!same_representation(known, AudioFormat { itag: Some(140) }));
        assert!(!same_representation(
            AudioFormat { itag: None },
            AudioFormat { itag: None }
        ));
    }

    #[test]
    fn replacements_are_spliced_only_when_the_representation_matches() {
        let mut same = track(5, Some(213));
        same.arm_replacement(
            "https://example.com/refreshed.m4a".into(),
            AudioFormat { itag: Some(140) },
        );
        assert_eq!(same.url, "https://example.com/refreshed.m4a");
        assert!(same.replacement.is_none());

        let mut different = track(5, Some(213));
        different.arm_replacement(
            "https://example.com/complete.mp4".into(),
            AudioFormat { itag: Some(18) },
        );
        assert_eq!(different.url, "https://example.com/a.m4a");
        assert!(matches!(
            different.replacement,
            Some((_, AudioFormat { itag: Some(18) }))
        ));
    }

    #[test]
    fn a_late_resume_is_ignored_after_the_downloader_recovered() {
        assert!(resume_is_stale(PlayState::Playing, false));
        assert!(!resume_is_stale(PlayState::Playing, true));
        assert!(!resume_is_stale(PlayState::Buffering, false));
    }

    /// The same track, whose downloader recorded a refusal at `offset`.
    fn faulted(position: u64, total: Option<u64>, offset: u64) -> Track {
        let cur = track(position, total);
        cur.link.record_for_test(StreamFault {
            status: Some(reqwest::StatusCode::FORBIDDEN),
            offset,
        });
        cur
    }

    /// The same track, with a seek to `target` outstanding.
    fn seeking(position: u64, total: Option<u64>, target: u64) -> Track {
        Track {
            seeking_to: Some(Duration::from_secs(target)),
            ..track(position, total)
        }
    }

    #[test]
    fn reaching_the_end_is_a_finished_track() {
        assert!(matches!(track(213, Some(213)).ending(), Ending::Finished));
    }

    #[test]
    fn the_last_partial_frame_still_counts_as_finished() {
        // The decoder yields slightly less than the container advertises, so an
        // exact match is not something a real track ever produces.
        assert!(matches!(track(211, Some(213)).ending(), Ending::Finished));
    }

    #[test]
    fn stopping_short_is_a_stall_worth_rebuilding() {
        let Ending::Stalled(from) = track(80, Some(213)).ending() else {
            panic!("a source that stopped 2 minutes early has not finished");
        };
        assert_eq!(
            from,
            Duration::from_secs(80),
            "rebuild picks up where it died"
        );
    }

    #[test]
    fn a_livestream_never_falls_short() {
        // Nothing states an end, so there is nothing to have stopped short of.
        assert!(matches!(track(80, None).ending(), Ending::Finished));
    }

    /// The silent-skip case, and the reason the downloader now writes down what
    /// it hit. A container that states no duration -- a livestream, or a
    /// fragmented mp4 whose sample table is short -- used to make every
    /// mid-track death indistinguishable from a track that ended, so the queue
    /// advanced to the next song with no error and no attempt to recover. A
    /// recorded refusal is evidence the position alone could never supply.
    #[test]
    fn a_livestream_that_was_refused_is_a_stall_not_an_ending() {
        let Ending::Stalled(from) = faulted(80, None, 1024 * 1024).ending() else {
            panic!("a stream refused mid-track has not finished, stated length or not");
        };
        assert_eq!(from, Duration::from_secs(80));
    }

    /// A refusal on the last chunk is real but no longer interesting: the music
    /// played. Recovering there would replay the last few seconds only to
    /// arrive at the same end, and would do it on every track whose final range
    /// request happens to be refused.
    #[test]
    fn a_fault_at_the_very_end_is_still_a_finished_track() {
        assert!(matches!(
            faulted(211, Some(213), 3_400_000).ending(),
            Ending::Finished
        ));
    }

    #[test]
    fn a_spent_budget_reports_instead_of_retrying() {
        let mut stuck = track(80, Some(213));
        stuck.rebuilds = MAX_REBUILDS;
        let Ending::GaveUp { at, total, .. } = stuck.ending() else {
            panic!("the budget is spent, so this must not ask for another rebuild");
        };
        assert_eq!(
            (at, total),
            (Duration::from_secs(80), Some(Duration::from_secs(213)))
        );
    }

    /// What the user is left with when nothing worked, and the one place the
    /// cause is allowed to reach them. "stopped at 0:31 of 3:33" describes what
    /// they can already hear; the fault names which of several very different
    /// failures it was.
    #[test]
    fn the_message_names_the_fault_when_one_was_recorded() {
        let mut stuck = faulted(31, Some(213), 1024 * 1024);
        stuck.rebuilds = MAX_REBUILDS;
        let Ending::GaveUp { at, total, fault } = stuck.ending() else {
            panic!("the budget is spent, so this must not ask for another rebuild");
        };
        assert_eq!(
            stopped_message(at, total, fault),
            "stream stopped at 0:31 of 3:33 (HTTP 403 at 1.0 MiB)"
        );

        // Nothing recorded, so nothing invented.
        assert_eq!(
            stopped_message(at, total, None),
            "stream stopped at 0:31 of 3:33"
        );

        // A livestream has no length to name, and saying "of 0:00" would be
        // worse than saying nothing.
        assert_eq!(stopped_message(at, None, None), "stream stopped at 0:31");
    }

    /// The rewind case: a stream that could not follow the seek ends silently,
    /// and picking up where the dead decoder claimed to be would put the audio
    /// back where the rewind started while the clock -- and the lyrics reading
    /// off it -- stayed at the target.
    #[test]
    fn a_drain_under_a_seek_is_recovered_at_the_target() {
        let Ending::Stalled(from) = seeking(80, Some(213), 45).ending() else {
            panic!("a source that drained under a seek has not finished");
        };
        assert_eq!(
            from,
            Duration::from_secs(45),
            "the rebuild picks up where the seek was aimed"
        );
    }

    /// Otherwise a rewind landing inside the last few seconds would be read as
    /// the track ending, and answered by playing the next song.
    #[test]
    fn a_seek_outstanding_is_never_a_finished_track() {
        assert!(matches!(
            seeking(213, Some(213), 200).ending(),
            Ending::Stalled(_)
        ));
        // Nor for a livestream, where a drain is otherwise always taken at its
        // word for want of anything to have fallen short of.
        assert!(matches!(seeking(80, None, 45).ending(), Ending::Stalled(_)));
    }

    #[test]
    fn a_seek_is_believed_once_the_stream_plays_past_it() {
        let mut rewinding = track(80, Some(213));
        rewinding.aim_at(Duration::from_secs(45));

        // Arriving at the target proves nothing: rodio reports the position it
        // was asked to seek to whether or not the stream could follow.
        rewinding.played_to(Duration::from_secs(45));
        assert_eq!(rewinding.seeking_to, Some(Duration::from_secs(45)));

        // Playing on from it could only have come from audio decoded there.
        rewinding.played_to(Duration::from_secs(46));
        assert_eq!(rewinding.seeking_to, None);
    }

    /// Otherwise pressing forward once more at the end of a song would open a
    /// stream only to discover the track was over.
    #[test]
    fn a_seek_past_the_end_is_not_held_outstanding() {
        let mut ending = track(211, Some(213));
        ending.aim_at(Duration::from_secs(240));
        assert_eq!(ending.seeking_to, None);
        // So a drain right afterwards is read as the track ending, which is
        // what it is, rather than as a seek to recover.
        assert!(matches!(ending.ending(), Ending::Finished));

        // A livestream has no end to have been sent past.
        let mut live = track(200, None);
        live.aim_at(Duration::from_secs(240));
        assert_eq!(live.seeking_to, Some(Duration::from_secs(240)));
    }

    #[test]
    fn a_spent_budget_reports_the_seek_target_too() {
        let mut stuck = seeking(80, Some(213), 45);
        stuck.rebuilds = MAX_REBUILDS;
        let Ending::GaveUp { at, total, .. } = stuck.ending() else {
            panic!("the budget is spent, so this must not ask for another rebuild");
        };
        assert_eq!(
            (at, total),
            (Duration::from_secs(45), Some(Duration::from_secs(213)))
        );
    }

    #[test]
    fn playing_on_from_a_rebuild_returns_the_budget() {
        // A rebuild at 1:00 that has since played to 1:06 has recovered, so the
        // next stall in a long track is not held against it.
        let mut recovered = track(66, Some(213));
        recovered.offset = Duration::from_secs(60);
        recovered.rebuilds = MAX_REBUILDS;
        recovered.note_progress();
        assert_eq!(recovered.rebuilds, 0);

        // One that died again immediately keeps spending it, so a stream that
        // cannot play at all stops instead of retrying forever.
        let mut stalled = track(61, Some(213));
        stalled.offset = Duration::from_secs(60);
        stalled.rebuilds = 1;
        stalled.note_progress();
        assert_eq!(stalled.rebuilds, 1);
    }

    /// Opening a stream blocks for a second or more. What arrives during it
    /// decides whether the track that was opened is still the one wanted.
    #[test]
    fn a_choice_made_while_a_stream_opens_supersedes_it() {
        let (tx, rx) = channel();
        let mut queued = VecDeque::new();

        // Nothing waiting: the track that was opened is still the one to play.
        assert!(!drain(&rx, &mut queued));
        assert!(queued.is_empty());

        // A volume change says nothing about which track is wanted, so it is
        // kept for the new track rather than taken as a reason to drop it.
        tx.send(Command::SetVolume(0.5)).unwrap();
        assert!(!drain(&rx, &mut queued));
        assert_eq!(queued.len(), 1);

        // The user chose something else while this was opening.
        tx.send(Command::Load {
            title: "another".into(),
        })
        .unwrap();
        assert!(drain(&rx, &mut queued));
        // Everything drained is kept in order: the commands still have to run,
        // it is only the stream in hand that is thrown away.
        assert_eq!(queued.len(), 2);
        assert!(matches!(queued[0], Command::SetVolume(_)));
        assert!(matches!(queued[1], Command::Load { .. }));
    }

    #[test]
    fn a_stop_during_an_open_also_supersedes_it() {
        let (tx, rx) = channel();
        let mut queued = VecDeque::new();
        tx.send(Command::Stop).unwrap();
        assert!(drain(&rx, &mut queued), "nothing should start after a stop");
    }

    #[test]
    fn positions_are_named_as_a_clock() {
        assert_eq!(clock(Duration::from_secs(83)), "1:23");
        assert_eq!(clock(Duration::from_secs(9)), "0:09");
    }
}
