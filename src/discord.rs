//! Discord Rich Presence: the card on the user's profile saying what MTUI is
//! playing, and where to hear it.
//!
//! Bound by hand rather than through a crate, for the same reason
//! [`crate::tray`] is. What Discord's local IPC actually asks for is a named
//! pipe, an eight-byte header and a JSON object -- and the crates that wrap it
//! bring a UUID generator, a second serialisation stack and in some cases an
//! async runtime to send a few hundred bytes to a socket on the same machine.
//! `serde_json` is already in the tree, so what is written below compiles
//! nothing new.
//!
//! Nothing here runs on the render path. The socket lives on a thread of its
//! own, and the app hands it an [`Activity`] only when one *materially*
//! changes: Discord rate-limits presence updates, and a card rewritten every
//! frame would be throttled into showing the track before last.
//!
//! Discord not running is not a failure and never surfaces as one. The worker
//! looks for it again every [`RECONNECT`], and until it finds it the whole
//! feature costs one sleeping thread.

use std::io;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// The Discord application this presence belongs to, and the source of the
/// name the card is headed with -- "Listening to **MTUI**" is this
/// application's name, not anything sent from here.
///
/// Baked in rather than asked of each user, and safe to bake in: an application
/// id is public by construction. It travels in every presence payload on the
/// wire and is visible to everyone who can see the card, so there is nothing
/// here to protect. This is exactly why it is *not* handled like the Google
/// client in [`crate::config`] -- that one carries a secret and needs a
/// verification review, and neither is true of this.
///
/// Registering one takes about two minutes and is done once for the project,
/// not once per user: <https://discord.com/developers/applications> -> New
/// Application -> name it `MTUI` -> copy the Application ID here. The Public
/// Key, Client Secret, bot settings and interaction endpoints are not used.
///
/// Empty means the feature is off and says so when asked for, which is the
/// honest failure: a wrong id would connect, hand Discord a card headed with
/// somebody else's application name, and look like a bug in MTUI.
const APPLICATION_ID: &str = "1540750108822339695";

/// Where the "Get MTUI" button goes. Taken from the manifest so the address
/// lives in one place and cannot drift from the one crates.io publishes.
const REPOSITORY: &str = env!("CARGO_PKG_REPOSITORY");

/// Discord's ceiling on presence updates is five in twenty seconds, counted
/// per connection. Held at a quarter of that: what actually changes a card is
/// a track starting, a pause, or a seek, and nothing about any of them is worth
/// spending a rate-limit budget that -- once exhausted -- is spent silently.
/// An update that arrives inside the gap is not dropped, it is held and sent
/// when the gap closes.
const MIN_GAP: Duration = Duration::from_secs(4);

/// How long to wait before looking for Discord again after failing to find it.
///
/// Long, deliberately. Discord being absent is the normal case for most of a
/// session, and probing ten pipe names on a tight loop to keep discovering that
/// is how a background feature becomes a line in someone's profiler.
const RECONNECT: Duration = Duration::from_secs(15);

/// How far two versions of the same clock may differ before the card is
/// rewritten.
///
/// The start timestamp is derived on every tick as "now, less how far in we
/// are", so a track playing undisturbed produces a value that wanders by a few
/// milliseconds a frame. Without a tolerance every one of those would read as a
/// change and the card would be rewritten five times a second. Two seconds is
/// below what anyone can see on Discord's progress bar and far above the jitter,
/// so what survives it is a real seek or a resumed pause.
const DRIFT: Duration = Duration::from_secs(2);

/// Ceiling Discord puts on `details` and `state`, in characters. Anything
/// longer is rejected outright rather than truncated for us.
const MAX_FIELD: usize = 128;

/// Frame opcodes. Only two of the five are ever written from here: the
/// handshake that opens a connection, and the frame every command travels in.
const OP_HANDSHAKE: u32 = 0;
const OP_FRAME: u32 = 1;

/// Where the card's picture comes from.
///
/// The same thumbnail [`crate::source::cover`] draws in the terminal, and free
/// for the same reason: the address is derived from the video id, so putting a
/// picture on the card costs no request from here at all. `hqdefault` rather
/// than the larger names that module ladders through -- Discord scales the
/// image down to a thumbnail either way, and this is the one name YouTube
/// generates for every upload, so it cannot come back missing.
fn cover_url(video_id: &str) -> String {
    format!("https://i.ytimg.com/vi/{video_id}/hqdefault.jpg")
}

fn watch_url(video_id: &str) -> String {
    format!("https://www.youtube.com/watch?v={video_id}")
}

/// Where playback is, as two wall-clock instants.
///
/// Discord's progress bar is not told a position; it is told when the track
/// started and when it ends, and works the bar out itself from the wall clock.
/// That is why a pause cannot be expressed as a state -- see [`Activity::clock`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Clock {
    /// Unix milliseconds at which this track was, or would have been, at
    /// position zero. Derived rather than recorded, so that a seek moves it and
    /// the bar follows.
    pub start_ms: u64,
    /// Unix milliseconds at which it reaches the end. `None` for a livestream,
    /// which has no end to count down to and gets a counting-up clock instead.
    pub end_ms: Option<u64>,
}

/// What the card should say. `None` anywhere this is expected means "show
/// nothing", which is a state worth being able to express: it is what a stop,
/// a quit and a switched-off toggle all reduce to.
#[derive(Debug, Clone, PartialEq)]
pub struct Activity {
    pub video_id: String,
    pub title: String,
    pub artist: String,
    /// Shown as the hover text over the artwork, since the card's two text
    /// lines are already spent on the title and the artist. `None` for the
    /// singles and plain YouTube uploads that genuinely have no album.
    pub album: Option<String>,
    pub paused: bool,
    /// Where the progress bar should be, or `None` for no bar at all.
    ///
    /// `None` while paused, and that is not a shortcut. Discord has no notion
    /// of a stopped clock: a card holds two timestamps and the client animates
    /// between them, so there is no value that means "frozen at 1:12". Leaving
    /// the last pair in place would show a bar that carried on advancing
    /// through a track nobody is hearing, which is worse than no bar -- so the
    /// bar goes away and the artist line says why.
    pub clock: Option<Clock>,
}

impl Activity {
    /// Whether this describes the same card as `other`, to within the tolerance
    /// the wall clock is worth trusting to.
    ///
    /// The whole point of this type's existence in the app: it is what turns
    /// "the presence is recomputed on every tick" into "the socket is written
    /// to when a track changes". See [`DRIFT`].
    fn same_as(&self, other: &Self) -> bool {
        let same_clock = match (self.clock, other.clock) {
            (None, None) => true,
            (Some(a), Some(b)) => {
                a.end_ms.is_some() == b.end_ms.is_some()
                    && a.start_ms.abs_diff(b.start_ms) <= DRIFT.as_millis() as u64
            }
            _ => false,
        };
        same_clock
            && self.video_id == other.video_id
            && self.title == other.title
            && self.artist == other.artist
            && self.album == other.album
            && self.paused == other.paused
    }

    /// The `activity` object of a `SET_ACTIVITY` command.
    fn payload(&self) -> serde_json::Value {
        // A paused track says so on the artist line, because that line is the
        // only place left to say it: the bar is gone (see `clock`), the two
        // text lines are spoken for, and the alternative -- Discord's small
        // corner badge -- is an image that would have to be uploaded to the
        // application by hand, which is setup this feature is otherwise free of.
        //
        // The empty-artist arms are not hypothetical. A plain YouTube search
        // often cannot name one, and this is a line on someone's public
        // profile -- " (paused)" with nothing in front of it is not something
        // to ship because the common case reads fine.
        let state = match (self.artist.trim(), self.paused) {
            ("", true) => "Paused".to_string(),
            ("", false) => String::new(),
            (artist, true) => format!("{artist} (paused)"),
            (artist, false) => artist.to_string(),
        };

        let mut activity = serde_json::json!({
            // 2 is LISTENING. It is what heads the card "Listening to MTUI"
            // rather than "Playing MTUI", and it is the difference between
            // reading as a music player and reading as a game.
            "type": 2,
            // 2 is Details. It makes Discord use the song title below as the
            // compact status text on the user card and member list, rather than
            // the fixed application name. This is the same field Pear Desktop
            // uses; the expanded activity still identifies MTUI normally.
            "status_display_type": 2,
            "details": clamp(&self.title),
            "state": clamp(&state),
            "assets": {
                // Discord accepts an ordinary https address here and proxies
                // it. The documented alternative is uploading named assets to
                // the application, which would mean one upload per album --
                // which is to say, it is not an alternative.
                "large_image": cover_url(&self.video_id),
                "large_text": clamp(self.album.as_deref().unwrap_or(&self.title)),
                // Discord does not show activity buttons on every compact
                // surface. Linking the artwork gives the repository a second
                // entry point wherever the client makes activity art clickable.
                "large_url": REPOSITORY,
            },
            // Two is the maximum, and both are spent deliberately: one so that
            // anyone reading the card can hear the track, one so they can find
            // out what played it.
            "buttons": [
                { "label": "Listen on YouTube", "url": watch_url(&self.video_id) },
                { "label": "Get MTUI", "url": REPOSITORY },
            ],
        });

        if let Some(clock) = self.clock {
            let mut timestamps = serde_json::json!({ "start": clock.start_ms });
            // Omitted for a livestream, where the two-timestamp form would ask
            // Discord to count down to an end that does not exist. With only a
            // start, the card counts up instead.
            if let Some(end) = clock.end_ms {
                timestamps["end"] = end.into();
            }
            activity["timestamps"] = timestamps;
        }
        activity
    }
}

/// Fits a string to what Discord will accept in `details` or `state`.
///
/// Both ends are real: a field of fewer than two characters is rejected along
/// with the whole command, and so is one over [`MAX_FIELD`]. Neither is
/// hypothetical here -- YouTube has single-character track titles, and it has
/// titles that are a paragraph of credits.
///
/// Counted in `chars` rather than bytes because Discord counts what it will
/// render, and a 128-byte cut through a CJK title would also be a cut through a
/// codepoint.
fn clamp(text: &str) -> String {
    let mut out: String = text.chars().take(MAX_FIELD).collect();
    // A space rather than a placeholder: it satisfies the minimum without
    // putting a word on the card that the track did not have.
    while out.chars().count() < 2 {
        out.push(' ');
    }
    out
}

/// Unix milliseconds now. Saturates rather than panicking on a clock set before
/// 1970, which is a machine misconfiguration and not a reason to lose audio.
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// What the app hands the worker.
enum Message {
    Set(Box<Activity>),
    Clear,
}

/// Handle to the presence worker. Cheap to hold, and holds nothing itself while
/// the feature is switched off.
pub struct Presence {
    tx: Sender<Message>,
    /// The last activity handed to the worker, so an unchanged card costs a
    /// comparison on this thread rather than a message, a serialisation and a
    /// socket write on the other.
    last: Option<Activity>,
    /// Whether a clear has already been sent for the current silence. Without
    /// it, every tick with nothing playing would post another one.
    cleared: bool,
    /// The user's switch. Off means the card is taken down and nothing is sent
    /// while it stays off -- not that updates are computed and discarded.
    enabled: bool,
    /// Empty unless the build has no [`APPLICATION_ID`] and no configured
    /// override, in which case it is why, ready for the status bar.
    unavailable: Option<String>,
}

impl Presence {
    /// Starts the worker. Never fails: a Discord that is not running, and a
    /// build with no application id, are both states this reports through
    /// [`Presence::status`] rather than errors that would have to be handled on
    /// the way up to a music player's `main`.
    pub fn spawn(application_id: Option<String>, enabled: bool) -> Self {
        let (tx, rx) = channel();
        let id = application_id
            .filter(|id| !id.trim().is_empty())
            .unwrap_or_else(|| APPLICATION_ID.to_string());

        if id.trim().is_empty() {
            return Self {
                tx,
                last: None,
                cleared: true,
                enabled: false,
                unavailable: Some(
                    "this build has no Discord application id -- see the README".to_string(),
                ),
            };
        }

        // Detached rather than held: see `Drop`. A failure to spawn is treated
        // exactly like Discord being absent, because to the user it is the same
        // thing -- no card, and everything else still playing.
        let spawn_error = thread::Builder::new()
            .name("mtui-discord".to_string())
            .spawn(move || run(&id, &rx))
            .err();
        if let Some(error) = &spawn_error {
            crate::diagnostics::error("discord", &format!("worker did not start: {error}"));
        }

        Self {
            tx,
            last: None,
            cleared: true,
            enabled: enabled && spawn_error.is_none(),
            unavailable: spawn_error.map(|_| "the presence worker could not start".to_string()),
        }
    }

    /// Flips the switch and returns where it landed. Taking the card down is
    /// immediate; putting one back up happens on the next tick, which is within
    /// a frame.
    pub fn toggle(&mut self) -> bool {
        self.enabled = !self.enabled && self.unavailable.is_none();
        if !self.enabled {
            self.clear();
        }
        self.enabled
    }

    /// Current value of the user's switch.
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// What to tell the user about the switch they just pressed.
    pub fn status(&self) -> String {
        match &self.unavailable {
            Some(reason) => format!("Discord presence unavailable: {reason}"),
            None if self.enabled => "Discord presence on".to_string(),
            None => "Discord presence off".to_string(),
        }
    }

    /// Offers the card that should be showing now.
    ///
    /// Called every tick and cheap on all but the few that change something --
    /// which is the entire reason [`Activity::same_as`] exists.
    pub fn publish(&mut self, next: Option<Activity>) {
        if !self.enabled {
            return;
        }
        let Some(next) = next else {
            self.clear();
            return;
        };
        if self.last.as_ref().is_some_and(|last| last.same_as(&next)) {
            return;
        }
        // A dead worker reads the same as a Discord that is not running, which
        // is what keeps this from ever mattering to playback.
        let _ = self.tx.send(Message::Set(Box::new(next.clone())));
        self.last = Some(next);
        self.cleared = false;
    }

    fn clear(&mut self) {
        self.last = None;
        if !self.cleared {
            let _ = self.tx.send(Message::Clear);
            self.cleared = true;
        }
    }
}

impl Drop for Presence {
    /// Asks for the card to come down, and does not wait to see it happen.
    ///
    /// Not joined on purpose. Quitting MTUI must never block on how quickly
    /// another program reads its socket, and it does not need to: closing the
    /// connection is what Discord treats as the end of a presence, and process
    /// exit closes it whether this message was ever read or not. The send is
    /// worth making anyway for the case that matters -- the worker is idle,
    /// takes it immediately, and the card is gone before the terminal is back.
    fn drop(&mut self) {
        let _ = self.tx.send(Message::Clear);
    }
}

/// The worker loop: hold a connection when Discord is there, find one again
/// when it is not, and never let either concern reach the caller.
fn run(application_id: &str, rx: &Receiver<Message>) {
    crate::diagnostics::info("discord", "worker started");
    let mut conn: Option<Connection> = None;
    // The card that should be showing, which is not the card that has been
    // sent: an update arriving inside `MIN_GAP`, or while Discord is closed,
    // waits here rather than being dropped.
    let mut wanted: Option<Activity> = None;
    let mut sent: Option<Activity> = None;
    let mut last_write: Option<Instant> = None;
    let mut next_attempt = Instant::now();
    let mut unavailable_logged = false;

    loop {
        // Whether the card Discord is showing is the card it should be showing.
        // Compared as whole options rather than as two activities, because
        // taking a card *down* is as much of a pending change as putting one
        // up -- and a stop that waited out a reconnect interval would leave a
        // finished track on the user's profile for fifteen seconds.
        let pending = wanted != sent;

        // Wake for whichever comes first: a message, the rate-limit gap
        // closing on a held update, or the next attempt at a connection.
        let wait = if !pending {
            RECONNECT
        } else if conn.is_none() {
            next_attempt.saturating_duration_since(Instant::now())
        } else {
            last_write.map_or(Duration::ZERO, |at| MIN_GAP.saturating_sub(at.elapsed()))
        };

        match rx.recv_timeout(wait) {
            Ok(Message::Set(activity)) => wanted = Some(*activity),
            Ok(Message::Clear) => wanted = None,
            Err(RecvTimeoutError::Timeout) => {}
            // The app is on its way out. Nothing to clean up: dropping the
            // connection closes the pipe, which is how Discord is told.
            Err(RecvTimeoutError::Disconnected) => {
                crate::diagnostics::info("discord", "worker stopped");
                return;
            }
        }

        // Nothing to show and nothing showing. Deliberately does not open a
        // connection: a user who has never played anything should not appear in
        // Discord's client list at all.
        if wanted.is_none() && sent.is_none() {
            continue;
        }

        if conn.is_none() {
            if Instant::now() < next_attempt {
                continue;
            }
            next_attempt = Instant::now() + RECONNECT;
            conn = match Connection::open(application_id) {
                Ok(connection) => {
                    crate::diagnostics::info("discord", "connected");
                    unavailable_logged = false;
                    Some(connection)
                }
                Err(error) => {
                    if !unavailable_logged {
                        crate::diagnostics::warn(
                            "discord",
                            &format!("connection unavailable; retrying: {error}"),
                        );
                        unavailable_logged = true;
                    }
                    None
                }
            };
            if conn.is_none() {
                continue;
            }
            // A fresh connection knows nothing of what was sent over the last
            // one, so the next write must happen whatever the comparison says.
            sent = None;
            last_write = None;
        }

        if sent.as_ref() == wanted.as_ref() {
            continue;
        }
        if let Some(at) = last_write
            && at.elapsed() < MIN_GAP
        {
            continue;
        }

        let Some(active) = conn.as_mut() else {
            continue;
        };
        // Every command is answered, and an unread reply is a few hundred bytes
        // left in the pipe. Left to accumulate they would eventually fill it and
        // block a write -- on the thread that must never block. Draining takes
        // whatever has arrived and blocks on none of it.
        active.drain();

        match active.set_activity(wanted.as_ref()) {
            Ok(()) => {
                crate::diagnostics::info(
                    "discord",
                    if wanted.is_some() {
                        "activity updated"
                    } else {
                        "activity cleared"
                    },
                );
                sent = wanted.clone();
                last_write = Some(Instant::now());
            }
            // Discord closed, or was closed. Drop the connection and let the
            // reconnect path pick it up; `wanted` survives, so whatever is
            // playing goes back up the moment Discord returns.
            Err(error) => {
                crate::diagnostics::error(
                    "discord",
                    &format!("activity update failed; reconnecting: {error}"),
                );
                conn = None;
                sent = None;
                next_attempt = Instant::now() + RECONNECT;
            }
        }
    }
}

/// An open IPC connection that has completed its handshake.
struct Connection {
    pipe: socket::Pipe,
    /// Distinguishes one command's reply from another's. Discord requires the
    /// field and echoes it back; nothing here reads the echo, so a counter is
    /// as much uniqueness as is called for -- which is the whole reason no UUID
    /// crate is needed.
    nonce: u64,
}

impl Connection {
    fn open(application_id: &str) -> io::Result<Self> {
        let mut conn = Self {
            pipe: socket::connect()?,
            nonce: 0,
        };
        conn.frame(
            OP_HANDSHAKE,
            &serde_json::json!({ "v": 1, "client_id": application_id }),
        )?;
        Ok(conn)
    }

    /// Sends the card, or takes it down when `activity` is `None`.
    fn set_activity(&mut self, activity: Option<&Activity>) -> io::Result<()> {
        self.nonce += 1;
        let payload = serde_json::json!({
            "cmd": "SET_ACTIVITY",
            "nonce": self.nonce.to_string(),
            "args": {
                // Discord ties the presence to a process so it can drop the
                // card if that process dies without saying goodbye.
                "pid": std::process::id(),
                "activity": activity.map(Activity::payload),
            },
        });
        self.frame(OP_FRAME, &payload)
    }

    /// Writes one frame: opcode, length, body -- all little-endian, all in a
    /// single `write_all`. One call rather than three on purpose: a header that
    /// went out without its body would desynchronise the stream permanently,
    /// and there is no resynchronising a length-prefixed protocol.
    fn frame(&mut self, op: u32, payload: &serde_json::Value) -> io::Result<()> {
        let body = serde_json::to_vec(payload).map_err(io::Error::other)?;
        let len = u32::try_from(body.len()).map_err(io::Error::other)?;

        let mut frame = Vec::with_capacity(8 + body.len());
        frame.extend_from_slice(&op.to_le_bytes());
        frame.extend_from_slice(&len.to_le_bytes());
        frame.extend_from_slice(&body);

        self.pipe.write_all(&frame)?;
        self.pipe.flush()
    }

    /// Reads and discards whatever replies have arrived, and blocks on none.
    ///
    /// Deliberately not parsed into frames, and not merely out of laziness: a
    /// reply that arrived split down the middle would leave a frame-wise
    /// reader holding half a header and permanently out of step with the
    /// stream, with no way to tell. Bytes have no such state. Nothing here
    /// needs what the replies say -- the only thing one could report that would
    /// change what happens next is that the connection is gone, and a gone
    /// connection announces itself on the next write. What this is for is the
    /// pipe buffer; see the call site.
    fn drain(&mut self) {
        self.pipe.discard();
    }
}

/// The transport, which is the only part of this that differs by platform.
///
/// Discord listens on up to ten sockets, numbered for the clients that may run
/// side by side -- stable, PTB and Canary each take the next free one. Trying
/// them in order is how the running client is found, and is the whole of the
/// discovery protocol.
mod socket {
    use std::io;

    /// How many `discord-ipc-N` names to try before concluding Discord is not
    /// running. Ten is what Discord's own libraries use.
    const MAX_SOCKETS: u32 = 10;

    #[cfg(windows)]
    mod imp {
        use std::fs::{File, OpenOptions};
        use std::io::{self, Read, Write};
        use std::os::windows::io::AsRawHandle;

        #[link(name = "kernel32")]
        unsafe extern "system" {
            /// Says how much is waiting without taking any of it, which is what
            /// makes an unblocking read possible on a pipe std opens in
            /// blocking mode.
            fn PeekNamedPipe(
                pipe: isize,
                buffer: *mut u8,
                size: u32,
                read: *mut u32,
                available: *mut u32,
                left: *mut u32,
            ) -> i32;
        }

        /// A named pipe, opened as an ordinary file.
        ///
        /// No FFI is needed to get here: Windows exposes pipes through the
        /// filesystem namespace, and `OpenOptions` on `\\.\pipe\...` is a real
        /// duplex connection to whatever is serving that name.
        pub struct Pipe(File);

        pub fn open(index: u32) -> io::Result<Pipe> {
            OpenOptions::new()
                .read(true)
                .write(true)
                .open(format!(r"\\.\pipe\discord-ipc-{index}"))
                .map(Pipe)
        }

        impl Pipe {
            pub fn write_all(&mut self, buf: &[u8]) -> io::Result<()> {
                self.0.write_all(buf)
            }

            pub fn flush(&mut self) -> io::Result<()> {
                self.0.flush()
            }

            /// Throws away whatever has arrived, and blocks on none of it.
            ///
            /// The peek is what makes that possible: std opens the pipe in
            /// blocking mode, so a plain read with nothing queued would park
            /// this thread until Discord happened to say something. Asking how
            /// much is there first means never reading more than has already
            /// landed.
            pub fn discard(&mut self) {
                let mut scratch = [0u8; 1024];
                loop {
                    let mut available = 0u32;
                    let ok = unsafe {
                        PeekNamedPipe(
                            self.0.as_raw_handle() as isize,
                            std::ptr::null_mut(),
                            0,
                            std::ptr::null_mut(),
                            &mut available,
                            std::ptr::null_mut(),
                        )
                    };
                    if ok == 0 || available == 0 {
                        return;
                    }
                    let want = (available as usize).min(scratch.len());
                    if self.0.read(&mut scratch[..want]).is_err() {
                        return;
                    }
                }
            }
        }
    }

    #[cfg(not(windows))]
    mod imp {
        use std::io::{self, Read, Write};
        use std::os::unix::net::UnixStream;
        use std::path::PathBuf;
        use std::time::Duration;

        /// Read timeout standing in for the peek Windows gets for free. Short
        /// enough not to matter to a thread whose other job is a fifteen-second
        /// reconnect, long enough that a reply already on its way is not
        /// mistaken for an empty pipe.
        const READ_TIMEOUT: Duration = Duration::from_millis(20);

        pub struct Pipe(UnixStream);

        /// Directories Discord may put its socket in. The bare runtime
        /// directory is where a system install lands; the two below it are
        /// where Flatpak and Snap relocate it to, and a user on either has no
        /// socket at the plain path at all.
        fn roots() -> Vec<PathBuf> {
            let base = std::env::var_os("XDG_RUNTIME_DIR")
                .or_else(|| std::env::var_os("TMPDIR"))
                .map_or_else(|| PathBuf::from("/tmp"), PathBuf::from);
            vec![
                base.clone(),
                base.join("app/com.discordapp.Discord"),
                base.join("snap.discord"),
            ]
        }

        pub fn open(index: u32) -> io::Result<Pipe> {
            let mut last = io::Error::new(
                io::ErrorKind::NotFound,
                "no Discord socket in any known directory",
            );
            for root in roots() {
                match UnixStream::connect(root.join(format!("discord-ipc-{index}"))) {
                    Ok(stream) => {
                        stream.set_read_timeout(Some(READ_TIMEOUT))?;
                        return Ok(Pipe(stream));
                    }
                    Err(err) => last = err,
                }
            }
            Err(last)
        }

        impl Pipe {
            pub fn write_all(&mut self, buf: &[u8]) -> io::Result<()> {
                self.0.write_all(buf)
            }

            pub fn flush(&mut self) -> io::Result<()> {
                self.0.flush()
            }

            /// Throws away whatever has arrived. Bounded by the read timeout
            /// rather than by a peek, which is the one behavioural difference
            /// from the Windows path -- and it costs nothing that matters,
            /// because nothing is being parsed out of these bytes and a
            /// truncated read is therefore not a truncated anything.
            pub fn discard(&mut self) {
                let mut scratch = [0u8; 1024];
                while let Ok(read) = self.0.read(&mut scratch) {
                    // Zero means the peer has closed. Returning rather than
                    // looping is what keeps that from spinning: the next write
                    // will fail and the connection will be rebuilt.
                    if read == 0 {
                        return;
                    }
                }
            }
        }
    }

    pub use imp::Pipe;

    /// The first socket that answers, or the last error if none did.
    pub fn connect() -> io::Result<Pipe> {
        let mut last = io::Error::new(io::ErrorKind::NotFound, "Discord is not running");
        for index in 0..MAX_SOCKETS {
            match imp::open(index) {
                Ok(pipe) => return Ok(pipe),
                Err(err) => last = err,
            }
        }
        Err(last)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn activity() -> Activity {
        Activity {
            video_id: "dQw4w9WgXcQ".to_string(),
            title: "Let It Happen".to_string(),
            artist: "Tame Impala".to_string(),
            album: Some("Currents".to_string()),
            paused: false,
            clock: Some(Clock {
                start_ms: 1_700_000_000_000,
                end_ms: Some(1_700_000_467_000),
            }),
        }
    }

    #[test]
    fn a_clock_that_only_jittered_is_the_same_card() {
        // The case this exists for: a track playing undisturbed, whose derived
        // start wanders by a frame's worth of milliseconds every tick. Every
        // one of these would otherwise be a socket write.
        let mut drifted = activity();
        drifted.clock = Some(Clock {
            start_ms: 1_700_000_001_800,
            end_ms: Some(1_700_000_468_800),
        });
        assert!(activity().same_as(&drifted));
    }

    #[test]
    fn a_seek_is_not() {
        let mut seeked = activity();
        seeked.clock = Some(Clock {
            start_ms: 1_700_000_030_000,
            end_ms: Some(1_700_000_497_000),
        });
        assert!(!activity().same_as(&seeked));
    }

    #[test]
    fn pausing_and_resuming_both_change_the_card() {
        let mut paused = activity();
        paused.paused = true;
        paused.clock = None;
        assert!(!activity().same_as(&paused));
        assert!(!paused.same_as(&activity()));
    }

    #[test]
    fn a_livestream_is_not_the_same_as_a_track_of_known_length() {
        // Both have a start and neither would fail the drift check, so without
        // the explicit test on `end_ms` a stream would inherit the previous
        // track's countdown.
        let mut live = activity();
        live.clock = Some(Clock {
            start_ms: 1_700_000_000_000,
            end_ms: None,
        });
        assert!(!activity().same_as(&live));
    }

    #[test]
    fn a_different_track_of_the_same_name_is_a_different_card() {
        let mut other = activity();
        other.video_id = "aaaaaaaaaaa".to_string();
        assert!(!activity().same_as(&other));
    }

    #[test]
    fn fields_are_fitted_to_what_discord_accepts() {
        // Both ends are reachable from real YouTube titles.
        assert_eq!(clamp("x").chars().count(), 2);
        assert_eq!(clamp(&"a".repeat(500)).chars().count(), MAX_FIELD);
        // Cut on a character, never through one -- a CJK title is three bytes
        // per glyph, so a byte-wise truncation would produce invalid UTF-8.
        let cjk = "夜".repeat(200);
        assert_eq!(clamp(&cjk).chars().count(), MAX_FIELD);
    }

    #[test]
    fn the_payload_says_listening_and_carries_both_buttons() {
        let payload = activity().payload();
        assert_eq!(payload["type"], 2);
        assert_eq!(payload["status_display_type"], 2);
        assert_eq!(payload["details"], "Let It Happen");
        assert_eq!(payload["state"], "Tame Impala");
        assert_eq!(payload["large_text"], serde_json::Value::Null);
        assert_eq!(payload["assets"]["large_text"], "Currents");
        assert_eq!(payload["assets"]["large_url"], REPOSITORY);
        assert!(
            payload["assets"]["large_image"]
                .as_str()
                .is_some_and(|url| url.contains("dQw4w9WgXcQ"))
        );
        assert_eq!(payload["timestamps"]["start"], 1_700_000_000_000u64);
        assert_eq!(payload["timestamps"]["end"], 1_700_000_467_000u64);

        let buttons = payload["buttons"].as_array().expect("two buttons");
        assert_eq!(buttons.len(), 2);
        assert!(
            buttons[0]["url"]
                .as_str()
                .is_some_and(|url| url.contains("dQw4w9WgXcQ"))
        );
        // Every label has to fit Discord's 32-character ceiling, and they are
        // written here rather than fetched, so this is the only thing that
        // would catch one growing past it.
        for button in buttons {
            let label = button["label"].as_str().expect("a label");
            assert!((1..=32).contains(&label.chars().count()), "{label:?}");
        }
    }

    #[test]
    fn a_paused_card_has_no_bar_and_says_so() {
        let mut paused = activity();
        paused.paused = true;
        paused.clock = None;
        let payload = paused.payload();
        assert_eq!(payload["state"], "Tame Impala (paused)");
        // The important half: no timestamps at all, rather than stale ones that
        // would leave the bar advancing through a track nobody is hearing.
        assert_eq!(payload["timestamps"], serde_json::Value::Null);
    }

    #[test]
    fn a_livestream_gets_a_start_and_no_end() {
        let mut live = activity();
        live.clock = Some(Clock {
            start_ms: 1_700_000_000_000,
            end_ms: None,
        });
        let payload = live.payload();
        assert_eq!(payload["timestamps"]["start"], 1_700_000_000_000u64);
        assert_eq!(payload["timestamps"]["end"], serde_json::Value::Null);
    }

    #[test]
    fn a_track_with_no_named_artist_is_not_left_saying_paused_about_nobody() {
        // What a plain YouTube search produces: a title, and no artist the
        // source was willing to name. The app blanks `unknown` before it gets
        // here, so both of these are reachable.
        let mut anonymous = activity();
        anonymous.artist = String::new();
        assert_eq!(anonymous.payload()["state"], "  ");

        anonymous.paused = true;
        anonymous.clock = None;
        assert_eq!(anonymous.payload()["state"], "Paused");
    }

    #[test]
    fn a_track_with_no_album_hovers_its_own_title() {
        let mut single = activity();
        single.album = None;
        assert_eq!(single.payload()["assets"]["large_text"], "Let It Happen");
    }

    #[test]
    fn a_blank_override_uses_the_built_in_application_id() {
        // An empty discord.json must not disable the official project id.
        assert!(!APPLICATION_ID.is_empty());
        let mut presence = Presence::spawn(Some("   ".to_string()), true);
        assert!(presence.enabled);
        assert!(!presence.toggle());
    }

    #[test]
    fn switching_off_takes_the_card_down_and_keeps_it_down() {
        let mut presence = Presence::spawn(Some("123".to_string()), true);
        presence.publish(Some(activity()));
        assert!(presence.last.is_some());

        assert!(!presence.toggle());
        assert!(presence.last.is_none(), "the card should have been dropped");

        // The half that matters for privacy: publishing while off must not
        // quietly restore what the user just took down.
        presence.publish(Some(activity()));
        assert!(presence.last.is_none());
    }

    #[test]
    fn an_unchanged_card_is_not_republished() {
        let mut presence = Presence::spawn(Some("123".to_string()), true);
        presence.publish(Some(activity()));
        let first = presence.last.clone();
        presence.publish(Some(activity()));
        assert_eq!(presence.last, first);
    }
}
