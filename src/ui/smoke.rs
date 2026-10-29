//! GUI smoke-test helpers (a unified implementation of the static atom +
//! arm/due function pair; each window only passes its environment variable
//! name and delay).
//!
//! With `INF_*_SMOKE=1` at application startup, some UI action fires after a
//! delay, for pywinauto screenshot scripts to consume: each window passes its
//! own environment variable name and delay in `arm_from_env()`, and asks
//! `due()` in the frame loop, which returns true when the time is up (and
//! resets, firing only once).

use std::sync::atomic::{AtomicU64, Ordering};

pub struct Smoke {
    due_at: AtomicU64,
}

impl Default for Smoke {
    fn default() -> Self {
        Self::new()
    }
}

impl Smoke {
    pub const fn new() -> Self {
        Smoke {
            due_at: AtomicU64::new(u64::MAX),
        }
    }

    /// If the environment variable `env` exists, fire once after `delay_ms`.
    pub fn arm_from_env(&self, env: &str, delay_ms: u64) {
        if std::env::var(env).is_ok() {
            self.due_at.store(now_ms() + delay_ms, Ordering::Relaxed);
        }
    }

    /// Returns true when the time is up (and resets, firing only once).
    pub fn due(&self) -> bool {
        if self.pending() {
            self.due_at.store(u64::MAX, Ordering::Relaxed);
            true
        } else {
            false
        }
    }

    /// Non-destructive probe: returns true when the time is up but **does not
    /// reset**.
    ///
    /// The probe branch of the idle frame hook (`needs_idle_poll`) must use
    /// this instead of [`Self::due`]: `due()` resets as soon as it returns
    /// true (one-shot semantics); if the probe consumes it first, the
    /// correction branch asking `due()` again would always be false — the
    /// trigger silently lost, exactly a variant of the iron rule "the probe
    /// and correction early-exit branches must be identical verbatim".
    pub fn pending(&self) -> bool {
        let d = self.due_at.load(Ordering::Relaxed);
        d != u64::MAX && now_ms() >= d
    }
}

/// Whether any smoke environment variable is armed (true only in a test
/// process; always false in production).
///
/// The companion piece to the render-loop fire-extinguishing fix (zero idle
/// frames): smoke timing relies on the frame hook's check, and with no frames
/// while the window is idle the deadline would never be reached. When armed,
/// the BlinkTick thread is borrowed as a guaranteed frame supplier (the
/// `want_blink` gate in editor.rs gained this item), sharing the same wake
/// source as the caret blink.
pub fn any_env_armed() -> bool {
    std::env::var("INF_FIND_SMOKE").is_ok()
        || std::env::var("INF_PREFS_SMOKE").is_ok()
        || std::env::var("INF_NUM_SMOKE").is_ok()
}

fn now_ms() -> u64 {
    // When the system time is before 1970 (dead CMOS battery, user set the
    // clock by hand) `duration_since` returns Err; this is only a relative
    // moment for smoke timing, so falling back to 0 is fine — it must never
    // panic.
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pending_does_not_consume_due() {
        let s = Smoke::new();
        assert!(!s.pending(), "pending is always false when not armed");
        assert!(!s.due());

        // Dial the due time directly into the past (bypassing the env
        // dependency)
        s.due_at.store(now_ms() - 1, Ordering::Relaxed);
        assert!(s.pending());
        assert!(
            s.pending(),
            "pending is non-destructive: true twice in a row"
        );
        assert!(s.due(), "due consumes the one-shot trigger");
        assert!(!s.pending(), "pending is false again after consumption");
        assert!(!s.due(), "due fires only once");
    }
}
