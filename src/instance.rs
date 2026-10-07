//! One Windows player per account directory, independent of tray visibility.

#[cfg(windows)]
mod imp {
    use std::ffi::c_void;
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use std::time::{Duration, Instant};

    use anyhow::{Context, Result, bail};

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn CreateMutexW(security: *const c_void, owned: i32, name: *const u16) -> isize;
        fn CreateEventW(security: *const c_void, manual: i32, set: i32, name: *const u16) -> isize;
        fn SetEvent(event: isize) -> i32;
        fn WaitForSingleObject(handle: isize, timeout: u32) -> u32;
        fn GetLastError() -> u32;
        fn GetCurrentProcessId() -> u32;
        fn CreateFileMappingW(
            file: isize,
            security: *const c_void,
            protection: u32,
            high: u32,
            low: u32,
            name: *const u16,
        ) -> isize;
        fn OpenFileMappingW(access: u32, inherit: i32, name: *const u16) -> isize;
        fn MapViewOfFile(
            mapping: isize,
            access: u32,
            high: u32,
            low: u32,
            bytes: usize,
        ) -> *mut c_void;
        fn UnmapViewOfFile(view: *const c_void) -> i32;
    }
    #[link(name = "user32")]
    unsafe extern "system" {
        fn AllowSetForegroundWindow(pid: u32) -> i32;
    }

    pub struct Instance {
        _identity: OwnedHandle,
        activation: OwnedHandle,
        // Release ownership last, after the activation and identity handles.
        _owner: OwnedHandle,
    }

    fn wide(value: &str) -> Vec<u16> {
        value.encode_utf16().chain(Some(0)).collect()
    }

    fn owned(handle: isize) -> Result<OwnedHandle> {
        if handle == 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(unsafe { OwnedHandle::from_raw_handle(handle as *mut c_void) })
    }

    impl Instance {
        pub fn acquire() -> Result<Option<Self>> {
            let path = crate::config::dir()?.to_string_lossy().to_lowercase();
            let key = path.bytes().fold(0xcbf29ce484222325u64, |hash, byte| {
                (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3)
            });
            Self::acquire_named(&format!("Local\\MTUI-{key:016x}"))
        }

        fn acquire_named(name: &str) -> Result<Option<Self>> {
            let name_wide = wide(name);
            let handle = unsafe { CreateMutexW(std::ptr::null(), 0, name_wide.as_ptr()) };
            let existing = unsafe { GetLastError() } == 183;
            let owner = owned(handle).context("could not reserve the player session")?;
            let event_name = wide(&format!("{name}-activate"));
            let activation =
                owned(unsafe { CreateEventW(std::ptr::null(), 0, 0, event_name.as_ptr()) })?;
            let identity_name = wide(&format!("{name}-pid"));
            if existing {
                // A launch can arrive before the first process finishes startup.
                let deadline = Instant::now() + Duration::from_secs(3);
                loop {
                    let handle = unsafe { OpenFileMappingW(4, 0, identity_name.as_ptr()) };
                    if handle != 0 {
                        let identity = owned(handle)?;
                        let view =
                            unsafe { MapViewOfFile(identity.as_raw_handle() as isize, 4, 0, 0, 4) };
                        if !view.is_null() {
                            let pid = unsafe { std::ptr::read_volatile(view.cast::<u32>()) };
                            unsafe { UnmapViewOfFile(view) };
                            if pid != 0 {
                                unsafe { AllowSetForegroundWindow(pid) };
                                if unsafe { SetEvent(activation.as_raw_handle() as isize) } == 0 {
                                    return Err(std::io::Error::last_os_error().into());
                                }
                                return Ok(None);
                            }
                        }
                    }
                    if Instant::now() >= deadline {
                        bail!(
                            "the running MTUI session did not become ready; try opening it again"
                        );
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
            let identity = owned(unsafe {
                CreateFileMappingW(-1, std::ptr::null(), 4, 0, 4, identity_name.as_ptr())
            })?;
            let view = unsafe { MapViewOfFile(identity.as_raw_handle() as isize, 2, 0, 0, 4) };
            if view.is_null() {
                return Err(std::io::Error::last_os_error().into());
            }
            unsafe {
                std::ptr::write_volatile(view.cast::<u32>(), GetCurrentProcessId());
                UnmapViewOfFile(view);
            }
            Ok(Some(Self {
                _identity: identity,
                activation,
                _owner: owner,
            }))
        }

        pub fn take_activation(&self) -> bool {
            unsafe { WaitForSingleObject(self.activation.as_raw_handle() as isize, 0) == 0 }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        #[test]
        fn relaunch_signals_the_owner_and_quitting_releases_the_session() {
            let name = format!(
                "Local\\MTUI-instance-test-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            );
            let first = Instance::acquire_named(&name).unwrap().unwrap();
            assert!(!first.take_activation());
            assert!(Instance::acquire_named(&name).unwrap().is_none());
            assert!(first.take_activation());
            assert!(!first.take_activation());
            drop(first);
            assert!(Instance::acquire_named(&name).unwrap().is_some());
        }
    }
}

#[cfg(not(windows))]
mod imp {
    pub struct Instance;
    impl Instance {
        pub fn acquire() -> anyhow::Result<Option<Self>> {
            Ok(Some(Self))
        }
        pub fn take_activation(&self) -> bool {
            false
        }
    }
}

pub use imp::Instance;
