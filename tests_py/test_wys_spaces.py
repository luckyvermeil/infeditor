"""WYSIWYG space compatibility: every typed space must be a real glyph.

The bug pinned here (2026-10): `.wys-content` had no `white-space:pre-wrap`,
so runs of spaces collapsed *visually only* — they stayed in the DOM text
node but vanished from the layout. The caret/selection byte ledger (geom)
assumes layout bytes == plain bytes for non-atom text, so typing a few spaces
misplaced the caret and click hit-testing by the swallowed width.

Oracle: the **pixel width of the native selection wash**. blitz paints the
text selection as rgba(100,140,255,110); over the light theme's white
background that blends to a light blue (~188,205,255), over the dark theme to
a dim blue (~53,71,121) — both unmistakably "blue dominant". Home → Shift+End
selects the whole line, and the blue run's horizontal extent is the line's
*visual* width. Every typed space must grow it; under the collapse bug the
width does not move. No font metrics are hardcoded.

Measure/collapse discipline: a measurement leaves the line selected, and the
next keystroke would *replace* the selection — collapse first (End before
typing at the line end, Home before typing at column 0), then type, then
re-select.

The emit side of the space contract (leading spaces escape as `&#32;` so a
save/reload keeps every offset) is asserted on disk bytes at the end.

Run (needs a GUI; build debug or release first):

    INFEDITOR_EXE=target/debug/infeditor.exe \
        python -m pytest tests_py/test_wys_spaces.py -v
"""

from __future__ import annotations

import time

# The document-view toolbar button (first of the two view buttons; the strip
# has no leading caption, so the toggles start right at the toolbar's left
# padding — centers 39 / 105).
DOC_BTN = (39.0, 80.0)
TEXT_BTN = (105.0, 80.0)

# Click point inside the first paragraph line. Content top = CHROME_TOP(64) +
# markdown toolbar(32) + .wys-content padding-top(16) = 112; a paragraph line
# spans 112..136, so 124 is its middle. X=80 is inside the text; clicks on
# empty space fall to end-of-document, which is the same (only) block here.
LINE_CLICK = (80.0, 124.0)
# Sampling band. Measured geometry (probe, 2026-10): the first line's
# selection wash spans window y 122..149, x from ~30 (content left) rightwards.
# The floating selection toolbar pops 250ms after the selection stabilizes and
# covers the same rows — so the scan must happen **fast** after Shift+End and
# sample the wash rows only. A screen grab can also catch a transient foreign
# window; the blue-dominant filter plus the model cross-checks (_sel_text)
# keep that from silently passing.
LINE_BOX = (0.0, 100.0, 300.0, 165.0)
_ROWS = range(24, 47)

BODY = "az\n"
SPACES = 4


# ---- UIA probes (menu-bar rows exist in the tree only while the menu is open)

def _uia_names(a) -> list[str]:
    from pywinauto import Desktop

    win = Desktop(backend="uia").window(handle=a.main_hwnd).wrapper_object()
    out: list[str] = []

    def rec(c, d=0):
        try:
            n = c.element_info.name or ""
            if n:
                out.append(n)
        except Exception:
            pass
        if d < 14:
            try:
                for ch in c.children():
                    rec(ch, d + 1)
            except Exception:
                pass

    rec(win)
    return out


def _uia_has(a, needle: str, timeout: float = 5.0) -> bool:
    deadline = time.time() + timeout
    while time.time() < deadline:
        if any(needle in n for n in _uia_names(a)):
            return True
        time.sleep(0.1)
    return False


def _enter_wys(a) -> None:
    """Idempotently ensure the document view (same stance as
    test_wys_all_features).

    The active view toggle is inert, so a click on 文档视图 while already in the
    document view reports nothing and the old "toggles out" loop could never
    return. Force a known state instead: 文本视图 first (silence ⇒ already
    text), then 文档视图, whose status text proves the switch landed."""
    for _ in range(2):
        a.click_at_css(*TEXT_BTN)
        _uia_has(a, "已切换至文本视图", timeout=1.2)  # silence ⇒ already text
        a.click_at_css(*DOC_BTN)
        if _uia_has(a, "已切换至文档视图", timeout=4.0):
            return
    raise AssertionError("did not enter the document view")


# ---- Selection-width oracle

def _sel_width(a) -> int:
    """Horizontal pixel extent of the selection wash on the target line.

    One fast scan: the wash is painted a frame after Shift+End, and the
    floating selection toolbar pops 250ms later right on top of it — measure
    between the two. Filter = "blue clearly dominant", which matches the
    blended wash on both themes and nothing else on the line (text and the
    caret bar are gray).
    """
    a.keys("home")
    time.sleep(0.12)
    a.keys("shift+end")
    time.sleep(0.12)
    img = a.snapshot(LINE_BOX)
    px = img.load()
    w, h = img.size
    xs = []
    for x in range(w):
        for y in _ROWS:
            if y >= h:
                break
            r, g, b = px[x, y]
            if b >= 90 and b - r >= 30 and b - g >= 15:
                xs.append(x)
                break
    if not xs:
        raise AssertionError(
            "selection wash not found — scanned too late (the floating "
            "toolbar covers it after 250ms) or the line is not selected"
        )
    return xs[-1] - xs[0] + 1


def _select_line(a) -> int:
    """Home → Shift+End → the line's visual width in pixels."""
    a.keys("home")
    time.sleep(0.15)
    a.keys("shift+end")
    return _sel_width(a)


def _sel_text(a) -> str:
    """Clipboard ground truth of the current selection (Home → Shift+End →
    Ctrl+C). Independent of pixels: this is the *model* selection. The wys
    copy is **markdown-serialized** (`&#32;` escapes included), so callers
    decode with `_plain`."""
    a.keys("home")
    time.sleep(0.1)
    a.keys("shift+end")
    return a.copy_selection()


def _plain(md: str) -> str:
    """Decode the emit-side space entities back to spaces (the clipboard is
    markdown: `&#32;` is the leading/trailing-space protection)."""
    return md.replace("&#32;", " ")


def test_interior_spaces_are_real_glyphs_and_reach_disk(app_factory, work_dir):
    f = work_dir / "spaces.md"
    f.write_text(BODY, encoding="utf-8")
    a = app_factory(files=[f])
    assert a.wait_title("spaces.md")
    _enter_wys(a)
    time.sleep(0.3)

    # Baseline: the whole line "az" selected — pixels AND model must agree.
    a.click_at_css(*LINE_CLICK)
    time.sleep(0.3)
    w_base = _select_line(a)
    assert w_base >= 10, f"baseline selection over 'az' too narrow: {w_base}px"
    assert _plain(_sel_text(a)) == "az", "baseline selection should be 'az'"

    # Interior run: type 4 spaces then an anchor letter, measuring after every
    # keystroke. Every space must widen the selection; under the collapse bug
    # spaces 2..4 leave the width untouched (the run renders as one space).
    widths = [w_base]
    for _ in range(SPACES):
        a.keys("end")  # collapse at the line end: typing must not replace the selection
        a.type_text(" ", click=False, focus=False)
        widths.append(_select_line(a))
    a.keys("end")
    a.type_text("z", click=False, focus=False)
    w_final = _select_line(a)

    for i in range(1, len(widths)):
        grew = widths[i] - widths[i - 1]
        assert grew >= 3, (
            f"space #{i} grew the line's visual width by only {grew}px "
            f"(widths={widths}) — spaces are collapsing in the layout"
        )
    anchor = w_final - widths[-1]
    assert anchor >= 3, (
        f"the anchor letter after the space run added only {anchor}px "
        f"(widths={widths}, final={w_final})"
    )
    # Model side: the whole line is the typed content, spaces included.
    assert _plain(_sel_text(a)) == "az    z", (
        f"the line's model content must be the typed text: {_sel_text(a)!r}"
    )

    # Save: interior spaces stay raw and the disk line decodes back to the
    # typed content.
    a.keys("end")
    a.save()
    line0 = f.read_text(encoding="utf-8").splitlines()[0]
    assert line0.replace("&#32;", " ") == "az    z", (
        f"disk line must decode back to the typed content: {line0!r}"
    )


def test_leading_spaces_stay_visible_and_survive_save(app_factory, work_dir):
    f = work_dir / "leading.md"
    f.write_text(BODY, encoding="utf-8")
    a = app_factory(files=[f])
    assert a.wait_title("leading.md")
    _enter_wys(a)
    time.sleep(0.3)

    a.click_at_css(*LINE_CLICK)
    time.sleep(0.3)
    w_base = _select_line(a)

    # Leading run: Home collapses at column 0; every typed space must widen
    # the line. Under the collapse bug leading spaces vanish entirely (the
    # width does not move).
    widths = [w_base]
    for _ in range(SPACES):
        a.keys("home")  # collapse at column 0: typing must not replace the selection
        a.type_text(" ", click=False, focus=False)
        widths.append(_select_line(a))
    for i in range(1, len(widths)):
        grew = widths[i] - widths[i - 1]
        assert grew >= 3, (
            f"leading space #{i} grew the line's visual width by only {grew}px "
            f"(widths={widths}) — leading spaces are collapsing away"
        )
    assert _plain(_sel_text(a)) == "    az", (
        f"the line's model content must be the typed text: {_sel_text(a)!r}"
    )

    # Emit side: leading spaces are escaped as `&#32;` (raw ones would be
    # stripped by CommonMark on reload — or turn 4 of them into an indented
    # code block), so the disk line decodes back to the typed content.
    a.keys("end")
    a.save()
    line0 = f.read_text(encoding="utf-8").splitlines()[0]
    assert line0.replace("&#32;", " ") == "    az", (
        f"disk line must decode back to the typed content: {line0!r}"
    )
    assert "&#32;" in line0, (
        f"leading spaces must be entity-escaped on disk (CommonMark would "
        f"strip them on reload): {line0!r}"
    )
