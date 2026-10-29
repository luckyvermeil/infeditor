"""InfEditor process driver: launch, windows, input, clipboard oracle, screenshots.

Design constraints (matching the Blitz fully self-drawn UI):

* window content is invisible to Win32 (wgpu self-drawing + lazy accesskit tree),
  so the **verification oracle for text state is the clipboard** -- read
  CF_UNICODETEXT after Ctrl+A/Ctrl+C;
* the OS title is the second trusted source (file name / `*` dirty marker /
  `[current/total]` all live in the title);
* all input goes through SendInput (`infed.keys`); the window must be brought to
  the foreground first.
"""

from __future__ import annotations

import atexit
import ctypes
import ctypes.wintypes as wt
import os
import subprocess
import time
from pathlib import Path

from . import clipboard as cb
from . import keys

user32 = ctypes.WinDLL("user32", use_last_error=True)

WM_CLOSE = 0x0010

# VK_MENU is unused on purpose: an ALT tap triggers the app's menu-mnemonic
# session (see grant_foreground).

# Clipboard backup states: `None` = not captured yet, this sentinel = the read
# failed (so there is nothing trustworthy to restore), `str` = the backup.
_CLIPBOARD_UNREADABLE = object()

_SPI_GETFOREGROUNDLOCKTIMEOUT = 0x2000
_SPI_SETFOREGROUNDLOCKTIMEOUT = 0x2001

# Original timeout, captured before the first SetForegroundWindow workaround.
# `None` = not captured yet; `_NO_TIMEOUT` = the read failed, so restoring is
# impossible and we must not invent a value.
_NO_TIMEOUT = object()
_saved_lock_timeout: int | object | None = None


def _disable_foreground_lock() -> None:
    """Zero SPI_SETFOREGROUNDLOCKTIMEOUT once, remembering the old value.

    The setting is machine-wide and survives the test run, so it is restored in
    an `atexit` hook (see [`_restore_foreground_lock`]). Zeroing it once is
    enough: the value only needs to be 0 while SetForegroundWindow is called.
    """
    global _saved_lock_timeout
    if _saved_lock_timeout is None:
        prev = wt.DWORD()
        ok = user32.SystemParametersInfoW(
            _SPI_GETFOREGROUNDLOCKTIMEOUT, 0, ctypes.byref(prev), 0)
        _saved_lock_timeout = prev.value if ok else _NO_TIMEOUT
    user32.SystemParametersInfoW(_SPI_SETFOREGROUNDLOCKTIMEOUT, 0, None, 0)


def _restore_foreground_lock() -> None:
    global _saved_lock_timeout
    if isinstance(_saved_lock_timeout, int):
        # For SPI_SETFOREGROUNDLOCKTIMEOUT the value travels in uiParam.
        user32.SystemParametersInfoW(
            _SPI_SETFOREGROUNDLOCKTIMEOUT, _saved_lock_timeout, None, 0)
    _saved_lock_timeout = None


atexit.register(_restore_foreground_lock)


def grant_foreground(hwnd: int) -> None:
    """Push `hwnd` to the foreground -- bypassing Windows' foreground activation lock.

    Windows only lets "the process that most recently received input events" call
    SetForegroundWindow successfully; after a session re-login the automation
    process no longer holds that right by default (measured 2026-10-14: even
    pywinauto set_focus against Notepad was rejected with
    `(0, 'SetForegroundWindow')`). The combination that actually works =
    **AttachThreadInput into the current foreground thread's input queue + zeroing
    SPI_SETFOREGROUNDLOCKTIMEOUT**, then SetForegroundWindow.

    ⚠️ Do not add the "ALT tap" (textbook workaround): this app implements **bare
    Alt** as a menu-mnemonic session (menu.alt); one ALT injection makes all
    subsequent letter keys get swallowed by mnemonic routing, and CSV cell clicks
    do not clear the session either -- symptoms look like "all typing lost".

    Side note: pywinauto set_focus()'s AttachThreadInput+SetFocus is the key to
    making a winit window accept subsequent SendInput; this function only solves
    the "system rejects the call" layer.
    """
    fg = user32.GetForegroundWindow()
    fgt = user32.GetWindowThreadProcessId(fg, None) if fg else 0
    tgt = user32.GetWindowThreadProcessId(hwnd, None)
    attached = bool(fgt) and fgt != tgt and bool(user32.AttachThreadInput(fgt, tgt, True))
    _disable_foreground_lock()
    try:
        if not user32.SetForegroundWindow(hwnd):
            time.sleep(0.1)
            user32.SetForegroundWindow(hwnd)
    finally:
        if attached:
            user32.AttachThreadInput(fgt, tgt, False)


def windows_of_pid(pid: int) -> list[int]:
    """Enumerate visible top-level windows belonging to `pid` (for finding popups etc.)."""
    found: list[int] = []

    @ctypes.WINFUNCTYPE(wt.BOOL, wt.HWND, wt.LPARAM)
    def _cb(hwnd, _l):
        proc = ctypes.c_ulong()
        user32.GetWindowThreadProcessId(hwnd, ctypes.byref(proc))
        if proc.value == pid and user32.IsWindowVisible(hwnd):
            found.append(hwnd)
        return True

    user32.EnumWindows(_cb, 0)
    return found


def window_title(hwnd: int) -> str:
    buf = ctypes.create_unicode_buffer(512)
    user32.GetWindowTextW(hwnd, buf, 512)
    return buf.value


class App:
    """A running InfEditor instance."""

    def __init__(self, exe: str | Path, config_dir: Path, work_dir: Path,
                 files: list[Path] | None = None, env_extra: dict | None = None,
                 screenshot_dir: Path | None = None):
        self.exe = str(exe)
        self.config_dir = config_dir
        self.work_dir = work_dir
        self.files = [Path(f) for f in (files or [])]
        self.env_extra = env_extra or {}
        self.screenshot_dir = screenshot_dir or Path("out")
        self.proc: subprocess.Popen | None = None
        self.main_hwnd: int | None = None
        self._clipboard_backup: str | object | None = None
        self._win_wrapper = None

    # ---- Lifecycle -----------------------------------------------------------

    def start(self, timeout: float = 30.0, settle: float = 0.8,
              handle_recovery: bool = True) -> None:
        env = dict(os.environ)
        env["INFEDITOR_CONFIG_DIR"] = str(self.config_dir)
        # Disable IME (app-side switch, see shell.rs enable_ime): a real Chinese IME
        # intercepts SendInput letters into its composition string and consumes
        # Enter for composition confirmation, wiping out editing tests entirely;
        # Ime::Commit cannot be injected externally (see the type_text dual-channel
        # note). IME semantics themselves are covered by the headless unit test
        # (csv_edit_preedit_lifecycle).
        env.setdefault("INFEDITOR_IME", "0")
        # Pin the UI language: every assertion in this suite spells Chinese
        # labels ("未命名", "保存", "已切换至文档视图", ...). With a fresh config
        # dir the app otherwise falls back to the host's system language, so an
        # English Windows would fail dozens of tests for no product reason.
        # `App.start`'s env only outranks the system guess (settings.json wins),
        # and each test starts with a fresh config dir -- deterministic.
        env.setdefault("INFEDITOR_LANG", "zh-CN")
        # Self-drawn control geometry probe (dialog button positions): see dialog_buttons().
        env.setdefault("INF_UI_PROBE", "1")
        env.update(self.env_extra)
        # Truncate shell.log on every run: probe lines ([dialog] / [frame] /
        # [timing]) are appended under config_dir, so when reusing a directory the
        # previous run's "last [dialog]" skews this run's button geometry/labels
        # (especially true for recovery-prompt probing).
        try:
            (Path(self.config_dir) / "shell.log").unlink(missing_ok=True)
        except OSError:
            pass
        args = [self.exe] + [str(f) for f in self.files]
        self.proc = subprocess.Popen(args, env=env, cwd=str(self.work_dir))
        deadline = time.time() + timeout
        while time.time() < deadline:
            if self.proc.poll() is not None:
                raise RuntimeError(f"InfEditor exited prematurely: code={self.proc.returncode}")
            # Main window criteria: visible, has a title (containing "InfEditor"),
            # large enough area. winit first creates a 15x15 message auxiliary
            # window which must never be taken as the main window.
            cand = [
                h for h in windows_of_pid(self.proc.pid)
                if "InfEditor" in window_title(h) and _area(h) > 40000
            ]
            if cand:
                self.main_hwnd = max(cand, key=_area)
                # Keep the window above everything else: the suite clicks by
                # geometry and reads the screen, so any always-on-top neighbour
                # (chat client, tooltip, the previous run's window) silently
                # absorbs both. Done before the settle sleep so the first frame
                # and the recovery prompt are already on top.
                self.set_topmost(True)
                time.sleep(settle)   # first-frame layout/recovery prompt ready; avoids swallowed clicks
                if handle_recovery:
                    # If the previous instance was killed (equivalent to an abnormal
                    # exit) and left a dirty-document snapshot, this launch pops a
                    # "restore session" modal -- it swallows all subsequent clicks,
                    # showing up as fake failures like "the maximize button does
                    # nothing". Except for the rescue test itself
                    # (handle_recovery=False), always auto-dismiss.
                    self.dismiss_recovery_if_prompted()
                return
            time.sleep(0.2)
        raise TimeoutError(f"InfEditor main window did not appear within {timeout}s")

    # ---- Window stacking -----------------------------------------------------

    def set_topmost(self, on: bool = True, hwnd: int | None = None) -> None:
        """Pin a window above every other window (`on=False` undoes it).

        "Foreground" is not enough for this suite: it clicks at computed points
        and reads screen grabs, and Windows happily puts a topmost window of
        another process over ours — the clicks then land in that window and the
        screenshot shows its pixels, i.e. every oracle reads a different app.
        Topmost is the one stacking property a test can actually own; keyboard
        focus still needs `focus_soft()`.
        """
        HWND_TOPMOST, HWND_NOTOPMOST = -1, -2
        SWP_NOSIZE, SWP_NOMOVE, SWP_NOACTIVATE = 0x0001, 0x0002, 0x0010
        user32.SetWindowPos(
            wt.HWND(hwnd or self.main_hwnd),
            wt.HWND(HWND_TOPMOST if on else HWND_NOTOPMOST),
            0, 0, 0, 0, SWP_NOSIZE | SWP_NOMOVE | SWP_NOACTIVATE)

    # ---- Abnormal-exit recovery prompt ----------------------------------------

    RECOVERY_PROMPT = ["恢复", "放弃快照"]
    RECOVERY_CONFIRM = ["取消", "确认放弃"]

    def _dialog_seen(self) -> bool:
        """Whether the probe has written at least one `[dialog]` record.

        `dialog_labels()==[]` has two meanings: "the probe hasn't written yet"
        (early startup) and "the probe explicitly reported buttons=0 (no dialog)".
        The recovery prompt is mounted when EditorState is constructed (before the
        first frame), so the probe's **first record** is trustworthy -- once a
        record has been seen, empty labels can safely mean "definitely no dialog".
        """
        log = self.config_dir / "shell.log"
        if not log.exists():
            return False
        return any(
            line.startswith("[dialog] ")
            for line in log.read_text(encoding="utf-8", errors="replace").splitlines()
        )

    def dismiss_recovery_if_prompted(self, timeout: float = 1.5) -> bool:
        """If a "restore session" prompt appears at startup, dismiss it via the two-step confirmation.

        Returns whether a recovery prompt was actually handled (False = none
        appeared). Only acts when the button labels are exactly the recovery
        prompt's two-step combination; any other dialog is treated as an error and
        raised -- silently clicking away an unrecognized dialog turns tests into
        "looks passing".

        Recovery-semantics tests (test_recovery.py) use
        `start(handle_recovery=False)` to skip auto handling and drive/assert each
        step themselves. The recovery prompt appears with the first frame (settle
        has already waited for it), so only short-poll `timeout` seconds here;
        **probe has written records and labels is empty = definitely no dialog,
        return immediately** -- don't wait out the full timeout: every second of
        delay postpones when tests get the window
        "auto action N seconds after launch" fire during the wait, making
        timing-sensitive tests (Markdown view-switch snapshot comparisons) fail
        in whole batches.
        """
        deadline = time.time() + timeout
        while time.time() < deadline:
            labels = self.dialog_labels()
            if not labels:
                if self._dialog_seen():
                    return False         # probe explicitly reported buttons=0: no dialog
                time.sleep(0.2)          # probe hasn't written its first record yet, wait more
                continue
            if labels[:2] != self.RECOVERY_PROMPT:
                raise RuntimeError(
                    f"unexpected dialog buttons at startup {labels}, "
                    f"not the recovery prompt {self.RECOVERY_PROMPT}"
                )
            self.click_dialog_label("放弃快照")
            self.wait_dialog_labels(self.RECOVERY_CONFIRM, 3.0,
                                    "second confirmation of discard-snapshot")
            self.click_dialog_label("确认放弃")
            return True
        return False

    def wait_dialog_labels(self, expected: list[str], timeout: float,
                           what: str = "dialog") -> None:
        """Poll until the dialog button labels equal `expected` (or raise on timeout).

        The recovery/confirm prompts are mounted after the first frame, and the
        probe writes its `[dialog]` line a beat or two later; must wait before
        asserting, otherwise random false failures under load.
        """
        deadline = time.time() + timeout
        last: list[str] = []
        while time.time() < deadline:
            last = self.dialog_labels()
            if last == expected:
                return
            time.sleep(0.2)
        raise RuntimeError(f"{what} did not appear: expected {expected}, got {last}")


    def quit(self, timeout: float = 10.0, force: bool = False) -> None:
        """Close via WM_CLOSE; kill the process on timeout or when force=True."""
        if self.proc is None:
            return
        if force:
            self.proc.kill()
        elif self.main_hwnd and cb.is_window(self.main_hwnd):
            user32.PostMessageW(wt.HWND(self.main_hwnd), WM_CLOSE, 0, 0)
        try:
            self.proc.wait(timeout=timeout)
        except subprocess.TimeoutExpired:
            self.proc.kill()
            self.proc.wait(timeout=5)
        self.proc = None

    # ---- Windows ---------------------------------------------------------------

    def is_maximized(self) -> bool:
        """Whether the window is maximized (`IsZoomed`)."""
        return bool(user32.IsZoomed(wt.HWND(self.main_hwnd)))

    def is_minimized(self) -> bool:
        """Whether the window is minimized (`IsIconic`)."""
        return bool(user32.IsIconic(wt.HWND(self.main_hwnd)))

    def wait_maximized(self, want: bool, timeout: float = 5.0) -> bool:
        deadline = time.time() + timeout
        while time.time() < deadline:
            if self.is_maximized() == want:
                return True
            time.sleep(0.1)
        return False

    def restore_window(self) -> None:
        """Restore the window from minimized/maximized via `ShowWindow(SW_RESTORE)` (test cleanup)."""
        user32.ShowWindow(wt.HWND(self.main_hwnd), 9)   # SW_RESTORE
        time.sleep(0.4)
        self._win_wrapper = None                        # geometry changed, cache invalid

    def wait_windows(self, count: int, timeout: float = 10.0) -> bool:
        """Wait until the process's visible top-level window count reaches `count` (for popups)."""
        deadline = time.time() + timeout
        while time.time() < deadline:
            if len(windows_of_pid(self.proc.pid)) >= count:
                return True
            time.sleep(0.2)
        return False

    def _thread_of(self, hwnd: int) -> int:
        pid = ctypes.c_ulong()
        return user32.GetWindowThreadProcessId(wt.HWND(hwnd), ctypes.byref(pid))

    def focus_soft(self) -> None:
        """Give keyboard focus to the main window **without mouse events** (protects selection and find focus).

        Measured conclusion (two revisions to get right): winit windows are not in
        the Win32 focus model (GetGUIThreadInfo's hwndActive/hwndFocus are always
        None), so `SetForegroundWindow` sometimes suffices and sometimes doesn't --
        keys get silently dropped. pywinauto's `set_focus()` (AttachThreadInput +
        SetFocus + activation messages) is the only stable way to make winit accept
        subsequent SendInput.
        """
        if not (self.main_hwnd and cb.is_window(self.main_hwnd)):
            raise RuntimeError("main window no longer exists")
        grant_foreground(self.main_hwnd)
        self._wrapper().set_focus()
        time.sleep(0.12)

    def _wrapper(self):
        return self._window_wrapper(self.main_hwnd)

    def _window_wrapper(self, hwnd: int):
        from pywinauto import Desktop
        if hwnd == self.main_hwnd:
            if self._win_wrapper is None:
                self._win_wrapper = Desktop(backend="win32").window(handle=hwnd)
            return self._win_wrapper
        return Desktop(backend="win32").window(handle=hwnd)

    # ---- Accessibility tree ----------------------------------------------------

    def a11y_names(self, max_depth: int = 14, attempts: int = 3) -> list[str]:
        """Every non-empty name in the main window's UIA tree, in tree order.

        States built from **names** are sound (status bar, menu rows, tab titles,
        sidebar rows): accesskit may report a 0x0 rect for a self-drawn node, but
        the name is what those oracles read. Anything needing geometry must not
        trust this tree -- use the layout constants instead.

        The walk is a fresh COM traversal per call, so it is retried: accesskit
        activates lazily and the first walk can fail while the tree materialises.
        """
        from pywinauto import Desktop

        last: Exception | None = None
        for _ in range(attempts):
            try:
                win = Desktop(backend="uia").window(handle=self.main_hwnd).wrapper_object()
                out: list[str] = []

                def rec(ctrl, d=0):
                    try:
                        n = ctrl.element_info.name or ""
                        if n:
                            out.append(n)
                    except Exception:
                        pass
                    if d < max_depth:
                        try:
                            for ch in ctrl.children():
                                rec(ch, d + 1)
                        except Exception:
                            pass

                rec(win)
                return out
            except Exception as e:  # tree not ready yet / COM flakiness
                last = e
                time.sleep(0.3)
        raise TimeoutError(f"UIA tree unavailable: {last!r}")

    def focus_click(self) -> None:
        """Foreground + click the viewport center so keyboard focus truly lands in the document.

        Measured: a winit window can be in a "foreground but no keyboard-focus
        window" state where SendInput keys are silently dropped; a real click
        always produces WM_MOUSEACTIVATE / WM_SETFOCUS and is the only reliable
        way to land focus. Clicking moves the caret (harmless on an empty
        document; on documents with content press Ctrl+End before inserting text).

        Leave 0.3s after the click: measured, keys sent immediately race the
        click's hit processing, producing odd results like "Ctrl+End then type,
        yet the trailing newline disappears".
        """
        self.focus_soft()
        self._raw_click_center()
        time.sleep(0.3)

    # legacy alias
    focus = focus_click

    def title(self) -> str:
        return window_title(self.main_hwnd)

    def wait_title(self, substr: str, timeout: float = 8.0) -> bool:
        deadline = time.time() + timeout
        while time.time() < deadline:
            if substr in self.title():
                return True
            time.sleep(0.15)
        return False

    # ---- Input -----------------------------------------------------------------

    def type_text(self, text: str, click: bool = True, focus: bool = True) -> None:
        """Type `text` into the currently focused control; `click=False` for cases
        where the app already owns focus (e.g. the find box).

        Dual channel (measured: winit only accepts virtual keys, see
        `infed.keys.type_text`):

        * ASCII -- injected key by key, Shift auto-supplied by `keys`, closest to
          real typing;
        * non-ASCII (Chinese / emoji / fullwidth punctuation) -- whole runs go via
          clipboard + Ctrl+V. IME commit events (`Ime::Commit`) cannot be injected
          externally, while paste uses the app's own insert path -- the standard
          GUI-automation substitute for IME input.

        With `focus=False` focus is left untouched -- popups (find/replace) are
        separate top-level windows; stealing focus would pour input into the main
        document.
        """
        if focus:
            if click:
                self.focus_click()
            else:
                self.focus_soft()
        ascii_run: list[str] = []

        def flush_ascii():
            if ascii_run:
                keys.type_text("".join(ascii_run))
                ascii_run.clear()

        i = 0
        while i < len(text):
            ch = text[i]
            if ch == "\n":
                flush_ascii()
                keys.press_combo("enter")
                i += 1
            elif ch == "\t":
                flush_ascii()
                keys.press_combo("tab")
                i += 1
            elif ord(ch) < 128:
                ascii_run.append(ch)
                i += 1
            else:
                flush_ascii()
                j = i
                while j < len(text) and ord(text[j]) >= 128:
                    j += 1
                # This channel overwrites the user's clipboard; capture it on
                # first use (not just in the copy oracle) so teardown restores
                # the original instead of leaving a test string behind.
                self._ensure_clipboard_backup()
                cb.set_text(text[i:j])
                keys.press_combo("ctrl+v")
                time.sleep(0.15)
                i = j
        flush_ascii()

    def keys(self, combo: str, focus: bool = True) -> None:
        """Key combo. No click by default (combos are often selection/focus-sensitive).

        `focus=False` for cases where a popup owns focus (see `type_text`).
        """
        if focus:
            self.focus_soft()
        keys.press_combo(combo)

    # ---- Save / dirty flag ------------------------------------------------------

    def is_dirty(self) -> bool:
        """A title starting with `*` means unsaved (the OS title is one trusted oracle)."""
        return self.title().startswith("*")

    def save(self, timeout: float = 8.0, expect_dirty: bool = True) -> None:
        """Ctrl+S and wait for the dirty marker to vanish from the title.

        In-memory state and on-disk state are **two different things**:
        replace/insert only touch memory; save before asserting disk bytes --
        skipping this step fabricates conclusions like "the replace never took
        effect" (been burned).

        With `expect_dirty=True` (default), error out immediately if there is no
        dirty marker right now: it means the earlier edits never landed on the
        document (keys swallowed by focus issues), and continuing would only yield
        a "disk content unchanged" false failure.
        """
        if not self.is_dirty():
            if expect_dirty:
                raise RuntimeError(
                    "expected the document to be dirty (title should carry the * "
                    "dirty marker) but it does not -- the earlier edits were most "
                    "likely dropped; first verify the keys landed in the editor")
            return
        self.keys("ctrl+s")
        deadline = time.time() + timeout
        while time.time() < deadline:
            if not self.is_dirty():
                return
            time.sleep(0.15)
        raise TimeoutError("title still carries the dirty marker after Ctrl+S")

    # ---- Coordinate utilities (self-drawn UI can only be hit by geometry) ------
    #
    # Coordinate conventions (`infed/__init__.py` pins the process to per-monitor
    # DPI awareness): Win32 gives us **physical pixels**, while blitz layout
    # constants (`WINBTN_W=46`, `TITLEBAR_H=36`, the find window's `BTN_H=22`) are
    # **CSS px**. The conversion factor is `GetDpiForWindow/96`. Multiplying once
    # too few or twice both misclick -- this project hit both.

    def dpi_scale(self) -> float:
        """Scale of the monitor the window is on (physical px / CSS px)."""
        dpi = user32.GetDpiForWindow(wt.HWND(self.main_hwnd))
        return (dpi / 96.0) if dpi else 1.0

    def client_origin(self, hwnd: int | None = None) -> tuple[int, int]:
        """Screen physical coordinates of the client-area top-left.

        Uses `ClientToScreen` rather than `GetWindowRect`: the find popup has a
        native frame and the two differ by (11, 45) physical pixels -- y computed
        from the window origin lands entirely outside the frame, so every click
        misses (been burned).
        """
        hwnd = hwnd or self.main_hwnd
        pt = wt.POINT(0, 0)
        if not user32.ClientToScreen(wt.HWND(hwnd), ctypes.byref(pt)):
            raise OSError("ClientToScreen failed")
        return pt.x, pt.y

    def _click_screen(self, sx: int, sy: int, settle: float = 0.3) -> None:
        user32.SetCursorPos(int(sx), int(sy))
        time.sleep(0.08)
        user32.mouse_event(0x0002, 0, 0, 0, 0)   # LEFTDOWN
        time.sleep(0.05)
        user32.mouse_event(0x0004, 0, 0, 0, 0)   # LEFTUP
        time.sleep(settle)

    def click_at_css(self, css_x: float, css_y: float, focus: bool = True) -> None:
        """Click a point in the **main window's** client area by CSS coordinates."""
        self._check_click_point(css_x, css_y)
        if focus:
            self.focus_soft()
        s = self.dpi_scale()
        l, t = self.client_origin()
        self._click_screen(l + css_x * s, t + css_y * s)

    def double_click_at_css(self, css_x: float, css_y: float, focus: bool = True) -> None:
        """**Double-click** a point in the main window's client area by CSS coordinates
        (CSV cell in-place editing etc.).

        Two DOWN/UP pairs ~100ms apart (within the system double-click limit) at
        the same point -- the event layer composes ondoubleclick from this. The
        first click also triggers single-click semantics (selection), matching
        real use.
        """
        self._check_click_point(css_x, css_y, what="double-click")
        if focus:
            self.focus_soft()
        s = self.dpi_scale()
        l, t = self.client_origin()
        user32.SetCursorPos(int(l + css_x * s), int(t + css_y * s))
        time.sleep(0.08)
        for _ in range(2):
            user32.mouse_event(0x0002, 0, 0, 0, 0)   # LEFTDOWN
            time.sleep(0.04)
            user32.mouse_event(0x0004, 0, 0, 0, 0)   # LEFTUP
            time.sleep(0.06)
        time.sleep(0.3)

    def triple_click_at_css(self, css_x: float, css_y: float, focus: bool = True) -> None:
        """**Triple-click** a point in the main window's client area by CSS coordinates
        (wys triple-click block selection etc.).

        Three DOWN/UP pairs ~100ms apart (within the 600ms cadence window) at the
        same point.
        """
        self._check_click_point(css_x, css_y, what="triple-click")
        if focus:
            self.focus_soft()
        s = self.dpi_scale()
        l, t = self.client_origin()
        user32.SetCursorPos(int(l + css_x * s), int(t + css_y * s))
        time.sleep(0.08)
        for _ in range(3):
            user32.mouse_event(0x0002, 0, 0, 0, 0)   # LEFTDOWN
            time.sleep(0.04)
            user32.mouse_event(0x0004, 0, 0, 0, 0)   # LEFTUP
            time.sleep(0.06)
        time.sleep(0.3)

    def client_size(self, hwnd: int | None = None) -> tuple[float, float]:
        """Client-area size, CSS px (the main window by default)."""
        r = wt.RECT()
        user32.GetClientRect(wt.HWND(hwnd or self.main_hwnd), ctypes.byref(r))
        s = self.dpi_scale()
        return r.right / s, r.bottom / s

    def _check_click_point(self, css_x: float, css_y: float,
                           hwnd: int | None = None, what: str = "click") -> None:
        """Refuse a `what` whose point lies outside the target's client area.

        Every click in this suite is geometry-derived (the UI is self-drawn and
        exposes no control rectangles), so a stale layout constant or a
        zero-sized UIA rect degrades into "nothing happened" instead of an
        error -- and the test then asserts something that holds either way.
        That is exactly how one regression test passed for months while its
        toolbar click landed **outside the window** (UIA reported a 0x0 rect,
        the computed CSS x came out negative). Failing here names the point and
        the client size, which is the difference between a five-second fix and
        an afternoon of "why does this assertion never fail".
        """
        w, h = self.client_size(hwnd)
        if not (0.0 <= css_x <= w and 0.0 <= css_y <= h):
            raise RuntimeError(
                f"{what} point ({css_x:.1f}, {css_y:.1f}) CSS px is outside the "
                f"{'popup' if hwnd and hwnd != self.main_hwnd else 'main window'} "
                f"client area ({w:.1f}x{h:.1f}) -- the coordinate is wrong, so the "
                f"input would land on the desktop (or another window) and the "
                f"app would never see it")

    def mouse_wheel_at_css(self, css_x: float, css_y: float, notches: int) -> None:
        """Move the cursor to (css_x, css_y) in the main window's client area and scroll `notches` notches.

        Direction matches the Windows wheel delta: positive = forward (content
        moves up), negative = backward (content down), one notch =
        WHEEL_DELTA(120). Precondition: the window is already in the foreground.
        """
        self._check_click_point(css_x, css_y, what="wheel")
        s = self.dpi_scale()
        l, t = self.client_origin()
        user32.SetCursorPos(int(l + css_x * s), int(t + css_y * s))
        time.sleep(0.1)
        delta = 120 if notches > 0 else -120
        for _ in range(abs(notches)):
            user32.mouse_event(0x0800, 0, 0, delta, 0)   # MOUSEEVENTF_WHEEL
            time.sleep(0.06)
        time.sleep(0.3)

    def drag_at_css(self, x0: float, y0: float, x1: float, y1: float,
                    steps: int = 14) -> None:
        """Drag with the left button from (x0,y0) to (x1,y1) inside the main window's client area (CSS coordinates).

        Intermediate multi-step SetCursorPos approximates a real mouse trajectory
        -- blitz's drag-select gate enters selection mode via a pointermove
        sequence (2px threshold); a single-step teleport reads as two independent
        clicks and selection never starts.
        """
        self._check_click_point(x0, y0, what="drag start")
        self._check_click_point(x1, y1, what="drag end")
        s = self.dpi_scale()
        l, t = self.client_origin()
        sx0, sy0 = int(l + x0 * s), int(t + y0 * s)
        sx1, sy1 = int(l + x1 * s), int(t + y1 * s)
        user32.SetCursorPos(sx0, sy0)
        time.sleep(0.12)
        user32.mouse_event(0x0002, 0, 0, 0, 0)   # LEFTDOWN
        time.sleep(0.08)
        for i in range(1, steps + 1):
            user32.SetCursorPos(sx0 + (sx1 - sx0) * i // steps,
                                sy0 + (sy1 - sy0) * i // steps)
            time.sleep(0.025)
        time.sleep(0.08)
        user32.mouse_event(0x0004, 0, 0, 0, 0)   # LEFTUP
        time.sleep(0.3)

    def click_in_window(self, hwnd: int, css_x: float, css_y: float,
                        focus: bool = False) -> None:
        """Click a point inside an **arbitrary window's** client area by CSS coordinates
        (find/replace popups).

        `focus=False`: popups are separate top-level windows; stealing focus would
        pour input into the main document.
        """
        self._check_click_point(css_x, css_y, hwnd=hwnd, what="popup click")
        s = self.dpi_scale()
        l, t = self.client_origin(hwnd)
        if focus:
            grant_foreground(hwnd)
            self._window_wrapper(hwnd).set_focus()
            time.sleep(0.1)
        self._click_screen(l + css_x * s, t + css_y * s)

    def window_button(self, kind: str) -> None:
        """Click a title-bar window control (`"min"` / `"max"` / `"close"`).

        Three 46 CSS px wide buttons hug the right edge in DOM order
        minimize/maximize/close, so counted from the right it is
        close/maximize/minimize.

        **Must `focus_soft()` first**: when the window isn't in the foreground the
        first click only activates it and the app never sees the press -- "clicked,
        nothing happened" (this took three rounds of investigation in this
        project).
        """
        idx = {"close": 0, "max": 1, "min": 2}[kind]
        self.focus_soft()
        s = self.dpi_scale()
        _, _, r, _ = cb.window_rect(self.main_hwnd)
        t = self.client_origin()[1]
        w = 46.0 * s
        self._click_screen(r - w * (idx + 0.5), t + 18 * s, settle=0.5)

    def _raw_click_center(self) -> None:
        """Click the main window's viewport center (no focus checks -- focus()'s fallback path)."""
        l, t, r, b = cb.window_rect(self.main_hwnd)
        cx, cy = (l + r) // 2, (t + b) // 2
        user32.SetCursorPos(cx, cy)
        time.sleep(0.05)
        user32.mouse_event(0x0002, 0, 0, 0, 0)  # LEFTDOWN
        user32.mouse_event(0x0004, 0, 0, 0, 0)  # LEFTUP

    # ---- Find / replace popup ----------------------------------------------------

    def dialog(self, timeout: float = 5.0) -> int | None:
        """Return the find/replace popup's hwnd (the top-level window opened by `Ctrl+F` / `Ctrl+H`).

        Criterion: "the largest visible top-level window of this process other than
        the main window" -- winit also creates a few 15x15 message auxiliary
        windows, filtered out by area.
        """
        deadline = time.time() + timeout
        while time.time() < deadline:
            cand = [h for h in windows_of_pid(self.proc.pid)
                    if h != self.main_hwnd and _area(h) > 10000]
            if cand:
                return max(cand, key=_area)
            time.sleep(0.2)
        return None

    def dialog_geometry(self) -> tuple[int, int]:
        """Popup client-area size in CSS px (for geometry assertions, no screenshot needed)."""
        h = self.dialog()
        if not h:
            raise RuntimeError("find/replace popup does not exist")
        r = wt.RECT()
        user32.GetClientRect(wt.HWND(h), ctypes.byref(r))
        s = self.dpi_scale()
        return round(r.right / s), round(r.bottom / s)

    def dialog_button(self, index: int) -> None:
        """Click the `index`-th (0-based, top-down) button in the popup's right button column.

        Geometry taken from `src/ui/findwin.rs` (flex layout): the button column
        hugs the right edge, `BTN_W=92`, right margin 10, row height
        `BTN_H + BTN_GAP = 24 + 3`, the first row starts `PAD = 8` from the client
        top (no tab row in the right column). Find mode has 5 buttons
        (next/prev/count/find-all/results); replace mode adds "Replace" and
        "Replace All" for 7 total.
        """
        h = self.dialog()
        if not h:
            raise RuntimeError("find/replace popup does not exist")
        # The popup is a separate top-level window: give it the same stacking
        # guarantee as the main window, or a topmost neighbour swallows its
        # clicks (and the button lands in another app entirely).
        self.set_topmost(True, hwnd=h)
        # When the popup isn't in the foreground, first do a "warm-up" click on
        # empty space: otherwise the first press only activates it and the button
        # never receives the onclick. The point must be *inside* the popup --
        # find mode is only 162 CSS px tall (`findwin::popup_size`), so the old
        # fixed y=192 landed below it and "warmed up" whatever was behind.
        if user32.GetForegroundWindow() != h:
            warm_y = min(192.0, self.client_size(h)[1] - 12.0)
            self.click_in_window(h, 20.0, warm_y)
        cw, _ = self.dialog_geometry()
        btn_x = max(cw - 92.0 - 10.0, 350.0)
        self.click_in_window(h, btn_x + 46.0, 8.0 + index * 27.0 + 12.0)

    def close_dialog(self) -> None:
        """Esc closes the find/replace popup (focus is on the popup; don't steal it back to the main window)."""
        keys.press_combo("escape")
        time.sleep(0.5)

    # ---- Self-drawn modal dialogs ------------------------------------------------

    def _dialog_probe(self) -> tuple[list[tuple[float, float]], list[str]]:
        """Read the last `[dialog]` probe record -> (button center points, button labels).

        `[dialog] buttons=N centers=x,y;x,y labels=恢复|放弃快照`
        With `buttons=0` (no dialog) both are empty lists.
        """
        log = self.config_dir / "shell.log"
        if not log.exists():
            return [], []
        last = None
        for line in log.read_text(encoding="utf-8", errors="replace").splitlines():
            if line.startswith("[dialog] "):
                last = line
        if last is None:
            return [], []
        if "centers=" not in last:
            return [], []                     # buttons=0
        _, tail = last.split("centers=", 1)
        labels: list[str] = []
        if " labels=" in tail:
            tail, lab = tail.split(" labels=", 1)
            labels = [s for s in lab.split("|") if s]
        out: list[tuple[float, float]] = []
        for part in tail.split(";"):
            if not part:
                continue
            xs, ys = part.split(",")
            out.append((float(xs), float(ys)))
        return out, labels

    def dialog_buttons(self) -> list[tuple[float, float]]:
        """Center points of the self-drawn dialog's action buttons (**CSS px**, relative to the client top-left).

        Data comes from the app's layout probe: `ui::shell::probe_dialog_buttons`
        uses DOM ids (`inf-dlg-btn-N`) + `get_client_bounding_rect` to write the
        real geometry into `shell.log`'s `[dialog]` line. More reliable than the
        "accent-color block" oracle: after primary/secondary button tiering
        (primary filled with the accent color, secondary on a neutral surface),
        the color oracle can identify only one button -- the 2026-10 sixth GUI
        round failed wholesale for exactly this reason.

        Requires the app started with `INF_UI_PROBE=1` (`App.start` injects it by
        default). An empty return = no dialog right now (or the probe hasn't
        written yet).
        """
        return self._dialog_probe()[0]

    def dialog_labels(self) -> list[str]:
        """The dialog's action-button labels (left -> right), one-to-one with [`dialog_buttons`]."""
        return self._dialog_probe()[1]

    def click_dialog_button(self, index: int) -> None:
        """Click the `index`-th action button of a self-drawn modal dialog (0 = leftmost).

        Geometry trusts only the app's layout probe (`shell.log`'s `[dialog]`
        line). A missing probe means the exe is too old or lacks
        `INF_UI_PROBE=1` -- error out directly, never fall back to
        color/coordinate guessing: clicking the wrong button in a modal can turn
        "cancel" into "confirm discard".
        """
        buttons = self.dialog_buttons()
        if index >= len(buttons):
            raise RuntimeError(
                f"cannot read dialog button #{index} ({len(buttons)} available); "
                "confirm the exe has INF_UI_PROBE=1 and a dialog is open"
            )
        cx, cy = buttons[index]
        self.focus_soft()
        self.click_at_css(cx, cy, focus=False)

    def click_dialog_label(self, text: str) -> None:
        """Click a dialog button by **label** (recommended).

        Button indexes depend on declaration order; one reorder and tests silently
        click the wrong button -- in the two-step confirm the "取消" and "确认放弃"
        buttons are adjacent, and a misclick deletes the user's only rescue
        snapshot. A missing label raises immediately; never degrade to "click the
        first one".
        """
        labels = self.dialog_labels()
        if not labels:
            raise RuntimeError(
                f"cannot read dialog button labels (probe off or no dialog); wanted to click 「{text}」"
            )
        if text not in labels:
            raise RuntimeError(f"dialog has no 「{text}」 button; existing buttons: {labels}")
        self.click_dialog_button(labels.index(text))

    # ---- Verification oracles ------------------------------------------------------

    def _ensure_clipboard_backup(self) -> None:
        if self._clipboard_backup is None:
            try:
                self._clipboard_backup = cb.get_text()
            except OSError:
                # Never restore a value we invented: clearing the clipboard on
                # teardown would be worse than leaving the test's text there.
                self._clipboard_backup = _CLIPBOARD_UNREADABLE

    def restore_clipboard(self) -> None:
        backup = self._clipboard_backup
        self._clipboard_backup = None
        if isinstance(backup, str):
            try:
                cb.set_text(backup)
            except OSError:
                pass

    def _input_alive(self) -> bool:
        """Probe: whether keys are actually received by the app.

        Uses `Ctrl+F` as an "echo" -- it opens a **new top-level window** (the
        find popup), a clear, observable app-side reaction. More direct than "did
        the clipboard change": on an empty document Ctrl+C legitimately doesn't
        touch the clipboard, so the clipboard alone can't tell "empty" from "not
        received".
        """
        before = len(windows_of_pid(self.proc.pid))
        keys.press_combo("ctrl+f")
        time.sleep(0.7)
        opened = len(windows_of_pid(self.proc.pid)) > before
        if opened:
            keys.press_combo("escape")
            time.sleep(0.4)
        return opened

    def _copy_via(self, combos: list[str], click: bool, timeout: float,
                  empty_ok: bool = False) -> str:
        """Copy oracle with sentinel + retries.

        Uses a one-shot sentinel to detect "did this Ctrl+C actually write the
        clipboard"; on failure (keys dropped by focus, copy intercepted) re-grab
        focus and resend -- winit's focus is flaky, and retries compress the
        instability into reliability.

        When retries are exhausted, **raise** instead of returning the sentinel to
        the caller: early versions returned it directly, so on "keys dropped"
        tests would get a `__INFED_SENTINEL_...__` string to assert against --
        either a false pass or a baffling diff (this project really got burned
        once: a goto test's "read document" was exactly the sentinel). Better to
        fail loudly here.

        With `empty_ok=True` (read selection / read full text), an "empty
        selection" is a legal result: first confirm via [`_input_alive`] that keys
        echoed, then return ""; error only when there's no echo.
        """
        self._ensure_clipboard_backup()
        result = None
        sentinel = ""
        for attempt in range(4):
            if click:
                self.focus_click()
            else:
                self.focus_soft()
            sentinel = f"__INFED_SENTINEL_{attempt}_{time.time_ns()}__"
            cb.set_text(sentinel)
            for combo in combos:
                keys.press_combo(combo)
            deadline = time.time() + max(1.0, timeout / 4)
            while time.time() < deadline:
                now = cb.get_text()
                if now != sentinel:
                    result = now
                    break
                time.sleep(0.1)
            if result is not None:
                break
        if result is None:
            if empty_ok and self._input_alive():
                return ""       # keys echoed -> empty selection, not dropped keys
            raise RuntimeError(
                f"copy oracle failed 4 times in a row ({'/'.join(combos)} never wrote the clipboard) -- "
                "usually the focus never landed on the editor and the keys were silently dropped. "
                "First check the window is in the foreground, "
                "and if needed do a real click once with focus_click()/click_at_css().")
        return result

    def copy_selection(self, timeout: float = 5.0) -> str:
        """Ctrl+C to read the current selection (empty when there is none). No click -- it would destroy the selection."""
        return self._copy_via(["ctrl+c"], click=False, timeout=timeout,
                              empty_ok=True)

    def doc_text(self, timeout: float = 8.0) -> str:
        """Ctrl+A -> Ctrl+C to read the full text -- the only lossless oracle for the editor's text state.

        Line endings are normalized to `\\n`: the document's real line endings are
        verified from disk bytes (save tests); the CRLF-vs-LF difference in the
        clipboard shouldn't make every test write its own normalization.

        Note it **leaves "select all" active**: if you need to type into the
        document afterwards, collapse the selection first with `End` /
        `ctrl+end`, otherwise the next character replaces the whole document
        (the first goto test wrote the document into "X" exactly this way).
        """
        return self._copy_via(["ctrl+a", "ctrl+c"], click=True,
                              timeout=timeout, empty_ok=True).replace("\r\n", "\n")

    # ---- Evidence -------------------------------------------------------------------

    def snapshot(self, box_css: tuple[float, float, float, float] | None = None):
        """Grab an image of the main window (or a CSS rect within it) for pixel-level comparison.

        Same path as `screenshot()`: a **screen grab**, so grab the foreground
        first; occluders (always-on-top windows) pollute the result -- keep
        tolerance in assertions.
        """
        from PIL import ImageGrab
        self.focus_soft()
        l, t = self.client_origin()
        if box_css is None:
            img = ImageGrab.grab(bbox=cb.window_rect(self.main_hwnd))
            return img.convert("RGB")
        s = self.dpi_scale()
        x0, y0, x1, y1 = box_css
        return ImageGrab.grab(
            bbox=(int(l + x0 * s), int(t + y0 * s), int(l + x1 * s), int(t + y1 * s))
        ).convert("RGB")

    def screenshot(self, name: str) -> Path:
        """Grab the main window area (for evidence, not the assertion mainstay).

        It is a **screen grab**: bring the window to the foreground first,
        otherwise you photograph whatever other window covers it. For wgpu
        self-drawn windows PrintWindow yields a black image, so screen grabbing is
        the only realistic option -- screenshots can still be polluted by
        "always-on-top windows"; don't rely on pixels for important assertions.
        """
        from PIL import ImageGrab
        try:
            if self.main_hwnd and cb.is_window(self.main_hwnd):
                self.focus_soft()
        except Exception:
            pass
        img = ImageGrab.grab(bbox=cb.window_rect(self.main_hwnd))
        path = self.screenshot_dir / f"{name}.png"
        path.parent.mkdir(parents=True, exist_ok=True)   # name may contain a subdirectory
        img.save(path)
        return path


def _area(hwnd: int) -> int:
    l, t, r, b = cb.window_rect(hwnd)
    return max(0, r - l) * max(0, b - t)


def mean_diff(a, b) -> float:
    """**Mean per-pixel difference** (0..255) of two same-size images; for rendering-style assertions.

    Why not "pixels exactly equal": the caret blinks, and antialiasing/font
    rasterization may jitter by a pixel or two between grabs. The mean difference
    pushes such noise below 1, while real changes like "text got bigger" score
    above 10 -- a threshold of 3 is both stable and sensitive.
    """
    from PIL import ImageChops, ImageStat
    if a.size != b.size:
        w = min(a.size[0], b.size[0])
        h = min(a.size[1], b.size[1])
        a = a.crop((0, 0, w, h))
        b = b.crop((0, 0, w, h))
    bands = ImageStat.Stat(ImageChops.difference(a, b)).mean
    # Average *all* channels: taking band 0 alone would miss a change that only
    # moves hue (e.g. a red mark turning blue) and overstates the visual delta.
    return sum(bands) / len(bands)
