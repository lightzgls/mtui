//! System media controls outlive every foreground console and tray transition.

use std::time::Duration;

#[cfg_attr(target_os = "macos", allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum MediaCommand {
    Play,
    Pause,
    Next,
    Previous,
    Seek(Duration),
    #[cfg(target_os = "linux")]
    TogglePause,
    #[cfg(target_os = "linux")]
    Stop,
    #[cfg(target_os = "linux")]
    SetVolume(f32),
    #[cfg(target_os = "linux")]
    Quit,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct MediaInfo {
    pub id: String,
    pub title: String,
    pub artist: String,
    pub state: crate::player::PlayState,
    pub position: Duration,
    pub duration: Option<Duration>,
    pub can_next: bool,
    pub can_previous: bool,
    pub can_seek: bool,
    pub volume: f32,
}

#[cfg(windows)]
mod imp;

#[cfg(windows)]
pub use imp::{MediaSession, set_app_id};

#[cfg(target_os = "linux")]
mod mpris;
#[cfg(target_os = "linux")]
pub use mpris::MediaSession;

#[cfg(not(any(windows, target_os = "linux")))]
pub struct MediaSession;

#[cfg(not(any(windows, target_os = "linux")))]
impl MediaSession {
    pub fn spawn() -> anyhow::Result<Self> { Ok(Self) }
    pub fn publish(&self, _info: MediaInfo) {}
    pub fn next_command(&self) -> Option<MediaCommand> { None }
}
