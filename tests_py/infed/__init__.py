"""InfEditor GUI automation test package (pure ctypes + pywinauto/PIL).

**DPI awareness must be settled before any coordinate read.** pywinauto
sets the process to per-monitor DPI aware at import time
(`SetProcessDpiAwareness(PerMonitor)` in `pywinauto/win32functions.py`),
and Windows virtualizes `GetWindowRect` / `SetCursorPos` / `ClientToScreen`
back to logical coordinates for **unaware** callers (at 150% scaling the
numbers are 2/3 of the physical values).

So the same window reads 1280×860 before import and 1920×1290 after — click
coordinates were sometimes right, sometimes wrong (this project was bitten:
the maximize button "did nothing" because the click actually landed in the
gap between the close and maximize buttons). This package pins the
awareness level at init time; from then on every coordinate is in
**physical pixels**, and CSS → screen conversion goes uniformly through
`driver.App.dpi_scale()`.
"""

from __future__ import annotations

_ctypes = __import__("ctypes")

try:  # do it explicitly (pywinauto does too, but this must come "before everything")
    _ctypes.WinDLL("shcore").SetProcessDpiAwareness(2)  # PerMonitor
except Exception:  # pragma: no cover - pre-Win8.1 / already pinned by manifest
    try:
        _ctypes.WinDLL("user32").SetProcessDPIAware()
    except Exception:
        pass
