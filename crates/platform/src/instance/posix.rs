//! POSIX implementation: `flock(LOCK_EX | LOCK_NB)` on a per-user lock file.
//!
//! The lock lives in the kernel, tied to the open file description: it is
//! released when the process dies (crash included), so there is no stale-lock
//! window to reason about. The file name carries the user id — the config
//! directory is per user, so one user's editor must not block another's.

use std::fs::{File, OpenOptions};
use std::os::unix::io::AsRawFd;

/// Holder of the flock. Closing the file (on drop) releases the lock; `None` is
/// the "lock file could not be created" degradation (see [`acquire`]).
pub(super) struct Guard {
    _file: Option<File>,
}

pub(super) fn acquire(name: &str) -> Option<Guard> {
    // SAFETY: getuid has no preconditions and cannot fail.
    let uid = unsafe { libc::getuid() };
    let path = std::env::temp_dir().join(format!("{name}-{uid}.lock"));
    let file = match OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
    {
        Ok(f) => f,
        // No temp directory / no permission: never block startup over it. The
        // editor then runs without single-instance protection, which is the
        // pre-existing behaviour.
        Err(_) => return Some(Guard { _file: None }),
    };
    // SAFETY: the fd stays valid as long as `file` lives, and flock only
    // touches kernel state for that open file description.
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc != 0 {
        // EWOULDBLOCK: another instance holds the lock.
        return None;
    }
    Some(Guard { _file: Some(file) })
}

/// No-op: X11/Wayland window managers raise the window themselves when the
/// application asks (winit's `focus_window`), and there is no "foreground
/// process" permission to hand over.
pub(super) fn yield_foreground() {}
