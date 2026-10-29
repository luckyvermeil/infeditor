"""View/rendering: zoom (Ctrl+= / Ctrl+- / Ctrl+0) round-trips.

This is the only test family in the suite that relies on **pixels** — zoom
changes rendering and there is no other oracle (the status bar's percentage
is unreadable, and the accesskit tree has no status bar text). So the
assertions are round-trips ("it changed / it changed back") rather than
absolute values:

* after Ctrl+= / Ctrl+-, the mean pixel difference over the same document
  region should be significantly > 0;
* after Ctrl+0 it should return near the baseline (threshold relaxed to 3,
  tolerating caret blinking and anti-aliasing jitter).

The three directions (zoom in / zoom out / reset) share one instance
chained together: a GUI test's fixed overhead (startup + settle + teardown)
far exceeds one keystroke + snapshot, so scenarios that can advance on one
instance are not split — each failure message names its direction, so
localization granularity is unaffected.

The document region is a plain-text patch from mid-window, avoiding the
status bar (its elapsed-seconds keep changing).
"""

from __future__ import annotations

import time

from infed.driver import mean_diff

TEXT_BOX = (60.0, 120.0, 700.0, 420.0)


def _doc(work_dir, name="view.txt"):
    f = work_dir / name
    f.write_text("".join(f"line {i:02d} the quick brown fox\n" for i in range(1, 41)),
                 encoding="utf-8")
    return f


def test_zoom_roundtrip_in_out_and_reset(app_factory, work_dir):
    a = app_factory(files=[_doc(work_dir)])
    assert a.wait_title("view.txt")
    time.sleep(0.5)

    base = a.snapshot(TEXT_BOX)

    a.keys("ctrl+=")
    time.sleep(0.6)
    assert mean_diff(base, a.snapshot(TEXT_BOX)) > 3.0, \
        "Ctrl+= should change the rendering (font size grows)"

    a.keys("ctrl+0")
    time.sleep(0.6)
    assert mean_diff(base, a.snapshot(TEXT_BOX)) < 3.0, "Ctrl+0 should return to the 100% rendering"

    a.keys("ctrl+-")
    time.sleep(0.6)
    assert mean_diff(base, a.snapshot(TEXT_BOX)) > 3.0, "Ctrl+- should change the rendering"

    a.keys("ctrl+0")
    time.sleep(0.6)
    assert mean_diff(base, a.snapshot(TEXT_BOX)) < 3.0, "after zooming out, Ctrl+0 should return to 100%"
