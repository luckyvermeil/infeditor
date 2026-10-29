//! System touch keyboard (TabTip) detection and invocation.
//!
//! The frameless window takes on some of the interactions Windows used to
//! provide, and soft-keyboard avoidance is one of them: when the touch keyboard
//! is invoked on a touchscreen it floats above the app as a system top-level
//! window, **without** changing the app window's size and **without** changing
//! `SPI_GETWORKAREA`. The editor has to detect whether the keyboard is visible
//! and how much screen it occupies, then yield the bottom of the viewport
//! (`EditorState::set_keyboard_inset`).
//!
//! Detection has a throttled cache (about 120ms), so calling it every frame is
//! cheap. When detection fails (the keyboard window cannot be found) it returns
//! `None`, equivalent to "no avoidance"; it never crashes.
//!
//! [`show`] is the reverse half: invite the keyboard out when the user taps into
//! the body text (see that function's doc).

// The public surface is platform-agnostic: on non-Windows "there is no soft
// keyboard" = `None` / no-op, so callers need not write cfg (soft-keyboard
// avoidance naturally degrades to zero inset).

/// The touch keyboard's top edge in screen physical y; returns `None` when
/// invisible / no touch keyboard.
///
/// Same coordinate system as `GetCursorPos`/`SetWindowPos` (physical pixels
/// under per-monitor-v2).
pub fn keyboard_top_screen_px() -> Option<i32> {
    imp::keyboard_top_screen_px()
}

/// Invite the system touch keyboard out (no-op on platforms where unavailable).
pub fn show() {
    imp::show()
}

/// Non-Windows: no touch keyboard detection — "keyboard invisible" (`None`) and
/// "cannot invite the keyboard" (no-op) are the whole of the semantics; callers
/// need no branching.
#[cfg(not(windows))]
mod imp {
    pub fn keyboard_top_screen_px() -> Option<i32> {
        None
    }

    pub fn show() {}
}

#[cfg(windows)]
mod imp {
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    /// The minimum interval between two real probes. The soft keyboard's
    /// show/hide is a hundred-millisecond-scale event; 120ms keeps up with the
    /// hand without calling `FindWindowW` every frame.
    const POLL: Duration = Duration::from_millis(120);

    /// The touch keyboard's main window class name (TabTip.exe). Historically
    /// stable as `IPTIP_Main_Window`; a second class name is kept in case a
    /// future version renames it and detection fails (failing = degrading to no
    /// avoidance, not a crash).
    const CLASS_NAMES: [&str; 2] = ["IPTIP_Main_Window", "TabTip"];

    /// The throttle cache: `(last probe instant, result)`.
    static LAST: Mutex<Option<(Instant, Option<i32>)>> = Mutex::new(None);

    /// The touch keyboard's top edge in screen physical y; returns `None` when
    /// invisible / no touch keyboard.
    ///
    /// Same coordinate system as `GetCursorPos`/`SetWindowPos` (physical pixels
    /// under per-monitor-v2).
    pub fn keyboard_top_screen_px() -> Option<i32> {
        {
            let guard = LAST.lock().unwrap_or_else(|e| e.into_inner());
            if let Some((t, v)) = *guard {
                if t.elapsed() < POLL {
                    return v;
                }
            }
        }
        let v = probe();
        *LAST.lock().unwrap_or_else(|e| e.into_inner()) = Some((Instant::now(), v));
        v
    }

    /// Do one real probe: find the visible touch keyboard's main window and
    /// return the top of its rect.
    fn probe() -> Option<i32> {
        use windows_sys::Win32::Foundation::RECT;
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            FindWindowW, GetWindowRect, IsWindowVisible,
        };
        for cls in CLASS_NAMES {
            let wide: Vec<u16> = cls.encode_utf16().chain(std::iter::once(0)).collect();
            let hwnd = unsafe { FindWindowW(wide.as_ptr(), std::ptr::null()) };
            if hwnd.is_null() {
                continue;
            }
            if unsafe { IsWindowVisible(hwnd) } == 0 {
                continue;
            }
            let mut r: RECT = unsafe { std::mem::zeroed() };
            if unsafe { GetWindowRect(hwnd, &mut r) } != 0 {
                return Some(r.top);
            }
        }
        None
    }

    /// Whether the touch keyboard is currently visible (bypasses the throttle
    /// cache).
    fn visible_now() -> bool {
        probe().is_some()
    }

    /// The instant of the last [`show`]: do not repeatedly spawn the process on
    /// rapid taps.
    static LAST_SHOW: Mutex<Option<Instant>> = Mutex::new(None);

    /// The minimum interval between two invocations.
    const SHOW_POLL: Duration = Duration::from_secs(1);

    /// Invite the system touch keyboard out.
    ///
    /// Windows only auto-pops the keyboard when **focus changes to a text
    /// control**; this editor's textarea has held focus since startup (blitz's
    /// `autofocus`), so tapping the body text afterwards produces no focus
    /// change, and touchscreen users would never see the keyboard — "tapping
    /// does nothing". Here TabTip is explicitly launched:
    ///
    /// * already visible → no-op (no toggle: that would close the keyboard the
    ///   user opened themselves);
    /// * called again within 1 second → no-op (rapid taps do not respawn the
    ///   process);
    /// * `TabTip.exe` not found → fall back to the classic on-screen keyboard
    ///   `osk.exe`; if neither exists, give up silently (normal on desktop
    ///   machines without touch components, not an error).
    pub fn show() {
        if visible_now() {
            return;
        }
        {
            let mut last = LAST_SHOW.lock().unwrap_or_else(|e| e.into_inner());
            if last.is_some_and(|t| t.elapsed() < SHOW_POLL) {
                return;
            }
            *last = Some(Instant::now());
        }
        let Some(exe) = tabtip_path().or_else(osk_path) else {
            return;
        };
        // Failing to launch is not an error: the touch keyboard is an optional
        // component; without it there is simply no soft keyboard.
        let _ = std::process::Command::new(exe).spawn();
    }

    /// `%CommonProgramFiles%\microsoft shared\ink\TabTip.exe`
    fn tabtip_path() -> Option<std::path::PathBuf> {
        let base = std::env::var_os("CommonProgramFiles")?;
        let p = std::path::PathBuf::from(base)
            .join("microsoft shared")
            .join("ink")
            .join("TabTip.exe");
        p.is_file().then_some(p)
    }

    /// `%SystemRoot%\System32\osk.exe` (the classic on-screen keyboard,
    /// backstop)
    fn osk_path() -> Option<std::path::PathBuf> {
        let base = std::env::var_os("SystemRoot")?;
        let p = std::path::PathBuf::from(base)
            .join("System32")
            .join("osk.exe");
        p.is_file().then_some(p)
    }
}

#[cfg(test)]
mod tests {
    // The probe depends on a real system touch keyboard, which unit tests cannot
    // reliably construct; here we only guarantee "calling does not panic, and
    // two calls within the throttle window return the same result", to avoid a
    // cache-logic mistake causing per-frame jitter.
    #[test]
    fn probe_is_total_and_cached() {
        let a = super::keyboard_top_screen_px();
        let b = super::keyboard_top_screen_px();
        assert_eq!(a, b, "two probes within the throttle window should agree");
    }
}
