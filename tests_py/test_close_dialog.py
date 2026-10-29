"""Dirty-document close confirmation dialog: modality, cancel, discard, save.

The close confirmation is an app-drawn **modal layer**
(`state/docs.rs::close_doc` + `components/dialog.rs`), not a Win32 dialog —
no handle to grab, but two stable hooks exist:

* **Modality**: while the dialog is open, all keystrokes are swallowed
  (`composer.rs::dialog_key`) and never reach the document;
* **Button locating**: the app's layout probe writes each action button's
  true center + label into shell.log (`[dialog]` lines, DOM id
  `inf-dlg-btn-N`); `driver.click_dialog_label("不保存")` clicks by label.
  Do **not** use "accent-color patch" as the oracle: after the
  primary/secondary split only the primary button has the accent color (a
  2026-10 round-six live GUI run tripped on exactly this stale assumption);
  do not use indices either — button order is declaration order, and one
  change silently clicks the wrong button.

Note: the confirmation dialog can only be dismissed with **Esc** — it is
not an input dialog; Enter deliberately does not submit (avoiding
"accidental Enter confirms").
"""

from __future__ import annotations

import time


def _dirty_app(app_factory, work_dir, name="d.txt"):
    """Build a dirty document via the keyboard (do not click the viewport:
    see `driver.focus_click`'s notes)."""
    f = work_dir / name
    f.write_text("orig\n", encoding="utf-8")
    a = app_factory(files=[f])
    assert a.wait_title(name)
    a.keys("ctrl+end")
    a.type_text("XYZ", click=False)
    time.sleep(0.3)
    assert a.is_dirty(), "the edit did not land in the document"
    return a, f


def test_close_dialog_is_modal_and_cancel_keeps_tab(app_factory, work_dir):
    a, f = _dirty_app(app_factory, work_dir)
    before = a.doc_text()

    a.keys("ctrl+w")
    time.sleep(0.7)

    # Modality: these keys must be swallowed by the dialog; not one
    # character may reach the document
    a.type_text("SHOULD_NOT_APPEAR", focus=False)
    time.sleep(0.3)

    a.keys("escape", focus=False)       # cancel
    time.sleep(0.6)
    assert a.doc_text() == before, "keystrokes leaked into the document while the confirmation dialog was open"
    assert "d.txt" in a.title(), "the tab should remain after cancel"
    assert a.is_dirty(), "cancel must not clear the dirty flag"
    assert f.read_text(encoding="utf-8") == "orig\n", "disk must not be written"


def test_close_dialog_discard_closes_without_saving(app_factory, work_dir):
    a, f = _dirty_app(app_factory, work_dir)

    a.keys("ctrl+w")
    time.sleep(0.7)
    a.wait_dialog_labels(["保存", "不保存", "取消"], 3.0, "close confirmation dialog")
    a.click_dialog_label("不保存")
    time.sleep(0.9)

    assert "d.txt" not in a.title(), "the tab should close after discard"
    assert f.read_text(encoding="utf-8") == "orig\n", "nothing should be written to disk"


def test_close_dialog_save_writes_then_closes(app_factory, work_dir):
    a, f = _dirty_app(app_factory, work_dir)
    in_memory = a.doc_text()            # in-memory state before the dialog (line endings normalized to \n)

    a.keys("ctrl+w")
    time.sleep(0.7)
    a.click_dialog_label("保存")
    time.sleep(1.2)

    saved = f.read_text(encoding="utf-8").replace("\r\n", "\n")
    assert saved == in_memory, "saved content should equal the in-memory content from before the dialog"
    assert "XYZ" in saved
    assert "d.txt" not in a.title(), "the tab should close after saving"
