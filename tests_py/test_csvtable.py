"""Excel-style in-place cell editing in the CSV table view.

`.csv` files open straight into the table view (`CsvView::for_doc`:
enabled=table=true), no smoke hook needed. Interaction semantics
(`components::csvtable` + `commands::csv`):

* single click selects a cell; double click / select then Enter/F2 →
  in-place edit (current value loaded into the buffer);
* Enter commits and moves down, Tab commits and moves right, Esc cancels
  (Excel semantics);
* typing appends to the cell buffer (Backspace deletes characters); commit
  writes back per line (minimal quoting), single-step undo;
* documents containing cross-line quoted fields are supported: write-back
  replaces each record's `[start_line, end_line)` span as a whole, so only the
  edited cell changes (unclosed quotes are still refused wholesale by the
  structural ops' `csv_struct_guard`);

oracle choices:

* **Disk bytes** (primary): edit → save → read the file and assert line
  content — much harder than pixels/status bar;
* **Pixels** (guard cases): the edit box is white with an accent border and
  a block caret; the selected state is only a highlight background — when a
  double click is refused, the two snapshots must be pixel-identical.

Geometry (CSS px; column width/row height use COL_MIN=40 / ROW_H=20,
single-character cell content): view.y = CHROME_TOP(64) + CSV toolbar
strip(32) = 96; column header strip 18px; data rows 20px. The row-number
slot is ≈29px wide (2 chars × Consolas advance ≈ 7.7 + 14); data columns
A/B/C are 40px each — click points take cell centers, insensitive to ±1px
drift in font advance.
"""

from __future__ import annotations

import time

from infed.driver import mean_diff

VIEW_Y = 96.0     # CHROME_TOP(64) + CSV toolbar strip(32)
STRIP_H = 18.0    # column header strip (A/B/C…)
ROW_H = 20.0

# Cell centers (col width 40, row-number slot ≈29: insensitive to ±1px font
# advance drift)
COL0 = (48.0, VIEW_Y + STRIP_H + 0.5 * ROW_H)    # row 0, col A
COL1_ROW1 = (88.0, VIEW_Y + STRIP_H + 1.5 * ROW_H)  # row 1, col B
# Sampling box for row 0 col A (pixel oracle: slightly larger than the cell)
CELL0_BOX = (28.0, VIEW_Y + STRIP_H - 2.0, 70.0, VIEW_Y + STRIP_H + ROW_H + 2.0)

# 3x3 grid, all single-character content → every column lands at COL_MIN=40,
# geometry predictable
CSV_BODY = "a,1,x\nb,2,y\nc,3,z\n"


def _csv(work_dir, name="cells.csv", body=CSV_BODY):
    f = work_dir / name
    f.write_text(body, encoding="utf-8")
    return f


def test_double_click_edits_cell_and_enter_commits(app_factory, work_dir):
    """Double click enters edit → change text → Enter commits → disk-byte
    verification.

    Covers the full Excel loop: double click loads the current value,
    Backspace deletes characters, typing appends, Enter commits and moves
    the selection down one row.
    """
    f = _csv(work_dir)
    a = app_factory(files=[f])
    assert a.wait_title("cells.csv")
    time.sleep(0.6)  # wait for the first table frame to render

    a.double_click_at_css(*COL0)
    time.sleep(0.4)  # edit box appears
    a.keys("backspace", focus=False)      # delete the loaded "a"
    a.type_text("grape", click=False, focus=False)
    a.keys("enter", focus=False)          # commit + move down
    a.save()
    lines = f.read_text(encoding="utf-8").splitlines()
    assert lines[0] == "grape,1,x", f"row 0 should hold the edited value: {lines[0]!r}"
    assert lines[1] == "b,2,y" and lines[2] == "c,3,z", "other rows must be untouched"


def test_enter_begins_edit_and_tab_commits_moving_right(app_factory, work_dir):
    """Keyboard path: single click selects → Enter begins editing; Tab commits
    and moves right.

    Click (row 1, col B): after committing the change, the selection should
    move right to col C — verify by entering edit again with F2: it should
    load col C's old value "y" (if the selection did not move, it loads
    col B's "99").
    """
    f = _csv(work_dir)
    a = app_factory(files=[f])
    assert a.wait_title("cells.csv")
    time.sleep(0.6)

    a.click_at_css(*COL1_ROW1)            # select (1,1), old value "2"
    time.sleep(0.3)
    a.keys("enter", focus=False)          # begin editing
    time.sleep(0.3)
    a.keys("backspace", focus=False)
    a.type_text("99", click=False, focus=False)
    a.keys("tab", focus=False)            # commit + move right to (1,2)
    a.save()
    lines = f.read_text(encoding="utf-8").splitlines()
    assert lines[1] == "b,99,y", f"Tab commit should rewrite row 1 col B: {lines[1]!r}"

    # selection should have moved right to col C: F2 enters edit and loads
    # "y" (if still on col B it loads "99")
    a.keys("f2", focus=False)
    time.sleep(0.3)
    a.type_text("Z", click=False, focus=False)  # append → "yZ"
    a.keys("enter", focus=False)
    a.save()
    lines = f.read_text(encoding="utf-8").splitlines()
    assert lines[1] == "b,99,yZ", (
        f"F2 should begin editing on col C after the move (expected 'yZ'; "
        f"getting '99Z' means the selection did not move right after Tab): "
        f"{lines[1]!r}"
    )


def test_escape_cancels_edit_without_touching_disk(app_factory, work_dir):
    """Esc cancels: buffer discarded, document not dirty, disk unchanged."""
    f = _csv(work_dir)
    a = app_factory(files=[f])
    assert a.wait_title("cells.csv")
    time.sleep(0.6)

    a.double_click_at_css(*COL0)
    time.sleep(0.4)
    a.type_text("ZZZ", click=False, focus=False)  # buffer becomes "aZZZ"
    a.keys("escape", focus=False)
    time.sleep(0.3)
    assert not a.is_dirty(), "document should not be dirty after Esc cancel"
    a.save(expect_dirty=False)
    assert f.read_text(encoding="utf-8") == CSV_BODY, "canceling an edit must not touch disk"


def test_comma_typed_into_cell_is_minimally_quoted_on_writeback(app_factory, work_dir):
    """Typing a comma into a cell value: wrapped in minimal quotes on
    write-back, per CSV semantics."""
    f = _csv(work_dir)
    a = app_factory(files=[f])
    assert a.wait_title("cells.csv")
    time.sleep(0.6)

    a.double_click_at_css(88.0, VIEW_Y + STRIP_H + 0.5 * ROW_H)  # row 0, col B
    time.sleep(0.4)
    a.keys("backspace", focus=False)      # delete the "1"
    a.type_text("x,y", click=False, focus=False)
    a.keys("enter", focus=False)
    a.save()
    lines = f.read_text(encoding="utf-8").splitlines()
    assert lines[0] == 'a,"x,y",x', (
        f"a value containing a comma should be quoted on write-back: {lines[0]!r}"
    )


def test_multiline_quoted_field_edits_whole_record(app_factory, work_dir):
    """Cross-line quoted field: written back by record span; editing is
    **available** (behavior change in round six).

    The old behavior made the whole table read-only whenever the document
    contained a single multi-line note field, with a generic message.
    `parse_csv_spans` now provides each record's `(start_line, end_line)`;
    cell write-back replaces only that span, so cross-line fields no longer
    block editing.

    Acceptance: double click enters edit (pixels change from selected to
    editing state), after changing the value and committing only the target
    cell changes on disk, and the note field with embedded newlines is
    preserved byte for byte.
    """
    body = 'name,note\na,ok\nb,"line1\nline2"\nc,done\n'
    f = _csv(work_dir, body=body)
    a = app_factory(files=[f])
    assert a.wait_title("cells.csv")
    time.sleep(0.6)

    a.click_at_css(*COL0)
    time.sleep(0.4)
    selected = a.snapshot(CELL0_BOX)

    a.double_click_at_css(*COL0)          # mouse entry → in-place editing
    time.sleep(0.4)
    assert mean_diff(selected, a.snapshot(CELL0_BOX)) > 0.5, (
        "double click should show the edit box even with a cross-line quoted field present"
    )

    a.type_text("Z", click=False, focus=False)   # COL0 = first row, first col "name"
    a.keys("enter", focus=False)          # commit (Enter moves down)
    a.save()
    got = f.read_text(encoding="utf-8").replace("\r\n", "\n")
    assert got == 'nameZ,note\na,ok\nb,"line1\nline2"\nc,done\n', (
        f"only the target cell should change and the cross-line record be "
        f"preserved verbatim: {got!r}"
    )
