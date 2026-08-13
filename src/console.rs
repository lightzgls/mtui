//! Letting go of the terminal, and taking one back.
//!
//! What this is for: a music player outlives the window it was started from.
//! On Windows that cannot be arranged after the fact. Closing a console window
//! sends `CTRL_CLOSE_EVENT` to every process attached to it and then kills them
//! all -- and the obvious dodge, detaching from inside the handler, does not
//! work: the process is terminated anyway, with the handler's own `FreeConsole`
//! having returned success moments earlier. That was measured, not assumed,
//! against Windows Terminal on Windows 11.
//!
//! So the detaching has to happen *before* the close, while the program is
//! still running normally -- which is why backgrounding MTUI is a key rather
//! than something it can infer from the window going away. Once detached the
//! process has no console at all, and closing the window it came from is
//! nothing to it.
//!
//! [`attach`] is the way back: a process with no console can allocate a fresh
//! one, and the terminal interface starts again in it.

#[cfg(windows)]
mod imp {
    use std::ffi::c_void;
    use std::fs::File;
    use std::io::{self, Write};
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use std::sync::{Mutex, OnceLock};
    use std::time::Duration;

    use anyhow::{Result, anyhow};
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn FreeConsole() -> i32;
        fn AllocConsole() -> i32;
        fn CloseHandle(handle: isize) -> i32;
        fn GetStdHandle(which: u32) -> isize;
        fn SetConsoleMode(handle: isize, mode: u32) -> i32;
        fn GetConsoleMode(handle: isize, mode: *mut u32) -> i32;
        fn CreateFileW(
            name: *const u16,
            access: u32,
            share: u32,
            security: *const c_void,
            creation: u32,
            flags: u32,
            template: isize,
        ) -> isize;
        fn SetStdHandle(which: u32, handle: isize) -> i32;
        fn SetConsoleCP(code_page: u32) -> i32;
        fn SetConsoleOutputCP(code_page: u32) -> i32;
        fn WaitForSingleObject(handle: isize, milliseconds: u32) -> u32;
        fn ReadConsoleInputW(
            input: isize,
            records: *mut InputRecord,
            length: u32,
            read: *mut u32,
        ) -> i32;
        fn MessageBoxW(window: isize, text: *const u16, caption: *const u16, kind: u32) -> i32;
    }

    #[repr(C)]
    struct InputRecord {
        event_type: u16,
        event: [u32; 4],
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    pub(super) struct KeyRecord {
        pub(super) down: i32,
        pub(super) repeat: u16,
        pub(super) virtual_key: u16,
        pub(super) scan_code: u16,
        pub(super) unicode: u16,
        pub(super) control: u32,
    }

    const STD_INPUT_HANDLE: u32 = -10i32 as u32;
    const STD_OUTPUT_HANDLE: u32 = -11i32 as u32;
    const STD_ERROR_HANDLE: u32 = -12i32 as u32;
    const GENERIC_READ: u32 = 0x8000_0000;
    const GENERIC_WRITE: u32 = 0x4000_0000;
    const FILE_SHARE_READ: u32 = 0x0000_0001;
    const FILE_SHARE_WRITE: u32 = 0x0000_0002;
    const OPEN_EXISTING: u32 = 3;
    const INVALID_HANDLE_VALUE: isize = -1;
    const CP_UTF8: u32 = 65001;
    const WAIT_OBJECT_0: u32 = 0;
    const WAIT_TIMEOUT: u32 = 258;
    const KEY_EVENT: u16 = 0x0001;
    const WINDOW_BUFFER_SIZE_EVENT: u16 = 0x0004;
    const SHIFT_PRESSED: u32 = 0x0010;
    const ALT_PRESSED: u32 = 0x0001 | 0x0002;
    const CONTROL_PRESSED: u32 = 0x0004 | 0x0008;
    const ENABLE_WINDOW_INPUT: u32 = 0x0008;
    const ENABLE_PROCESSED_OUTPUT: u32 = 0x0001;
    const ENABLE_WRAP_AT_EOL_OUTPUT: u32 = 0x0002;
    const ENABLE_VIRTUAL_TERMINAL_PROCESSING: u32 = 0x0004;
    const MB_ICONERROR: u32 = 0x0000_0010;

    struct StandardHandles {
        _input: OwnedHandle,
        _output: OwnedHandle,
    }

    static STANDARD_HANDLES: OnceLock<Mutex<Option<StandardHandles>>> = OnceLock::new();

    /// Gives up the console. The window MTUI was started from can be closed
    /// after this without touching the process.
    pub fn detach() -> Result<()> {
        if unsafe { FreeConsole() } == 0 {
            return Err(anyhow!("could not detach from the terminal"));
        }
        if let Ok(mut handles) = STANDARD_HANDLES.get_or_init(|| Mutex::new(None)).lock() {
            *handles = None;
        }
        Ok(())
    }

    /// Allocates a new console and makes it fit to draw a terminal interface on.
    ///
    /// `AllocConsole` points the standard handles at the new console by itself,
    /// but it does not turn on escape-sequence processing, and it is not safe to
    /// assume crossterm will: it decides once whether the output handle
    /// understands ANSI and remembers the answer, and the handle it asked about
    /// is the one that went away with the old console. Left alone, the first
    /// frame would print its escape sequences as text. So the mode is set here,
    /// on the handle that is actually going to be written to.
    pub fn attach() -> Result<()> {
        if unsafe { AllocConsole() } == 0 {
            return Err(anyhow!("could not open a new terminal window"));
        }

        let result = configure();
        if result.is_err() {
            unsafe { FreeConsole() };
        }
        result
    }

    fn configure() -> Result<()> {
        // AllocConsole does not reliably replace handles inherited from the
        // terminal that FreeConsole detached. Reopen the console devices and
        // publish them as the process standard handles before crossterm asks.
        let old = [
            unsafe { GetStdHandle(STD_INPUT_HANDLE) },
            unsafe { GetStdHandle(STD_OUTPUT_HANDLE) },
            unsafe { GetStdHandle(STD_ERROR_HANDLE) },
        ];
        let input = open_console("CONIN$", GENERIC_READ | GENERIC_WRITE)?;
        let output = open_console("CONOUT$", GENERIC_READ | GENERIC_WRITE)?;
        let input_raw = input.as_raw_handle() as isize;
        let output_raw = output.as_raw_handle() as isize;
        if unsafe { SetStdHandle(STD_INPUT_HANDLE, input_raw) } == 0
            || unsafe { SetStdHandle(STD_OUTPUT_HANDLE, output_raw) } == 0
            || unsafe { SetStdHandle(STD_ERROR_HANDLE, output_raw) } == 0
        {
            return Err(anyhow!(
                "could not connect standard handles to the new terminal"
            ));
        }
        for (index, handle) in old.into_iter().enumerate() {
            if handle != 0
                && handle != INVALID_HANDLE_VALUE
                && !old[..index].contains(&handle)
                && handle != input_raw
                && handle != output_raw
            {
                unsafe { CloseHandle(handle) };
            }
        }
        if unsafe { SetConsoleCP(CP_UTF8) } == 0 || unsafe { SetConsoleOutputCP(CP_UTF8) } == 0 {
            return Err(anyhow!("could not enable UTF-8 in the new terminal"));
        }

        let mut input_mode = 0;
        if unsafe { GetConsoleMode(input_raw, &mut input_mode) } == 0
            || unsafe { SetConsoleMode(input_raw, input_mode | ENABLE_WINDOW_INPUT) } == 0
        {
            return Err(anyhow!("the new console has no usable input handle"));
        }

        let out = unsafe { GetStdHandle(STD_OUTPUT_HANDLE) };
        let mut mode = 0;
        if unsafe { GetConsoleMode(out, &mut mode) } == 0 {
            return Err(anyhow!("the new console has no usable output handle"));
        }
        if unsafe {
            SetConsoleMode(
                out,
                mode | ENABLE_PROCESSED_OUTPUT
                    | ENABLE_WRAP_AT_EOL_OUTPUT
                    | ENABLE_VIRTUAL_TERMINAL_PROCESSING,
            )
        } == 0
        {
            return Err(anyhow!("could not enable terminal output"));
        }
        *STANDARD_HANDLES
            .get_or_init(|| Mutex::new(None))
            .lock()
            .map_err(|_| anyhow!("could not retain the new terminal handles"))? =
            Some(StandardHandles {
                _input: input,
                _output: output,
            });
        Ok(())
    }

    /// A fresh writer for the currently attached console.
    pub fn output() -> Result<Output> {
        let handle = open_console("CONOUT$", GENERIC_READ | GENERIC_WRITE)?;
        let file = File::from(handle);
        Ok(Output {
            file,
            frame: Vec::with_capacity(64 * 1024),
        })
    }

    /// Collects a complete ratatui frame before handing it to Windows.
    ///
    /// `BufWriter` flushes whenever its fixed capacity is crossed, which makes
    /// large pages regress to several synchronous console writes. Ratatui
    /// already flushes once at the end of every frame, so keeping an unbounded,
    /// reusable frame buffer preserves that boundary exactly.
    pub struct Output {
        file: File,
        frame: Vec<u8>,
    }

    impl Write for Output {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.frame.extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            self.file.write_all(&self.frame)?;
            self.frame.clear();
            self.file.flush()
        }
    }

    impl Drop for Output {
        fn drop(&mut self) {
            let _ = self.flush();
        }
    }

    pub struct Input {
        handle: OwnedHandle,
        high_surrogate: Option<u16>,
    }

    impl Input {
        pub fn open() -> Result<Self> {
            Ok(Self {
                handle: open_console("CONIN$", GENERIC_READ | GENERIC_WRITE)?,
                high_surrogate: None,
            })
        }

        pub fn next(&mut self, timeout: Duration) -> Result<Option<Event>> {
            let timeout = timeout.as_millis().min(u128::from(u32::MAX)) as u32;
            let handle = self.handle.as_raw_handle() as isize;
            match unsafe { WaitForSingleObject(handle, timeout) } {
                WAIT_TIMEOUT => return Ok(None),
                WAIT_OBJECT_0 => {}
                _ => return Err(anyhow!("could not wait for terminal input")),
            }

            loop {
                let mut record = InputRecord {
                    event_type: 0,
                    event: [0; 4],
                };
                let mut read = 0;
                if unsafe { ReadConsoleInputW(handle, &mut record, 1, &mut read) } == 0 {
                    return Err(anyhow!("could not read terminal input"));
                }
                if read == 0 {
                    return Ok(None);
                }
                if let Some(event) = event(record, &mut self.high_surrogate) {
                    return Ok(Some(event));
                }
                if unsafe { WaitForSingleObject(handle, 0) } != WAIT_OBJECT_0 {
                    return Ok(None);
                }
            }
        }
    }

    fn event(record: InputRecord, high_surrogate: &mut Option<u16>) -> Option<Event> {
        match record.event_type {
            KEY_EVENT => {
                let key =
                    unsafe { std::ptr::read_unaligned(record.event.as_ptr().cast::<KeyRecord>()) };
                key_event(key, high_surrogate).map(Event::Key)
            }
            WINDOW_BUFFER_SIZE_EVENT => {
                let packed = record.event[0];
                Some(Event::Resize(packed as u16, (packed >> 16) as u16))
            }
            _ => None,
        }
    }

    pub(super) fn key_event(
        record: KeyRecord,
        high_surrogate: &mut Option<u16>,
    ) -> Option<KeyEvent> {
        let mut modifiers = KeyModifiers::empty();
        if record.control & SHIFT_PRESSED != 0 {
            modifiers.insert(KeyModifiers::SHIFT);
        }
        if record.control & ALT_PRESSED != 0 {
            modifiers.insert(KeyModifiers::ALT);
        }
        if record.control & CONTROL_PRESSED != 0 {
            modifiers.insert(KeyModifiers::CONTROL);
        }

        let code = match record.virtual_key {
            0x08 => KeyCode::Backspace,
            0x09 if modifiers.contains(KeyModifiers::SHIFT) => KeyCode::BackTab,
            0x09 => KeyCode::Tab,
            0x0d => KeyCode::Enter,
            0x1b => KeyCode::Esc,
            0x21 => KeyCode::PageUp,
            0x22 => KeyCode::PageDown,
            0x23 => KeyCode::End,
            0x24 => KeyCode::Home,
            0x25 => KeyCode::Left,
            0x26 => KeyCode::Up,
            0x27 => KeyCode::Right,
            0x28 => KeyCode::Down,
            0x2e => KeyCode::Delete,
            key @ 0x41..=0x5a
                if modifiers.contains(KeyModifiers::CONTROL)
                    && !modifiers.contains(KeyModifiers::ALT) =>
            {
                KeyCode::Char(char::from_u32(u32::from(key) + 32)?)
            }
            _ => KeyCode::Char(unicode_char(record.unicode, high_surrogate)?),
        };
        Some(KeyEvent {
            code,
            modifiers,
            kind: if record.down != 0 || (record.virtual_key == 0x12 && record.unicode != 0) {
                KeyEventKind::Press
            } else {
                KeyEventKind::Release
            },
            state: KeyEventState::empty(),
        })
    }

    pub(super) fn unicode_char(unit: u16, high_surrogate: &mut Option<u16>) -> Option<char> {
        if (0xd800..=0xdbff).contains(&unit) {
            *high_surrogate = Some(unit);
            return None;
        }
        if (0xdc00..=0xdfff).contains(&unit) {
            let high = high_surrogate.take()?;
            let scalar = 0x1_0000 + ((u32::from(high) - 0xd800) << 10) + (u32::from(unit) - 0xdc00);
            return char::from_u32(scalar);
        }
        *high_surrogate = None;
        (unit != 0).then(|| char::from_u32(u32::from(unit)))?
    }

    fn open_console(name: &str, access: u32) -> Result<OwnedHandle> {
        let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
        let handle = unsafe {
            CreateFileW(
                wide.as_ptr(),
                access,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                std::ptr::null(),
                OPEN_EXISTING,
                0,
                0,
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(anyhow!("could not open {name} for the new terminal"));
        }
        Ok(unsafe { OwnedHandle::from_raw_handle(handle as *mut c_void) })
    }

    pub fn report_error(message: &str) {
        let text: Vec<u16> = message.encode_utf16().chain(std::iter::once(0)).collect();
        let caption: Vec<u16> = "MTUI error"
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        unsafe { MessageBoxW(0, text.as_ptr(), caption.as_ptr(), MB_ICONERROR) };
    }
}

#[cfg(not(windows))]
mod imp {
    use std::io;

    use anyhow::{Result, anyhow};
    use crossterm::event::{self, Event};
    use std::time::Duration;

    /// Detaching a running process from its controlling terminal is a different
    /// problem on Unix -- `setsid` after a fork, which has to happen before the
    /// audio thread exists rather than at the user's convenience -- and there is
    /// no notification area to put the result in. Refused rather than
    /// half-implemented.
    pub fn detach() -> Result<()> {
        Err(anyhow!(
            "running in the background is only implemented on Windows"
        ))
    }

    pub fn attach() -> Result<()> {
        Err(anyhow!(
            "running in the background is only implemented on Windows"
        ))
    }

    pub type Output = io::Stdout;

    pub fn output() -> Result<Output> {
        Ok(io::stdout())
    }

    pub struct Input;

    impl Input {
        pub fn open() -> Result<Self> {
            Ok(Self)
        }

        pub fn next(&mut self, timeout: Duration) -> Result<Option<Event>> {
            if event::poll(timeout)? {
                Ok(Some(event::read()?))
            } else {
                Ok(None)
            }
        }
    }

    pub fn report_error(message: &str) {
        eprintln!("{message}");
    }
}

pub use imp::{Input, Output, attach, detach, output, report_error};

#[cfg(all(test, windows))]
mod tests {
    use crossterm::event::{KeyCode, KeyModifiers};

    use super::imp::{KeyRecord, key_event, unicode_char};

    fn record(virtual_key: u16, unicode: u16, control: u32) -> KeyRecord {
        KeyRecord {
            down: 1,
            repeat: 1,
            virtual_key,
            scan_code: 0,
            unicode,
            control,
        }
    }

    #[test]
    fn modifier_only_records_do_not_type_nul() {
        let mut high = None;
        assert!(key_event(record(0x10, 0, 0x0010), &mut high).is_none());
    }

    #[test]
    fn control_letters_keep_the_control_modifier() {
        let mut high = None;
        let key = key_event(record(0x53, 0x13, 0x0008), &mut high).unwrap();
        assert_eq!(key.code, KeyCode::Char('s'));
        assert!(key.modifiers.contains(KeyModifiers::CONTROL));
    }

    #[test]
    fn altgr_letters_keep_the_character_windows_composed() {
        let mut high = None;
        let key = key_event(record(0x53, 'ś' as u16, 0x0009), &mut high).unwrap();
        assert_eq!(key.code, KeyCode::Char('ś'));
        assert!(key.modifiers.contains(KeyModifiers::CONTROL));
        assert!(key.modifiers.contains(KeyModifiers::ALT));
    }

    #[test]
    fn alt_numpad_characters_are_delivered_as_press_events() {
        let mut high = None;
        let mut release = record(0x12, 'é' as u16, 0);
        release.down = 0;
        let key = key_event(release, &mut high).unwrap();
        assert!(key.is_press());
        assert_eq!(key.code, KeyCode::Char('é'));
    }

    #[test]
    fn surrogate_pairs_form_one_character() {
        let mut high = None;
        assert_eq!(unicode_char(0xd83c, &mut high), None);
        assert_eq!(unicode_char(0xdfb5, &mut high), Some('🎵'));
    }

    /// Exercises the same detach/attach cycle used by a tray-icon click.
    /// Ignored because it replaces the test runner's console window.
    #[test]
    #[ignore = "opens a real console window"]
    fn tray_restore_reopens_standard_handles() {
        super::detach().expect("the old console should detach");
        super::attach().expect("a new console should attach");
        assert!(std::io::IsTerminal::is_terminal(&std::io::stdin()));
        assert!(std::io::IsTerminal::is_terminal(&std::io::stdout()));
    }
}
