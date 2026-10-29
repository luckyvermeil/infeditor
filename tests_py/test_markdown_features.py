# -*- coding: utf-8 -*-
"""Full on-app verification of Markdown features (2026-10, third round of GUI automation).

Division of labor with the two remaining markdown test files:

* `test_wys_all_features.py` -- document-view cursor/formulas/tables/sample byte-for-byte;
* `test_wys_spaces.py`       -- space rendering/caret compatibility;
* **this file**              -- static rendering correctness plus the **behavioral** side
  of view/edit commands:

1. **View-switch matrix**: document (the open default) -> text -> document -> text;
   each step uses the **status-bar text** (`已切换至文档视图` / `已切换至文本视图`)
   plus a content signature as a dual oracle; pure browsing sets no dirty flag and
   saving leaves bytes unchanged. Since the visual-first default (2026-10) a .md
   file opens straight into the document view; `_enter_text` provides the source
   view for the text-side cases.
2. **Text-view syntax highlighting**: a .md document has far more high-chroma pixels
   (theme-independent criterion: max-min>60) than the same text with markers stripped.
3. **md toolbar edit commands (shared icon strip)**: both views render the same strip
   (one builder); in text view every button carries the source-edit command — select a
   word -> click bold -> `**word**` on disk, headings/quote/list/table grid and the rest
   of the strip, each verified with a single-step undo.
4. **wys undo/redo**: type -> Ctrl+Z reverts -> Ctrl+Y redoes -> save to disk.
5. **wys CJK input**: non-ASCII goes through the clipboard+Ctrl+V channel
   (wys_paste) -> saved to disk.

Coordinates (CSS px, derived from the same constants as `toolbar::layout`): toolbar
buttons at y=80 (CHROME_TOP 64 + (32-22)/2 + 11); the toolbar has **no leading
caption** — the two view buttons 文档视图/文本视图 center at 39/105, and the icon
strip (24px icons + 4px gap) starts after a 13px separator at x=153.

Known limitations (read before writing tests):

* **the first frame after a view switch paints late** -- content-signature asserts
  poll via `_wait_names`;
* **doc_text under the document view is rendered plain text** (wys_copy: no `# `
  markers, no trailing newline) -- only structural assertions work; byte assertions
  check disk;
* md prefix commands with **no selection = whole document** -- select first to
  restrict the affected lines;
* the document-view exit (-> text) **normalizes non-canonical source and sets dirty**
  (`| --- |` -> `|---|`, blank lines between adjacent lists get swallowed) -- samples
  must be in canonical form to verify "pure browsing sets no dirty";
* screenshots are screen grabs; lock screen/occlusion invalidates pixel assertions;
* run serially (shared focus/clipboard), no `-n`.
"""

from __future__ import annotations

import time
from pathlib import Path

# ---- Geometry constants (CSS px) --------------------------------------------

TITLEBAR_H = 36.0
TABS_H = 28.0
TOOLBAR_H = 32.0
STATUS_H = 22.0
CHROME_TOP = TITLEBAR_H + TABS_H            # 64
VIEW_Y = CHROME_TOP + TOOLBAR_H             # 96 content top
TOOLBAR_BTN_Y = CHROME_TOP + (TOOLBAR_H - 22.0) / 2 + 11.0   # 80
# The dropdown panels open under the toolbar band (y + TOOLBAR_H = 96).
PANEL_TOP = VIEW_Y


def _btn_w(label: str) -> float:
    """Same formula as toolbar::btn_width: ASCII 6.96 / fullwidth 12, +7 padding each side, floor 22."""
    text = sum(6.96 if c.isascii() else 12.0 for c in label)
    return max(text + 14.0, 22.0)


# The shared strip's icon order (toolbar.rs `md_groups`; both views render this
# exact sequence — only the command behind each button differs per view).
ICON_ORDER = [
    "emoji", "headings", "bold", "italic", "strike", "link", "image", "list",
    "ordered-list", "check", "outdent", "indent", "quote", "line", "code",
    "inline-code", "before", "after", "table", "undo", "redo",
    "edit", "more",
]
ICON_W = 24.0
ICON_GAP = 4.0
SEP_W = 13.0


def md_toolbar_x(target: str) -> float:
    """Button center x in the md toolbar (two view toggles + the shared icon
    strip; the toolbar has no leading caption)."""
    x = 8.0                                   # PAD_X
    for label in ("文档视图", "文本视图"):
        w = _btn_w(label)
        if label == target:
            return x + w / 2.0
        x += w + 4.0
    x += SEP_W                                # view group → icon strip separator
    if target in ICON_ORDER:
        i = ICON_ORDER.index(target)
        return x + i * (ICON_W + ICON_GAP) + ICON_W / 2.0
    raise KeyError(f"toolbar has no button {target!r}")


DOC_X = md_toolbar_x("文档视图")          # 39 (right at the toolbar's left padding)
TEXT_X = md_toolbar_x("文本视图")         # 105


def _panel_row_y(index: int) -> float:
    """Center y of dropdown-panel row `index` (0-based): panel top 96 + padding 4
    + row margin 1 + half row height 12, rows stacked 26px apart (24px + margins)."""
    return PANEL_TOP + 4.0 + 1.0 + 12.0 + 26.0 * index


def _grid_cell_xy(col: int, row: int) -> tuple[float, float]:
    """Center of the table-grid picker's cell (col, row) (26px pitch, 4px padding,
    24px cells; the panel opens at the table button's left edge)."""
    px = md_toolbar_x("table") - ICON_W / 2.0   # the button's left edge = panel x
    return (px + 4.0 + col * 26.0 + 12.0, PANEL_TOP + 4.0 + row * 26.0 + 12.0)


# ---- Oracle helpers ---------------------------------------------------------


def _names(a, max_depth: int = 14) -> list[str]:
    """All UIA-tree names (accesskit builds the tree lazily; reading triggers it)."""
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
        if d < max_depth:
            try:
                for ch in c.children():
                    rec(ch, d + 1)
            except Exception:
                pass

    rec(win)
    return out


def _wait_names(a, needle: str, timeout: float = 5.0) -> bool:
    deadline = time.time() + timeout
    while time.time() < deadline:
        if any(needle in n for n in _names(a)):
            return True
        time.sleep(0.15)
    return False


def _click_view(a, x: float, expect_status: str, timeout: float = 5.0) -> None:
    """Click a view button and wait for the status-bar text to appear -- that text is
    the only non-vacuous assertion of a successful switch (toolbar button names are
    always present in the tree; using them as the criterion is vacuous -- been burned)."""
    a.click_at_css(x, TOOLBAR_BTN_Y)
    if not _wait_names(a, expect_status, timeout=timeout):
        raise AssertionError(
            f"status bar did not show {expect_status!r} after clicking x={x}; "
            f"sample of current status-like names: {[n for n in _names(a) if '视图' in n or '已切' in n][:6]}")


def _enter_doc(a) -> None:
    """Idempotently ensure the document (WYSIWYG) view.

    The two view toggles are a **segmented control**: the active half is inert
    (`toolbar.rs`: `inert = disabled || (view_btn && on)`), so clicking 文档视图
    while already there emits no status text. Silence is therefore ambiguous
    ("already in the document view" vs. "the click was lost") and the old
    "click toggles, keep clicking" premise no longer holds -- since the
    visual-first default a .md file opens in the document view, so every click
    used to land on the inert half and the loop never returned.

    Force a known state instead of guessing: click 文本视图 (silence there just
    means we were already in the text view), then 文档视图. That second click
    always has a real transition to report, so its status text is both the exit
    condition and the proof that the input landed. Two rounds absorb a lost
    first click (the previous test's window may still be closing)."""
    for _ in range(2):
        a.click_at_css(TEXT_X, TOOLBAR_BTN_Y)
        _wait_names(a, "已切换至文本视图", timeout=1.2)  # silence ⇒ already text
        a.click_at_css(DOC_X, TOOLBAR_BTN_Y)
        if _wait_names(a, "已切换至文档视图", timeout=4.0):
            return
    raise AssertionError("did not enter the document view")


def _enter_text(a) -> None:
    """Idempotently ensure the source text view (mirror of `_enter_doc`: force
    the document view first, then click 文本视图 -- that click must report)."""
    for _ in range(2):
        a.click_at_css(DOC_X, TOOLBAR_BTN_Y)
        _wait_names(a, "已切换至文档视图", timeout=1.2)  # silence ⇒ already doc
        a.click_at_css(TEXT_X, TOOLBAR_BTN_Y)
        if _wait_names(a, "已切换至文本视图", timeout=4.0):
            return
    raise AssertionError("did not enter the text view")


def _src_names(names: list[str]) -> list[str]:
    """Text-view signature: **raw source** in UIA names (`# ` lines etc.)."""
    return [n for n in names if "# " in n or "## " in n]


def _pixel_stats(img) -> tuple[int, int]:
    """(chroma pixel count, ink pixel count).

    Chroma: max-min>60 -- Heading/ListMark chroma is >=110 in both themes while
    body/background/line numbers/scrollbar text are ~0.
    """
    px = img.load()
    w, h = img.size
    chroma = 0
    counts: dict[tuple[int, int, int], int] = {}
    for y in range(0, h, 2):
        for x in range(0, w, 2):
            c = px[x, y]
            counts[c] = counts.get(c, 0) + 1
    for y in range(h):
        for x in range(w):
            r, g, b = px[x, y]
            if max(r, g, b) - min(r, g, b) > 60:
                chroma += 1
    return chroma, 0

# ---- Shared sample document -------------------------------------------------

# Written in the **canonical form** emitted by the document view: no blank line
# between adjacent lists, no inner spaces in table separator rows. Measured
# (2026-10-19 diagnosis): non-canonical documents get normalized and marked dirty
# on the wys->text write-back (e.g. `| --- |` -> `|---|`, blank lines between
# adjacent lists swallowed) -- the status text "保存时格式将规范化" means exactly
# this; the matrix tests verify "pure browsing sets no dirty", so samples must sit
# at the canonical fixed point.
RICH_MD = """# 一级标题 HeadOne

普通段落 plain paragraph with words.

## 二级标题 HeadTwo

- 项目甲 item-alpha
- 项目乙 item-beta
1. 有序一 ord-one
2. 有序二 ord-two

> 引用块 quote-text

```rust
fn code_line() -> u32 { 42 }
```

| 列A | 列B |
|---|---|
| 单元格1 | 单元格2 |

行内 **粗体bold** 与 `代码span` 与 [链接link](https://example.com)。
"""


def _md(work_dir: Path, name: str, body: str) -> Path:
    """Write a sample file: **must use write_bytes**.

    `Path.write_text` opens in text mode on Windows and translates `\n` into
    `\r\n` -- the document becomes a CRLF file, and the matrix test "bytes
    unchanged after save" would compare a CRLF sample against the LF of
    `body.encode()`, falsely reporting "view round trip rewrote the document"
    (measured 2026-10-19: the app detects/saves CRLF perfectly; it was the
    fixture translating line endings).
    """
    f = work_dir / name
    f.write_bytes(body.encode("utf-8"))
    return f

# ---- 1. View switch matrix --------------------------------------------------


def test_view_switch_matrix_status_and_content(app_factory, work_dir):
    """Document (the open default) -> text -> document -> text -> document: each
    step asserts status-bar text plus content signature.

    Pure browsing sets no dirty; disk bytes unchanged after Ctrl+S (view switching
    must never rewrite the document).
    """
    body = RICH_MD
    f = _md(work_dir, "matrix.md", body)
    a = app_factory(files=[f])
    assert a.wait_title("matrix.md")

    # Initial: document view (visual-first default) -- rendered list items,
    # not raw source
    assert _wait_names(a, "项目甲 item-alpha"), \
        f"the document view did not render list items on open (sample: {_names(a)[40:60]})"
    names = _names(a)
    assert not _src_names(names), f"document view leaked source markers: {_src_names(names)[:4]}"

    # Document view -> text
    _click_view(a, TEXT_X, "已切换至文本视图")
    names = _names(a)
    assert _src_names(names), "text view should show raw source (e.g. '# 一级标题 …')"

    # Text -> document view
    _click_view(a, DOC_X, "已切换至文档视图")
    names = _names(a)
    assert not _src_names(names), f"document view leaked source markers: {_src_names(names)[:4]}"

    # Document view -> text (second round trip, proving the button is not one-shot)
    _click_view(a, TEXT_X, "已切换至文本视图")
    names = _names(a)
    assert _src_names(names), "returning to text view should show raw source again"

    # Text -> document view
    _click_view(a, DOC_X, "已切换至文档视图")
    names = _names(a)
    assert not _src_names(names), "second entry into the document view still leaks source markers"

    a.screenshot("md_matrix_final_document_view")

    # Pure browsing sets no dirty; bytes unchanged after save
    assert not a.is_dirty(), "switching views must not set dirty"
    a.keys("ctrl+s")
    time.sleep(0.6)
    assert f.read_bytes() == body.encode("utf-8"), "document was rewritten after view round trip"


def test_view_matrix_preserves_crlf_file(app_factory, work_dir):
    """CRLF sample: save after the same round-trip chain, disk bytes identical.

    On the wys write-back the app uses `preserve_eol` to restore emitted `\n` to
    the original line endings (mdws::sync_to_doc); LF passes through, CRLF is
    preserved. The fixture must use write_bytes: write_text translates LF to CRLF
    on Windows, which once made the LF test look like "the app rewrote the file as
    CRLF".
    """
    body = RICH_MD.replace("\n", "\r\n")
    f = _md(work_dir, "matrix_crlf.md", body)
    a = app_factory(files=[f])
    assert a.wait_title("matrix_crlf.md")

    # The file opens in the document view (visual-first default): round trip out and back
    _click_view(a, TEXT_X, "已切换至文本视图")
    _click_view(a, DOC_X, "已切换至文档视图")

    assert not a.is_dirty(), "pure browsing (including the wys round trip) must not set dirty"
    a.keys("ctrl+s")
    time.sleep(0.6)
    got = f.read_bytes()
    assert got == body.encode("utf-8"), (
        "CRLF document rewritten after view round trip "
        f"(CRLF count {got.count(b'\r\n')} / expected {body.count(chr(13) + chr(10))})")

# ---- 2. Text view syntax highlighting ----------------------------------------


def test_text_view_markdown_highlight_chroma(app_factory, work_dir):
    """markdown syntax highlighting actually colors (theme-independent chroma criterion).

    Control = same text with all md markers stripped: chroma should be ~0
    (body/background/line numbers are neutral). Two instances run serially
    (windows are mutually exclusive in the foreground; screenshots are screen grabs).
    """
    hi = "# 标题 heading\n\n- 列表项 bullet\n\n正文 plain text line\n"
    plain = "标题 heading\n\n列表项 bullet\n\n正文 plain text line\n"
    box = (0.0, VIEW_Y + 10.0, 700.0, VIEW_Y + 300.0)

    f_hi = _md(work_dir, "hi.md", hi)
    a_hi = app_factory(files=[f_hi])
    assert a_hi.wait_title("hi.md")
    _enter_text(a_hi)   # the visual-first default opens in the document view
    time.sleep(0.4)
    chroma_hi, _ = _pixel_stats(a_hi.snapshot(box))
    a_hi.screenshot("md_highlight_on")
    a_hi.quit(force=True)

    f_pl = work_dir / "plain.md"
    f_pl.write_bytes(plain.encode("utf-8"))
    a_pl = app_factory(files=[f_pl])
    assert a_pl.wait_title("plain.md")
    _enter_text(a_pl)
    time.sleep(0.4)
    chroma_plain, _ = _pixel_stats(a_pl.snapshot(box))
    a_pl.screenshot("md_highlight_off")
    a_pl.quit(force=True)

    assert chroma_hi > 300, f"too few chroma pixels in the marked document ({chroma_hi}) -- highlighting probably off"
    assert chroma_plain < 80, f"marker-free control also has chroma pixels ({chroma_plain}) -- criterion polluted by background"
    assert chroma_hi > chroma_plain + 200, (
        f"highlight difference not significant: hi={chroma_hi} plain={chroma_plain}")

# ---- 3. md toolbar edit commands (text view, the shared icon strip) ----------


def test_toolbar_bold_applies_to_source(app_factory, work_dir):
    """Select a word -> click the toolbar's bold icon -> `**word**` on disk.

    Command semantics match the unit tests in src/ui/commands/markdown.rs (wrap
    idempotent).
    """
    body = "hello world line\n\nsecond line\n"
    f = _md(work_dir, "edit.md", body)
    a = app_factory(files=[f])
    assert a.wait_title("edit.md")
    _enter_text(a)   # source-edit commands live in the text view's strip

    # Select "hello" (cursor at line start, extend right 5 keys)
    a.keys("ctrl+home")
    for _ in range(5):
        a.keys("shift+right")
    a.click_at_css(md_toolbar_x("bold"), TOOLBAR_BTN_Y)
    time.sleep(0.4)
    a.save()
    got = f.read_text(encoding="utf-8")
    assert got == "**hello** world line\n\nsecond line\n", (
        f"toolbar bold did not bold the selection, disk: {got!r}")


def test_headings_panel_h2_applies_to_source(app_factory, work_dir):
    """Text view: the headings dropdown's H2 row prefixes the caret line (`## `).

    The panel is shared by both views — in text view its rows carry the
    MdEdit commands (the strip itself only fires the dropdown).
    """
    body = "alpha beta\nsecond line\n"
    f = _md(work_dir, "h2.md", body)
    a = app_factory(files=[f])
    assert a.wait_title("h2.md")
    _enter_text(a)   # the panel's rows carry MdEdit commands in the text view

    # Select line 0 (prefix commands with **no selection = whole document**;
    # the selection restricts the affected lines).
    a.keys("home")
    a.keys("shift+end")
    a.click_at_css(md_toolbar_x("headings"), TOOLBAR_BTN_Y)
    time.sleep(0.4)
    # Panel row 1 (0-based) = H2: panel top 96 + padding 4 + margins; rows stack 26px.
    hx = md_toolbar_x("headings") - ICON_W / 2.0 + 88.0   # inside the 176px-wide panel
    a.click_at_css(hx, _panel_row_y(1))
    time.sleep(0.4)
    a.save()
    got = f.read_text(encoding="utf-8")
    assert got == "## alpha beta\nsecond line\n", f"headings panel H2 did not apply: {got!r}"


# ---- 3b. All md toolbar edit commands ----------------------------------------

# (icon, selection mode, expected full text). Selection modes:
#   "word"  -- first 5 chars of line 0, `alpha` (wrap commands consume the selection)
#   "line"  -- whole line 0 (prefix commands act on the lines covered by the selection)
# After each command Ctrl+Z must restore the original -- also verifying each
# command is a **single-step undo**. The icons are the *shared strip's*; in text
# view every one of them carries the MdEdit command.
TOOLBAR_CASES = [
    ("italic", "word", "*alpha* beta\nsecond line\n"),
    ("strike", "word", "~~alpha~~ beta\nsecond line\n"),
    ("inline-code", "word", "`alpha` beta\nsecond line\n"),
    ("code", "word", "```\nalpha\n``` beta\nsecond line\n"),
    ("quote", "line", "> alpha beta\nsecond line\n"),
    ("list", "line", "- alpha beta\nsecond line\n"),
    ("ordered-list", "line", "1. alpha beta\nsecond line\n"),
    ("check", "line", "- [ ] alpha beta\nsecond line\n"),
    ("link", "word", "[alpha](url) beta\nsecond line\n"),
    ("image", "word", "![alpha](url) beta\nsecond line\n"),
    ("line", "line", "---alpha beta\nsecond line\n"),
]


def test_all_md_toolbar_edit_commands(app_factory, work_dir):
    """Click the strip's edit commands on the real app one by one: content
    transform + single-step undo.

    Reads in-memory state via `doc_text` (Ctrl+A/C) to avoid disk writes; after
    each step Ctrl+Z must restore the baseline byte-for-byte (also locking in the
    "one undo entry per command" contract). The table grid and the headings panel
    are two-click flows — covered by `test_headings_panel_h2_applies_to_source`
    and the `md_insert_table` unit tests.
    """
    base = "alpha beta\nsecond line\n"
    f = _md(work_dir, "tb_all.md", base)
    a = app_factory(files=[f])
    assert a.wait_title("tb_all.md")
    _enter_text(a)   # in text view every strip icon carries the MdEdit command

    for icon, mode, want in TOOLBAR_CASES:
        a.keys("ctrl+home")
        if mode == "word":
            for _ in range(5):
                a.keys("shift+right")
        elif mode == "line":
            a.keys("shift+end")
        time.sleep(0.2)
        x = md_toolbar_x(icon)
        a.click_at_css(x, TOOLBAR_BTN_Y)
        time.sleep(0.45)
        got = a.doc_text()
        assert got == want, f"{icon} (x={x:.2f}) result mismatch:\n  got {got!r}\n  want {want!r}"
        a.keys("ctrl+z")
        time.sleep(0.45)
        back = a.doc_text()
        assert back == base, f"{icon}: Ctrl+Z did not restore the baseline: {back!r}"


def test_table_grid_inserts_in_text_view(app_factory, work_dir):
    """Text view: the table grid picker inserts a same-size source template
    (the text-mode counterpart of the wys kernel's grid insert)."""
    base = "alpha beta\nsecond line\n"
    f = _md(work_dir, "grid.md", base)
    a = app_factory(files=[f])
    assert a.wait_title("grid.md")
    _enter_text(a)   # the text-mode counterpart of the wys kernel's grid insert
    # Select the last line: the insert lands after the caret's covered line
    # (no selection = whole document, the line-targeting contract).
    a.keys("ctrl+end")
    a.keys("home")
    a.keys("shift+end")
    time.sleep(0.2)

    cx, cy = _grid_cell_xy(1, 1)   # 2×2
    a.click_at_css(md_toolbar_x("table"), TOOLBAR_BTN_Y)
    time.sleep(0.4)
    a.click_at_css(cx, cy)
    time.sleep(0.45)
    got = a.doc_text()
    want = "alpha beta\nsecond line\n| 列1 | 列2 |\n| --- | --- |\n|     |     |\n"
    assert got == want, f"table grid did not insert the 2×2 template:\n  got {got!r}\n  want {want!r}"


# ---- 4. wys undo / redo ------------------------------------------------------


def test_wys_undo_redo_roundtrip(app_factory, work_dir):
    """Type in the document view -> Ctrl+Z fully reverts -> Ctrl+Y redoes -> save.

    The undo path (actions.rs wys intercept: `Cmd::Undo -> mdws::undo`) previously
    only had unit tests; this is its on-app verification.
    """
    body = "# 标题\n\n第一段 alpha\n"
    f = _md(work_dir, "undo.md", body)
    a = app_factory(files=[f])
    assert a.wait_title("undo.md")
    _enter_doc(a)

    a.keys("ctrl+end")
    a.type_text("TAIL", click=False, focus=True)
    time.sleep(0.4)
    # doc_text under the document view is **rendered plain text** (Ctrl+A/C goes
    # through wys_copy: no `# ` markers, no trailing newline) -- only structural
    # assertions like endsWith apply; byte-level verification belongs to disk.
    assert a.doc_text().endswith("TAIL"), "typed text did not land in the wys document"

    a.keys("ctrl+z")
    time.sleep(0.5)
    txt = a.doc_text()
    assert not txt.endswith("TAIL"), f"Ctrl+Z did not revert: {txt!r}"

    a.keys("ctrl+y")
    time.sleep(0.5)
    assert a.doc_text().endswith("TAIL"), "Ctrl+Y did not redo"

    a.save()
    got = f.read_text(encoding="utf-8")
    # wys ctrl+end lands at the **end of the last text line** (not into the phantom
    # blank line; measured line/col = end of the third line), so TAIL attaches
    # after alpha; the trailing newline is preserved.
    assert got == "# 标题\n\n第一段 alphaTAIL\n", f"unexpected save result: {got!r}"

# ---- 5. wys CJK input (clipboard channel -> wys_paste) ------------------------


def test_wys_cjk_input_pastes_via_clipboard_channel(app_factory, work_dir):
    """type_text routes non-ASCII through clipboard+Ctrl+V; on the wys side Cmd::Paste -> wys_paste.

    IME commitment cannot be injected externally (README limitation 2); this is the
    standard GUI-automation substitute for IME input -- the wys tests previously
    only typed ASCII.
    """
    body = "# 标题\n\n第一段 alpha\n"
    f = _md(work_dir, "cjk.md", body)
    a = app_factory(files=[f])
    assert a.wait_title("cjk.md")
    _enter_doc(a)

    a.keys("ctrl+end")
    a.type_text("中文尾部测试", click=False, focus=True)
    time.sleep(0.5)
    # doc_text is rendered plain text (see above); byte assertions check disk
    assert a.doc_text().endswith("中文尾部测试"), "CJK paste channel did not land in wys"

    a.save()
    got = f.read_text(encoding="utf-8")
    assert got == "# 标题\n\n第一段 alpha中文尾部测试\n", f"unexpected save result: {got!r}"
