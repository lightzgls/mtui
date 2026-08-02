//! Audio playback, owned by a single dedicated thread.
//!
//! The UI never touches rodio or the decoder. It sends [`Command`]s down a
//! channel and reads an atomically-published [`Snapshot`]; the player thread
//! owns everything else. That keeps the audio path free of locks the UI could
//! contend on, and keeps redraws from ever blocking on I/O.

pub mod backend;
pub mod chunked;

use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};
use rodio::Source;

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
    /// Stream and play a resolved URL, replacing whatever is playing.
    Play {
        url: String,
        title: String,
    },
    Pause,
    Resume,
    TogglePause,
    Stop,
    Seek(Duration),
    /// Clamped to 0.0..=2.0 by the player thread.
    SetVolume(f32),
    Shutdown,
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
    snapshot: Arc<Mutex<Snapshot>>,
    handle: Option<thread::JoinHandle<()>>,
}

impl Player {
    /// Spawns the player thread and its private tokio runtime.
    pub fn spawn() -> Result<Self> {
        let (tx, rx) = channel();
        let snapshot = Arc::new(Mutex::new(Snapshot {
            volume: 1.0,
            ..Default::default()
        }));

        let thread_snapshot = Arc::clone(&snapshot);
        let handle = thread::Builder::new()
            .name("mtui-player".to_string())
            .spawn(move || run(rx, thread_snapshot))
            .context("failed to spawn player thread")?;

        Ok(Self {
            tx,
            snapshot,
            handle: Some(handle),
        })
    }

    /// Sends a command. Fails only if the player thread has died.
    pub fn send(&self, cmd: Command) -> Result<()> {
        self.tx.send(cmd).context("player thread is gone")
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
    /// Length according to the container. `None` for a livestream, which has no
    /// end to fall short of -- so a drained source is always taken at its word.
    total: Option<Duration>,
    /// Track time the current rodio source begins at. Non-zero once a stream
    /// has been rebuilt: rodio's clock restarts with every source it is given.
    offset: Duration,
    /// Last position published, and where a rebuild picks up.
    position: Duration,
    rebuilds: u8,
}

/// Why the queue went empty, decided before anything is touched so the caller
/// is free to replace the track it was read from.
enum Ending {
    /// The track ran out, which is the ordinary case.
    Finished,
    /// The stream died this far in. Worth another attempt.
    Stalled(Duration),
    /// The stream died and the rebuild budget is spent.
    GaveUp(Duration, Duration),
}

impl Track {
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

    fn ending(&self) -> Ending {
        let Some(total) = self.total else {
            return Ending::Finished;
        };
        if self.position + END_GRACE >= total {
            return Ending::Finished;
        }
        if self.rebuilds >= MAX_REBUILDS {
            return Ending::GaveUp(self.position, total);
        }
        Ending::Stalled(self.position)
    }
}

/// `M:SS`, for the one place the player has to name a position itself.
fn clock(d: Duration) -> String {
    let secs = d.as_secs();
    format!("{}:{:02}", secs / 60, secs % 60)
}

/// Player thread body. Owns the output device, the decoder, and the runtime.
fn run(rx: Receiver<Command>, snapshot: Arc<Mutex<Snapshot>>) {
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

    loop {
        let cmd = match rx.recv_timeout(TICK) {
            Ok(cmd) => Some(cmd),
            Err(RecvTimeoutError::Timeout) => None,
            // The Player handle was dropped; nothing can command us again.
            Err(RecvTimeoutError::Disconnected) => break,
        };

        if let Some(cmd) = cmd {
            match cmd {
                Command::Play { url, title } => {
                    update(&snapshot, |s| {
                        s.state = PlayState::Buffering;
                        s.title = title.clone();
                        s.position = Duration::ZERO;
                        s.error = None;
                    });
                    // The loop's own copy is left alone until this settles: the
                    // open below blocks, and nothing can read it in the
                    // meantime. What the UI renders is the snapshot above.
                    match start_stream(&runtime, &player, &url, Duration::ZERO) {
                        Ok(total) => {
                            track = Some(Track {
                                url,
                                total,
                                offset: Duration::ZERO,
                                position: Duration::ZERO,
                                rebuilds: 0,
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
                Command::Pause => {
                    player.pause();
                    state = set_state(&snapshot, PlayState::Paused);
                }
                Command::Resume => {
                    player.play();
                    state = set_state(&snapshot, PlayState::Playing);
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
                    if let Some(offset) = track.as_ref().map(|cur| cur.offset) {
                        match target.checked_sub(offset) {
                            // Served from the ring buffer, which is instant.
                            // If it turns out not to hold the target, the read
                            // after it fails and the source drains -- and the
                            // tick below rebuilds at `position`, which is the
                            // target, so the seek still lands.
                            Some(within) if player.try_seek(within).is_ok() => {
                                if let Some(cur) = track.as_mut() {
                                    cur.position = target;
                                }
                            }
                            // Before this source begins, or refused outright:
                            // only a fresh stream can reach it.
                            _ => {
                                state = rebuild(
                                    &runtime, &player, &snapshot, &mut track, target,
                                );
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
                cur.position = cur.offset + player.get_pos();
            }
            cur.note_progress();
            let position = cur.position;
            update(&snapshot, |s| s.position = position);
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
                state = rebuild(&runtime, &player, &snapshot, &mut track, from);
            }
            Ending::GaveUp(at, total) => {
                track = None;
                state = PlayState::Idle;
                // Silence with a full progress bar is the worst outcome here:
                // it tells the user their music stopped and nothing else.
                set_error(
                    &snapshot,
                    format!("stream stopped at {} of {}", clock(at), clock(total)),
                );
            }
        }
    }

    player.stop();
}

/// Reopens the current track's stream so it plays from `from`, and reports the
/// state to settle in.
///
/// This is the whole recovery path: a stream that died mid-track cannot be
/// resumed in place, because the bytes it needs are long gone from a ring
/// buffer that holds thirty seconds. A fresh stream can be wound forward to any
/// point in the track, so that is what a rebuild is.
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
    let url = cur.url.clone();

    update(snapshot, |s| {
        s.state = PlayState::Buffering;
        s.position = from;
    });

    match start_stream(runtime, player, &url, from) {
        Ok(_) => set_state(snapshot, PlayState::Playing),
        Err(e) => {
            player.stop();
            *track = None;
            set_error(snapshot, format!("stream stopped at {}: {e:#}", clock(from)));
            PlayState::Idle
        }
    }
}

/// Opens the network stream and hands it to rodio, starting `skip` into the
/// track. Returns the track's length when the container states one.
///
/// `Decoder::new_mp4` is used rather than the probing `Decoder::new`: we always
/// request itag 140, so format sniffing would only waste a seek and a read.
fn start_stream(
    runtime: &tokio::runtime::Runtime,
    player: &rodio::Player,
    url: &str,
    skip: Duration,
) -> Result<Option<Duration>> {
    let stream = runtime.block_on(backend::open(url))?;
    let decoder = rodio::Decoder::new_mp4(stream).context("could not decode audio stream")?;
    // Read before the decoder is moved: this is what tells a track that ended
    // from a stream that died, and it is the only place it can be had.
    let total = decoder.total_duration();

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
    Ok(total)
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
            total: total.map(Duration::from_secs),
            offset: Duration::ZERO,
            position: Duration::from_secs(position),
            rebuilds: 0,
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
        assert_eq!(from, Duration::from_secs(80), "rebuild picks up where it died");
    }

    #[test]
    fn a_livestream_never_falls_short() {
        // Nothing states an end, so there is nothing to have stopped short of.
        assert!(matches!(track(80, None).ending(), Ending::Finished));
    }

    #[test]
    fn a_spent_budget_reports_instead_of_retrying() {
        let mut stuck = track(80, Some(213));
        stuck.rebuilds = MAX_REBUILDS;
        let Ending::GaveUp(at, total) = stuck.ending() else {
            panic!("the budget is spent, so this must not ask for another rebuild");
        };
        assert_eq!((at, total), (Duration::from_secs(80), Duration::from_secs(213)));
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

    #[test]
    fn positions_are_named_as_a_clock() {
        assert_eq!(clock(Duration::from_secs(83)), "1:23");
        assert_eq!(clock(Duration::from_secs(9)), "0:09");
    }
}
