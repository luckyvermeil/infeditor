"""Title bar window buttons: geometry calibration, maximize/restore,
minimize, close.

The self-drawn title bar hides the buttons from Win32 entirely, but the
buttons' **effects** are OS state (`IsZoomed` / `IsIconic` / process
alive), so assertions never touch pixels. Geometry constants live in
`src/ui/state/mod.rs` (`WINBTN_W = 46`, `TITLEBAR_H = 36`); hit conversion
is in `infed.driver.window_button`.

Right to left the three buttons are close / maximize / minimize (the DOM
order in `components/titlebar.rs` is Min, Max, Close, laid out flush
right).

Calibration and maximize/minimize merge into one test (any DPI/coordinate
space error fails the very first click, and the failure message names which
button); the close button ends the process and must be standalone.
"""

from __future__ import annotations

import time


def test_window_buttons_hit_maximize_and_minimize(app):
    a = app
    # calibration self-check: the DPI scale must be in a sane range or every
    # click below is firing blind
    s = a.dpi_scale()
    assert 0.5 < s < 4.0, f"unexpected DPI scale: {s}"
    assert not a.is_maximized(), "should start as a normal window"

    # maximize → restore (a real SW_RESTORE, not a no-op)
    a.window_button("max")
    assert a.wait_maximized(True), "maximize button not hit"
    a.window_button("max")
    assert a.wait_maximized(False), "clicking again should restore"
    assert not a.is_maximized()

    # minimize → restore
    a.window_button("min")
    deadline = time.time() + 5.0
    while time.time() < deadline and not a.is_minimized():
        time.sleep(0.1)
    assert a.is_minimized(), "minimize button not hit"
    a.restore_window()
    assert not a.is_minimized()


def test_close_button_exits_clean_app(app):
    """With a clean document, clicking × should exit directly (must not pop
    a confirmation dialog and hang)."""
    a = app
    assert not a.is_dirty()
    a.window_button("close")
    a.proc.wait(timeout=15)
    # wait() already guarantees the process ended; the exit code is what
    # distinguishes "the window closed and main() returned" from a crash.
    assert a.proc.returncode == 0, f"unclean exit: returncode={a.proc.returncode}"
    a.main_hwnd = None          # already exited; teardown must not screenshot/close
