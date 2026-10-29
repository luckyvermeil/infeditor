"""Abnormal-exit rescue: session written to disk → restart shows the
recovery prompt → both the restore and discard paths.

Rescue mechanism (`src/ui/recovery.rs`):

* `session/session.json` (session manifest) + `session/backup-<doc_id>.dat`
  (a full snapshot of every **dirty** document) under the config dir; the
  manifest is written last so it never points at a missing snapshot;
* snapshot writes are **throttled to 3s** — tests must wait long enough
  before forcing the abnormal exit, otherwise they only see a "clean"
  session.json (the first test version passed spuriously exactly this way);
* on restart, if a session is found, a "Recover session" modal pops up
  (self-drawn dialog, buttons: restore / discard snapshot). **This dialog
  swallows clicks** — so the driver by default auto-dismisses any
  unexpected recovery prompt at startup with a two-step confirmation
  (`App.start(handle_recovery=True)`, see conftest); only this file opts
  out via `handle_recovery=False` and drives/asserts each step itself.
* Buttons are always clicked by **label** (`click_dialog_label`); the probe
  provides geometry + label. The self-drawn modal has no Win32 handle, and
  position must never be guessed from colors.
"""

from __future__ import annotations

import json
import time

SNAPSHOT_SETTLE = 4.0       # slightly above recovery's 3s throttle


def _crash_with_unsaved(app_factory, tmp_path, text="orig\n", typed="UNSAVED"):
    """Stage a "dirty document + abnormal exit" scene; returns (config dir,
    file, in-memory full text)."""
    cfg = tmp_path / "sess"
    work = tmp_path / "sesswork"
    work.mkdir(parents=True, exist_ok=True)
    f = work / "s.txt"
    f.write_text(text, encoding="utf-8")

    a = app_factory(files=[f], config_dir=cfg)
    assert a.wait_title("s.txt")
    a.keys("ctrl+end")
    a.type_text(typed, click=False)
    time.sleep(SNAPSHOT_SETTLE)         # wait for the rescue snapshot to flush (3s throttle)
    in_memory = a.doc_text()
    assert a.is_dirty()

    a.quit(force=True)                  # equivalent to an abnormal exit
    time.sleep(1.0)
    return cfg, f, in_memory


def test_crash_leaves_session_snapshot(app_factory, tmp_path):
    cfg, f, in_memory = _crash_with_unsaved(app_factory, tmp_path)

    sess = cfg / "session"
    assert sess.is_dir(), "an abnormal exit should leave a session directory"
    manifest = sess / "session.json"
    assert manifest.is_file(), "the session manifest should be written"

    data = json.loads(manifest.read_text(encoding="utf-8"))
    assert data["format"] == "infeditor-recovery"
    assert len(data["tabs"]) == 1

    backups = [p for p in sess.iterdir() if p.name.startswith("backup-")]
    assert backups, "a snapshot of the dirty document should be written"
    snapshot = "".join(p.read_text(encoding="utf-8") for p in backups)
    assert "UNSAVED" in snapshot, "the snapshot should contain the unsaved changes"
    assert f.read_text(encoding="utf-8") == "orig\n", "the original file must not be written"


def test_restart_shows_recovery_prompt_and_restores(app_factory, tmp_path):
    cfg, f, in_memory = _crash_with_unsaved(app_factory, tmp_path)

    b = app_factory(config_dir=cfg, handle_recovery=False)
    b.wait_dialog_labels(["恢复", "放弃快照"], 5.0, "recovery prompt dialog")

    b.click_dialog_label("恢复")
    time.sleep(1.2)

    assert "s.txt" in b.title(), "after restore, the original tab should reopen"
    assert b.doc_text() == in_memory, "unsaved changes should be recovered from the snapshot"
    assert b.is_dirty(), "recovered content is still in unsaved state"


def test_recovery_discard_drops_snapshot(app_factory, tmp_path):
    """Discard is a two-step confirmation: discard snapshot → second dialog
    (cancel / confirm discard) → snapshot deleted.

    Buttons are clicked by **label**, not index: in the two-step dialog
    「取消」 and 「确认放弃」 sit side by side; if the order changes, the
    test silently clicks the wrong one and deletes the only rescue snapshot
    (or clicks cancel while believing it discarded).
    """
    cfg, f, _ = _crash_with_unsaved(app_factory, tmp_path)

    b = app_factory(config_dir=cfg, handle_recovery=False)
    b.wait_dialog_labels(["恢复", "放弃快照"], 5.0, "recovery prompt dialog")
    b.click_dialog_label("放弃快照")
    # Second dialog: 「取消」 is still the primary button (safe default, Enter
    # works directly); 「确认放弃」 is secondary — destructive actions must
    # not sit on the default slot.
    b.wait_dialog_labels(["取消", "确认放弃"], 3.0, "second confirmation dialog")

    b.click_dialog_label("确认放弃")
    time.sleep(1.2)

    sess = cfg / "session"
    left = list(sess.iterdir()) if sess.is_dir() else []
    assert not any(p.name.startswith("backup-") for p in left), \
        "snapshot should be deleted after discard"

    # launched the normal way: tab count back to "untitled"
    assert "s.txt" not in b.title()
    b.type_text("hello", click=False)   # focus is still on the document
    time.sleep(0.3)
    assert "hello" in b.doc_text()


def test_recovery_discard_confirm_cancel_keeps_snapshot(app_factory, tmp_path):
    """Clicking 「取消」 on the second dialog must return to the recovery
    prompt with the snapshot untouched.

    This is the inverse of the previous test: a destructive confirmation
    must have a reversible path. The old test only covered "confirm
    discard"; the cancel branch (`RecoveryDiscardCancel` → re-show the
    recovery prompt) was never exercised.
    """
    cfg, f, in_memory = _crash_with_unsaved(app_factory, tmp_path)

    b = app_factory(config_dir=cfg, handle_recovery=False)
    b.wait_dialog_labels(["恢复", "放弃快照"], 5.0, "recovery prompt dialog")
    b.click_dialog_label("放弃快照")
    b.wait_dialog_labels(["取消", "确认放弃"], 3.0, "second confirmation dialog")

    b.click_dialog_label("取消")        # back out: return to the recovery prompt
    time.sleep(1.2)
    b.wait_dialog_labels(["恢复", "放弃快照"], 3.0, "back at the recovery prompt after cancel")

    sess = cfg / "session"
    backups = [p for p in sess.iterdir() if p.name.startswith("backup-")] \
        if sess.is_dir() else []
    assert backups, "canceling the discard must not delete the snapshot"

    b.click_dialog_label("恢复")        # snapshot still there, restore still usable
    time.sleep(1.2)
    assert b.doc_text() == in_memory, "unsaved changes should still be recoverable after canceling the discard"


def test_clean_quit_leaves_no_session(app_factory, work_dir):
    """A normal exit (WM_CLOSE) should clean up leftovers; the next launch
    must not show a recovery prompt."""
    f = work_dir / "c.txt"
    f.write_text("clean\n", encoding="utf-8")
    a = app_factory(files=[f])
    assert a.wait_title("c.txt")

    a.quit()                            # WM_CLOSE, normal close path
    sess = a.config_dir / "session"
    left = []
    if sess.is_dir():
        left = [p.name for p in sess.iterdir() if p.name.startswith("backup-")]
    assert not left, f"no snapshot should remain after a normal exit: {left}"
