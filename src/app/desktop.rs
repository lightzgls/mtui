//! Desktop media controls, tray lifecycle, and Discord activity.

use std::time::Duration;

use crate::desktop::discord::{Activity, Clock};
use crate::player::{Command, PlayState};
use crate::source::UNKNOWN_ARTIST;
use crate::desktop::tray::TrayCommand;

use super::App;

impl App {

    pub fn presence_enabled(&self) -> bool {
        self.presence.enabled()
    }


    /// Offers Discord the card that should be showing. Called once per frame in
    /// both faces of the program -- the card is the one thing about a
    /// backgrounded player that other people can see, so it must not stop
    /// updating when the window goes away.
    ///
    /// Cheap on every tick that changes nothing, which is nearly all of them:
    /// [`crate::desktop::discord::Presence::publish`] compares before it sends, and nothing here touches
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
    pub(super) fn activity(&self) -> Option<Activity> {
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
                crate::desktop::discord::now_ms().saturating_sub(snap.position.as_millis() as u64);
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
            artwork_url: now.artwork_url.clone(),
            paused,
            clock,
        })
    }


    /// Asks the event loop to let go of the terminal and carry on playing.
    ///
    /// Refused with nothing playing, which is not a limitation but the whole
    /// point: backgrounding a silent player leaves an icon in the notification
    /// area representing nothing, and the user's next move -- finding something
    /// to play -- is one the icon cannot help with.
    pub(super) fn background(&mut self) {
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
            TrayCommand::Click if self.tray_click_pauses => self.toggle_pause(),
            TrayCommand::Click => self.wants_foreground = true,
            TrayCommand::Show => self.wants_foreground = true,
            TrayCommand::TogglePause => self.toggle_pause(),
            TrayCommand::Next => self.advance(1, false),
            TrayCommand::Previous => self.advance(-1, false),
            TrayCommand::Quit => self.should_quit = true,
        }
    }


    pub fn handle_window_loss(&mut self, end_task: bool) {
        self.should_quit |= end_task;
        self.wants_background = !self.should_quit;
        if self.should_quit { self.wants_foreground = false; }
    }


    /// System buttons use the same player and queue commands as the UI.
    pub fn handle_media(&mut self, command: crate::desktop::media::MediaCommand) {
        use crate::desktop::media::MediaCommand;
        match command {
            MediaCommand::Play | MediaCommand::Pause if self.now.is_some() && !self.playback.is_loading() => {
                let _ = self.player.send(Command::SetPaused(command == MediaCommand::Pause));
            }
            MediaCommand::Next => self.advance(1, false),
            MediaCommand::Previous => self.advance(-1, false),
            #[cfg(target_os = "linux")]
            MediaCommand::TogglePause if self.now.is_some() && !self.playback.is_loading() => self.toggle_pause(),
            #[cfg(target_os = "linux")]
            MediaCommand::Stop => self.stop(),
            #[cfg(target_os = "linux")]
            MediaCommand::SetVolume(volume) => self.set_volume(volume),
            #[cfg(target_os = "linux")]
            MediaCommand::Quit => self.should_quit = true,
            MediaCommand::Seek(position) if self.media_info().can_seek => {
                let total = self.now.as_ref().and_then(|now| now.duration).unwrap();
                let target = position.min(total);
                let seek = if target == total { Command::SeekToEnd(target) } else { Command::Seek(target) };
                let _ = self.player.send(seek);
                self.playback.seeked();
            }
            _ => {}
        }
    }


    pub fn media_info(&self) -> crate::desktop::media::MediaInfo {
        let Some(now) = self.now.as_ref() else {
            return crate::desktop::media::MediaInfo { volume: self.snapshot().volume, ..Default::default() };
        };
        let snapshot = self.snapshot();
        let loading = self.playback.is_loading();
        crate::desktop::media::MediaInfo {
            id: now.video_id.clone(), title: now.title.clone(), artist: now.artist.clone(),
            state: if loading { PlayState::Buffering } else { snapshot.state },
            position: if loading { Duration::ZERO } else { snapshot.position },
            duration: now.duration,
            can_next: now.advanced_index(1, false).is_some(),
            can_previous: now.advanced_index(-1, false).is_some(),
            can_seek: !loading && matches!(snapshot.state, PlayState::Playing | PlayState::Paused)
                && now.duration.is_some_and(|duration| !duration.is_zero()),
            volume: snapshot.volume,
        }
    }


    /// The raised strips only appear while the current track is playing.
    /// `now` clears synchronously on Stop, before the audio worker catches up.
    pub fn tray_playing(&self) -> bool {
        let snapshot = self.snapshot();
        self.now.is_some()
            && !self.playback.is_loading()
            && snapshot.state == PlayState::Playing
            && snapshot.error.is_none()
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
}
