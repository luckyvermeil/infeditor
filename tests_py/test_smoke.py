"""Smoke: real launch, OS title, window structure, clean exit.

Title and Ctrl+N share one instance (the title assertion's final state is
exactly the new-document pre-state); WM_CLOSE exit and the INF_FIND_SMOKE
smoke hook each have special actions of their own (kill process / extra
env var), so they stay on standalone instances.
"""

from __future__ import annotations


def test_launch_title_then_ctrl_n_bumps_tab_counter(app):
    # initial title is the launch string "InfEditor"; after the first frame
    # the app syncs it to "未命名 - InfEditor"
    assert app.wait_title("未命名", timeout=8), f"unexpected title: {app.title()!r}"
    assert app.title().endswith("- InfEditor"), f"unexpected title: {app.title()!r}"

    app.keys("ctrl+n")
    assert app.wait_title("[2/2]"), f"[2/2] did not appear in the title after Ctrl+N: {app.title()!r}"


def test_quit_via_wm_close_exits_cleanly(app_factory):
    a = app_factory()
    proc = a.proc                     # quit() clears a.proc, so keep the handle
    assert proc is not None
    a.quit()  # WM_CLOSE → event loop returns → rescue session cleaned → process exits
    # `quit()` falls back to kill() on timeout, and a killed process also has a
    # return code -- only 0 proves the WM_CLOSE path was taken. (The earlier
    # `a.proc is None or a.proc.poll() is not None` was true by construction.)
    assert proc.returncode == 0, (
        f"clean WM_CLOSE exit expected, got returncode={proc.returncode} "
        "(killed after timeout?)")


def test_smoke_hook_opens_find_window(app_factory):
    """INF_FIND_SMOKE: pops the find window automatically after ~1.5s → ≥2
    top-level windows visible to the process."""
    a = app_factory(env_extra={"INF_FIND_SMOKE": "1"})
    assert a.wait_windows(2, timeout=10), "the smoke hook did not pop up the find window"
    a.screenshot("smoke_find_popup")
