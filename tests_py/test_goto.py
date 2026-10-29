"""Go to line: Ctrl+G opens the **in-page bar** (not a standalone dialog);
type a line number and press Enter.

The in-page bar and the find dialog are two different paths:
`EditorState::open_goto` always moves focus to the bar's goto box (and
dismisses the find dialog first), so asserting `dialog()` is None is part
of the test — it pins the deliberate "goto does not use a dialog" behavior.

Landing semantics (`find.rs::goto_line`): line numbers are 1-based,
out-of-range values are **clamped** to the first/last line, and the caret
lands at the **start** of the line (column 0).

The three landing scenarios (normal line / out-of-range clamp / back to the
first line) share one instance chained together: after each goto, type "X"
and assert the landing with `doc_text()` (Ctrl+C full-text oracle), then
Ctrl+Z to reset the document — saves two app launches with equally hard
landing verification.
"""

from __future__ import annotations

import time

DOC = "l1\nl2\nl3\nl4\n"


def _open(work_dir, name="g.txt", text=DOC):
    f = work_dir / name
    f.write_text(text, encoding="utf-8")
    return f


def _goto(a, line: str):
    """Open the in-page bar → type the line number → Enter.

    No viewport clicks at all: the bar takes keyboard focus by itself
    (`open_goto` focuses the goto box), and "click the viewport center"
    would move the caret, injecting an unrelated variable; in practice,
    keys sent right after a click also race the landing handler.
    """
    a.keys("ctrl+g")
    time.sleep(0.7)
    assert a.dialog() is None, "go-to-line should use the in-page bar, not a standalone dialog"
    a.type_text(line, focus=False)
    time.sleep(0.2)
    a.keys("enter", focus=False)
    time.sleep(0.6)


def test_goto_line_landing_clamps_and_wrap(app_factory, work_dir):
    f = _open(work_dir)
    a = app_factory(files=[f])
    assert a.wait_title("g.txt")

    # normal landing: start of line 3
    _goto(a, "3")
    a.type_text("X", click=False)
    time.sleep(0.3)
    assert a.doc_text() == "l1\nl2\nXl3\nl4\n", "goto 3 should land at the start of line 3"
    a.keys("ctrl+z")                     # reset the document for the next landing check
    time.sleep(0.3)

    # out-of-range line numbers should clamp to the last line, not error out
    # or jump to the file start
    _goto(a, "9999")
    a.type_text("X", click=False)
    time.sleep(0.3)
    got = a.doc_text()
    # the last line may be line 4 or the "phantom blank line" after the
    # terminator — both are valid landings
    assert got in ("l1\nl2\nl3\nXl4\n", "l1\nl2\nl3\nl4\nX\n"), f"unexpected out-of-range landing: {got!r}"
    a.keys("ctrl+z")
    time.sleep(0.3)

    # back to the first line: move elsewhere first so we don't start
    # "already on line 1"
    _goto(a, "4")
    _goto(a, "1")
    a.type_text("X", click=False)
    time.sleep(0.3)
    final = a.doc_text()
    assert final == "Xl1\nl2\nl3\nl4\n", f"goto 1 should land at the start of the first line: {final!r}"

    # disk round-trip backstop: after saving the final state, it matches the
    # on-screen text
    a.save()
    assert f.read_text(encoding="utf-8") == "Xl1\nl2\nl3\nl4\n"
