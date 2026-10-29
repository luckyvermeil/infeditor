"""Files/tabs: command-line open, multi-tab switching and closing.

Single-file open (title without [n/m], content round-trip) is semantically
independent, its own instance; two-tab switching and Ctrl+W close share one
instance chained together (the switch round-trip's final state is exactly
the close-tab starting point), saving one app launch.
"""

from __future__ import annotations

import time


def test_cli_open_file_shows_content_and_title(app_factory, work_dir):
    f = work_dir / "hello.txt"
    f.write_text("GUI 自动化打开的文件\n", encoding="utf-8")
    a = app_factory(files=[f])
    assert a.wait_title("hello.txt"), a.title()
    assert a.doc_text() == "GUI 自动化打开的文件\n"


def test_two_files_opened_switch_and_close_tab(app_factory, work_dir):
    f1 = work_dir / "first.txt"
    f2 = work_dir / "second.txt"
    f1.write_text("AAA first\n", encoding="utf-8")
    f2.write_text("BBB second\n", encoding="utf-8")
    a = app_factory(files=[f1, f2])
    # The two files load in parallel in the background; which one is active
    # depends on completion order, so only assert "two tabs" and "Ctrl+Tab
    # really switched documents" — do not pin the order.
    assert a.wait_title("/2]"), a.title()

    first = a.doc_text()
    a.keys("ctrl+tab")           # wrap around to the other tab
    time.sleep(0.3)
    second = a.doc_text()
    assert {first, second} == {"AAA first\n", "BBB second\n"}, (first, second)
    assert first != second, "Ctrl+Tab should switch to the other document"

    a.keys("ctrl+shift+tab")     # switch back
    time.sleep(0.3)
    assert a.doc_text() == first, "Ctrl+Shift+Tab should switch back to the previous document"

    # Close the current tab (currently the one holding `first` content): with
    # one tab left, [n/m] must no longer be shown, and what remains should be
    # the other document's content
    a.keys("ctrl+w")
    time.sleep(0.4)
    assert "[" not in a.title(), f"with a single tab, [n/m] must not be shown: {a.title()!r}"
    assert a.doc_text() == second, f"the remaining document should be the other one, got: {a.doc_text()!r}"
