use std::sync::{Arc, Mutex, mpsc::{Receiver, Sender, channel}};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use windows::core::{HSTRING, factory, w};
use windows::Foundation::{TimeSpan, TypedEventHandler};
use windows::Media::{
    MediaPlaybackStatus, MediaPlaybackType, PlaybackPositionChangeRequestedEventArgs,
    SystemMediaTransportControls, SystemMediaTransportControlsButton,
    SystemMediaTransportControlsButtonPressedEventArgs, SystemMediaTransportControlsTimelineProperties,
};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::WinRT::{
    ISystemMediaTransportControlsInterop, RO_INIT_MULTITHREADED, RoInitialize, RoUninitialize,
};
use windows::Win32::UI::Shell::SetCurrentProcessExplicitAppUserModelID;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW,
    MSG, PostMessageW, PostQuitMessage, RegisterClassW, TranslateMessage,
    WINDOW_EX_STYLE, WINDOW_STYLE, WM_APP, WM_CLOSE, WM_DESTROY, WNDCLASSW,
};

use super::{MediaCommand, MediaInfo};

const WM_PUBLISH: u32 = WM_APP + 1;

/// Also used by the installed shortcuts and the foreground console helper.
pub fn set_app_id() -> Result<()> {
    unsafe { SetCurrentProcessExplicitAppUserModelID(w!("io.github.lightzgls.MTUI")) }
        .context("could not set MTUI's application identity")
}

pub struct MediaSession {
    hwnd: isize,
    pending: Arc<Mutex<Option<MediaInfo>>>,
    commands: Receiver<MediaCommand>,
    pump: Option<JoinHandle<()>>,
}

impl MediaSession {
    pub fn spawn() -> Result<Self> {
        let (ready_tx, ready_rx) = channel();
        let (tx, commands) = channel();
        let pending = Arc::new(Mutex::new(None));
        let updates = Arc::clone(&pending);
        let pump = thread::Builder::new().name("mtui-media".into()).spawn(move || {
            let result = serve(tx, updates, &ready_tx);
            if let Err(error) = result {
                let message = format!("{error:#}");
                let _ = ready_tx.send(Err(anyhow!(message.clone())));
                crate::diagnostics::error("media", &message);
            }
        }).context("could not start media controls")?;
        match ready_rx.recv().context("media thread stopped during startup")? {
            Ok(hwnd) => Ok(Self { hwnd, pending, commands, pump: Some(pump) }),
            Err(error) => { let _ = pump.join(); Err(error) }
        }
    }

    pub fn publish(&self, info: MediaInfo) {
        // Only retain the newest update if Windows is busy servicing callbacks.
        *self.pending.lock().unwrap_or_else(|e| e.into_inner()) = Some(info);
        let _ = unsafe { PostMessageW(Some(HWND(self.hwnd as *mut _)), WM_PUBLISH, WPARAM(0), LPARAM(0)) };
    }

    pub fn next_command(&self) -> Option<MediaCommand> { self.commands.try_recv().ok() }
}

impl Drop for MediaSession {
    fn drop(&mut self) {
        let _ = unsafe { PostMessageW(Some(HWND(self.hwnd as *mut _)), WM_CLOSE, WPARAM(0), LPARAM(0)) };
        if let Some(pump) = self.pump.take() { let _ = pump.join(); }
    }
}

struct Apartment;
impl Drop for Apartment { fn drop(&mut self) { unsafe { RoUninitialize() }; } }

struct Window(HWND);
impl Drop for Window { fn drop(&mut self) { let _ = unsafe { DestroyWindow(self.0) }; } }

unsafe extern "system" fn window_proc(hwnd: HWND, message: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    if message == WM_CLOSE {
        // Keep the HWND alive until handlers have been revoked and SMTC disabled.
        unsafe { PostQuitMessage(0) };
        return LRESULT(0);
    }
    if message == WM_DESTROY { return LRESULT(0); }
    unsafe { DefWindowProcW(hwnd, message, w, l) }
}

struct Controls {
    smtc: SystemMediaTransportControls,
    buttons: Option<i64>,
    seek: Option<i64>,
}

impl Controls {
    fn new(hwnd: HWND, tx: Sender<MediaCommand>) -> Result<Self> {
        let interop: ISystemMediaTransportControlsInterop = factory::<SystemMediaTransportControls, _>()?;
        let smtc: SystemMediaTransportControls = unsafe { interop.GetForWindow(hwnd) }?;
        let mut controls = Self { smtc, buttons: None, seek: None };
        let buttons_tx = tx.clone();
        controls.buttons = Some(controls.smtc.ButtonPressed(&TypedEventHandler::<
            SystemMediaTransportControls, SystemMediaTransportControlsButtonPressedEventArgs,
        >::new(move |_, args| {
            if let Some(args) = args.as_ref() {
                let command = match args.Button()? {
                    SystemMediaTransportControlsButton::Play => Some(MediaCommand::Play),
                    SystemMediaTransportControlsButton::Pause => Some(MediaCommand::Pause),
                    SystemMediaTransportControlsButton::Next => Some(MediaCommand::Next),
                    SystemMediaTransportControlsButton::Previous => Some(MediaCommand::Previous),
                    _ => None,
                };
                if let Some(command) = command { let _ = buttons_tx.send(command); }
            }
            Ok(())
        }))?);
        controls.seek = Some(controls.smtc.PlaybackPositionChangeRequested(&TypedEventHandler::<
            SystemMediaTransportControls, PlaybackPositionChangeRequestedEventArgs,
        >::new(move |_, args| {
            if let Some(args) = args.as_ref() {
                let ticks = args.RequestedPlaybackPosition()?.Duration.max(0) as u64;
                let _ = tx.send(MediaCommand::Seek(Duration::from_nanos(ticks.saturating_mul(100))));
            }
            Ok(())
        }))?);
        controls.smtc.SetIsEnabled(true)?;
        Ok(controls)
    }

    fn publish(&self, info: &MediaInfo, previous: Option<&MediaInfo>) -> Result<()> {
        let active = !info.id.is_empty();
        self.smtc.SetIsPlayEnabled(active)?;
        self.smtc.SetIsPauseEnabled(active)?;
        self.smtc.SetIsNextEnabled(info.can_next)?;
        self.smtc.SetIsPreviousEnabled(info.can_previous)?;
        let status = match info.state {
            crate::player::PlayState::Playing => MediaPlaybackStatus::Playing,
            crate::player::PlayState::Paused => MediaPlaybackStatus::Paused,
            crate::player::PlayState::Buffering => MediaPlaybackStatus::Changing,
            crate::player::PlayState::Idle => MediaPlaybackStatus::Stopped,
        };
        self.smtc.SetPlaybackStatus(status)?;
        if previous.is_none_or(|old| old.id != info.id || old.title != info.title || old.artist != info.artist) {
            let display = self.smtc.DisplayUpdater()?;
            display.ClearAll()?;
            if active {
                display.SetType(MediaPlaybackType::Music)?;
                display.SetAppMediaId(&HSTRING::from(&info.id))?;
                let music = display.MusicProperties()?;
                music.SetTitle(&HSTRING::from(&info.title))?;
                music.SetArtist(&HSTRING::from(&info.artist))?;
            }
            display.Update()?;
        }
        let timeline = SystemMediaTransportControlsTimelineProperties::new()?;
        let total = info.duration.unwrap_or(info.position);
        timeline.SetStartTime(timespan(Duration::ZERO))?;
        timeline.SetEndTime(timespan(total))?;
        // A zero seek range suppresses a scrubber for unknown lengths/loading.
        timeline.SetMinSeekTime(timespan(Duration::ZERO))?;
        timeline.SetMaxSeekTime(timespan(if info.can_seek { total } else { Duration::ZERO }))?;
        timeline.SetPosition(timespan(info.position.min(total)))?;
        self.smtc.UpdateTimelineProperties(&timeline)?;
        Ok(())
    }
}

impl Drop for Controls {
    fn drop(&mut self) {
        if let Some(token) = self.buttons { let _ = self.smtc.RemoveButtonPressed(token); }
        if let Some(token) = self.seek { let _ = self.smtc.RemovePlaybackPositionChangeRequested(token); }
        let _ = self.smtc.SetPlaybackStatus(MediaPlaybackStatus::Closed);
        let _ = self.smtc.DisplayUpdater().and_then(|display| { display.ClearAll()?; display.Update() });
        let _ = self.smtc.SetIsEnabled(false);
    }
}

fn timespan(duration: Duration) -> TimeSpan {
    TimeSpan { Duration: (duration.as_nanos() / 100).min(i64::MAX as u128) as i64 }
}

fn serve(
    tx: Sender<MediaCommand>, pending: Arc<Mutex<Option<MediaInfo>>>,
    ready: &Sender<Result<isize>>,
) -> Result<()> {
    unsafe { RoInitialize(RO_INIT_MULTITHREADED) }?;
    let _apartment = Apartment;
    let window = create_window()?;
    let controls = Controls::new(window.0, tx)?;
    controls.publish(&MediaInfo::default(), None)?;
    ready.send(Ok(window.0.0 as isize))?;
    let mut previous = None;
    let mut message = MSG::default();
    loop {
        let result = unsafe { GetMessageW(&mut message, None, 0, 0) }.0;
        if result == -1 { return Err(windows::core::Error::from_thread().into()); }
        if result == 0 { break; }
        if message.message == WM_PUBLISH {
            let info = pending.lock().unwrap_or_else(|e| e.into_inner()).take();
            if let Some(info) = info
                && previous.as_ref() != Some(&info)
            {
                match controls.publish(&info, previous.as_ref()) {
                    Ok(()) => previous = Some(info),
                    Err(error) => crate::diagnostics::error("media", &format!("update failed: {error:#}")),
                }
            }
        } else {
            unsafe { let _ = TranslateMessage(&message); DispatchMessageW(&message); }
        }
    }
    Ok(())
}

fn create_window() -> Result<Window> {
    let instance = unsafe { GetModuleHandleW(None) }?;
    let class = WNDCLASSW {
        lpfnWndProc: Some(window_proc), hInstance: instance.into(),
        lpszClassName: w!("MTUI.MediaSession"), ..Default::default()
    };
    // Registration can already exist in a repeated in-process smoke test.
    unsafe { RegisterClassW(&class) };
    // SMTC requires a top-level HWND owned by this process. A hidden window
    // with no parent satisfies that requirement and survives console changes.
    Ok(Window(unsafe { CreateWindowExW(
        WINDOW_EX_STYLE::default(), w!("MTUI.MediaSession"), w!("MTUI"), WINDOW_STYLE::default(),
        0, 0, 0, 0, None, None, Some(instance.into()), None,
    ) }?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "registers a native Windows desktop media session"]
    fn native_media_session_publishes_and_owns_a_persistent_top_level_window() {
        use windows::Win32::UI::WindowsAndMessaging::{GetParent, GetWindowThreadProcessId, IsWindow};
        set_app_id().unwrap();
        unsafe { RoInitialize(RO_INIT_MULTITHREADED) }.unwrap();
        let _apartment = Apartment;
        let window = create_window().unwrap();
        let mut pid = 0;
        unsafe { GetWindowThreadProcessId(window.0, Some(&mut pid)) };
        assert_eq!(pid, std::process::id());
        assert!(unsafe { GetParent(window.0) }.is_err(), "SMTC needs a top-level app-owned window");
        let (tx, _rx) = channel();
        let controls = Controls::new(window.0, tx).unwrap();
        let mut info = MediaInfo {
            id: "test-song".into(), title: "Native test track".into(), artist: "MTUI test artist".into(),
            state: crate::player::PlayState::Playing, position: Duration::from_secs(23),
            duration: Some(Duration::from_secs(240)), can_next: true, can_previous: true, can_seek: true,
            ..Default::default()
        };
        controls.publish(&info, None).unwrap();
        assert_eq!(controls.smtc.PlaybackStatus().unwrap(), MediaPlaybackStatus::Playing);
        let music = controls.smtc.DisplayUpdater().unwrap().MusicProperties().unwrap();
        assert_eq!(music.Title().unwrap(), "Native test track");
        assert_eq!(music.Artist().unwrap(), "MTUI test artist");
        info.state = crate::player::PlayState::Paused;
        controls.publish(&info, None).unwrap();
        assert_eq!(controls.smtc.PlaybackStatus().unwrap(), MediaPlaybackStatus::Paused);
        controls.publish(&MediaInfo::default(), None).unwrap();
        drop(controls);
        drop(window);
        let session = MediaSession::spawn().unwrap();
        let hwnd = HWND(session.hwnd as *mut _);
        session.publish(info);
        assert!(unsafe { IsWindow(Some(hwnd)) }.as_bool());
        drop(session);
        assert!(!unsafe { IsWindow(Some(hwnd)) }.as_bool());
    }
}
