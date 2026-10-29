"""Editing pipeline: typing (incl. CJK/emoji), undo/redo, paste, save
round-trip, dirty marker.

Two instances, two chains, advancing continuously within each scenario:

* **Blank-document chain**: typing round-trip → undo/redo → paste (undo
  restores the document to empty, exactly a clean starting point for the
  paste assertion);
* **File chain**: open via command line → edit → dirty marker appears →
  Ctrl+S → dirty marker cleared + disk content verified (save round-trip
  and dirty marker are two observations of the same edit; splitting them is
  pure duplicated startup).
"""

from __future__ import annotations

import time


def test_typing_undo_redo_and_paste_on_blank_doc(app):
    # -- Typing round-trip (CJK + emoji via the SendInput dual channel) ----
    text = "Hello InfEditor 你好，世界 🎉 OK"
    app.focus_click()          # click to settle keyboard focus (blank document, caret position irrelevant)
    app.type_text(text, click=False)
    got = app.doc_text()
    assert got == text, f"typing round-trip mismatch: {got!r}"

    # -- Undo / redo (undo returns to the blank document, a clean starting
    # -- point for paste) --------------------------------------------------
    app.keys("ctrl+z")
    time.sleep(0.15)
    after_undo = app.doc_text()
    assert after_undo != text, "text should change after undo"
    app.keys("ctrl+y")
    time.sleep(0.15)
    assert app.doc_text() == text, "redo should restore the text"
    app.keys("ctrl+z")
    time.sleep(0.15)

    # -- Clipboard paste ---------------------------------------------------
    from infed import clipboard as cb
    cb.set_text("剪贴板粘贴内容-paste")
    app.keys("ctrl+v")
    time.sleep(0.4)
    got = app.doc_text()
    assert "剪贴板粘贴内容-paste" in got


def test_save_roundtrip_and_dirty_marker_cycle(app_factory, work_dir):
    """Command-line open → edit → dirty marker → Ctrl+S → disk verification
    (the save path without a dialog)."""
    f = work_dir / "roundtrip.txt"
    f.write_text("line1\nline2\n", encoding="utf-8")
    a = app_factory(files=[f])
    assert a.wait_title("roundtrip.txt"), a.title()

    # append at the end: click to settle focus first, Ctrl+End moves the
    # caret to the document end
    a.focus_click()
    a.keys("ctrl+end")
    a.type_text("appended\n", click=False)
    assert a.wait_title("*roundtrip.txt"), f"dirty marker did not appear: {a.title()!r}"

    a.keys("ctrl+s")
    assert a.wait_title("roundtrip.txt", timeout=5), \
        f"dirty marker not cleared after save: {a.title()!r}"
    assert not a.title().startswith("*")
    assert f.read_text(encoding="utf-8") == "line1\nline2\nappended\n"
