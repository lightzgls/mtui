//! MPRIS on the desktop session bus. D-Bus calls never run on the UI thread.

use std::sync::{Arc, Mutex, atomic::{AtomicBool, Ordering}, mpsc::{Receiver, Sender, channel}};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use dbus::{Message, Path, arg::{PropMap, Variant}, blocking::Connection,
    channel::{MatchingReceiver, Sender as DbusSender}, message::MatchRule};
use dbus_crossroads::{Crossroads, IfaceBuilder};

use super::{MediaCommand, MediaInfo};
use crate::player::PlayState;

const PATH: &str = "/org/mpris/MediaPlayer2";
const PLAYER: &str = "org.mpris.MediaPlayer2.Player";
const NAME: &str = "org.mpris.MediaPlayer2.mtui";

pub struct MediaSession {
    info: Arc<Mutex<MediaInfo>>,
    commands: Receiver<MediaCommand>,
    stopped: Arc<AtomicBool>,
    pump: Option<JoinHandle<()>>,
}

impl MediaSession {
    pub fn spawn() -> Result<Self> {
        let (tx, commands) = channel();
        let (ready_tx, ready_rx) = channel();
        let info = Arc::new(Mutex::new(MediaInfo::default()));
        let stopped = Arc::new(AtomicBool::new(false));
        let state = State { info: Arc::clone(&info), tx };
        let stop = Arc::clone(&stopped);
        let pump = thread::Builder::new().name("mtui-mpris".into()).spawn(move || {
            if let Err(error) = serve(state, stop, &ready_tx) {
                let message = format!("{error:#}");
                let _ = ready_tx.send(Err(anyhow!(message.clone())));
                crate::diagnostics::error("media", &message);
            }
        }).context("could not start MPRIS")?;
        match ready_rx.recv().context("MPRIS stopped during startup")? {
            Ok(()) => Ok(Self { info, commands, stopped, pump: Some(pump) }),
            Err(error) => { let _ = pump.join(); Err(error) }
        }
    }

    pub fn publish(&self, info: MediaInfo) {
        *self.info.lock().unwrap_or_else(|e| e.into_inner()) = info;
    }

    pub fn next_command(&self) -> Option<MediaCommand> { self.commands.try_recv().ok() }
}

impl Drop for MediaSession {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Relaxed);
        if let Some(pump) = self.pump.take() { let _ = pump.join(); }
    }
}

struct State { info: Arc<Mutex<MediaInfo>>, tx: Sender<MediaCommand> }
impl State {
    fn info(&self) -> MediaInfo { self.info.lock().unwrap_or_else(|e| e.into_inner()).clone() }
    fn command(&self, command: MediaCommand) { let _ = self.tx.send(command); }
}

fn playback_status(info: &MediaInfo) -> &'static str {
    match info.state { PlayState::Playing => "Playing", PlayState::Paused => "Paused", _ => "Stopped" }
}

fn micros(position: Duration) -> i64 { position.as_micros().min(i64::MAX as u128) as i64 }

fn track_id(info: &MediaInfo) -> Path<'static> {
    if info.id.is_empty() { return Path::from("/org/mpris/MediaPlayer2/TrackList/NoTrack"); }
    let encoded: String = info.id.bytes().map(|byte| format!("{byte:02x}")).collect();
    Path::from(format!("/io/github/lightzgls/mtui/track/{encoded}"))
}

fn metadata(info: &MediaInfo) -> PropMap {
    let mut values = PropMap::new();
    values.insert("mpris:trackid".into(), Variant(Box::new(track_id(info))));
    if !info.id.is_empty() {
        values.insert("xesam:title".into(), Variant(Box::new(info.title.clone())));
        values.insert("xesam:artist".into(), Variant(Box::new(vec![info.artist.clone()])));
        values.insert("xesam:url".into(), Variant(Box::new(format!("https://music.youtube.com/watch?v={}", info.id))));
        if let Some(duration) = info.duration {
            values.insert("mpris:length".into(), Variant(Box::new(micros(duration))));
        }
    }
    values
}

fn interfaces(cr: &mut Crossroads, state: State) {
    let root = cr.register("org.mpris.MediaPlayer2", |b: &mut IfaceBuilder<State>| {
        b.method("Raise", (), (), |_, _, ()| Ok(()));
        b.method("Quit", (), (), |_, state, ()| { state.command(MediaCommand::Quit); Ok(()) });
        b.property("CanQuit").get(|_, _| Ok(true)).emits_changed_const();
        b.property("CanRaise").get(|_, _| Ok(false)).emits_changed_const();
        b.property("HasTrackList").get(|_, _| Ok(false)).emits_changed_const();
        b.property("Identity").get(|_, _| Ok("MTUI".to_string())).emits_changed_const();
        b.property("DesktopEntry").get(|_, _| Ok("mtui".to_string())).emits_changed_const();
        b.property("SupportedUriSchemes").get(|_, _| Ok(Vec::<String>::new())).emits_changed_const();
        b.property("SupportedMimeTypes").get(|_, _| Ok(Vec::<String>::new())).emits_changed_const();
    });
    let player = cr.register(PLAYER, |b: &mut IfaceBuilder<State>| {
        for (name, command) in [
            ("Play", MediaCommand::Play), ("Pause", MediaCommand::Pause),
            ("PlayPause", MediaCommand::TogglePause), ("Stop", MediaCommand::Stop),
            ("Next", MediaCommand::Next), ("Previous", MediaCommand::Previous),
        ] {
            b.method(name, (), (), move |_, state, ()| { state.command(command); Ok(()) });
        }
        b.method("Seek", ("Offset",), (), |_, state, (offset,): (i64,)| {
            let info = state.info();
            if info.can_seek {
                let position = micros(info.position).saturating_add(offset).max(0);
                let total = info.duration.map(micros).unwrap_or(i64::MAX);
                // Seeking beyond the end must use the app's normal queue advance.
                if position > total { state.command(MediaCommand::Next); }
                else { state.command(MediaCommand::Seek(Duration::from_micros(position as u64))); }
            }
            Ok(())
        });
        b.method("SetPosition", ("TrackId", "Position"), (), |_, state, (id, position): (Path<'static>, i64)| {
            let info = state.info();
            // A widget may still be holding a track which has just been skipped.
            if info.can_seek && id == track_id(&info) && position >= 0
                && info.duration.is_none_or(|total| position <= micros(total))
            {
                state.command(MediaCommand::Seek(Duration::from_micros(position as u64)));
            }
            Ok(())
        });
        b.method("OpenUri", ("Uri",), (), |_, _, (_uri,): (String,)| {
            Err::<(), _>(dbus::MethodErr::failed("OpenUri is not supported; select music in MTUI."))
        });
        b.signal::<(i64,), _>("Seeked", ("Position",));
        b.property("PlaybackStatus").get(|_, state| Ok(playback_status(&state.info()).to_string()));
        b.property("Metadata").get(|_, state| Ok(metadata(&state.info())));
        b.property("Position").get(|_, state| Ok(micros(state.info().position))).emits_changed_false();
        b.property("Rate").get(|_, _| Ok(1.0f64)).set(|_, _, value: f64| {
            if value == 1.0 { Ok(None) } else { Err(dbus::MethodErr::failed("Only normal playback speed is supported.")) }
        });
        b.property("MinimumRate").get(|_, _| Ok(1.0f64)).emits_changed_const();
        b.property("MaximumRate").get(|_, _| Ok(1.0f64)).emits_changed_const();
        b.property("Volume").get(|_, state| Ok(f64::from(state.info().volume))).set(|_, state, value: f64| {
            if !value.is_finite() { return Err(dbus::MethodErr::failed("Volume must be finite.")); }
            state.command(MediaCommand::SetVolume(value.clamp(0.0, 1.0) as f32));
            Ok(None)
        });
        b.property("CanGoNext").get(|_, state| Ok(state.info().can_next));
        b.property("CanGoPrevious").get(|_, state| Ok(state.info().can_previous));
        b.property("CanPlay").get(|_, state| Ok(!state.info().id.is_empty()));
        b.property("CanPause").get(|_, state| Ok(!state.info().id.is_empty()));
        b.property("CanSeek").get(|_, state| Ok(state.info().can_seek));
        b.property("CanControl").get(|_, _| Ok(true)).emits_changed_const();
    });
    cr.insert(PATH, &[root, player], state);
}

fn changed_properties(info: &MediaInfo, old: &MediaInfo) -> PropMap {
    let mut changes = PropMap::new();
    if playback_status(info) != playback_status(old) {
        changes.insert("PlaybackStatus".into(), Variant(Box::new(playback_status(info).to_string())));
    }
    if info.id != old.id || info.title != old.title || info.artist != old.artist || info.duration != old.duration {
        changes.insert("Metadata".into(), Variant(Box::new(metadata(info))));
    }
    if info.volume != old.volume { changes.insert("Volume".into(), Variant(Box::new(f64::from(info.volume)))); }
    for (name, value, previous) in [
        ("CanGoNext", info.can_next, old.can_next), ("CanGoPrevious", info.can_previous, old.can_previous),
        ("CanSeek", info.can_seek, old.can_seek), ("CanPlay", !info.id.is_empty(), !old.id.is_empty()),
        ("CanPause", !info.id.is_empty(), !old.id.is_empty()),
    ] {
        if value != previous { changes.insert(name.into(), Variant(Box::new(value))); }
    }
    changes
}

fn serve(state: State, stopped: Arc<AtomicBool>, ready: &Sender<Result<()>>) -> Result<()> {
    let connection = Connection::new_session().context("no desktop D-Bus session")?;
    use dbus::blocking::stdintf::org_freedesktop_dbus::RequestNameReply;
    if connection.request_name(NAME, false, false, true)? != RequestNameReply::PrimaryOwner {
        let name = format!("{NAME}.instance_{}", std::process::id());
        if connection.request_name(name, false, false, true)? != RequestNameReply::PrimaryOwner {
            return Err(anyhow!("could not reserve an MPRIS name"));
        }
    }
    let info = Arc::clone(&state.info);
    let mut cr = Crossroads::new();
    interfaces(&mut cr, state);
    connection.start_receive(MatchRule::new_method_call(), Box::new(move |message, connection| {
        let _ = cr.handle_message(message, connection);
        true
    }));
    ready.send(Ok(()))?;
    let mut old = MediaInfo::default();
    let mut updated = Instant::now();
    while !stopped.load(Ordering::Relaxed) {
        let latest = info.lock().unwrap_or_else(|e| e.into_inner()).clone();
        if latest != old {
            let changes = changed_properties(&latest, &old);
            if !changes.is_empty() {
                let message = Message::new_signal(PATH, "org.freedesktop.DBus.Properties", "PropertiesChanged")
                    .map_err(|error| anyhow!(error))?
                    .append3(PLAYER, changes, Vec::<String>::new());
                connection.send(message).map_err(|_| anyhow!("MPRIS signal could not be sent"))?;
            }
            let expected = if old.state == PlayState::Playing { old.position.saturating_add(updated.elapsed()) } else { old.position };
            if latest.id == old.id && latest.can_seek && expected.abs_diff(latest.position) > Duration::from_millis(750) {
                let message = Message::new_signal(PATH, PLAYER, "Seeked").map_err(|error| anyhow!(error))?
                    .append1(micros(latest.position));
                connection.send(message).map_err(|_| anyhow!("MPRIS seek signal could not be sent"))?;
            }
            old = latest;
            updated = Instant::now();
        }
        connection.process(Duration::from_millis(100))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbus::blocking::stdintf::org_freedesktop_dbus::Properties;

    #[test]
    #[ignore = "run under dbus-run-session to test a private desktop bus"]
    fn desktop_controllers_discover_metadata_commands_and_seek_signals() {
        let session = MediaSession::spawn().unwrap();
        let client = Connection::new_session().unwrap();
        let proxy = client.with_proxy(NAME, PATH, Duration::from_secs(2));
        let identity: String = proxy.get("org.mpris.MediaPlayer2", "Identity").unwrap();
        assert_eq!(identity, "MTUI");
        let desktop: String = proxy.get("org.mpris.MediaPlayer2", "DesktopEntry").unwrap();
        assert_eq!(desktop, "mtui");
        let mut info = MediaInfo {
            id: "video-with-hyphen".into(), title: "Linux test song".into(), artist: "MTUI artist".into(),
            duration: Some(Duration::from_secs(240)), position: Duration::from_secs(30),
            state: PlayState::Playing, volume: 0.7, can_seek: true, can_next: true, can_previous: true,
        };
        session.publish(info.clone());
        let status: String = proxy.get(PLAYER, "PlaybackStatus").unwrap();
        assert_eq!(status, "Playing");
        let meta: PropMap = proxy.get(PLAYER, "Metadata").unwrap();
        assert_eq!(meta["xesam:title"].0.as_str(), Some("Linux test song"));
        assert_eq!(meta["mpris:length"].0.as_i64(), Some(240_000_000));
        let position: i64 = proxy.get(PLAYER, "Position").unwrap();
        assert_eq!(position, 30_000_000);
        for (name, command) in [
            ("Play", MediaCommand::Play), ("Pause", MediaCommand::Pause), ("Pause", MediaCommand::Pause),
            ("PlayPause", MediaCommand::TogglePause), ("Next", MediaCommand::Next), ("Previous", MediaCommand::Previous),
        ] {
            let _: () = proxy.method_call(PLAYER, name, ()).unwrap();
            assert_eq!(session.commands.recv_timeout(Duration::from_secs(1)).unwrap(), command);
        }
        let _: () = proxy.method_call(PLAYER, "Seek", (-40_000_000i64,)).unwrap();
        assert_eq!(session.commands.recv_timeout(Duration::from_secs(1)).unwrap(), MediaCommand::Seek(Duration::ZERO));
        let _: () = proxy.method_call(PLAYER, "Seek", (999_000_000i64,)).unwrap();
        assert_eq!(session.commands.recv_timeout(Duration::from_secs(1)).unwrap(), MediaCommand::Next);
        let _: () = proxy.method_call(PLAYER, "SetPosition", (track_id(&info), 70_000_000i64)).unwrap();
        assert_eq!(session.commands.recv_timeout(Duration::from_secs(1)).unwrap(), MediaCommand::Seek(Duration::from_secs(70)));
        let _: () = proxy.method_call(PLAYER, "SetPosition", (Path::from("/stale/track"), 70_000_000i64)).unwrap();
        let _: () = proxy.method_call(PLAYER, "SetPosition", (track_id(&info), -1i64)).unwrap();
        let _: () = proxy.method_call(PLAYER, "SetPosition", (track_id(&info), 999_000_000i64)).unwrap();
        assert!(session.next_command().is_none());
        proxy.set(PLAYER, "Volume", 0.4f64).unwrap();
        assert_eq!(session.commands.recv_timeout(Duration::from_secs(1)).unwrap(), MediaCommand::SetVolume(0.4));
        let signals = Arc::new(Mutex::new(Vec::<i64>::new()));
        let received = Arc::clone(&signals);
        client.add_match(MatchRule::new_signal(PLAYER, "Seeked"), move |(position,): (i64,), _, _| {
            received.lock().unwrap().push(position); true
        }).unwrap();
        // Allow the worker to publish the initial track before changing position.
        thread::sleep(Duration::from_millis(150));
        info.position = Duration::from_secs(70);
        info.state = PlayState::Paused;
        session.publish(info);
        let until = Instant::now() + Duration::from_secs(2);
        while signals.lock().unwrap().is_empty() && Instant::now() < until {
            client.process(Duration::from_millis(100)).unwrap();
        }
        assert_eq!(*signals.lock().unwrap(), vec![70_000_000]);
        let status: String = proxy.get(PLAYER, "PlaybackStatus").unwrap();
        assert_eq!(status, "Paused");
        let _: () = proxy.method_call(PLAYER, "Stop", ()).unwrap();
        assert_eq!(session.commands.recv_timeout(Duration::from_secs(1)).unwrap(), MediaCommand::Stop);
        session.publish(MediaInfo::default());
        let status: String = proxy.get(PLAYER, "PlaybackStatus").unwrap();
        assert_eq!(status, "Stopped");
        drop(session);
        assert!(proxy.get::<String>(PLAYER, "PlaybackStatus").is_err(), "quit must release the discoverable player name");
    }
}
