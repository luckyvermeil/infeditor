"""Clipboard read/write (CF_UNICODETEXT), pure ctypes, no pywin32 dependency.

Opening the clipboard can fail because another process holds it; every
operation retries briefly.
"""

from __future__ import annotations

import ctypes
import ctypes.wintypes as wt
import time

kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)
user32 = ctypes.WinDLL("user32", use_last_error=True)

CF_UNICODETEXT = 13
GMEM_MOVEABLE = 0x0002

# On 64-bit, handles/pointers must never land on ctypes' default c_int
# restype (truncated to 32 bits, GlobalLock fails instantly) — declare all
# of them explicitly.
kernel32.GlobalAlloc.restype = ctypes.c_void_p
kernel32.GlobalAlloc.argtypes = [wt.UINT, ctypes.c_size_t]
kernel32.GlobalLock.restype = ctypes.c_void_p
kernel32.GlobalLock.argtypes = [ctypes.c_void_p]
kernel32.GlobalUnlock.argtypes = [ctypes.c_void_p]
kernel32.GlobalFree.argtypes = [ctypes.c_void_p]
user32.OpenClipboard.argtypes = [ctypes.c_void_p]
user32.GetClipboardData.restype = ctypes.c_void_p
user32.GetClipboardData.argtypes = [wt.UINT]
user32.SetClipboardData.restype = ctypes.c_void_p
user32.SetClipboardData.argtypes = [wt.UINT, ctypes.c_void_p]


def _retry(fn, tries: int = 20, delay: float = 0.05):
    last = None
    for _ in range(tries):
        try:
            return fn()
        except OSError as e:  # clipboard unavailable (held by another process)
            last = e
            time.sleep(delay)
    raise last  # type: ignore[misc]


def get_text() -> str:
    def _get():
        if not user32.OpenClipboard(None):
            raise OSError("OpenClipboard failed")
        try:
            h = user32.GetClipboardData(CF_UNICODETEXT)
            if not h:
                return ""
            ptr = kernel32.GlobalLock(h)
            if not ptr:
                raise OSError("GlobalLock failed")
            try:
                return ctypes.wstring_at(ptr)
            finally:
                kernel32.GlobalUnlock(h)
        finally:
            user32.CloseClipboard()
    return _retry(_get)


def set_text(text: str) -> None:
    def _set():
        if not user32.OpenClipboard(None):
            raise OSError("OpenClipboard failed")
        try:
            user32.EmptyClipboard()
            if not text:
                return
            data = text.encode("utf-16-le") + b"\x00\x00"
            h = kernel32.GlobalAlloc(GMEM_MOVEABLE, len(data))
            if not h:
                raise OSError("GlobalAlloc failed")
            ptr = kernel32.GlobalLock(h)
            if not ptr:
                raise OSError("GlobalLock failed")
            try:
                ctypes.memmove(ptr, data, len(data))
            finally:
                kernel32.GlobalUnlock(h)
            if not user32.SetClipboardData(CF_UNICODETEXT, h):
                kernel32.GlobalFree(h)
                raise OSError("SetClipboardData failed")
            # ownership has been transferred to the system; do not GlobalFree
        finally:
            user32.CloseClipboard()
    _retry(_set)


def window_rect(hwnd: int):
    r = wt.RECT()
    if not user32.GetWindowRect(wt.HWND(hwnd), ctypes.byref(r)):
        raise OSError("GetWindowRect failed")
    return r.left, r.top, r.right, r.bottom


def is_window(hwnd: int) -> bool:
    return bool(user32.IsWindow(wt.HWND(hwnd)))
