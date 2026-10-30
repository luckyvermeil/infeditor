"""GUI end-to-end regressions for fixes made in this repo-wide audit round.

cargo-layer unit/integration regressions already exist (see the #[test]s in
src/buffer/doc.rs, src/io/file.rs, src/ui/commands/csv.rs, etc.); this file
only pins eight scenarios that go **through the full GUI pipeline**:

1. Typing after a save must re-dirty the document (doc.rs hpos accounting
   fix — typing merged into an already-saved entry did not mark dirty, so
   closing the window never warned about lost work). oracle = the `*` dirty
   marker in the OS title.
2. A >=64MB mmap-backed document must save back to the **original path** on
   Ctrl+S (file.rs share mode fix — MOVEFILE_REPLACE_EXISTING used to clash
   with the mapped handle and every save reported a sharing violation).
   oracle = first-line bytes on disk.
3. CSV cross-line field with an **unclosed** quote: insert column / sort
   must refuse wholesale (csv.rs csv_struct_guard / csv_sort gained per-line
   quote-closure checks — the existing row-map continuity + record-count
   guards could not catch it and the cross-line field was shredded on
   write-back). oracle = the refusal text in the status bar + unchanged disk
   bytes + document not dirty.
4. Replace-all above the 4MB threshold goes through the **background** scan
   and must land on disk (find.rs SearchJob's doc_id/gen validation is new
   this round; this test pins the happy path against regressions).
   oracle = disk bytes.
5. An open menu must close when a click lands in the WYSIWYG view (the view
   reports primary presses to the host; before the fix the dropdown hung on
   screen because its panel has no backdrop). oracle = the dropdown rows
   leave the UIA tree.
6. CSV sorting is a **view lens**, not an edit: the table reorders, the
   document stays clean and the file keeps its byte order (the sort used to
   rewrite the records on disk as one undo step). oracle = the three status
   texts of the header-click cycle + disk bytes.
7. The CSV header row must be a **row of the table**: every rendered column
   gets a header cell, blank filler columns included (the header loop used to
   stop at the last header field, leaving a bare white strip without rules
   where the body below shows a grid). oracle = pixel colour of the same blank
   column in the header row vs the body row.
8. A pointer drag across an inline formula must keep the caret on the formula's
   **front edge**, not walk back mid-formula. A typeset formula is an SVG with
   no text node, so the text hit-test answered with a neighbouring text's edge
   and flipped between the two sides as the pointer crossed the box
   (`Geom::drag_pos_at` answers the whole box with the front edge; the carrier's
   box spans the line band, so the ink's margins are included too).
   oracle = the copied selection: no `$` while the pointer is over it.

Clicking a self-drawn control: geometry is the **only** source, and every
geometric click has to prove it landed (see `_click_toolbar_button`). accesskit
reports a 0x0 rect for every toolbar node, so a "found by name" button still
yields a point in the window corner — this file used to click that point and
assert only "the document is still clean", which held whether or not the click
reached the app. `infed.driver` now refuses any CSS point outside the client
area (`_check_click_point`) and each toolbar click asserts the status text its
command produces.
"""

from __future__ import annotations

import time

from infed.driver import mean_diff

# ---- Geometry (CSS px, same constants as toolbar::layout / csvtable.rs) -----

TITLEBAR_H = 36.0
TABS_H = 28.0
TOOLBAR_H = 32.0
CHROME_TOP = TITLEBAR_H + TABS_H                 # 64: top of the toolbar band

PAD_X = 8.0                                      # toolbar::PAD_X
GAP = 4.0                                        # toolbar::GAP
SEP_W = 13.0                                     # toolbar::SEP_W
BTN_PAD_X = 7.0                                  # toolbar::BTN_PAD_X
BTN_H = 22.0                                     # toolbar::BTN_H
WIDE = 12.0                                      # toolbar::WIDE (fullwidth glyph)
NARROW = WIDE * 0.58                             # toolbar::NARROW (ASCII glyph)

# Button vertical center: (TOOLBAR_H - BTN_H)/2 offset + half the button.
TOOLBAR_Y = CHROME_TOP + (TOOLBAR_H - BTN_H) / 2.0 + BTN_H / 2.0          # 80

VIEW_Y = CHROME_TOP + TOOLBAR_H                  # 96: content top
STRIP_H = 18.0                                   # column-letter strip
# Column-letter strip center: selects the whole column (the "click the column
# letter" gesture the insert/delete-column commands ask for).
STRIP_Y = VIEW_Y + 0.5 * STRIP_H                 # 105
ROW_H = 20.0                                     # data row / header row height
# Column-header cell center: strip + half a header row (`head_h` == lh >= ROW_H)
HEADER_Y = VIEW_Y + STRIP_H + 0.5 * ROW_H        # 124
# Column A center: row-number gutter (~29) + half of COL_MIN (40)
COL_A_X = 48.0

# The CSV toolbar's groups in `toolbar::csv_items` order. `None` = `Group::bare`
# (the view toggles carry **no** caption, so the first button starts at PAD_X);
# a name is rendered as `"名称:"` ahead of the group. The "分隔符:" entry is a
# `ToolBtn::group_label` placeholder: laid out like a button, not clickable.
#
# Cross-check: the same two view labels in the markdown band give centers 39/105
# (test_markdown_features.DOC_X / TEXT_X, long verified on screen) — if this table
# ever starts producing other numbers for them, the caption rule above broke.
CSV_GROUPS: list[tuple[str | None, list[str]]] = [
    (None, ["表格视图", "文本视图"]),
    ("解析", ["首行标题", "分隔符:", ",", "Tab", ";", "|"]),
    ("编辑", ["行上+", "行下+", "删行", "列左+", "列右+", "删列"]),
]

# Cross-line field with an unclosed quote: record2 starts at line 1 → row map
# is continuous [0,1] and the whole-document reparse yields record count ==
# line count — both existing guards let it through before the fix.
CSV_UNCLOSED = 'a,b\n"x\ny\n'

# A table that can really be sorted (4 records: header + 3 data rows), column A
# deliberately out of order so a lens has something to reorder.
CSV_SORTABLE = "n,v\n3,c\n1,a\n2,b\n"

# ---- UIA oracles -----------------------------------------------------------


def _uia_names(app, max_depth: int = 14, attempts: int = 3) -> list[str]:
    """All non-empty names in the main window's UIA tree (one tree build).

    States built from names only (status bar, menu rows, tab titles) are sound:
    accesskit may report a 0x0 rect, but the *name* is what those oracles read.
    Anything that needs geometry must not trust this tree.

    Thin alias for `App.a11y_names` -- the walk lives in the driver so every test
    file shares one implementation (it is a whole-DOM COM traversal, easy to
    get subtly wrong: a failed walk must not read as "nothing there").
    """
    return app.a11y_names(max_depth=max_depth, attempts=attempts)


def _uia_has(app, needle: str, timeout: float = 4.0) -> bool:
    """Poll until `needle` appears in the UIA tree."""
    deadline = time.time() + timeout
    while time.time() < deadline:
        if any(needle in n for n in _uia_names(app)):
            return True
        time.sleep(0.1)
    return False


def _uia_missing(app, needle: str, timeout: float = 4.0) -> bool:
    """Poll until `needle` is gone from the UIA tree.

    An **empty** snapshot is not evidence of absence: accesskit builds lazily and
    a failed walk also yields no names, so "the UI could not be read" would
    otherwise read as "the menu closed". Absence only counts once the tree is
    populated (the window title alone guarantees that).
    """
    deadline = time.time() + timeout
    while time.time() < deadline:
        names = _uia_names(app)
        if names and not any(needle in n for n in names):
            return True
        time.sleep(0.1)
    return False

# ---- Toolbar geometry ------------------------------------------------------


def _btn_w(label: str) -> float:
    """toolbar::btn_width: ASCII ≈ NARROW, fullwidth ≈ WIDE, +BTN_PAD_X each
    side, never narrower than the button height (a square floor)."""
    text = sum(NARROW if c.isascii() else WIDE for c in label)
    return max(text + BTN_PAD_X * 2.0, BTN_H)


def _toolbar_x(target: str) -> float:
    """Button **center** x in the CSV toolbar (mirrors `toolbar::layout` over
    `toolbar::csv_items`).

    Raises for an unknown label instead of returning a sentinel: a `-1` was
    swallowed by the old `assert x > 0` after the fact, which is how a wrong
    table survived. `toolbar::tests::csv_toolbar_button_positions` pins the same
    numbers on the Rust side.
    """
    x = PAD_X
    for gi, (caption, items) in enumerate(CSV_GROUPS):
        if gi:
            x += SEP_W
        if caption is not None:
            x += _btn_w(f"{caption}:") + GAP
        for bi, label in enumerate(items):
            if bi:
                x += GAP
            w = _btn_w(label)
            if label == target:
                return x + w / 2.0
            x += w
    raise KeyError(f"the CSV toolbar has no button {target!r}; known: "
                   f"{[i for _, items in CSV_GROUPS for i in items]}")


def _click_toolbar_button(app, label: str, *, expect: str) -> None:
    """Click a toolbar button and **prove the click landed**.

    `expect` is a status-bar substring the button's command writes; the status
    bar is in the UIA tree, so it doubles as the "the app saw this click"
    signal. Without it the helper cannot tell "clicked the button" from
    "clicked 200px of empty toolbar", and a test asserting "the document is
    still clean" passes either way — the false-green this file shipped with.

    The click point comes from `_toolbar_x` (layout constants), never from UIA:
    every self-drawn toolbar button reports a 0x0 rect, so a name hit converts
    to a point in the window's corner. `driver.click_at_css` also refuses
    points outside the client area, so a stale constant fails loudly here
    instead of on the desktop.
    """
    x = _toolbar_x(label)
    # Activate the accesskit tree *before* the timed poll: the first UIA walk is
    # what triggers the build (test_a11y budgets 6s for it) and would otherwise
    # eat the whole status-text timeout.
    _uia_names(app)
    app.click_at_css(x, TOOLBAR_Y)
    seen: list[str] = []
    deadline = time.time() + 4.0
    while time.time() < deadline:
        seen = _uia_names(app)
        if any(expect in n for n in seen):
            return
        time.sleep(0.1)
    raise AssertionError(
        f"clicking {label!r} at ({x:.1f}, {TOOLBAR_Y:.1f}) never produced "
        f"{expect!r} in the status bar, so the click missed the button — check "
        f"_toolbar_x against toolbar::layout. UIA names: {seen[:24]!r}"
    )

# ---- 1. Typing after save keeps the dirty flag ------------------------------


def test_typing_after_save_keeps_dirty_flag(app_factory, work_dir):
    """After saving (within the merge window), further typing must bring the
    `*` back to the title.

    Regression: typing merged into an **already-saved** undo entry did not
    bump hpos → is_dirty() stayed false. Even if a slow machine misses the
    800ms merge window (taking the standalone-transaction path), this test
    must still pass — it only hits the regression trap when timing lands on
    the merge path, so it never fails spuriously.
    """
    f = work_dir / "dirty.txt"
    f.write_text("", encoding="utf-8")
    a = app_factory(files=[f])
    assert a.wait_title("dirty.txt")
    time.sleep(0.5)
    a.focus_click()

    # "abc" in one burst (merged into a single undo entry), then save, then
    # "d" — all three within the 800ms merge window, so "d" merges into the
    # saved entry
    a.type_text("abc", click=False, focus=False)
    a.keys("ctrl+s", focus=False)
    deadline = time.time() + 5.0
    while time.time() < deadline and a.is_dirty():
        time.sleep(0.05)
    assert not a.is_dirty(), "dirty flag should disappear after save"
    a.type_text("d", click=False, focus=False)
    time.sleep(0.6)  # wait for the title to refresh across frames
    assert a.is_dirty(), (
        "typing after save must re-dirty the document (regression: the merge "
        "path missed the hpos bump)"
    )
    a.save()
    assert f.read_text(encoding="utf-8") == "abcd"

# ---- 2. Large mmap document saves back to the original path -----------------


def test_mmap_document_saves_back_to_original_path(app_factory, work_dir):
    """A >=64MB (mmap path) document must be written back to the original file
    on Ctrl+S.

    Regression: the mmap handle's share_mode was FILE_SHARE_READ only, while
    the save's rename(REPLACE_EXISTING) needs DELETE access on the target →
    every save hit ERROR_SHARING_VIOLATION and large documents could never
    be saved directly.
    """
    f = work_dir / "huge.log"
    line = "0123456789abcdef\n"          # 16 B
    n = 64 * 1024 * 1024 // 16 + 10     # ≈68 MB, safely past HUGE_THRESHOLD
    with open(f, "w", encoding="utf-8", newline="") as fh:
        fh.write(line * n)
    assert f.stat().st_size >= 64 * 1024 * 1024

    a = app_factory(files=[f])
    assert a.wait_title("huge.log")
    time.sleep(1.5)  # incremental indexing only needs to be underway; editing does not wait for it

    a.focus_click()
    a.keys("ctrl+home", focus=False)    # caret to start of file
    time.sleep(0.3)
    a.type_text("X", click=False, focus=False)
    time.sleep(0.3)
    assert a.is_dirty(), "typing should dirty the document"

    a.save(timeout=120)                 # 68MB takes seconds to flush; widened timeout
    with open(f, "rb") as fh:
        head = fh.read(17)
    assert head == b"X0123456789abcdef", f"unexpected first-line bytes after save: {head!r}"
    # Line endings may be normalized to CRLF by the app's default newline
    # setting; both are fine. The core assertion is "the save really wrote
    # back to the original path" — before the fix this hit a sharing violation.
    size = f.stat().st_size
    assert size in (16 * n + 1, 17 * n + 1), (
        f"unexpected file size: {size} (expected LF {16 * n + 1} or CRLF {17 * n + 1})"
    )

# ---- 3. CSV unclosed quote: structural ops refuse wholesale -----------------


def test_csv_unclosed_quote_refuses_insert_col_and_sort(app_factory, work_dir):
    """Insert column must refuse wholesale on a cross-line field with an unclosed
    quote (disk unchanged, document not dirty); sorting must not rewrite the
    document either.

    Regression: row-map continuity + equal record count on whole-document
    reparse — neither existing guard could stop a field like `"x\\ny` whose
    first line is unclosed; before the fix, insert column shredded
    `"x\\ny` into two lines, `""x"",` + `y,,` (data corruption).

    Every click is proven by the status text its command writes, so the test
    cannot pass on "nothing happened" (it did before: the toolbar click landed
    outside the window and the only oracle was "the document is still clean").
    The refusal text is also asserted **specifically**: the generic "no
    selection" answer would otherwise stand in for the quote guard.
    """
    f = work_dir / "unclosed.csv"
    f.write_text(CSV_UNCLOSED, encoding="utf-8")
    a = app_factory(files=[f])
    assert a.wait_title("unclosed.csv")
    time.sleep(0.8)

    # Insert column needs a **selection**: with none it refuses earlier, with
    # "先选中一列（点击列标）", and never reaches the quote guard this test is
    # about (measured: on open `csv.sel` is None; the old comment's "focus
    # defaults to the first cell" is wrong). Clicking the column letter is the
    # documented gesture; the quote refusal proves both hops landed.
    a.click_at_css(COL_A_X, STRIP_Y)              # column letter A → whole column
    time.sleep(0.3)
    _click_toolbar_button(a, "列右+", expect="未闭合的引号")
    time.sleep(0.3)
    assert not a.is_dirty(), "document must not go dirty when the guard refuses insert column"
    assert f.read_text(encoding="utf-8") == CSV_UNCLOSED, "insert column must be refused wholesale"

    # Header on (csv tables open without one), then click its first cell: the
    # table holds a single data row, so the sort lens refuses with its own
    # message — which is also the proof that the header cell was hit.
    _click_toolbar_button(a, "首行标题", expect="首行作为标题")
    time.sleep(0.3)
    a.click_at_css(COL_A_X, HEADER_Y)      # first header cell → sort tri-state cycle
    assert _uia_has(a, "数据行不足", timeout=3.0), (
        "clicking the header cell must report the refused sort (too few rows)"
    )
    assert not a.is_dirty(), "sorting a table view must not mark the document modified"
    assert f.read_text(encoding="utf-8") == CSV_UNCLOSED, "a view sort must never rewrite the file"

# ---- 4. Background replace-all (>4MB async path) lands on disk --------------


def test_replace_all_async_lands_on_disk(app_factory, work_dir):
    """Replace-all above the 4MB threshold goes through background scanning:
    scan → apply on the UI thread → flush to disk.

    find.rs added doc_id validation to SearchJob this round; this test pins
    the async pipeline's happy path — if the validation over-fires (dropping
    results for its own document too), this is the first to go red.

    Match count is deliberately kept low (200 hits): replace-all writes back
    as a **single transaction**, and 700k replacements would freeze the UI
    thread for minutes — that is a pre-existing performance issue (see the
    audit's leftover list), not the behavior pinned here; this test only
    verifies the async pipeline itself.
    """
    f = work_dir / "big.txt"
    filler = "aaaaaaaa\n"            # 9 B × 520k ≈ 4.7 MB > the 4MB async threshold
    hits = "aaa bbb\n"               # 8 B × 200 = 200 replaceable matches
    with open(f, "w", encoding="utf-8", newline="") as fh:
        fh.write(filler * 520_000)
        fh.write(hits * 200)

    a = app_factory(files=[f])
    assert a.wait_title("big.txt")
    time.sleep(3.0)  # incremental indexing still running; let the first frame settle before opening the dialog

    a.keys("ctrl+h")
    time.sleep(1.5)
    assert a.dialog() is not None, "Ctrl+H should open the replace dialog"
    a.type_text("bbb", focus=False)
    time.sleep(0.3)
    a.keys("tab", focus=False)
    time.sleep(0.3)
    a.type_text("ZZZ", focus=False)
    time.sleep(0.3)
    a.dialog_button(5)                  # replace all (background scan)
    deadline = time.time() + 30.0
    while time.time() < deadline and not a.is_dirty():
        time.sleep(0.3)
    assert a.is_dirty(), "document should go dirty after replace-all finishes (it won't if background results were dropped)"
    a.close_dialog()
    time.sleep(0.4)
    a.save(timeout=60)

    data = f.read_text(encoding="utf-8")
    assert "bbb" not in data, "no unreplaced 'bbb' may remain on disk"
    assert data.count("ZZZ") == 200, f"expected 200 ZZZ occurrences: {data.count('ZZZ')}"

# ---- 5. Clicking the WYSIWYG view closes an open menu ------------------------


def _enter_wys(app) -> None:
    """Idempotently ensure the WYSIWYG view (same stance as test_wys_all_features;
    the coordinates are the measured ones).

    The active view toggle is inert (`toolbar.rs`), so clicking 文档视图 while
    already there reports nothing -- with the visual-first default a .md opens
    there, and the old "click toggles out" loop could never return. Force a
    known state: 文本视图 first (silence ⇒ already text), then 文档视图, whose
    status text proves the switch landed."""
    for _ in range(2):
        app.click_at_css(105.0, 80.0)
        _uia_has(app, "已切换至文本视图", timeout=1.2)  # silence ⇒ already text
        app.click_at_css(39.0, 80.0)
        if _uia_has(app, "已切换至文档视图", timeout=4.0):
            return
    raise AssertionError("did not enter the document view")


def test_menu_closes_when_clicking_wys_view(app_factory, work_dir):
    """With a menu-bar dropdown open, a click into the markdown WYSIWYG view
    must close it.

    Regression: every other surface closed the dropdown from its own pointer
    handler (the composer sets menu.open = None on press, tabs on click), but
    the WYSIWYG view never notified the host. The panel is a positioned div
    with **no backdrop**, so a click into the content reached nothing that
    closed the menu and it hung on screen. The view now reports every primary
    press to the host (`on_caret`), which closes the menu first.

    Oracle: the dropdown's row names exist in the UIA tree only while the menu
    is open (the panel renders nothing when closed).
    """
    f = work_dir / "menu-wys.md"
    f.write_text("hello\n", encoding="utf-8")
    a = app_factory(files=[f])
    assert a.wait_title("menu-wys.md")
    _enter_wys(a)
    time.sleep(0.3)

    # Open 文件 via the keyboard (bare Alt shows mnemonics → F picks 文件).
    # Proven in test_a11y; the caption element carries no UIA geometry
    # (accesskit's skeleton tree reports a 0×0 rect), so a coordinate click
    # cannot target it reliably.
    a.keys("alt")
    time.sleep(0.3)
    a.keys("f")
    # The dropdown's row names exist in the UIA tree only while the menu is
    # open; poll — the tree build lags the keystrokes.
    opened = False
    deadline = time.time() + 5.0
    while time.time() < deadline:
        if any("另存为" in n for n in _uia_names(a)):
            opened = True
            break
        time.sleep(0.1)
    assert opened, "the 文件 dropdown should be open (its rows must be in the UIA tree)"

    # Click into the WYSIWYG content: an outside click for the menu.
    a.click_at_css(80.0, 124.0)
    assert _uia_missing(a, "另存为", timeout=4.0), (
        "clicking the WYSIWYG view must close the open menu (the dropdown "
        "rows must leave the UIA tree)"
    )

# ---- 6. CSV sorting is a view lens, not an edit ------------------------------


def test_csv_sort_is_a_view_lens(app_factory, work_dir):
    """Header-click sorting reorders the **view** only: the document never goes
    dirty and the file keeps its byte order.

    Regression: sorting used to be an edit — it rewrote every record on disk
    (one undo step, `*` in the title, "sort" then "save" reordered the user's
    file). It is now a view lens: `sort_col`/`sort_desc` plus a permutation of
    the parse cache, with the document bytes untouched.

    The three-state header cycle (ascending → descending → cleared) is asserted
    through the status bar, which is simultaneously the proof that each click
    landed — three different strings, so a missed click cannot pass.
    """
    f = work_dir / "sortable.csv"
    f.write_text(CSV_SORTABLE, encoding="utf-8")
    a = app_factory(files=[f])
    assert a.wait_title("sortable.csv")
    time.sleep(0.8)

    # csv tables open **without** a header row, and only the header row carries
    # the sort binding (the A/B/C strip hosts the resize grips) — turn it on.
    _click_toolbar_button(a, "首行标题", expect="首行作为标题")
    time.sleep(0.3)

    a.click_at_css(COL_A_X, HEADER_Y)          # 1st click → ascending
    assert _uia_has(a, "已按第 1 列升序排列", timeout=3.0), (
        "the first header click must sort ascending"
    )
    assert not a.is_dirty(), "a view sort must not mark the document modified"
    assert f.read_text(encoding="utf-8") == CSV_SORTABLE, (
        "a view sort must never rewrite the file"
    )

    a.click_at_css(COL_A_X, HEADER_Y)          # 2nd click → descending
    assert _uia_has(a, "已按第 1 列降序排列", timeout=3.0), (
        "the second header click must sort descending"
    )

    a.click_at_css(COL_A_X, HEADER_Y)          # 3rd click → back to file order
    assert _uia_has(a, "排序已取消", timeout=3.0), (
        "the third header click must drop the lens"
    )
    assert not a.is_dirty()
    assert f.read_text(encoding="utf-8") == CSV_SORTABLE, (
        "the round trip through all three sort states must leave the file alone"
    )

# ---- 7. The CSV header row spans blank columns like every body row ----------


def test_csv_header_band_covers_blank_columns(app_factory, work_dir):
    """Every rendered column gets a header cell, blank filler columns included.

    Regression: the header loop walked the header's own fields only, so the
    shaded band stopped at the last field and the padded columns to its right
    stayed bare canvas — no fill, no vertical rules — while every body row
    below showed the grid. The header row has to read as a row of the table.

    oracle = pixels of the **same blank column**: its header cell must wear the
    banner colour (identical to the first header cell) and its body cell must
    stay on the canvas colour, so the difference proves both "the cell exists"
    and "it is a header". Sample boxes sit inside the cells, away from the 1px
    rules (which are a different colour by design).
    """
    f = work_dir / "sparse.csv"
    f.write_text("a,b\n1,2\n", encoding="utf-8")
    a = app_factory(files=[f])
    assert a.wait_title("sparse.csv")
    time.sleep(0.8)

    _click_toolbar_button(a, "首行标题", expect="首行作为标题")
    time.sleep(0.4)

    # Geometry: gutter ≈29 + two 40px data columns (single-char content hits
    # COL_MIN) → column C is the first 72px blank filler column, x ≈ 109..181.
    hdr_a = a.snapshot((50.0, HEADER_Y - 6.0, 64.0, HEADER_Y + 6.0))
    hdr_c = a.snapshot((120.0, HEADER_Y - 6.0, 160.0, HEADER_Y + 6.0))
    body_c = a.snapshot((120.0, HEADER_Y + 14.0, 160.0, HEADER_Y + 26.0))

    same_band = mean_diff(hdr_a, hdr_c)
    assert same_band < 3.0, (
        "the blank column's header cell is not painted with the header band's "
        f"colour (mean_diff={same_band:.1f}) — the band stops at the last "
        "header field again"
    )
    band_vs_body = mean_diff(hdr_c, body_c)
    assert band_vs_body > 4.0, (
        "the blank column looks the same in the header row and the body row "
        f"(mean_diff={band_vs_body:.1f}) — the header cell is missing and the "
        "canvas shows through"
    )


# ---- 8. A drag across an inline formula keeps the caret before it ------------


def test_wys_drag_across_a_formula_stops_before_it(app_factory, work_dir):
    """A pointer drag that enters a **typeset formula** keeps the caret on the
    formula's front edge — right after the last thing before it — instead of
    jumping into or behind the formula.

    Regression: a typeset formula is an `<svg>` with no text node, so the text
    hit-test answered with the *preceding* text node and a byte near its start —
    the caret jumped back into "abc " and then crawled forward again while the
    pointer crossed the formula's box ("拖动到公式中间时光标回退").
    `Geom::drag_pos_at` now answers any point over a typeset formula's box with
    its front edge; the box never changes shape (the atom stays typeset), so the
    answer cannot oscillate.

    Oracle: the copied selection. Sweeping the pointer right along one line, the
    selection may only **grow** (it is a prefix chain), and every sample taken
    while the pointer is over the formula's box must be the same one — the caret
    sits on the box's front edge, whatever pixel of the box the pointer is on.
    """
    body = "abc $x+y$ def\n"
    f = work_dir / "drag-formula.md"
    f.write_text(body, encoding="utf-8")
    a = app_factory(files=[f])
    assert a.wait_title("drag-formula.md")
    _enter_wys(a)
    time.sleep(0.3)

    # First row: content top 96 + 16 padding + half of the 24px row. The formula
    # sits between "abc " and " def" — its box spans ≈x 49..85 at the default
    # 15px body font, so samples 3..6 (54..78) are inside it and 86.. are past it.
    y = 124.0
    xs = list(range(30, 111, 8))
    sels: list[str] = []
    for x in xs:
        # A previous selection's floating toolbar sits above the line and would
        # swallow this press: an empty click first dismisses it.
        a.click_at_css(700.0, 260.0)
        time.sleep(0.22)
        a.drag_at_css(26.0, y, float(x), y, steps=14)
        time.sleep(0.22)
        # `copy_selection` returns "" for a collapsed selection (it raises only
        # when the keys never reached the app, which must fail the test, not be
        # recorded as an empty sample). The writer escapes a trailing space as
        # `&#32;`; compare like with like.
        sels.append(a.copy_selection().replace("&#32;", " "))

    for prev, nxt, px, nx in zip(sels, sels[1:], xs, xs[1:]):
        assert nxt.startswith(prev), (
            f"dragging right from x={px} to x={nx} shrank the selection "
            f"{prev!r} -> {nxt!r}: the caret walked back over the formula"
        )
    inside = sels[3:7]
    assert len(set(inside)) == 1, (
        f"the selection changes while the pointer is inside the formula's box: "
        f"{inside!r} — the caret must stay on the box's front edge"
    )
    assert inside[0] and "$" not in inside[0], (
        f"the selection {inside[0]!r} does not stop before the formula"
    )
    assert "$" in sels[-1], (
        f"x={xs[-1]} is past the formula, so the selection must include it: {sels[-1]!r}"
    )
    assert f.read_text(encoding="utf-8") == body, "the drags must not edit the document"
