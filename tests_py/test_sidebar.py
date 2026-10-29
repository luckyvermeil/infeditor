"""Workspace sidebar: directory tree rendering, click to open files, Ctrl+B
toggle, restart restore.

oracle choices (Blitz fully self-drawn; the sidebar is invisible to Win32):

* **Behavior oracle primary** — click a file row in the tree and assert via
  the OS title that the corresponding document opened (the title is a
  trustworthy oracle; row geometry is constant, matching
  `components/sidebar.rs`);
* **Pixel oracle secondary** — Ctrl+B collapses the sidebar and the viewport
  shifts left 220px; take a patch from mid-viewport and compare mean pixel
  difference (round-trip assertions like test_view.py);
* **Restart restore** — settings.json should contain `ws_path`, and after
  restart clicking a file in the tree still opens it (the tree really was
  restored).

Clicking/collapsing and the Ctrl+B toggle share one instance chained
together (the collapse round-trip's final state is exactly the toggle
test's starting point); restart restore must be standalone (requires quit +
relaunch).

The native directory picker (rfd) cannot be automated; the
`INF_WS_SMOKE=<dir>` smoke env var enters the workspace directly (same idea
as find/prefs/numwin's arm_smoke_from_env, see the use_hook in `editor.rs`).
"""

from __future__ import annotations

import time

from infed.driver import mean_diff

# ---- Sidebar geometry (CSS px, matching the src/ui constants) ---------------
# Sidebar = single 220px panel (SIDEBAR_W), flush with the window's left
# edge (no activity bar: the toggle sits at the status bar's bottom-left).
# Panel header 35px (HEADER_H), tree area padding-top 2, row height 22.
# rows top = view.y (CHROME_TOP=36+28) + header 35 + padding 2; row height 22
ROW0_Y = 64.0 + 35.0 + 2.0 + 11.0      # first row center ≈ 112
ROW_H = 22.0
SIDEBAR_X = 25.0                        # inside the indent + arrow slot

# The workspace lives one level **below** the process CWD (`work_dir`, see
# `driver.App.start`), and the CWD is where the graphics driver dumps its own
# files on startup (measured on this machine: `NVIDIA Corporation/umdlogs`
# appears within a second of the first frame). A stray directory inside the
# tree root becomes its **first row**, shifting every row index by one, so the
# clicks below open the wrong file — or nothing — and the test fails for a
# reason that has nothing to do with the sidebar. Keeping the root a subfolder
# puts that noise next to the tree instead of inside it.
WS_SUB = "ws"

# Chrome names the UIA tree reports around the rows (panel title, refresh,
# close, expand/collapse twisties). Everything else between the header and the
# status bar is a tree row.
_WS_CHROME = frozenset({"刷新", "×", "▸", "▾"})


def _make_tree(work_dir):
    """Standard directory tree: directories sort first → [src, a.txt, b.md];
    with src expanded, row 1 = inner.txt.

    a.txt gets enough lines: the Ctrl+B test's pixel box spans y 120..400;
    a one-line document (y≈70) falls outside it and a blank-canvas shift
    produces no pixel difference (been bitten by this).

    Returns the tree root (= the workspace to open, see `WS_SUB`).
    """
    ws = work_dir / WS_SUB
    (ws / "src").mkdir(parents=True)
    (ws / "src" / "inner.txt").write_text("inner doc\n", encoding="utf-8")
    (ws / "a.txt").write_text(
        "".join(f"alpha line {i:02d} the quick brown fox\n" for i in range(1, 41)),
        encoding="utf-8",
    )
    (ws / "b.md").write_text("beta\n", encoding="utf-8")
    return ws


def _sidebar_rows(app) -> list[str]:
    """Names the workspace panel exposes, top to bottom, starting at its first row.

    Sliced out of the UIA tree after the panel's `×` (end of the header). The
    app reports the rows as unnamed-by-role Custom nodes, so this ordered name
    list is the only view of the tree it offers — and rows are what the geometry
    indexes into, so a mismatch has to be reported *before* the click, not after
    a mysteriously wrong file opens.

    The tail of the slice is not a row list: once the tree ends, the walk
    continues into the document (its text nodes) and the status bar fields. Call
    sites compare a **prefix** — only the rows above a click target move it.
    """
    names = app.a11y_names()
    try:
        start = names.index("×")           # panel close button = end of the header
    except ValueError:
        return []
    return [n for n in names[start + 1:] if n not in _WS_CHROME]


def _expect_rows(app, want: list[str], timeout: float = 4.0) -> None:
    """Wait until the sidebar's **leading** rows are exactly `want`.

    Prefix, not equality: trailing names are document content and status fields
    (see `_sidebar_rows`), and rows *below* the ones a test clicks cannot move
    that click. Rows *above* them can, and a stray one is exactly what this
    catches (measured: the GPU driver dropping a log folder into the process
    CWD added a first row and every click landed one file down).
    """
    deadline = time.time() + timeout
    seen: list[str] = []
    while time.time() < deadline:
        seen = _sidebar_rows(app)
        if len(seen) >= len(want) and seen[:len(want)] == want:
            return
        time.sleep(0.15)
    raise AssertionError(
        f"the sidebar should start with rows {want}, but it shows {seen[:12]} — an "
        f"unexpected row above them shifts every click target below it"
    )


def test_sidebar_tree_click_collapse_then_ctrl_b_hides(app_factory, work_dir):
    ws = _make_tree(work_dir)
    a = app_factory(env_extra={"INF_WS_SMOKE": str(ws)})
    assert a.wait_title("InfEditor")
    time.sleep(0.8)

    # row 0 = src (directory, sorted first): click = expand
    _expect_rows(a, ["src", "a.txt", "b.md"])
    a.click_at_css(SIDEBAR_X, ROW0_Y)
    # with src expanded, row 1 = src/inner.txt: click = open
    _expect_rows(a, ["src", "inner.txt", "a.txt", "b.md"])
    a.click_at_css(SIDEBAR_X, ROW0_Y + ROW_H)
    assert a.wait_title("inner.txt"), "clicking a file in the sidebar should open that document"

    # click row 0 again = collapse src: the tree returns to
    # [src, a.txt, b.md], row 1 back to a.txt
    a.click_at_css(SIDEBAR_X, ROW0_Y)
    _expect_rows(a, ["src", "a.txt", "b.md"])
    a.click_at_css(SIDEBAR_X, ROW0_Y + ROW_H)
    assert a.wait_title("a.txt"), "after collapsing src, row order returns to root level (a.txt becomes row 1)"

    # Whole-window round-trip assertion: sidebar collapses → panel disappears
    # + viewport shifts left, significant full-window pixel difference.
    # Do not use a local box inside the viewport — under the light theme the
    # blank canvas is pure white; with no text in the box it is pure white
    # before and after the shift and mean_diff stays 0 (been bitten).
    base = a.snapshot()
    a.keys("ctrl+b")
    time.sleep(0.6)
    after = a.snapshot()
    d = mean_diff(base, after)
    print(f"[ws] mean_diff={d}")
    assert d > 3.0, "Ctrl+B collapsing the sidebar should shift the viewport left"

    a.keys("ctrl+b")
    time.sleep(0.6)
    assert mean_diff(base, a.snapshot()) < 3.0, "a second Ctrl+B should restore the original layout"


def test_workspace_persists_across_restart(app_factory, work_dir, tmp_path):
    ws = _make_tree(work_dir)
    cfg = tmp_path / "wsconfig"
    a = app_factory(env_extra={"INF_WS_SMOKE": str(ws)}, config_dir=cfg)
    assert a.wait_title("InfEditor")
    time.sleep(0.8)

    a.quit()  # graceful WM_CLOSE exit
    import json

    s = json.loads((cfg / "settings.json").read_text(encoding="utf-8"))
    assert s.get("ws_path") == str(ws), "workspace path should be written to settings.json"
    assert s.get("ws_show") is True

    # restart (no smoke var): the workspace should restore from settings and
    # the tree be directly clickable
    b = app_factory(config_dir=cfg)
    assert b.wait_title("InfEditor")
    _expect_rows(b, ["src", "a.txt", "b.md"])   # restored, not expanded
    b.click_at_css(SIDEBAR_X, ROW0_Y + ROW_H)  # row 1 = a.txt (root level, not expanded)
    assert b.wait_title("a.txt"), "after restart the sidebar should restore the tree and be directly interactive"
