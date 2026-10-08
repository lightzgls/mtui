//! Native clipboard support for copying Music share links.

use anyhow::{Result, bail};

#[cfg(windows)]
pub fn copy(text: &str) -> Result<()> {
    use std::ffi::c_void;
    #[link(name = "user32")]
    unsafe extern "system" {
        fn CreateWindowExW(
            ex_style: u32,
            class: *const u16,
            title: *const u16,
            style: u32,
            x: i32,
            y: i32,
            width: i32,
            height: i32,
            parent: isize,
            menu: isize,
            instance: isize,
            param: *const c_void,
        ) -> isize;
        fn DestroyWindow(window: isize) -> i32;
        fn OpenClipboard(owner: isize) -> i32;
        fn CloseClipboard() -> i32;
        fn EmptyClipboard() -> i32;
        fn SetClipboardData(format: u32, data: *mut c_void) -> *mut c_void;
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GlobalAlloc(flags: u32, bytes: usize) -> *mut c_void;
        fn GlobalLock(memory: *mut c_void) -> *mut c_void;
        fn GlobalUnlock(memory: *mut c_void) -> i32;
        fn GlobalFree(memory: *mut c_void) -> *mut c_void;
    }
    let data: Vec<u16> = text.encode_utf16().chain([0]).collect();
    // Allocate before emptying the clipboard; ownership transfers only on success.
    unsafe {
        // A message-only STATIC window gives this process valid ownership.
        // OpenClipboard(NULL) followed by EmptyClipboard cannot set data.
        let class: Vec<u16> = "STATIC".encode_utf16().chain([0]).collect();
        let owner = CreateWindowExW(
            0,
            class.as_ptr(),
            std::ptr::null(),
            0,
            0,
            0,
            0,
            0,
            -3,
            0,
            0,
            std::ptr::null(),
        );
        if owner == 0 {
            bail!("Could not open the clipboard helper.");
        }
        let memory = GlobalAlloc(2, data.len() * 2);
        if memory.is_null() {
            DestroyWindow(owner);
            bail!("Could not allocate the share link.");
        }
        let target = GlobalLock(memory);
        if target.is_null() {
            GlobalFree(memory);
            DestroyWindow(owner);
            bail!("Could not prepare the share link.");
        }
        std::ptr::copy_nonoverlapping(data.as_ptr(), target.cast::<u16>(), data.len());
        GlobalUnlock(memory);
        if OpenClipboard(owner) == 0 {
            GlobalFree(memory);
            DestroyWindow(owner);
            bail!("Clipboard is busy. Try Copy link again.");
        }
        let success = EmptyClipboard() != 0 && !SetClipboardData(13, memory).is_null();
        CloseClipboard();
        DestroyWindow(owner);
        if !success {
            GlobalFree(memory);
            bail!("Could not copy the share link.");
        }
    }
    Ok(())
}

#[cfg(not(windows))]
pub fn copy(_text: &str) -> Result<()> {
    bail!("Clipboard copying is available in the Windows build. Select the link to copy it.")
}
