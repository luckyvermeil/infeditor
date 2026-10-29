//! Single-instance guard: "one editor per user session".
//!
//! Two editors running as the same user fight over the same per-user state —
//! the crash-recovery journal (`%APPDATA%\InfEditor\session\`, see
//! `ui::recovery`), the settings file, the macro library. The recovery journal
//! is the visible symptom: the second instance starts before it has written a
//! journal of its own, finds the first one's, and offers to *restore the
//! previous session* — for a session that is in fact still running.
//!
//! So the first process takes a **process-lifetime lock**, and every later
//! process steps aside. What "steps aside" means is the caller's decision:
//! `infeditor`'s `main` hands its command line over to the running instance and
//! exits (see `ui::single`).
//!
//! Per platform:
//! * **Windows** — a named mutex (`CreateMutexW`) in the `Local\` namespace: per
//!   logon session (a fast-user-switched session gets its own instance), and the
//!   object dies with the last handle, so a crash leaves nothing to clean up.
//! * **POSIX** — `flock(LOCK_EX | LOCK_NB)` on a lock file under the temp
//!   directory; the kernel releases it when the process dies, so again no stale
//!   lock. (A pid file would need staleness detection, which is precisely the
//!   racy part.)
//!
//! Failure policy: a lock that *cannot* be created (no permission, an API error)
//! counts as **primary** — a broken lock must never keep the editor from
//! starting. Only a positively detected other holder yields
//! [`Acquire::AlreadyRunning`].

#[cfg(windows)]
mod win32;
#[cfg(windows)]
use win32 as imp;

#[cfg(not(windows))]
mod posix;
#[cfg(not(windows))]
use posix as imp;

/// Result of [`acquire`].
pub enum Acquire {
    /// This process is the one and only instance; the lock is held until the
    /// returned guard is dropped (i.e. until the process exits).
    Primary(Guard),
    /// Another process of the same application already holds the lock. The
    /// caller should hand over whatever it was asked to open and exit.
    AlreadyRunning,
}

/// Held for the lifetime of the primary instance; dropping it releases the
/// lock. Deliberately not `Clone` — one owner, one lock.
///
/// The inner guard is only ever *held*: its whole job is to release on drop, so
/// reading it would be pointless (hence the leading underscore).
pub struct Guard {
    _inner: imp::Guard,
}

impl Guard {
    /// Release the lock explicitly (same as dropping it; provided so `main` can
    /// express "the editor is gone, others may start" at the point where that
    /// becomes true instead of at the end of a scope).
    pub fn release(self) {
        drop(self);
    }
}

/// Try to become the one instance of `name` (an ASCII identifier; "InfEditor"-style,
/// no path separators).
///
/// The lock is scoped to the user session on Windows and to the user id in the
/// lock file name on POSIX, so two different users can each run their own
/// instance — which is what the per-user state in the config directory implies.
pub fn acquire(name: &str) -> Acquire {
    match imp::acquire(name) {
        Some(inner) => Acquire::Primary(Guard { _inner: inner }),
        None => Acquire::AlreadyRunning,
    }
}

/// Called by the instance that is **about to exit** because another one holds
/// the lock: hand the foreground right over, so the running instance can raise
/// its window (Windows otherwise only flashes the taskbar button; POSIX has no
/// such restriction and this is a no-op).
pub fn yield_foreground() {
    imp::yield_foreground();
}

#[cfg(test)]
mod tests {
    use super::{acquire, Acquire};

    /// Second call sees the first holder; after releasing, the name is free
    /// again.
    ///
    /// The name carries the pid so a real editor running on the machine is
    /// never disturbed and parallel test processes cannot collide.
    #[test]
    fn second_acquire_reports_already_running_until_released() {
        let name = format!("InfEditorTest-{}", std::process::id());
        let first = match acquire(&name) {
            Acquire::Primary(g) => g,
            Acquire::AlreadyRunning => panic!("the first acquire must win"),
        };
        assert!(
            matches!(acquire(&name), Acquire::AlreadyRunning),
            "a second acquire must be turned away"
        );
        first.release();
        assert!(
            matches!(acquire(&name), Acquire::Primary(_)),
            "releasing must free the name for the next process"
        );
    }
}
