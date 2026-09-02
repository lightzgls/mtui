//! Letting go of the terminal, and taking one back.
//!
//! On Windows the player never attaches to the visible console. Closing a
//! console forcibly terminates every attached process, so a private copy of
//! MTUI owns the window and forwards rendered bytes and native input records
//! over pipes. Closing the window kills that helper alone; the player notices
//! the closed pipes and continues behind its tray icon.

#[cfg(windows)]
mod imp {
    use std::ffi::c_void;
    use std::fs::File;
    use std::io::{self, Read, Write};
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use std::sync::{Mutex, OnceLock};
    use std::time::{Duration, Instant};

    use anyhow::{Context, Result, anyhow};
    use crossterm::event::{
        Event, KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers, MouseButton,
        MouseEvent, MouseEventKind,
    };

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn AllocConsole() -> i32;
        fn DuplicateHandle(
            source_process: isize,
            source: isize,
            target_process: isize,
            target: *mut isize,
            access: u32,
            inherit: i32,
            options: u32,
        ) -> i32;
        fn GetCurrentProcess() -> isize;
        fn GetStdHandle(which: u32) -> isize;
        fn SetConsoleMode(handle: isize, mode: u32) -> i32;
        fn GetConsoleMode(handle: isize, mode: *mut u32) -> i32;
        fn GetConsoleScreenBufferInfo(handle: isize, info: *mut ConsoleScreenBufferInfo) -> i32;
        fn CreateFileW(
            name: *const u16,
            access: u32,
            share: u32,
            security: *const c_void,
            creation: u32,
            flags: u32,
            template: isize,
        ) -> isize;
        fn SetConsoleCP(code_page: u32) -> i32;
        fn SetConsoleOutputCP(code_page: u32) -> i32;
        fn PeekNamedPipe(
            pipe: isize,
            buffer: *mut u8,
            buffer_size: u32,
            bytes_read: *mut u32,
            total_bytes_available: *mut u32,
            bytes_left_in_message: *mut u32,
        ) -> i32;
        fn ReadConsoleInputW(
            input: isize,
            records: *mut InputRecord,
            length: u32,
            read: *mut u32,
        ) -> i32;
        fn MessageBoxW(window: isize, text: *const u16, caption: *const u16, kind: u32) -> i32;
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    pub(super) struct InputRecord {
        pub(super) event_type: u16,
        pub(super) event: [u32; 4],
    }

    #[repr(C)]
    struct ConsoleScreenBufferInfo {
        size: [i16; 2],
        cursor: [i16; 2],
        attributes: u16,
        window: [i16; 4],
        maximum_window_size: [i16; 2],
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

    #[repr(C)]
    #[derive(Clone, Copy)]
    pub(super) struct MouseRecord {
        pub(super) position: [i16; 2],
        pub(super) button_state: u32,
        pub(super) control: u32,
        pub(super) event_flags: u32,
    }

    const STD_INPUT_HANDLE: u32 = -10i32 as u32;
    const STD_OUTPUT_HANDLE: u32 = -11i32 as u32;
    const GENERIC_READ: u32 = 0x8000_0000;
    const GENERIC_WRITE: u32 = 0x4000_0000;
    const FILE_SHARE_READ: u32 = 0x0000_0001;
    const FILE_SHARE_WRITE: u32 = 0x0000_0002;
    const OPEN_EXISTING: u32 = 3;
    const INVALID_HANDLE_VALUE: isize = -1;
    const CP_UTF8: u32 = 65001;
    const DUPLICATE_SAME_ACCESS: u32 = 0x0000_0002;
    const KEY_EVENT: u16 = 0x0001;
    const MOUSE_EVENT: u16 = 0x0002;
    const WINDOW_BUFFER_SIZE_EVENT: u16 = 0x0004;
    const SHIFT_PRESSED: u32 = 0x0010;
    const ALT_PRESSED: u32 = 0x0001 | 0x0002;
    const CONTROL_PRESSED: u32 = 0x0004 | 0x0008;
    const ENABLE_PROCESSED_INPUT: u32 = 0x0001;
    const ENABLE_LINE_INPUT: u32 = 0x0002;
    const ENABLE_ECHO_INPUT: u32 = 0x0004;
    const ENABLE_WINDOW_INPUT: u32 = 0x0008;
    const ENABLE_MOUSE_INPUT: u32 = 0x0010;
    const ENABLE_QUICK_EDIT_MODE: u32 = 0x0040;
    const ENABLE_EXTENDED_FLAGS: u32 = 0x0080;
    const MOUSE_WHEELED: u32 = 0x0004;
    const ENABLE_PROCESSED_OUTPUT: u32 = 0x0001;
    const ENABLE_WRAP_AT_EOL_OUTPUT: u32 = 0x0002;
    const ENABLE_VIRTUAL_TERMINAL_PROCESSING: u32 = 0x0004;
    const MB_ICONERROR: u32 = 0x0000_0010;
    const HOST_ARGUMENT: &str = "--mtui-console-host";
    const HANDSHAKE_LEN: usize = 512;
    const HANDSHAKE_MAGIC: &[u8; 8] = b"MTUICON1";
    const HANDSHAKE_READY: u8 = 1;
    const HANDSHAKE_ERROR: u8 = 2;
    const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
    const PIPE_POLL_INTERVAL: Duration = Duration::from_millis(2);
    const RECORD_LEN: usize = 18;

    static SESSION: OnceLock<Mutex<Option<ConsoleSession>>> = OnceLock::new();
    static CLOSED: AtomicBool = AtomicBool::new(true);
    static SIZE: AtomicU32 = AtomicU32::new((24 << 16) | 80);

    struct ConsoleSession {
        child: Child,
        input: ChildStdout,
        output: ChildStdin,
    }

    struct HostConsole {
        input: File,
        output: File,
        size: (u16, u16),
    }

    pub fn is_host() -> bool {
        std::env::args_os().nth(1).as_deref() == Some(std::ffi::OsStr::new(HOST_ARGUMENT))
    }

    /// Makes crossterm emit VT bytes instead of trying Win32 calls in this
    /// deliberately unattached process.
    pub fn prepare_parent() {
        // This runs before ordinary startup creates any threads, which is the
        // safety requirement for changing the process environment in Rust 2024.
        let previous = std::env::var_os("TERM");
        unsafe { std::env::set_var("TERM", "xterm-256color") };
        let _ = crossterm::ansi_support::supports_ansi();
        match previous {
            Some(value) => unsafe { std::env::set_var("TERM", value) },
            None => unsafe { std::env::remove_var("TERM") },
        }
    }

    /// Runs the private process which alone is attached to the visible console.
    pub fn run_host() -> Result<()> {
        // AllocConsole is allowed to replace process standard handles. Keep the
        // inherited pipes alive independently before making that call.
        let mut parent_output = File::from(duplicate_standard(STD_OUTPUT_HANDLE)?);
        let parent_input = match duplicate_standard(STD_INPUT_HANDLE) {
            Ok(handle) => File::from(handle),
            Err(err) => {
                let _ = write_handshake(&mut parent_output, Err(&err));
                return Ok(());
            }
        };

        let console = match open_host_console() {
            Ok(console) => console,
            Err(err) => {
                let _ = write_handshake(&mut parent_output, Err(&err));
                return Ok(());
            }
        };
        let HostConsole {
            input,
            mut output,
            size,
        } = console;
        let forwarder = std::thread::Builder::new()
            .name("mtui-console-output".to_string())
            .spawn(move || {
                let mut parent_input = parent_input;
                let _ = io::copy(&mut parent_input, &mut output);
                // The input loop may be blocked in ReadConsoleInputW. EOF means
                // the parent detached or exited, so end the helper here.
                std::process::exit(0);
            });
        if let Err(err) = forwarder {
            let err = anyhow!(err).context("could not start the console output forwarder");
            let _ = write_handshake(&mut parent_output, Err(&err));
            return Ok(());
        }
        write_handshake(&mut parent_output, Ok(size))?;

        forward_input(input, parent_output)
    }

    /// Starts a fresh console helper and waits for it to prove setup succeeded.
    pub fn attach() -> Result<()> {
        let sessions = SESSION.get_or_init(|| Mutex::new(None));
        let mut slot = sessions
            .lock()
            .map_err(|_| anyhow!("could not lock the console session"))?;
        if let Some(session) = slot.as_mut()
            && session.child.try_wait()?.is_none()
        {
            CLOSED.store(false, Ordering::Release);
            return Ok(());
        }
        if let Some(session) = slot.take() {
            stop(session);
        }

        let executable = std::env::current_exe().context("could not locate the MTUI executable")?;
        let mut child = Command::new(executable)
            .arg(HOST_ARGUMENT)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .context("could not start the terminal helper")?;
        let output = child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("the terminal helper has no output pipe"))?;
        let mut input = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("the terminal helper has no input pipe"))?;

        let handshake = read_handshake(&mut input);
        if let Err(err) = handshake {
            stop(ConsoleSession {
                child,
                input,
                output,
            });
            return Err(err);
        }
        let (width, height) = handshake.expect("the error case returned above");
        set_size(width, height);
        CLOSED.store(false, Ordering::Release);
        *slot = Some(ConsoleSession {
            child,
            input,
            output,
        });
        Ok(())
    }

    /// Closes the helper. It is also valid after the console X already killed it.
    pub fn detach() -> Result<()> {
        let Some(sessions) = SESSION.get() else {
            CLOSED.store(true, Ordering::Release);
            return Ok(());
        };
        let session = sessions
            .lock()
            .map_err(|_| anyhow!("could not lock the console session"))?
            .take();
        CLOSED.store(true, Ordering::Release);
        if let Some(session) = session {
            stop(session);
        }
        Ok(())
    }

    /// Whether the console helper or either half of its pipe has gone away.
    pub fn closed() -> bool {
        if CLOSED.load(Ordering::Acquire) {
            return true;
        }
        let Some(sessions) = SESSION.get() else {
            return true;
        };
        let Ok(mut slot) = sessions.lock() else {
            CLOSED.store(true, Ordering::Release);
            return true;
        };
        let alive = match slot.as_mut() {
            Some(session) => matches!(session.child.try_wait(), Ok(None)),
            None => false,
        };
        if !alive {
            CLOSED.store(true, Ordering::Release);
        }
        !alive
    }

    /// The last viewport reported by the helper.
    pub fn size() -> Result<(u16, u16)> {
        if closed() {
            return Err(anyhow!("the terminal helper is not running"));
        }
        let packed = SIZE.load(Ordering::Acquire);
        Ok((packed as u16, (packed >> 16) as u16))
    }

    fn set_size(width: u16, height: u16) {
        if width != 0 && height != 0 {
            SIZE.store(
                u32::from(width) | (u32::from(height) << 16),
                Ordering::Release,
            );
        }
    }

    fn stop(session: ConsoleSession) {
        let ConsoleSession {
            mut child,
            input,
            output,
        } = session;
        drop(input);
        drop(output);
        if matches!(child.try_wait(), Ok(None)) {
            let _ = child.kill();
        }
        let _ = child.wait();
    }

    fn read_handshake(input: &mut ChildStdout) -> Result<(u16, u16)> {
        let mut message = [0; HANDSHAKE_LEN];
        let mut filled = 0;
        let deadline = Instant::now() + HANDSHAKE_TIMEOUT;
        while filled < message.len() {
            match pipe_available(input.as_raw_handle() as isize)? {
                PipeState::Available(0) => {
                    let left = deadline.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        return Err(anyhow!("the terminal helper did not become ready in time"));
                    }
                    std::thread::sleep(left.min(PIPE_POLL_INTERVAL));
                }
                PipeState::Available(available) => {
                    let take = available.min(message.len() - filled);
                    match input.read(&mut message[filled..filled + take]) {
                        Ok(0) => {
                            return Err(anyhow!("the terminal helper exited during setup"));
                        }
                        Ok(read) => filled += read,
                        Err(err) if pipe_closed(&err) => {
                            return Err(anyhow!("the terminal helper exited during setup"));
                        }
                        Err(err) => {
                            return Err(err).context("could not read terminal helper setup");
                        }
                    }
                }
                PipeState::Closed => {
                    return Err(anyhow!("the terminal helper exited during setup"));
                }
            }
        }
        if &message[..8] != HANDSHAKE_MAGIC {
            return Err(anyhow!(
                "the terminal helper sent an invalid setup response"
            ));
        }
        let width = u16::from_le_bytes([message[9], message[10]]);
        let height = u16::from_le_bytes([message[11], message[12]]);
        let length =
            usize::from(u16::from_le_bytes([message[13], message[14]])).min(HANDSHAKE_LEN - 15);
        match message[8] {
            HANDSHAKE_READY if width != 0 && height != 0 => Ok((width, height)),
            HANDSHAKE_READY => Err(anyhow!("the terminal helper reported an invalid size")),
            HANDSHAKE_ERROR => Err(anyhow!(
                "terminal helper setup failed: {}",
                String::from_utf8_lossy(&message[15..15 + length])
            )),
            _ => Err(anyhow!("the terminal helper sent an invalid setup status")),
        }
    }

    fn write_handshake(
        output: &mut File,
        result: std::result::Result<(u16, u16), &anyhow::Error>,
    ) -> Result<()> {
        let mut message = [0; HANDSHAKE_LEN];
        message[..8].copy_from_slice(HANDSHAKE_MAGIC);
        match result {
            Ok((width, height)) => {
                message[8] = HANDSHAKE_READY;
                message[9..11].copy_from_slice(&width.to_le_bytes());
                message[11..13].copy_from_slice(&height.to_le_bytes());
            }
            Err(err) => {
                message[8] = HANDSHAKE_ERROR;
                let text = format!("{err:#}");
                let length = text.len().min(HANDSHAKE_LEN - 15);
                message[13..15].copy_from_slice(&(length as u16).to_le_bytes());
                message[15..15 + length].copy_from_slice(&text.as_bytes()[..length]);
            }
        }
        output.write_all(&message)?;
        output.flush()?;
        Ok(())
    }

    fn duplicate_standard(which: u32) -> Result<OwnedHandle> {
        let source = unsafe { GetStdHandle(which) };
        if source == 0 || source == INVALID_HANDLE_VALUE {
            return Err(anyhow!("the terminal helper did not inherit its pipes"));
        }
        duplicate_raw(source)
    }

    fn duplicate_raw(source: isize) -> Result<OwnedHandle> {
        let process = unsafe { GetCurrentProcess() };
        let mut target = 0;
        if unsafe {
            DuplicateHandle(
                process,
                source,
                process,
                &mut target,
                0,
                0,
                DUPLICATE_SAME_ACCESS,
            )
        } == 0
        {
            return Err(anyhow!(
                "could not duplicate a terminal pipe: {}",
                io::Error::last_os_error()
            ));
        }
        Ok(unsafe { OwnedHandle::from_raw_handle(target as *mut c_void) })
    }

    fn open_host_console() -> Result<HostConsole> {
        if unsafe { AllocConsole() } == 0 {
            return Err(anyhow!(
                "could not allocate a console: {}",
                io::Error::last_os_error()
            ));
        }
        let input = File::from(open_console("CONIN$", GENERIC_READ | GENERIC_WRITE)?);
        let output = File::from(open_console("CONOUT$", GENERIC_READ | GENERIC_WRITE)?);
        let input_raw = input.as_raw_handle() as isize;
        let output_raw = output.as_raw_handle() as isize;

        if unsafe { SetConsoleCP(CP_UTF8) } == 0 || unsafe { SetConsoleOutputCP(CP_UTF8) } == 0 {
            return Err(anyhow!("could not enable UTF-8 in the new terminal"));
        }
        let mut input_mode = 0;
        if unsafe { GetConsoleMode(input_raw, &mut input_mode) } == 0 {
            return Err(anyhow!("the new console has no usable input handle"));
        }
        input_mode &= !(ENABLE_PROCESSED_INPUT
            | ENABLE_LINE_INPUT
            | ENABLE_ECHO_INPUT
            | ENABLE_QUICK_EDIT_MODE);
        input_mode |= ENABLE_WINDOW_INPUT | ENABLE_MOUSE_INPUT | ENABLE_EXTENDED_FLAGS;
        if unsafe { SetConsoleMode(input_raw, input_mode) } == 0 {
            return Err(anyhow!("could not enable raw terminal input"));
        }

        let mut output_mode = 0;
        if unsafe { GetConsoleMode(output_raw, &mut output_mode) } == 0
            || unsafe {
                SetConsoleMode(
                    output_raw,
                    output_mode
                        | ENABLE_PROCESSED_OUTPUT
                        | ENABLE_WRAP_AT_EOL_OUTPUT
                        | ENABLE_VIRTUAL_TERMINAL_PROCESSING,
                )
            } == 0
        {
            return Err(anyhow!("could not enable terminal output"));
        }

        let mut info = ConsoleScreenBufferInfo {
            size: [0; 2],
            cursor: [0; 2],
            attributes: 0,
            window: [0; 4],
            maximum_window_size: [0; 2],
        };
        if unsafe { GetConsoleScreenBufferInfo(output_raw, &mut info) } == 0 {
            return Err(anyhow!("could not read the terminal size"));
        }
        let width = i32::from(info.window[2]) - i32::from(info.window[0]) + 1;
        let height = i32::from(info.window[3]) - i32::from(info.window[1]) + 1;
        if width <= 0 || height <= 0 || width > i32::from(u16::MAX) || height > i32::from(u16::MAX)
        {
            return Err(anyhow!("the new terminal reported an invalid size"));
        }
        Ok(HostConsole {
            input,
            output,
            size: (width as u16, height as u16),
        })
    }

    fn forward_input(input: File, mut output: File) -> Result<()> {
        let handle = input.as_raw_handle() as isize;
        loop {
            let mut record = InputRecord {
                event_type: 0,
                event: [0; 4],
            };
            let mut read = 0;
            if unsafe { ReadConsoleInputW(handle, &mut record, 1, &mut read) } == 0 {
                return Ok(());
            }
            if read != 0 && output.write_all(&encode_record(record)).is_err() {
                return Ok(());
            }
        }
    }

    pub(super) fn encode_record(record: InputRecord) -> [u8; RECORD_LEN] {
        let mut wire = [0; RECORD_LEN];
        wire[..2].copy_from_slice(&record.event_type.to_le_bytes());
        for (index, word) in record.event.into_iter().enumerate() {
            let start = 2 + index * 4;
            wire[start..start + 4].copy_from_slice(&word.to_le_bytes());
        }
        wire
    }

    pub(super) fn decode_record(wire: &[u8; RECORD_LEN]) -> InputRecord {
        let mut event = [0; 4];
        for (index, word) in event.iter_mut().enumerate() {
            let start = 2 + index * 4;
            *word = u32::from_le_bytes(wire[start..start + 4].try_into().unwrap());
        }
        InputRecord {
            event_type: u16::from_le_bytes([wire[0], wire[1]]),
            event,
        }
    }

    /// A fresh writer to the currently attached helper.
    pub fn output() -> Result<Output> {
        let sessions = SESSION
            .get()
            .ok_or_else(|| anyhow!("the terminal helper is not running"))?;
        let slot = sessions
            .lock()
            .map_err(|_| anyhow!("could not lock the console session"))?;
        let session = slot
            .as_ref()
            .ok_or_else(|| anyhow!("the terminal helper is not running"))?;
        let file = File::from(duplicate_raw(session.output.as_raw_handle() as isize)?);
        Ok(Output {
            file,
            frame: Vec::with_capacity(64 * 1024),
        })
    }

    /// Collects a complete ratatui frame before handing it to the helper.
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
            if let Err(err) = self.file.write_all(&self.frame) {
                self.frame.clear();
                if pipe_closed(&err) {
                    CLOSED.store(true, Ordering::Release);
                    return Ok(());
                }
                return Err(err);
            }
            self.frame.clear();
            if let Err(err) = self.file.flush() {
                if pipe_closed(&err) {
                    CLOSED.store(true, Ordering::Release);
                    return Ok(());
                }
                return Err(err);
            }
            Ok(())
        }
    }

    impl Drop for Output {
        fn drop(&mut self) {
            let _ = self.flush();
        }
    }

    pub struct Input {
        file: File,
        wire: [u8; RECORD_LEN],
        filled: usize,
        high_surrogate: Option<u16>,
    }

    impl Input {
        pub fn open() -> Result<Self> {
            let sessions = SESSION
                .get()
                .ok_or_else(|| anyhow!("the terminal helper is not running"))?;
            let slot = sessions
                .lock()
                .map_err(|_| anyhow!("could not lock the console session"))?;
            let session = slot
                .as_ref()
                .ok_or_else(|| anyhow!("the terminal helper is not running"))?;
            Ok(Self {
                file: File::from(duplicate_raw(session.input.as_raw_handle() as isize)?),
                wire: [0; RECORD_LEN],
                filled: 0,
                high_surrogate: None,
            })
        }

        pub fn next(&mut self, timeout: Duration) -> Result<Option<Event>> {
            let mut wait = timeout;
            loop {
                if !self.read_record(wait)? {
                    return Ok(None);
                }
                let record = decode_record(&self.wire);
                if let Some(event) = event(record, &mut self.high_surrogate) {
                    if let Event::Resize(width, height) = &event {
                        set_size(*width, *height);
                    }
                    return Ok(Some(event));
                }
                wait = Duration::ZERO;
            }
        }

        fn read_record(&mut self, timeout: Duration) -> Result<bool> {
            let deadline = Instant::now() + timeout;
            while self.filled < RECORD_LEN {
                let handle = self.file.as_raw_handle() as isize;
                match pipe_available(handle)? {
                    PipeState::Available(0) => {
                        let left = deadline.saturating_duration_since(Instant::now());
                        if left.is_zero() {
                            return Ok(false);
                        }
                        std::thread::sleep(left.min(PIPE_POLL_INTERVAL));
                    }
                    PipeState::Available(available) => {
                        let take = available.min(RECORD_LEN - self.filled);
                        match self
                            .file
                            .read(&mut self.wire[self.filled..self.filled + take])
                        {
                            Ok(0) => {
                                CLOSED.store(true, Ordering::Release);
                                self.filled = 0;
                                return Ok(false);
                            }
                            Ok(read) => self.filled += read,
                            Err(err) if pipe_closed(&err) => {
                                CLOSED.store(true, Ordering::Release);
                                self.filled = 0;
                                return Ok(false);
                            }
                            Err(err) => return Err(err).context("could not read terminal input"),
                        }
                    }
                    PipeState::Closed => {
                        CLOSED.store(true, Ordering::Release);
                        self.filled = 0;
                        return Ok(false);
                    }
                }
            }
            self.filled = 0;
            Ok(true)
        }
    }

    enum PipeState {
        Available(usize),
        Closed,
    }

    fn pipe_available(handle: isize) -> Result<PipeState> {
        let mut available = 0;
        if unsafe {
            PeekNamedPipe(
                handle,
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                &mut available,
                std::ptr::null_mut(),
            )
        } == 0
        {
            let err = io::Error::last_os_error();
            if pipe_closed(&err) {
                return Ok(PipeState::Closed);
            }
            return Err(err).context("could not inspect terminal input");
        }
        Ok(PipeState::Available(available as usize))
    }

    fn pipe_closed(err: &io::Error) -> bool {
        matches!(err.raw_os_error(), Some(109 | 232 | 233))
            || matches!(
                err.kind(),
                io::ErrorKind::BrokenPipe | io::ErrorKind::NotConnected
            )
    }

    fn event(record: InputRecord, high_surrogate: &mut Option<u16>) -> Option<Event> {
        match record.event_type {
            KEY_EVENT => {
                let key =
                    unsafe { std::ptr::read_unaligned(record.event.as_ptr().cast::<KeyRecord>()) };
                key_event(key, high_surrogate).map(Event::Key)
            }
            MOUSE_EVENT => {
                let mouse = unsafe {
                    std::ptr::read_unaligned(record.event.as_ptr().cast::<MouseRecord>())
                };
                mouse_event(mouse).map(Event::Mouse)
            }
            WINDOW_BUFFER_SIZE_EVENT => {
                let packed = record.event[0];
                Some(Event::Resize(packed as u16, (packed >> 16) as u16))
            }
            _ => None,
        }
    }

    pub(super) fn mouse_event(record: MouseRecord) -> Option<MouseEvent> {
        let column = u16::try_from(record.position[0]).ok()?;
        let row = u16::try_from(record.position[1]).ok()?;
        let kind = if record.event_flags == MOUSE_WHEELED {
            if (record.button_state >> 16) as i16 > 0 {
                MouseEventKind::ScrollUp
            } else {
                MouseEventKind::ScrollDown
            }
        } else if record.event_flags == 0 {
            let button = if record.button_state & 0x0001 != 0 {
                MouseButton::Left
            } else if record.button_state & 0x0002 != 0 {
                MouseButton::Right
            } else if record.button_state & 0x0004 != 0 {
                MouseButton::Middle
            } else {
                return None;
            };
            MouseEventKind::Down(button)
        } else {
            return None;
        };
        Some(MouseEvent {
            kind,
            column,
            row,
            modifiers: modifiers(record.control),
        })
    }

    fn modifiers(control: u32) -> KeyModifiers {
        let mut modifiers = KeyModifiers::empty();
        if control & SHIFT_PRESSED != 0 {
            modifiers.insert(KeyModifiers::SHIFT);
        }
        if control & ALT_PRESSED != 0 {
            modifiers.insert(KeyModifiers::ALT);
        }
        if control & CONTROL_PRESSED != 0 {
            modifiers.insert(KeyModifiers::CONTROL);
        }
        modifiers
    }

    pub(super) fn key_event(
        record: KeyRecord,
        high_surrogate: &mut Option<u16>,
    ) -> Option<KeyEvent> {
        let modifiers = modifiers(record.control);

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
            return Err(anyhow!(
                "could not open {name} for the new terminal: {}",
                io::Error::last_os_error()
            ));
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
    use std::time::Duration;

    use anyhow::{Result, anyhow};
    use crossterm::event::{self, Event};

    /// Detaching a running process from its controlling terminal is a different
    /// problem on Unix and there is no notification area to put the result in.
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

    pub fn closed() -> bool {
        false
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

pub use imp::{Input, Output, attach, closed, detach, output, report_error};

#[cfg(windows)]
pub use imp::{is_host, prepare_parent, run_host, size};

#[cfg(all(test, windows))]
mod tests {
    use crossterm::event::{KeyCode, KeyModifiers, MouseButton, MouseEventKind};

    use super::imp::{
        InputRecord, KeyRecord, MouseRecord, decode_record, encode_record, key_event, mouse_event,
        unicode_char,
    };

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

    #[test]
    fn input_record_wire_format_is_stable_and_round_trips() {
        let record = InputRecord {
            event_type: 0x1234,
            event: [0x0102_0304, 5, 0xaabb_ccdd, u32::MAX],
        };
        let wire = encode_record(record);
        assert_eq!(wire.len(), 18);
        assert_eq!(&wire[..6], &[0x34, 0x12, 4, 3, 2, 1]);
        let decoded = decode_record(&wire);
        assert_eq!(decoded.event_type, record.event_type);
        assert_eq!(decoded.event, record.event);
    }

    #[test]
    fn native_left_click_becomes_a_terminal_mouse_event() {
        let event = mouse_event(MouseRecord {
            position: [12, 7],
            button_state: 1,
            control: 0,
            event_flags: 0,
        })
        .unwrap();
        assert_eq!(event.kind, MouseEventKind::Down(MouseButton::Left));
        assert_eq!((event.column, event.row), (12, 7));
    }

    #[test]
    fn native_wheel_direction_keeps_its_sign() {
        let up = mouse_event(MouseRecord {
            position: [0, 0],
            button_state: (120u32) << 16,
            control: 0,
            event_flags: 4,
        })
        .unwrap();
        let down = mouse_event(MouseRecord {
            button_state: ((-120i16 as u16) as u32) << 16,
            ..MouseRecord {
                position: [0, 0],
                button_state: 0,
                control: 0,
                event_flags: 4,
            }
        })
        .unwrap();
        assert_eq!(up.kind, MouseEventKind::ScrollUp);
        assert_eq!(down.kind, MouseEventKind::ScrollDown);
    }

    /// Opens a real helper window, so it is kept out of automated test runs.
    #[test]
    #[ignore = "opens a real console window"]
    fn tray_restore_reopens_a_console_helper() {
        super::attach().expect("a helper console should open");
        assert!(!super::closed());
        super::detach().expect("the helper console should close");
    }
}
