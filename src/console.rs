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
    use anyhow::{Result, anyhow};

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn FreeConsole() -> i32;
        fn AllocConsole() -> i32;
        fn GetStdHandle(which: u32) -> isize;
        fn SetConsoleMode(handle: isize, mode: u32) -> i32;
        fn GetConsoleMode(handle: isize, mode: *mut u32) -> i32;
    }

    const STD_OUTPUT_HANDLE: u32 = -11i32 as u32;
    const ENABLE_PROCESSED_OUTPUT: u32 = 0x0001;
    const ENABLE_WRAP_AT_EOL_OUTPUT: u32 = 0x0002;
    const ENABLE_VIRTUAL_TERMINAL_PROCESSING: u32 = 0x0004;

    /// Gives up the console. The window MTUI was started from can be closed
    /// after this without touching the process.
    pub fn detach() -> Result<()> {
        if unsafe { FreeConsole() } == 0 {
            return Err(anyhow!("could not detach from the terminal"));
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

        let out = unsafe { GetStdHandle(STD_OUTPUT_HANDLE) };
        let mut mode = 0;
        if unsafe { GetConsoleMode(out, &mut mode) } == 0 {
            return Err(anyhow!("the new console has no usable output handle"));
        }
        unsafe {
            SetConsoleMode(
                out,
                mode | ENABLE_PROCESSED_OUTPUT
                    | ENABLE_WRAP_AT_EOL_OUTPUT
                    | ENABLE_VIRTUAL_TERMINAL_PROCESSING,
            )
        };
        Ok(())
    }
}

#[cfg(not(windows))]
mod imp {
    use anyhow::{Result, anyhow};

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
}

pub use imp::{attach, detach};
