//! Windows implementation: a named mutex in the `Local\` namespace.
//!
//! `Local\` makes the name per logon session — Fast User Switching therefore
//! gives each user their own instance, matching the per-user config directory.
//! The mutex object is destroyed together with the last handle, so a crashed
//! editor leaves no stale lock behind (unlike a file-based lock that would need
//! staleness detection).

use std::ptr;

use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, HANDLE};
use windows_sys::Win32::System::Threading::CreateMutexW;
use windows_sys::Win32::UI::WindowsAndMessaging::{AllowSetForegroundWindow, ASFW_ANY};

/// Windows' SetForegroundWindow refuses to raise a window unless the caller is
/// allowed to: the process that holds the foreground has to hand the right over.
///
/// This is exactly the second instance's situation — the user just double-clicked
/// it, so it *is* the foreground process, and it is about to exit. Calling this
/// lets the running instance raise its own window instead of only flashing the
/// taskbar button.
pub(super) fn yield_foreground() {
    unsafe {
        AllowSetForegroundWindow(ASFW_ANY);
    }
}

/// Holder of the named mutex. `HANDLE` is not `Send`, and it never needs to be:
/// the guard stays on the thread that created it (main), for the whole process
/// lifetime.
pub(super) struct Guard {
    handle: HANDLE,
}

impl Drop for Guard {
    fn drop(&mut self) {
        if !self.handle.is_null() {
            unsafe {
                CloseHandle(self.handle);
            }
        }
    }
}

/// `Some(guard)` = we own the lock (now or by degrading); `None` = another
/// process owns it.
pub(super) fn acquire(name: &str) -> Option<Guard> {
    let wide: Vec<u16> = format!("Local\\{name}")
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    // bInitialOwner = TRUE: if the name is free we own it immediately; if it
    // exists we get a handle to the *other* instance's mutex and must not keep
    // it.
    let handle = unsafe { CreateMutexW(ptr::null(), 1, wide.as_ptr()) };
    if handle.is_null() {
        // Cannot create a mutex at all: treat as primary rather than refusing
        // to start. (Recorded nowhere on purpose — this is a "should not
        // happen" OS failure, and the recovery journal's multi-instance guard
        // still keeps the two processes from wiping each other's session.)
        return Some(Guard { handle });
    }
    if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        unsafe {
            CloseHandle(handle);
        }
        return None;
    }
    Some(Guard { handle })
}
