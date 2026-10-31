"""WYSIWYG full-feature on-app tests: full sample + cursor/formula/table multi-path coverage.

Three layers covered:

* **Sample layer**: `samples/wys-all-features.md` (H1-H6, inline formats,
  quotes/nesting, lists/tasks/nesting, code blocks, three table alignments,
  rule, inline/display formulas, HTML blocks, escapes) open -> page -> two-view
  round trip -> save **byte-for-byte unchanged**;
* **Cursor layer**: mouse click, double-click word, triple-click block, drag
  select, Shift extend, arrow keys/Home/End/Ctrl+Home/End, empty-block click,
  **whole crossing of atomic nodes (formula/image)**, with the status bar's
  "line/column" and disk bytes as dual oracles;
* **Formula/table layer**: inline formula ←/→ crossing and whole deletion,
  display-formula click positioning and Enter, invalid-formula fallback; table
  click/Tab/Shift+Tab/Enter(`<br>`)/↑↓ by column/cell-start backspace/auto-table.

Run (needs a GUI; build debug or release first):

    INFEDITOR_EXE=target/debug/infeditor.exe \
        python -m pytest tests_py/test_wys_all_features.py -v
"""

from __future__ import annotations

import re
import time
from pathlib import Path

from infed.driver import mean_diff

REPO = Path(__file__).resolve().parent.parent
SAMPLE = REPO / "samples" / "wys-all-features.md"

# Toolbar button centers (CSS px): the view group has **no leading caption**
# (2026-10: the "视图:" label was dropped — it was the toolbar's dead lead-in
# blank), so the two toggles start right at PAD_X: centers 39 / 105. They do
# not move across modes.
DOC_BTN = (39.0, 80.0)
TEXT_BTN = (105.0, 80.0)

# Scenario geometry (1280 wide, content top 96, line height 16, left padding 20)
Y_HEAD = 130.0
# Block geometry is the wys content's own line height (body font × 1.6), so
# these y values move with DEFAULT_FONT_SIZE: measured at 15px = the row of
# `第一段 alpha` in the caret tests' scene starts at y≈184, centre ≈196
# (at the old 14px default it started at 170 and 182 was the middle).
Y_PARA = 196.0
Y_LIST1 = 222.0
Y_TABLE_HDR = 130.0
Y_TABLE_ROW1 = 165.0
X_LEFT = 80.0
X_COL2 = 700.0

# ---- Common helpers -----------------------------------------------------------


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


def _status(a, timeout: float = 3.0) -> tuple[int, int, int | None]:
    """Status bar (line, column, total lines); the UIA tree lags, so poll until it appears."""
    deadline = time.time() + timeout
    while time.time() < deadline:
        ns = _uia_names(a)
        m = next((re.match(r"行 (\d+), 列 (\d+)", n) for n in ns if re.match(r"行 \d+, 列 \d+", n)), None)
        total = next((int(n[2:].split()[0]) for n in ns if re.match(r"共 \d+ 行", n)), None)
        if m:
            return int(m.group(1)), int(m.group(2)), total
        time.sleep(0.1)
    raise AssertionError(f"status bar not ready: {_uia_names(a)[:12]}")


def _has_name(a, needle: str, timeout: float = 5.0) -> bool:
    deadline = time.time() + timeout
    while time.time() < deadline:
        if any(needle in n for n in _uia_names(a)):
            return True
        time.sleep(0.1)
    return False


def _enter_wys(a) -> None:
    """Idempotently ensure the document (WYSIWYG) view.

    The view toggles are a **segmented control**: the active half is inert
    (`toolbar.rs`: `inert = disabled || (view_btn && on)`), so clicking 文档视图
    while already there reports nothing. Since the visual-first default a .md
    file *opens* there, which made the old "click toggles out, keep clicking"
    loop spin forever.

    Force a known state instead: click 文本视图 (silence ⇒ already text), then
    文档视图 -- that click always has a transition to report, so the status text
    doubles as proof the input landed. Two rounds absorb a lost first click
    (the previous test's window may still be closing)."""
    for _ in range(2):
        a.click_at_css(*TEXT_BTN)
        _has_name(a, "已切换至文本视图", timeout=1.2)  # silence ⇒ already text
        a.click_at_css(*DOC_BTN)
        if _has_name(a, "已切换至文档视图", timeout=4.0):
            return
    raise AssertionError("did not enter the WYSIWYG view")


def _scene(work_dir: Path, body: str, name: str = "case.md") -> Path:
    f = work_dir / name
    f.write_bytes(body.encode("utf-8"))
    return f


def _open(app_factory, f: Path):
    a = app_factory(files=[f])
    assert a.wait_title(f.name)
    _enter_wys(a)
    time.sleep(0.3)
    return a


def _click(a, x: float, y: float, settle: float = 0.4) -> None:
    a.click_at_css(x, y)
    time.sleep(settle)


def _save_if_dirty(a) -> None:
    if a.is_dirty():
        a.save(timeout=8.0)


def _disk(f: Path) -> str:
    return f.read_bytes().decode("utf-8")

# ---- 1. Full-feature sample: open / page / two views / byte-identical save ----


def test_all_features_sample_navigates_and_saves_verbatim(app_factory, work_dir):
    text = SAMPLE.read_bytes()
    body = text.decode("utf-8")
    f = _scene(work_dir, body, "all-features.md")
    a = _open(app_factory, f)

    line, col, total = _status(a)
    # The file opens in the document view (visual-first default), and the wys
    # model has no caret position *before* a block marker: "document start"
    # lands on the first block's text. This sample opens with
    # "# Heading Level 1", so the source coordinate is column 3 (right after
    # "# "), not (1,1) as it would be in the text view.
    assert (line, col) == (1, 3), "caret should sit at the start of the first block's text"
    assert total == body.count("\n"), f"total lines should be {body.count(chr(10))}, got {total}"
    assert not a.is_dirty(), "opening a canonical sample must not set dirty"

    # Keyboard navigation: Ctrl+End -> last line; Ctrl+Home -> first line
    a.keys("ctrl+end")
    time.sleep(0.6)
    line, _, _ = _status(a)
    assert line == total, f"Ctrl+End should reach the last line {total}, got {line}"
    a.screenshot("all_features_bottom")
    a.keys("ctrl+home")
    time.sleep(0.5)
    assert _status(a)[0] == 1

    # Page through screens: block rendering/formulas/tables each get a pass on different screens
    base = a.snapshot((0.0, 96.0, 1000.0, 600.0))
    for _ in range(3):
        a.keys("pagedown")
        time.sleep(0.4)
    a.screenshot("all_features_mid")
    assert mean_diff(base, a.snapshot((0.0, 96.0, 1000.0, 600.0))) > 3.0, "view should change after paging"

    # Two-view round trip: document view -> text -> document view
    a.click_at_css(*TEXT_BTN)
    assert _has_name(a, "已切换至文本视图"), "did not switch to text view (no status text)"
    a.click_at_css(*DOC_BTN)
    assert _has_name(a, "已切换至文档视图"), "did not switch back to the document view"
    time.sleep(0.5)
    a.click_at_css(*TEXT_BTN)
    assert _has_name(a, "已切换至文本视图"), "did not switch to text view (no status text)"
    a.click_at_css(*DOC_BTN)
    assert _has_name(a, "已切换至文档视图")
    time.sleep(0.5)

    # Pure browsing throughout: must not modify the document; bytes unchanged after save
    assert not a.is_dirty(), "pure browsing/view switching must not set dirty"
    a.keys("ctrl+s")
    time.sleep(0.6)
    assert f.read_bytes() == text, "sample was rewritten after save (non-canonical emit or unauthorized sync)"

# ---- 2. Caret positioning: multiple input methods -----------------------------


def _caret_case(app_factory, work_dir, body, action, expected, name):
    f = _scene(work_dir, body, f"{name}.md")
    a = _open(app_factory, f)
    action(a)
    time.sleep(0.5)
    _save_if_dirty(a)
    assert _disk(f) == expected, f"{name}: {_disk(f)!r}"
    return a


def test_caret_by_click_and_keyboard(app_factory, work_dir):
    body = "# 标题\n\n第一段 alpha\n"
    exp = "# 标题\n\n第一段 alphaX\n"

    def act(a):
        _click(a, X_LEFT, Y_PARA)
        assert _status(a)[0] == 3, "clicking the paragraph should land on line 3"
        a.keys("end")
        a.type_text("X", click=False, focus=False)

    _caret_case(app_factory, work_dir, body, act, exp, "click")

    def act_kb(a):
        a.keys("ctrl+home")
        a.keys("down")
        assert _status(a)[0] == 3, "down should reach line 3"
        a.keys("end")
        a.type_text("X", click=False, focus=False)

    _caret_case(app_factory, work_dir, body, act_kb, exp, "keyboard")


def test_caret_by_double_click_word(app_factory, work_dir):
    body = "# 标题\n\n第一段 alpha\n"
    exp = "# 标题\n\n第一段 W\n"

    def act(a):
        a.double_click_at_css(100.0, Y_PARA)
        time.sleep(0.3)
        a.type_text("W", click=False, focus=False)

    _caret_case(app_factory, work_dir, body, act, exp, "dblclick")


def test_caret_by_triple_click_block(app_factory, work_dir):
    body = "# 标题\n\n第一段 alpha\n"
    exp = "# 标题\n\nT\n"

    def act(a):
        a.triple_click_at_css(100.0, Y_PARA)
        time.sleep(0.3)
        a.type_text("T", click=False, focus=False)

    _caret_case(app_factory, work_dir, body, act, exp, "tripleclick")


def test_caret_by_drag_selection_cut(app_factory, work_dir):
    """Drag-select a word, Ctrl+X, save: exactly the dragged range is gone.

    The cut leaves a **trailing space** behind, and the wys emitter writes that
    as `&#32;` (`md::escape_line_end_spaces`): Markdown strips line-end
    whitespace on reparse, so a literal space would not survive the round trip.
    The space surviving *at all* is the assertion — the drag covered `alpha`
    only, not the space before it.
    """
    body = "# 标题\n\n第一段 alpha\n"
    exp = "# 标题\n\n第一段&#32;\n"

    def act(a):
        # 66px was the start of `alpha` at the old 14px body font; at 15px the
        # same word starts at 72px. Probed by reading the selection back off the
        # clipboard (72 -> "alpha"; 66 grabs the preceding space too, 76/80 lose
        # the first glyphs).
        a.drag_at_css(72.0, Y_PARA, 132.0, Y_PARA)
        time.sleep(0.4)
        a.keys("ctrl+x")
        time.sleep(0.4)

    _caret_case(app_factory, work_dir, body, act, exp, "dragcut")


def test_caret_by_shift_selection_bold(app_factory, work_dir):
    body = "# 标题\n\n第一段 alpha\n"
    exp = "# 标题\n\n第一段 **alpha**\n"

    def act(a):
        a.keys("ctrl+home")
        a.keys("down")
        a.keys("end")
        for _ in range(5):
            a.keys("shift+left")
        a.keys("ctrl+b")
        time.sleep(0.5)

    _caret_case(app_factory, work_dir, body, act, exp, "shiftbold")


def test_caret_in_new_empty_block(app_factory, work_dir):
    body = "# 标题\n\n第一段 alpha\n"
    exp = "# 标题\n\n第一段 alpha\n\nE\n"

    def act(a):
        a.keys("ctrl+end")
        a.keys("enter")
        # New empty block sits below the paragraph (content top + heading 42 + paragraph 40 + 16)
        _click(a, X_LEFT, 240.0)
        a.type_text("E", click=False, focus=False)

    _caret_case(app_factory, work_dir, body, act, exp, "emptyblock")


def test_caret_in_list_items(app_factory, work_dir):
    body = "# 标题\n\n- 列表一\n- 列表二\n"
    exp = "# 标题\n\n- 列表一\n- 列表二X\n"

    def act(a):
        _click(a, X_LEFT, 246.0)
        assert _status(a)[0] in (3, 4), "clicking a list item should land on a list line"
        a.keys("ctrl+end")
        a.keys("end")
        a.type_text("X", click=False, focus=False)

    _caret_case(app_factory, work_dir, body, act, exp, "listcaret")

# ---- 3. Formulas: atomic caret / deletion / click / display / invalid fallback ----


def test_inline_math_atom_caret_and_delete(app_factory, work_dir):
    """A formula at the caret shows its LaTeX and is edited character by
    character; a typed character at its **edge** still belongs to the line.

    Contract (in-place source expansion): the caret expands the formula it sits
    in or right next to, so the arrow keys walk the **visible source** instead of
    stepping over it — two steps from the block start put the caret on the
    formula's front edge (`$|E=mc^2$`), the next one inside it (`$E|…`). Typing
    at the edge prepends to the line (the formula is never split open), and a
    character typed inside the source edits the LaTeX in place: one formula in,
    one formula out.
    """
    body = "前 $E=mc^2$ 后\n"
    f = _scene(work_dir, body, "math-atom.md")
    a = _open(app_factory, f)

    # 2 steps: after 前 (0..3), then the formula's front edge (4). Its source is
    # on screen there, so the character goes in front of the `$` — the LaTeX
    # itself is untouched (the edge belongs to the surrounding text).
    a.keys("ctrl+home")
    for _ in range(2):
        a.keys("right")
    a.type_text("1", click=False, focus=False)
    time.sleep(0.5)
    _save_if_dirty(a)
    assert _disk(f) == "前 1$E=mc^2$ 后\n", _disk(f)

    # Drop the 1 and step once more: the caret is inside the visible source, so
    # typing edits the LaTeX in place instead of splitting the node.
    a.keys("backspace")
    a.keys("right")
    a.type_text("2", click=False, focus=False)
    time.sleep(0.5)
    _save_if_dirty(a)
    assert _disk(f) == "前 $E2=mc^2$ 后\n", _disk(f)

    # Undo takes the edit back; the formula is still one whole node (the undo
    # granularity is not the point here — only that no `$E…$` half survives).
    a.keys("ctrl+z")
    time.sleep(0.6)
    _save_if_dirty(a)
    d = _disk(f)
    assert "$E=mc^2$" in d and "$E2=" not in d, d


def test_inline_math_mouse_click_never_enters_source(app_factory, work_dir):
    body = "前 $E=mc^2$ 后\n"
    f = _scene(work_dir, body, "math-click.md")
    a = _open(app_factory, f)

    _click(a, X_LEFT, 124.0)
    a.type_text("Q", click=False, focus=False)
    time.sleep(0.5)
    _save_if_dirty(a)
    d = _disk(f)
    # Clicked on the atom: the caret snaps to the atom boundary; the formula source must not be split
    assert "$E=mc^2$" in d, d
    assert d.replace("Q", "") == body, f"formula was split open: {d!r}"


def test_image_atom_delete_whole(app_factory, work_dir):
    body = "前 $E=mc^2$ 后 ![图](./nope.png) 尾\n"
    f = _scene(work_dir, body, "img-atom.md")
    a = _open(app_factory, f)

    # From the document end: one step lands before 尾, the next steps over the
    # **whole image** (its source is not on screen, so it is one atom) to its
    # front edge; -> then answers its end, where Backspace takes the image whole
    # instead of eating one alt character.
    a.keys("ctrl+end")
    a.keys("left")
    a.keys("left")
    a.keys("right")
    a.keys("backspace")
    time.sleep(0.5)
    _save_if_dirty(a)
    assert _disk(f) == "前 $E=mc^2$ 后  尾\n", _disk(f)


def test_display_math_click_and_enter(app_factory, work_dir):
    body = "$$\n\\int_0^1 x\\,dx\n$$\n\nafter\n"
    f = _scene(work_dir, body, "display-math.md")
    a = _open(app_factory, f)

    # Click the display formula's **left half**: the caret lands at block start
    # (line 1); the right half is block end (closing $$). The formula is centered
    # and the block top has a 12px margin; y=120 falls in the gap, use 130.
    _click(a, 600.0, 130.0)
    line, _, _ = _status(a)
    assert line == 1, f"clicking the display formula's left half should land on line 1, got {line}"
    a.keys("end")
    a.keys("enter")
    a.type_text("tail", click=False, focus=False)
    time.sleep(0.6)
    _save_if_dirty(a)
    assert _disk(f) == "$$\n\\int_0^1 x\\,dx\n$$\n\ntail\n\nafter\n", _disk(f)


def test_invalid_math_falls_back_without_crash(app_factory, work_dir):
    body = "前 $\\frac{$ 后\n"
    f = _scene(work_dir, body, "math-bad.md")
    a = _open(app_factory, f)
    a.screenshot("invalid_math")
    # Process alive + navigation/editing still work; formula source kept verbatim
    # (render fallback doesn't touch the model)
    a.keys("ctrl+end")
    line, _, _ = _status(a)
    assert line == 1
    assert not a.is_dirty(), "render fallback must not modify the document"
    assert _disk(f) == body

# ---- 4. Tables: click / Tab / Enter / up-down / backspace / auto-table --------

TABLE = "| h1 | h2 |\n| --- | --- |\n| a | b |\n| c | d |\n"


def _table_case(app_factory, work_dir, action, expected, name):
    f = _scene(work_dir, TABLE, f"{name}.md")
    a = _open(app_factory, f)
    action(a)
    time.sleep(0.6)
    _save_if_dirty(a)
    assert _disk(f) == expected, f"{name}: {_disk(f)!r}"


def test_table_cell_click_and_type(app_factory, work_dir):
    def act(a):
        _click(a, X_LEFT, Y_TABLE_HDR)
        assert _status(a)[0] == 1, "clicking the header should land on line 1"
        a.keys("end")
        a.type_text("H", click=False, focus=False)

    _table_case(
        app_factory, work_dir, act,
        "| h1H | h2 |\n|---|---|\n| a | b |\n| c | d |\n", "table-click",
    )


def test_table_tab_moves_and_replaces_cell(app_factory, work_dir):
    def act(a):
        _click(a, X_LEFT, Y_TABLE_HDR)
        a.keys("tab")          # select the whole next cell
        a.type_text("Z", click=False, focus=False)

    _table_case(
        app_factory, work_dir, act,
        "| h1 | Z |\n|---|---|\n| a | b |\n| c | d |\n", "table-tab",
    )


def test_table_enter_inserts_br(app_factory, work_dir):
    def act(a):
        _click(a, X_LEFT, Y_TABLE_HDR)
        a.keys("end")
        a.keys("enter")
        a.type_text("B", click=False, focus=False)

    _table_case(
        app_factory, work_dir, act,
        "| h1<br>B | h2 |\n|---|---|\n| a | b |\n| c | d |\n", "table-enter",
    )


def test_table_down_moves_by_column(app_factory, work_dir):
    def act(a):
        _click(a, X_COL2, Y_TABLE_ROW1)   # b cell (column 2, first data row)
        assert _status(a)[0] == 3
        a.keys("down")
        a.type_text("D", click=False, focus=False)

    _table_case(
        app_factory, work_dir, act,
        "| h1 | h2 |\n|---|---|\n| a | b |\n| c | dD |\n", "table-down",
    )


def test_table_backspace_at_cell_start_is_pure_move(app_factory, work_dir):
    def act(a):
        _click(a, X_COL2, Y_TABLE_ROW1)   # b cell
        a.keys("home")
        a.keys("backspace")               # move into the previous cell (end of a), table structure untouched
        a.type_text("K", click=False, focus=False)

    _table_case(
        app_factory, work_dir, act,
        "| h1 | h2 |\n|---|---|\n| aK | b |\n| c | d |\n", "table-backspace",
    )


def test_table_autoformat_on_enter(app_factory, work_dir):
    body = "pre\n"

    def act(a):
        a.keys("ctrl+end")
        a.keys("enter")
        a.type_text("| a | b |", click=False, focus=False)
        time.sleep(0.3)
        a.keys("enter")                    # auto-table
        time.sleep(0.5)
        a.type_text("1", click=False, focus=False)

    f = _scene(work_dir, body, "table-auto.md")
    a = _open(app_factory, f)
    act(a)
    time.sleep(0.6)
    _save_if_dirty(a)
    assert _disk(f) == "pre\n\n| a | b |\n|---|---|\n| 1 |  |\n", _disk(f)


def test_table_with_math_and_marks_roundtrip(app_factory, work_dir):
    # Use the emit canonical form (WYS save normalizes table separator rows --
    # the status bar already says "保存时格式将规范化"; this test only checks
    # that caret/formulas are not rewritten)
    body = "| **粗** | $a^2$ |\n|---|---|\n| $x$ | b |\n"
    f = _scene(work_dir, body, "table-math.md")
    a = _open(app_factory, f)
    _click(a, 300.0, Y_TABLE_ROW1)   # click the data cell containing a formula
    assert _status(a)[0] == 3
    a.keys("left")                   # move around the formula atom, must not enter source
    a.keys("right")
    assert not a.is_dirty(), "pure caret movement must not set dirty"
    a.keys("ctrl+s")
    time.sleep(0.5)
    assert f.read_bytes() == body.encode(), f"table with formulas/marks rewritten on round trip: {_disk(f)!r}"

# ---- 5. Formula render size/centering + caret height; blank-line save and empty-block caret ----


def _open_logged(app_factory, f):
    a = app_factory(files=[f], env_extra={"INF_WYS_LOG": "1", "INF_SHELL_LOG": "1"})
    assert a.wait_title(f.name)
    _enter_wys(a)
    time.sleep(0.4)
    return a


def _caret_logs(a, n: int = 1) -> list[dict]:
    """Read the caret rects (left/top/height, CSS px) of the last n `[wys-caret]` entries in shell.log.

    Coordinates come from the log's
    `overlays=Overlays { caret: Some(OverlayRect { x, y, w, h }) }`:
    **relative to the wys view** (a block at viewport y=96 logs y≈0). The old
    parser's `css=Some("left:…")` is no longer in the log (caret_debug now prints
    the geom breakdown directly) -- looking for it only yields "failed to parse"
    false failures.
    """
    import re

    log = Path(a.config_dir) / "shell.log"
    assert log.exists(), f"missing diagnostic log {log}"
    lines = [l for l in log.read_text(encoding="utf-8", errors="replace").splitlines()
             if "[wys-caret]" in l]
    out = []
    for line in lines[-n:]:
        m = re.search(r"OverlayRect \{ x: ([\d.]+), y: ([\d.]+), w: ([\d.]+), h: ([\d.]+)",
                      line)
        if m:
            out.append({"left": float(m.group(1)), "top": float(m.group(2)),
                        "height": float(m.group(4))})
    assert len(out) == n, f"failed to parse {n} caret log entries: {lines[-3:]}"
    return out


def test_display_math_scale_center_and_caret(app_factory, work_dir):
    """Display formula: size follows the font size, horizontally centered, caret hugs the formula's left/right edges (not line start/end)."""
    f = _scene(work_dir, "$$\n\\frac{a}{b}\n$$\n", "dm-scale.md")
    a = _open_logged(app_factory, f)
    time.sleep(0.5)
    a.screenshot("display_math_scale")

    # Formula ink bounding box (content area only, excluding the window frame):
    # height >=28 CSS px (the old 12px font only gave ~21px), horizontal center in
    # the middle of the window (the old impl hugged the left at x≈20).
    # Only look at the central vertical band where the formula should be
    # (excluding the line-start caret, status bar, window frame).
    box = (520.0, 96.0, 800.0, 300.0)
    im = a.snapshot(box)
    scale = a.dpi_scale()
    xs, ys = [], []
    for y in range(im.height):
        for x in range(im.width):
            r, g, b = im.getpixel((x, y))
            if r + g + b < 420:
                xs.append(x)
                ys.append(y)
    assert xs, "no ink at all in the formula region (render failed? or not centered)"
    w_css = (max(xs) - min(xs)) / scale
    h_css = (max(ys) - min(ys)) / scale
    # Thresholds come from the **current contract**: display formula font size =
    # body font size × 1.2 (math.rs DISPLAY_SCALE, 14px -> 17px); the actual box
    # height of `\frac{a}{b}` is 30.1px and the dark-ink bbox is 26.7px
    # (antialiased edges cut off by the r+g+b<420 threshold). The old 28/35px
    # values were measured in the **double-scaling era** (1.2² ≈ 20px font) and no
    # longer hold after DESIGN §5.9 removed the double multiply -- using them as
    # the gate would report the fix as a regression. A floor of 25 still catches
    # "reverted to inline layout" (14px font ≈ 21px ink).
    assert h_css >= 25.0, f"display formula height {h_css:.1f}px too small (font size not applied?)"
    assert 4.0 <= w_css <= 80.0, f"formula width abnormal: {w_css:.1f}"

    # Caret: click the formula -> Home (block start) / End (block end); both should
    # hug the formula's edges and have height = formula height
    _click(a, 640.0, 130.0)
    # End **first**: the click can land on the block's left edge, and a Home
    # that changes nothing is not consumed, so it logs no caret rect at all
    # (the `[wys-caret]` line only carries real moves). Ending first guarantees
    # both ends are reached from the opposite side.
    a.keys("end")
    time.sleep(0.4)
    left1 = _caret_logs(a, 1)[0]
    a.keys("home")
    time.sleep(0.4)
    left0 = _caret_logs(a, 1)[0]
    assert left0["left"] > 300.0, f"block-start caret not hugging the formula's left edge: {left0}"
    assert left1["left"] > left0["left"], f"block-end caret should be right of block-start: {left0} vs {left1}"
    # Caret rect = formula box height (measured 30.1px), not the 21.3px line height
    assert left0["height"] >= 28.0, f"display formula caret height too small: {left0}"
    assert abs(left0["height"] - h_css) <= 8.0, f"caret height should ≈ formula height: {left0} vs {h_css:.1f}"


def test_enter_at_end_preserves_blank_line_on_save(app_factory, work_dir):
    """A blank line created by Enter at document end must survive saving (the old impl's emit tail-trimming swallowed it)."""
    f = _scene(work_dir, "A\n", "blank-tail.md")
    a = _open(app_factory, f)
    a.keys("ctrl+end")
    a.keys("enter")
    time.sleep(0.4)
    _save_if_dirty(a)
    assert _disk(f) == "A\n\n", f"trailing blank line lost: {_disk(f)!r}"


def test_multiple_empty_paragraphs_edit_and_save(app_factory, work_dir):
    """Two Enters -> both blank lines present; typing on each blank line then saving yields the correct structure."""
    f = _scene(work_dir, "前文\n\n后文\n", "blank-multi.md")
    a = _open(app_factory, f)
    a.keys("ctrl+end")
    a.keys("enter")
    a.keys("enter")
    time.sleep(0.3)
    a.type_text("A", click=False, focus=False)
    a.keys("up")
    a.type_text("B", click=False, focus=False)
    time.sleep(0.4)
    _save_if_dirty(a)
    assert _disk(f) == "前文\n\n后文\n\nB\n\nA\n", _disk(f)


def test_empty_block_caret_moves_down_per_line(app_factory, work_dir):
    """Empty-block caret height/position: each Enter moves the caret down about one line (24+16), height >=20."""
    f = _scene(work_dir, "前文\n\n后文\n", "blank-caret.md")
    a = _open_logged(app_factory, f)
    a.keys("ctrl+end")
    a.keys("enter")
    a.keys("home")
    time.sleep(0.4)
    first = _caret_logs(a, 1)[0]
    a.keys("enter")
    a.keys("home")
    time.sleep(0.4)
    second = _caret_logs(a, 1)[0]
    assert second["top"] > first["top"], f"new blank-line caret did not move down: {first} vs {second}"
    delta = second["top"] - first["top"]
    assert 30.0 <= delta <= 50.0, f"blank-line step abnormal: {delta:.1f}px (should be ≈40)"
    assert first["height"] >= 20.0 and second["height"] >= 20.0, f"empty-block caret height abnormal: {first} {second}"
