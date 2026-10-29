"""SendInput keyboard injection: Unicode text + key combos.

pywinauto's keyboard module has no VK_PACKET branch for non-ASCII characters
and drops Chinese outright; this module sends SendInput directly via ctypes:

* text — ``KEYEVENTF_UNICODE`` per UTF-16 code unit (surrogate pairs of
  emoji get through too), newlines become VK_RETURN, tabs become VK_TAB;
* combos — descriptive strings like ``"ctrl+shift+s"``: modifiers held →
  key tapped → released.
"""

from __future__ import annotations

import ctypes
import time

user32 = ctypes.WinDLL("user32", use_last_error=True)

# --- SendInput structures ----------------------------------------------------

INPUT_KEYBOARD = 1
KEYEVENTF_KEYUP = 0x0002
KEYEVENTF_UNICODE = 0x0004

wintypes = ctypes.wintypes


class KEYBDINPUT(ctypes.Structure):
    _fields_ = [
        ("wVk", wintypes.WORD),
        ("wScan", wintypes.WORD),
        ("dwFlags", wintypes.DWORD),
        ("time", wintypes.DWORD),
        ("dwExtraInfo", ctypes.POINTER(ctypes.c_ulong)),
    ]


class _INPUTUNION(ctypes.Union):
    _fields_ = [("ki", KEYBDINPUT), ("padding", ctypes.c_byte * 32)]


class INPUT(ctypes.Structure):
    _fields_ = [("type", wintypes.DWORD), ("union", _INPUTUNION)]


# --- virtual-key map ---------------------------------------------------------

_VK = {
    "enter": 0x0D, "return": 0x0D,
    "tab": 0x09,
    "esc": 0x1B, "escape": 0x1B,
    "backspace": 0x08, "bs": 0x08,
    "del": 0x2E, "delete": 0x2E,
    "ins": 0x2D, "insert": 0x2D,
    "home": 0x24, "end": 0x23,
    "pgup": 0x21, "pageup": 0x21,
    "pgdn": 0x22, "pagedown": 0x22,
    "left": 0x25, "up": 0x26, "right": 0x27, "down": 0x28,
    "space": 0x20,
}
for _f in range(1, 25):
    _VK[f"f{_f}"] = 0x6F + _f  # VK_F1 = 0x70

_MODS = {"ctrl": 0x11, "control": 0x11, "shift": 0x10, "alt": 0x12,
         "win": 0x5B, "meta": 0x5B}

#: Punctuation virtual-key table for the US layout (the **unshifted** level).
_PUNCT = {"`": 0xC0, "-": 0xBD, "=": 0xBB, "[": 0xDB, "]": 0xDD,
          "\\": 0xDC, ";": 0xBA, "'": 0xDE, ",": 0xBC, ".": 0xBE, "/": 0xBF}

#: Shifted characters → the same physical key, unshifted.
#:
#: Without this table, `type_text("A_B")` raises ValueError right at `_`:
#: `_` is the product of shift+`-` and is not a virtual key itself. Only the
#: "needs Shift?" check used `SHIFTED_PUNCT` before; the table lookup did no
#: normalization — `{} : " <` and friends would blow up too.
_SHIFT_BASE = {
    "~": "`", "!": "1", "@": "2", "#": "3", "$": "4", "%": "5", "^": "6",
    "&": "7", "*": "8", "(": "9", ")": "0", "_": "-", "+": "=",
    "{": "[", "}": "]", "|": "\\", ":": ";", '"': "'", "<": ",", ">": ".",
    "?": "/",
}


def _vk_of(name: str) -> int:
    n = name.lower()
    if n in _VK:
        return _VK[n]
    if n in _MODS:
        return _MODS[n]
    if len(n) == 1:
        ch = n.upper()
        if ch.isdigit():
            return ord(ch)
        if "A" <= ch <= "Z":
            return ord(ch)
        if ch in _SHIFT_BASE:               # shifted level → base level (must re-lookup after normalizing!)
            ch = _SHIFT_BASE[ch]
            if ch.isdigit():
                return ord(ch)
        if ch in _PUNCT:
            return _PUNCT[ch]
    raise ValueError(f"unmappable key name: {name!r}")


def _send(events: list[INPUT]) -> None:
    n = user32.SendInput(len(events), (INPUT * len(events))(*events),
                         ctypes.sizeof(INPUT))
    if n != len(events):
        raise OSError(f"SendInput injected only {n}/{len(events)} events")


def _key_event(vk: int, scan: int, flags: int) -> INPUT:
    e = INPUT()
    e.type = INPUT_KEYBOARD
    e.union.ki.wVk = vk
    e.union.ki.wScan = scan
    e.union.ki.dwFlags = flags
    return e


# Punctuation that requires holding Shift to type (US layout)
SHIFTED_PUNCT = set('~!@#$%^&*()_+{}|:"<>?')


def type_text(text: str, char_delay: float = 0.005) -> None:
    """Inject ASCII text (really typed key by key).

    Measured constraint: winit only turns **virtual-key** keydowns into
    `Key::Character`; `KEYEVENTF_UNICODE` (VK_PACKET) and manual `WM_CHAR`
    are not accepted — so only the VK path is used here, with uppercase
    letters and `!@#…`-style characters auto-prefixed with Shift.
    Non-ASCII (Chinese/emoji) must go through [`infed.driver.App.type_text`]'s
    paste channel.
    """
    for ch in text:
        if ch == "\n":
            press_combo("enter")
        elif ch == "\t":
            press_combo("tab")
        elif ch == "\r":
            continue
        elif ord(ch) < 128:
            # space must not go through press_combo(" ") (treated as an empty combo)
            name = "space" if ch == " " else ch.lower()
            if ch.isupper() or ch in SHIFTED_PUNCT:
                press_combo(f"shift+{name}")
            else:
                press_combo(name)
        else:  # pragma: no cover — known not to work; kept in case winit supports it later
            raw = ch.encode("utf-16-le")
            for i in range(0, len(raw), 2):
                u = int.from_bytes(raw[i:i + 2], "little")
                _send([_key_event(0, u, KEYEVENTF_UNICODE)])
                _send([_key_event(0, u, KEYEVENTF_UNICODE | KEYEVENTF_KEYUP)])
        time.sleep(char_delay)


def press_combo(combo: str, hold: float = 0.03) -> None:
    """Press a key combo: ``"ctrl+s"``, ``"ctrl+shift+tab"``, ``"f3"``."""
    parts = [p.strip() for p in combo.split("+") if p.strip()]
    if not parts:
        raise ValueError("empty key combo")
    mods, key = [], parts[-1]
    for p in parts[:-1]:
        if p.lower() not in _MODS:
            raise ValueError(f"not a modifier key: {p!r}")
        mods.append(_MODS[p.lower()])
    vk = _vk_of(key)
    events = [_key_event(m, 0, 0) for m in mods] + [_key_event(vk, 0, 0)]
    _send(events)
    time.sleep(hold)
    up = [_key_event(vk, 0, KEYEVENTF_KEYUP)] + [
        _key_event(m, 0, KEYEVENTF_KEYUP) for m in reversed(mods)]
    _send(up)


def press_key(key: str) -> None:
    press_combo(key)
