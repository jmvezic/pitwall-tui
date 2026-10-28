//! Keeps the display awake while there's something to watch, the way a
//! video player does: a macOS power assertion that stops the display
//! sleeping and the screensaver starting. Released when no longer wanted,
//! and by macOS when the process exits. Elsewhere it does nothing.

#[derive(Debug, Default)]
pub struct Awake {
    #[cfg(target_os = "macos")]
    id: Option<u32>,
}

impl Awake {
    /// Takes or releases the assertion; cheap to call every tick.
    pub fn set(&mut self, on: bool) {
        #[cfg(target_os = "macos")]
        match (on, self.id) {
            (true, None) => self.id = macos::create("pitwall: following a session"),
            (false, Some(id)) => {
                macos::release(id);
                self.id = None;
            }
            _ => {}
        }
        #[cfg(not(target_os = "macos"))]
        let _ = on;
    }
}

impl Drop for Awake {
    fn drop(&mut self) {
        self.set(false);
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use std::ffi::{CString, c_char, c_void};

    type CFStringRef = *const c_void;

    const UTF8: u32 = 0x0800_0100;
    /// `kIOPMAssertionLevelOn`.
    const LEVEL_ON: u32 = 255;

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFStringCreateWithCString(
            alloc: *const c_void,
            s: *const c_char,
            encoding: u32,
        ) -> CFStringRef;
        fn CFRelease(cf: *const c_void);
    }

    #[link(name = "IOKit", kind = "framework")]
    unsafe extern "C" {
        fn IOPMAssertionCreateWithName(
            kind: CFStringRef,
            level: u32,
            name: CFStringRef,
            id: *mut u32,
        ) -> i32;
        fn IOPMAssertionRelease(id: u32) -> i32;
    }

    fn cf_string(s: &str) -> Option<CFStringRef> {
        let c = CString::new(s).ok()?;
        // SAFETY: `c` is a valid NUL-terminated string for the call.
        let cf = unsafe { CFStringCreateWithCString(std::ptr::null(), c.as_ptr(), UTF8) };
        (!cf.is_null()).then_some(cf)
    }

    /// `PreventUserIdleDisplaySleep` under `name`, as shown by
    /// `pmset -g assertions`.
    pub fn create(name: &str) -> Option<u32> {
        let kind = cf_string("PreventUserIdleDisplaySleep")?;
        let Some(label) = cf_string(name) else {
            // SAFETY: `kind` was created above and is released once.
            unsafe { CFRelease(kind) };
            return None;
        };
        let mut id = 0;
        // SAFETY: both strings are live CFStrings and `id` is writable;
        // IOKit copies what it keeps, so they're released after.
        let res = unsafe {
            let res = IOPMAssertionCreateWithName(kind, LEVEL_ON, label, &mut id);
            CFRelease(kind);
            CFRelease(label);
            res
        };
        if res != 0 {
            tracing::warn!("keeping the display awake failed: IOReturn {res:#x}");
            return None;
        }
        Some(id)
    }

    pub fn release(id: u32) {
        // SAFETY: `id` came from IOPMAssertionCreateWithName and is
        // released once.
        unsafe { IOPMAssertionRelease(id) };
    }
}
