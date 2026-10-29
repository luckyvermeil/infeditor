"""Replace: Ctrl+H standalone dialog + action button clicks.

The dialog is a Blitz-drawn **standalone top-level window**
(`src/ui/findwin.rs`); the Win32 side is only an empty shell window, so:

* window geometry is assertable (client area 452×202 CSS; `popup_size` is
  derived from the dialog's own flex layout constants: the right column's
  7 buttons + row spacing + padding determine the height) — this pins "the
  dialog size was not broken by layout changes";
* buttons can only be clicked geometrically (`driver.dialog_button`); the
  search term / replacement go in via keyboard;
* **replace only changes memory**; Ctrl+S (`driver.save`) is required
  before verifying disk bytes.

Button column top to bottom (replace mode): 0 find next / 1 find prev /
2 count / 3 find all / 4 replace / 5 replace all / 6 results.
"""

from __future__ import annotations

import time

SRC = "aaa bbb aaa\nccc aaa\n"


def _open_replace(a, pattern: str, replacement: str):
    a.keys("ctrl+h")
    time.sleep(1.0)
    assert a.dialog() is not None, "Ctrl+H should open the replace dialog"
    a.type_text(pattern, focus=False)
    time.sleep(0.2)
    a.keys("tab", focus=False)          # focus: search box → replace box
    time.sleep(0.2)
    a.type_text(replacement, focus=False)
    time.sleep(0.2)


def _file(work_dir, name="r.txt", text=SRC):
    f = work_dir / name
    f.write_text(text, encoding="utf-8")
    return f


def test_replace_dialog_geometry(app_factory, work_dir):
    """Dialog client area size should match the layout constants (452 × 202
    CSS, flex layout constants)."""
    a = app_factory(files=[_file(work_dir)])
    assert a.wait_title("r.txt")

    a.keys("ctrl+h")
    time.sleep(1.0)
    assert a.dialog() is not None
    w, h = a.dialog_geometry()
    assert (w, h) == (452, 202), f"replace dialog client area size changed: {w}×{h} CSS"

    a.close_dialog()
    time.sleep(0.4)
    assert a.dialog() is None, "Esc should close the dialog"


def test_replace_all_writes_through(app_factory, work_dir):
    f = _file(work_dir)
    a = app_factory(files=[f])
    assert a.wait_title("r.txt")

    _open_replace(a, "aaa", "ZZ")
    a.dialog_button(5)                  # replace all
    time.sleep(0.8)

    assert a.is_dirty(), "document should go dirty after replace"
    a.close_dialog()
    time.sleep(0.4)
    a.save()

    assert f.read_text(encoding="utf-8") == "ZZ bbb ZZ\nccc ZZ\n"


def test_replace_one_after_find_next(app_factory, work_dir):
    """'Find next' → 'Replace': replaces the first hit and advances.
    """
    f = _file(work_dir)
    a = app_factory(files=[f])
    assert a.wait_title("r.txt")

    _open_replace(a, "aaa", "ZZ")
    a.dialog_button(0)                  # find next → selects the first hit
    time.sleep(0.5)
    a.dialog_button(4)                  # replace
    time.sleep(0.6)

    a.close_dialog()
    time.sleep(0.4)
    a.save()
    assert f.read_text(encoding="utf-8") == "ZZ bbb aaa\nccc aaa\n"


def test_replace_one_without_prior_find_replaces_exactly_one(app_factory, work_dir):
    """Clicking 'Replace' without 'Find next' first: still replaces **exactly
    one** occurrence.

    No assertion on which one — currently it lands on "the target left by
    incremental find" (in type-into-document mode, find reveals along the
    way, so the caret is not at the document start). This test only pins the
    bottom line "one occurrence per click, no wholesale damage"; which spot
    it lands on is a product call, not part of the contract.
    """
    f = _file(work_dir)
    a = app_factory(files=[f])
    assert a.wait_title("r.txt")

    _open_replace(a, "aaa", "ZZ")
    a.dialog_button(4)                  # replace
    time.sleep(0.6)

    a.close_dialog()
    time.sleep(0.4)
    a.save()
    saved = f.read_text(encoding="utf-8")
    assert saved.count("ZZ") == 1, f"exactly one occurrence should be replaced: {saved!r}"
    assert saved.count("aaa") == 2, f"the other two occurrences should be preserved verbatim: {saved!r}"


def test_replace_cancel_leaves_document_untouched(app_factory, work_dir):
    f = _file(work_dir)
    a = app_factory(files=[f])
    assert a.wait_title("r.txt")

    _open_replace(a, "aaa", "ZZ")
    a.close_dialog()                    # Esc cancel, no replacement performed
    time.sleep(0.5)

    assert not a.is_dirty(), "merely typing a query must not change the document"
    assert f.read_text(encoding="utf-8") == SRC
    assert a.doc_text() == SRC


def test_find_in_dialog_selects_match(app_factory, work_dir):
    """The dialog's 'Find next' button (index 0) should select the first
    match.

    Read the selection **before closing the dialog**: Esc's first act is
    `clear_selection()`.
    """
    a = app_factory(files=[_file(work_dir)])
    assert a.wait_title("r.txt")

    a.keys("ctrl+h")
    time.sleep(1.0)
    a.type_text("bbb", focus=False)
    time.sleep(0.2)
    a.dialog_button(0)                  # find next
    time.sleep(0.6)

    assert a.copy_selection() == "bbb"
    a.close_dialog()
