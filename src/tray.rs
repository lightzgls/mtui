//! The notification-area icon, which is the whole of MTUI's interface while it
//! is running detached from a terminal.
//!
//! Bound by hand rather than through a crate. What is needed here is one hidden
//! window, one icon and one popup menu -- a few hundred lines of declarations
//! against `user32` and `shell32` -- and the alternative pulls a window-manager
//! abstraction and its dependency tree into a binary that has no windows.
//!
//! The icon needs a window to deliver its clicks to, and a window needs a thread
//! pumping messages for it, so both live on a thread of their own. Clicks come
//! back as [`TrayCommand`] over a channel; nothing else about the window is
//! visible from outside this module.

#[cfg(windows)]
mod imp {
    use std::sync::mpsc::{Receiver, Sender, channel};
    use std::thread::{self, JoinHandle};
    use std::time::Duration;

    use anyhow::{Context, Result, anyhow};

    type Handle = isize;
    type Wparam = usize;
    type Lparam = isize;
    type Lresult = isize;

    #[link(name = "user32")]
    unsafe extern "system" {
        fn RegisterClassExW(class: *const WndClassExW) -> u16;
        fn CreateWindowExW(
            ex_style: u32,
            class: *const u16,
            name: *const u16,
            style: u32,
            x: i32,
            y: i32,
            w: i32,
            h: i32,
            parent: Handle,
            menu: Handle,
            instance: Handle,
            param: *const core::ffi::c_void,
        ) -> Handle;
        fn DefWindowProcW(hwnd: Handle, msg: u32, w: Wparam, l: Lparam) -> Lresult;
        fn DestroyWindow(hwnd: Handle) -> i32;
        fn GetMessageW(msg: *mut Msg, hwnd: Handle, min: u32, max: u32) -> i32;
        fn TranslateMessage(msg: *const Msg) -> i32;
        fn DispatchMessageW(msg: *const Msg) -> Lresult;
        fn PostQuitMessage(code: i32);
        fn PostMessageW(hwnd: Handle, msg: u32, w: Wparam, l: Lparam) -> i32;
        fn LoadIconW(instance: Handle, name: *const u16) -> Handle;
        fn CreatePopupMenu() -> Handle;
        fn AppendMenuW(menu: Handle, flags: u32, id: usize, item: *const u16) -> i32;
        fn TrackPopupMenu(
            menu: Handle,
            flags: u32,
            x: i32,
            y: i32,
            reserved: i32,
            hwnd: Handle,
            rect: *const core::ffi::c_void,
        ) -> i32;
        fn DestroyMenu(menu: Handle) -> i32;
        fn SetForegroundWindow(hwnd: Handle) -> i32;
        fn GetCursorPos(point: *mut Point) -> i32;
    }

    #[link(name = "shell32")]
    unsafe extern "system" {
        fn Shell_NotifyIconW(message: u32, data: *const NotifyIconDataW) -> i32;
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetModuleHandleW(name: *const u16) -> Handle;
    }

    #[repr(C)]
    struct WndClassExW {
        cb_size: u32,
        style: u32,
        wnd_proc: Option<unsafe extern "system" fn(Handle, u32, Wparam, Lparam) -> Lresult>,
        cls_extra: i32,
        wnd_extra: i32,
        instance: Handle,
        icon: Handle,
        cursor: Handle,
        background: Handle,
        menu_name: *const u16,
        class_name: *const u16,
        icon_sm: Handle,
    }

    #[repr(C)]
    #[derive(Default)]
    struct Point {
        x: i32,
        y: i32,
    }

    #[repr(C)]
    struct Msg {
        hwnd: Handle,
        message: u32,
        wparam: Wparam,
        lparam: Lparam,
        time: u32,
        point: Point,
    }

    /// The shell's icon descriptor. Laid out field for field as `shellapi.h`
    /// declares it, including the trailing members added after Windows 2000 --
    /// `cb_size` tells the shell which vintage of the struct it is looking at,
    /// and it is computed from this declaration rather than written down, so the
    /// two cannot drift apart.
    #[repr(C)]
    struct NotifyIconDataW {
        cb_size: u32,
        hwnd: Handle,
        id: u32,
        flags: u32,
        callback_message: u32,
        icon: Handle,
        tip: [u16; 128],
        state: u32,
        state_mask: u32,
        info: [u16; 256],
        version: u32,
        info_title: [u16; 64],
        info_flags: u32,
        guid: [u8; 16],
        balloon_icon: Handle,
    }

    const WM_DESTROY: u32 = 0x0002;
    const WM_CLOSE: u32 = 0x0010;
    const WM_COMMAND: u32 = 0x0111;
    const WM_LBUTTONUP: u32 = 0x0202;
    const WM_LBUTTONDBLCLK: u32 = 0x0203;
    const WM_RBUTTONUP: u32 = 0x0205;
    /// The private message the icon reports its clicks through. Anything at or
    /// above `WM_APP` is reserved for the application, so it cannot collide with
    /// a system message the default window procedure expects to handle.
    const WM_TRAY: u32 = 0x8000 + 1;

    const NIM_ADD: u32 = 0;
    const NIM_MODIFY: u32 = 1;
    const NIM_DELETE: u32 = 2;
    const NIF_MESSAGE: u32 = 0x01;
    const NIF_ICON: u32 = 0x02;
    const NIF_TIP: u32 = 0x04;

    const MF_STRING: u32 = 0x0000;
    const MF_SEPARATOR: u32 = 0x0800;
    const TPM_RIGHTBUTTON: u32 = 0x0002;
    const TPM_RETURNCMD: u32 = 0x0100;

    /// Parent that makes a window message-only: never drawn, never listed, and
    /// -- the point of it here -- never shown on the taskbar next to real
    /// windows. It exists solely to be sent the icon's clicks.
    const HWND_MESSAGE: Handle = -3;
    const IDI_APPLICATION: *const u16 = 32512 as *const u16;

    /// Menu ids. Arbitrary, and only ever compared against what
    /// [`TrackPopupMenu`] returns.
    const ID_SHOW: usize = 1;
    const ID_PLAY_PAUSE: usize = 2;
    const ID_NEXT: usize = 3;
    const ID_PREVIOUS: usize = 4;
    const ID_QUIT: usize = 5;

    /// What the user asked of the icon.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum TrayCommand {
        /// Put the terminal interface back up.
        Show,
        TogglePause,
        Next,
        Previous,
        Quit,
    }

    // The window procedure runs on the pump thread and nowhere else, so the
    // sender it needs can live in that thread rather than in the window's user
    // data -- which would mean a raw pointer to reconstruct on every message.
    thread_local! {
        static SENDER: std::cell::RefCell<Option<Sender<TrayCommand>>> =
            const { std::cell::RefCell::new(None) };
    }

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(std::iter::once(0)).collect()
    }

    fn send(command: TrayCommand) {
        SENDER.with_borrow(|tx| {
            if let Some(tx) = tx {
                // A closed channel means the app is already on its way down.
                let _ = tx.send(command);
            }
        });
    }

    unsafe extern "system" fn wnd_proc(
        hwnd: Handle,
        msg: u32,
        wparam: Wparam,
        lparam: Lparam,
    ) -> Lresult {
        match msg {
            // Without version 4 of the icon protocol -- which this deliberately
            // does not ask for, since all it adds is balloon behaviour -- the
            // mouse message arrives in the low word of `lparam`.
            WM_TRAY => {
                match lparam as u32 {
                    // The plain click does what double-clicking a tray icon
                    // conventionally does, because a background player is a
                    // thing users click once and expect to see.
                    WM_LBUTTONUP | WM_LBUTTONDBLCLK => send(TrayCommand::Show),
                    WM_RBUTTONUP => unsafe { popup_menu(hwnd) },
                    _ => {}
                }
                0
            }
            WM_COMMAND => {
                match wparam & 0xffff {
                    ID_SHOW => send(TrayCommand::Show),
                    ID_PLAY_PAUSE => send(TrayCommand::TogglePause),
                    ID_NEXT => send(TrayCommand::Next),
                    ID_PREVIOUS => send(TrayCommand::Previous),
                    ID_QUIT => send(TrayCommand::Quit),
                    _ => {}
                }
                0
            }
            WM_CLOSE => {
                unsafe { DestroyWindow(hwnd) };
                0
            }
            WM_DESTROY => {
                unsafe { PostQuitMessage(0) };
                0
            }
            _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
        }
    }

    /// The right-click menu, built and torn down per click.
    ///
    /// Rebuilt every time rather than held: it is four fixed items and a
    /// separator, so building it costs nothing measurable, and a menu kept
    /// between clicks is a handle to leak.
    unsafe fn popup_menu(hwnd: Handle) {
        let menu = unsafe { CreatePopupMenu() };
        if menu == 0 {
            return;
        }

        let item = |flags: u32, id: usize, label: &str| {
            let text = wide(label);
            unsafe { AppendMenuW(menu, flags, id, text.as_ptr()) };
        };
        item(MF_STRING, ID_SHOW, "Show the player");
        item(MF_SEPARATOR, 0, "");
        item(MF_STRING, ID_PLAY_PAUSE, "Play / pause");
        item(MF_STRING, ID_NEXT, "Next track");
        item(MF_STRING, ID_PREVIOUS, "Previous track");
        item(MF_SEPARATOR, 0, "");
        item(MF_STRING, ID_QUIT, "Quit MTUI");

        let mut point = Point::default();
        unsafe { GetCursorPos(&mut point) };
        // Both of these are load-bearing quirks of tray menus: without the
        // foreground call the menu will not close when the user clicks away
        // from it, and `TPM_RETURNCMD` is what makes the call return the chosen
        // id instead of posting it somewhere this window would have to wait for.
        unsafe { SetForegroundWindow(hwnd) };
        let chosen = unsafe {
            TrackPopupMenu(
                menu,
                TPM_RIGHTBUTTON | TPM_RETURNCMD,
                point.x,
                point.y,
                0,
                hwnd,
                std::ptr::null(),
            )
        };
        unsafe { DestroyMenu(menu) };

        if chosen > 0 {
            unsafe { wnd_proc(hwnd, WM_COMMAND, chosen as Wparam, 0) };
        }
    }

    /// A live icon in the notification area, and the thread that serves it.
    pub struct Tray {
        hwnd: Handle,
        rx: Receiver<TrayCommand>,
        pump: Option<JoinHandle<()>>,
        /// Whether the icon is currently in the notification area, so that
        /// showing or hiding twice is not an error and the icon is not left
        /// behind on the way out.
        visible: bool,
    }

    impl Tray {
        /// Creates the hidden window and starts pumping messages for it.
        pub fn spawn() -> Result<Self> {
            let (tx, rx) = channel();
            let (ready_tx, ready_rx) = channel();

            let pump = thread::Builder::new()
                .name("mtui-tray".to_string())
                .spawn(move || {
                    SENDER.with_borrow_mut(|slot| *slot = Some(tx));
                    let hwnd = unsafe { create_window() };
                    // Reported before the loop starts either way: a failure here
                    // has to reach the caller, not hang it.
                    let _ = ready_tx.send(hwnd);
                    if hwnd == 0 {
                        return;
                    }
                    unsafe { pump_messages() };
                })
                .context("failed to spawn the tray thread")?;

            let hwnd = ready_rx
                .recv()
                .context("the tray thread died before it reported a window")?;
            if hwnd == 0 {
                return Err(anyhow!("could not create the notification-area window"));
            }

            Ok(Self {
                hwnd,
                rx,
                pump: Some(pump),
                visible: false,
            })
        }

        /// Puts the icon in the notification area, or updates the tooltip of one
        /// already there. `tip` is what hovering the icon says.
        pub fn show(&mut self, tip: &str) -> Result<()> {
            let message = if self.visible { NIM_MODIFY } else { NIM_ADD };
            if unsafe { Shell_NotifyIconW(message, &self.icon_data(tip)) } == 0 {
                return Err(anyhow!("the notification area refused the icon"));
            }
            self.visible = true;
            Ok(())
        }

        /// Takes the icon back out. Called when the terminal interface returns,
        /// so that MTUI is in one place at a time rather than two.
        pub fn hide(&mut self) {
            if self.visible {
                unsafe { Shell_NotifyIconW(NIM_DELETE, &self.icon_data("")) };
                self.visible = false;
            }
        }

        /// The next click, or `None` if `timeout` passes without one. The
        /// timeout is what keeps the background loop ticking the queue along.
        pub fn next_command(&self, timeout: Duration) -> Option<TrayCommand> {
            // A dead pump thread reads the same as a quiet one, which is what
            // keeps it from wedging the app: the loop plays on, the icon just
            // stops being clickable.
            self.rx.recv_timeout(timeout).ok()
        }

        fn icon_data(&self, tip: &str) -> NotifyIconDataW {
            let mut data = NotifyIconDataW {
                cb_size: size_of::<NotifyIconDataW>() as u32,
                hwnd: self.hwnd,
                id: 1,
                flags: NIF_MESSAGE | NIF_ICON | NIF_TIP,
                callback_message: WM_TRAY,
                icon: unsafe { LoadIconW(0, IDI_APPLICATION) },
                tip: [0; 128],
                state: 0,
                state_mask: 0,
                info: [0; 256],
                version: 0,
                info_title: [0; 64],
                info_flags: 0,
                guid: [0; 16],
                balloon_icon: 0,
            };
            // One short of the array, so the text is always terminated however
            // long the title was.
            for (slot, ch) in data.tip.iter_mut().zip(tip.encode_utf16().take(127)) {
                *slot = ch;
            }
            data
        }
    }

    impl Drop for Tray {
        fn drop(&mut self) {
            self.hide();
            // Asks the window to close, which ends the pump loop by way of
            // WM_DESTROY. Posting rather than sending: this is the wrong thread
            // to run a window procedure on.
            unsafe { PostMessageW(self.hwnd, WM_CLOSE, 0, 0) };
            if let Some(pump) = self.pump.take() {
                let _ = pump.join();
            }
        }
    }

    /// Registers the class and creates the message-only window. Returns 0 on
    /// any failure, which the caller turns into an error.
    unsafe fn create_window() -> Handle {
        let class_name = wide("mtui-tray");
        let instance = unsafe { GetModuleHandleW(std::ptr::null()) };

        let class = WndClassExW {
            cb_size: size_of::<WndClassExW>() as u32,
            style: 0,
            wnd_proc: Some(wnd_proc),
            cls_extra: 0,
            wnd_extra: 0,
            instance,
            icon: 0,
            cursor: 0,
            background: 0,
            menu_name: std::ptr::null(),
            class_name: class_name.as_ptr(),
            icon_sm: 0,
        };
        // A zero return is only fatal if the class is not already registered,
        // which cannot happen here -- the window is created once per process --
        // so the result is not worth branching on. `CreateWindowExW` reports the
        // real failure.
        unsafe { RegisterClassExW(&class) };

        unsafe {
            CreateWindowExW(
                0,
                class_name.as_ptr(),
                class_name.as_ptr(),
                0,
                0,
                0,
                0,
                0,
                HWND_MESSAGE,
                0,
                instance,
                std::ptr::null(),
            )
        }
    }

    /// Runs until the window is destroyed.
    unsafe fn pump_messages() {
        let mut msg = Msg {
            hwnd: 0,
            message: 0,
            wparam: 0,
            lparam: 0,
            time: 0,
            point: Point::default(),
        };
        // `GetMessageW` returns 0 for WM_QUIT and -1 for an error; either is the
        // end of this thread's work.
        while unsafe { GetMessageW(&mut msg, 0, 0, 0) } > 0 {
            unsafe {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// Puts a real icon in the real notification area for a moment.
        ///
        /// Ignored by default because it is visible on the desktop of whoever
        /// runs it and needs a logged-in session to have a notification area at
        /// all. It is still the only thing that proves the struct layouts here
        /// match what the shell expects -- a wrong `cb_size` or a misplaced
        /// field is not a compile error, it is an icon that never appears.
        ///
        /// `cargo test tray_icon -- --ignored --nocapture`
        #[test]
        #[ignore]
        fn tray_icon_appears_and_goes_away() {
            let mut tray = Tray::spawn().expect("the tray window should be creatable");
            tray.show("MTUI test -- this should vanish in a second")
                .expect("the shell should accept the icon");
            assert!(tray.visible);

            // A second show is the tooltip update the background loop does on
            // every tick, and must be a modify rather than a second icon.
            tray.show("MTUI test -- updated tooltip")
                .expect("the shell should accept the update");

            std::thread::sleep(std::time::Duration::from_secs(1));
            tray.hide();
            assert!(!tray.visible);
            // Dropping joins the pump thread, which only returns if the window
            // actually went away -- so a hang here is a real failure.
            drop(tray);
        }
    }
}

#[cfg(not(windows))]
mod imp {
    use std::time::Duration;

    use anyhow::{Result, anyhow};

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum TrayCommand {
        Show,
        TogglePause,
        Next,
        Previous,
        Quit,
    }

    /// The same shape as the Windows implementation so the event loop compiles
    /// everywhere, and refuses at the one point where it would have to do
    /// something platform-specific. A tray icon is not a portable idea: X11 and
    /// Wayland disagree about what one even is, and macOS puts it somewhere
    /// else entirely.
    pub struct Tray;

    impl Tray {
        pub fn spawn() -> Result<Self> {
            Err(anyhow!(
                "a notification-area icon is only implemented on Windows"
            ))
        }

        pub fn show(&mut self, _tip: &str) -> Result<()> {
            unreachable!("no Tray can be constructed off Windows")
        }

        pub fn hide(&mut self) {}

        pub fn next_command(&self, _timeout: Duration) -> Option<TrayCommand> {
            None
        }
    }
}

pub use imp::{Tray, TrayCommand};
