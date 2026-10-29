"""Find flow: Ctrl+F → type term → Enter/F3 → selection equals the match.

The find UI is Blitz-drawn (in-window find bar or standalone dialog); Win32
sees no widgets, so the flow is keyboard-only, and verification uses "the
selection after a hit" = the text read via Ctrl+C.

Hits and F3 walking share one instance (F3 is the natural continuation of
the hit assertion); the dialog caret blink is a pixel-burst oracle and
requires a blank-document instance, so it stays standalone.
"""

from __future__ import annotations

import time

from infed import clipboard as cb


def _doc_with_needles(work_dir, name="find.txt"):
    f = work_dir / name
    f.write_text("alpha needle one\nbeta two\ngamma needle three\n", encoding="utf-8")
    return f


def _open_find(a, term: str):
    """Ctrl+F opens the find dialog and types the search term.

    Two pitfalls, both learned the hard way:

    1. Find is a **standalone top-level window** and the app takes focus
       itself — `focus=False` avoids stealing focus, otherwise the search
       term gets typed into the main document;
    2. Do not read the selection after Esc: Esc's first act is
       `clear_selection()`. So hit verification goes through F3, and the
       selection is the one F3 left behind.
    """
    a.keys("ctrl+f")
    time.sleep(0.6)
    a.type_text(term, focus=False)
    a.keys("enter", focus=False)     # find next
    time.sleep(0.4)


def test_find_selects_match_then_f3_walks(app_factory, work_dir):
    a = app_factory(files=[_doc_with_needles(work_dir)])
    assert a.wait_title("find.txt")

    _open_find(a, "needle")
    # Read the selection first: copy_selection's Ctrl+C leaves the selection
    # alone, so what it reads is the find hit
    sel = a.copy_selection()
    assert sel == "needle", f"selection after find should be the matched text, got: {sel!r}"

    # Second hit: F3 continues; the selection is still the same word
    # (position advanced to the next occurrence).
    # Mind the order: must run before doc_text() — doc_text's Ctrl+A swaps
    # the selection for select-all, and leftover select-all clobbers the hit
    # selection F3 left (bitten during a merge).
    a.keys("f3", focus=False)
    time.sleep(0.4)
    assert a.copy_selection() == "needle", "F3 should continue selecting the next needle"

    # Finally verify the document was not polluted by the search term (focus
    # was never stolen back). The select-all state doc_text leaves behind
    # does not matter: this is the test's final assertion.
    assert a.doc_text() == _doc_with_needles(work_dir).read_text(encoding="utf-8")


def test_find_caret_blinks(app_factory):
    """The dialog input's virtual caret must **blink** (historical defect: a
    static vertical line gave no cue where input would land).

    Fix: the caret div carries `animation: caret-blink ... step-end
    infinite` (keyframes come from the dialog's own POPUP_CSS — the dialog
    is a separate DioxusDocument and the main window's GLOBAL_CSS keyframes
    are unreachable).

    The oracle is a **pixel pulse**: burst-capture 14 frames (70ms apart ≈
    two and a half phases of the 530ms blink cycle); everything in the
    dialog except the blinking caret is static, so take the per-pixel max
    amplitude — the blinking caret (≈1×14 CSS px) leaves ≥5 high-amplitude
    pixels; a static line leaves 0.
    Note: screenshot oracles fail under a locked screen (see the
    "Screenshots" field note in tests_py/README.md); run this test with an
    unlocked desktop.
    """
    from PIL import ImageChops, ImageGrab

    a = app_factory()
    assert a.wait_title("InfEditor", timeout=8)

    a.keys("ctrl+f")
    h = a.dialog()
    assert h, "the find dialog should appear after Ctrl+F"

    frames = []
    for _ in range(14):
        frames.append(ImageGrab.grab(bbox=cb.window_rect(h)).convert("RGB"))
        time.sleep(0.07)

    # Take the max inter-frame amplitude (0..255) per pixel across the RGB
    # channels, then use getbbox to measure the coverage of high-amplitude
    # (>50) pixels — not getdata (deprecated by Pillow).
    lo = frames[0]
    hi = frames[0]
    for f in frames[1:]:
        lo = ImageChops.darker(lo, f)
        hi = ImageChops.lighter(hi, f)
    amp = ImageChops.difference(lo, hi)
    box = amp.point(lambda v: 255 if v > 50 else 0).getbbox()
    varying = 0 if box is None else (box[2] - box[0]) * (box[3] - box[1])
    assert varying >= 5, (
        f"only {varying} pixels varied across 14 frames in the dialog — the "
        f"caret is not blinking (or the desktop is locked/occluded and the "
        f"pixel oracle is void)"
    )
