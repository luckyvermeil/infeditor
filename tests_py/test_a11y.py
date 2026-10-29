"""Accessibility: UIA tree (accesskit lazily activated) + keyboard-only menu
operation.

blitz's accesskit adapter only builds its tree when it receives
WM_GETOBJECT (the first query from a screen reader / UIA client), so this
file connects via pywinauto's UIA backend as a "screen reader" and allows
generous retries for the first tree build.

The three tree-content assertions (window name / window control buttons /
menu titles) share **one tree build**: building costs a 6s retry budget and
all three read the same tree; splitting them is pure duplicated overhead.
"""

from __future__ import annotations

import time


def _poll_names(app, ok, timeout: float = 5.0):
    """Walk the tree until `ok(names)` holds or `timeout` expires (returns the
    last snapshot either way).

    accesskit pushes its tree from the document it was activated on and the
    first successful walk can still describe an earlier render, so assertions
    about *content* poll instead of trusting one snapshot.
    """
    deadline = time.time() + timeout
    names = []
    while True:
        names = [n for _, n in _walk_names(app)]
        if ok(names) or time.time() >= deadline:
            return names
        time.sleep(0.3)


def _walk_names(app, max_depth: int = 12, settle: float = 6.0, require=None):
    """Trigger accesskit tree building as a UIA client and collect
    (control_type, name).

    `require`: a name that must appear before the snapshot is accepted. The
    first successful walk can still describe the previous render (accesskit
    pushes its tree from the document it was activated on), so callers that
    assert on *content* let it settle instead of trusting the first build.
    """
    from pywinauto import Desktop
    deadline = time.time() + settle
    last_err = None
    while time.time() < deadline:
        try:
            win = Desktop(backend="uia").window(
                handle=app.main_hwnd).wrapper_object()
            out = []
            def rec(ctrl, depth):
                try:
                    out.append((ctrl.element_info.control_type,
                                ctrl.element_info.name or ""))
                except Exception:
                    pass
                if depth < max_depth:
                    try:
                        for ch in ctrl.children():
                            rec(ch, depth + 1)
                    except Exception:
                        pass
            rec(win, 0)
            if require is None or any(require in (n or "") for _, n in out):
                return out
            # The walk succeeded but still describes the previous render
            # (accesskit pushes lazily); yield before rebuilding instead of
            # hammering COM in a tight loop.
            time.sleep(0.3)
        except Exception as e:  # tree not ready yet / COM flakiness
            last_err = e
            time.sleep(0.5)
    raise TimeoutError(f"UIA tree unavailable: {last_err}")


def test_uia_tree_exposes_window_buttons_and_menubar(app):
    """Window name, window control buttons, and menu titles all verified in
    one tree build."""
    names = [n for _, n in _walk_names(app, require="最小化")]
    joined = " | ".join(t for t in names)

    assert "InfEditor" in joined, f"window name not exposed to UIA: {joined[:200]!r}"
    # blitz's initial tree only pushed a skeleton, and labelled_by resolution
    # reads the label field while blitz only wrote value — all names were
    # empty. name-from-content has since been added in vendor/blitz's
    # blitz-dom per HTML-AAM (skipping aria-hidden subtrees); names and roles
    # are now fully exposed.
    for expected in ("最小化", "最大化", "关闭"):
        assert expected in names, \
            f"{expected!r} missing from the accessibility tree; see {names[:40]!r}"
    # Captions read bare (Windows 11 style): the access key `(F)` is appended
    # only while Alt is held, so the accessible name — derived from the DOM
    # text (see vendor/blitz's name-from-content) — is the bare label too.
    bare = _poll_names(app, lambda ns: "文件" in ns and "文件(F)" not in ns)
    for expected in ("文件", "编辑", "帮助"):
        assert expected in bare, f"menu titles not exposed: {bare[:40]!r}"

    # Holding Alt reveals the access keys, and the accessible name follows the
    # visible text (the mnemonics are not a hidden string behind the name).
    app.keys("alt")
    hot = _poll_names(app, lambda ns: "文件(F)" in ns)
    for expected in ("文件(F)", "编辑(E)", "帮助(H)"):
        assert expected in hot, f"Alt did not reveal the access keys: {hot[:40]!r}"
    app.keys("escape")


def test_uia_tree_has_tabs(app_factory, work_dir):
    f = work_dir / "a11ytab.txt"
    f.write_text("tab content\n", encoding="utf-8")
    a = app_factory(files=[f])
    time.sleep(0.5)
    names = [n for _, n in _walk_names(a)]
    assert "a11ytab.txt" in names, f"tab name not exposed: {names[:40]!r}"


def test_keyboard_only_menu_opens_new_doc(app):
    """Keyboard only: bare Alt shows mnemonics → F opens 「文件」 → Enter
    picks the first item (new document).

    A menu opened via mnemonic already has the cursor on the first row
    (`menu_open_from_alt` sets cursor=0, matching Windows' keyboard menu
    behavior), so Enter activates directly — pressing ↓ instead moves to
    the second item 「打开…」 and pops a file dialog.
    """
    app.keys("alt")
    time.sleep(0.3)
    app.keys("f")
    time.sleep(0.5)
    app.keys("enter")
    assert app.wait_title("[2/2]", timeout=6), \
        f"keyboard-only menu operation did not create a document: {app.title()!r}"
