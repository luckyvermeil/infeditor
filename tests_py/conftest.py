"""pytest fixtures: exe location, isolated directories, app lifecycle, failure screenshots.

Safety conventions (mirroring cargo tests/common):

* `INFEDITOR_CONFIG_DIR` points to a per-test-case temp directory, so the
  user's real settings.json / macros / plugins / recovery sessions are
  never read or written;
* test files live in a working directory under `tempfile`;
* clipboard backup/restore; the process is guaranteed to exit
  (WM_CLOSE first, then kill).
"""

from __future__ import annotations

import os
import sys
from pathlib import Path

import pytest

sys.path.insert(0, str(Path(__file__).parent))

from infed.driver import App  # noqa: E402

REPO = Path(__file__).resolve().parent.parent
OUT_DIR = Path(__file__).resolve().parent / "out"


def _locate_exe() -> Path:
    env = os.environ.get("INFEDITOR_EXE")
    if env:
        p = Path(env)
        if not p.exists():
            raise FileNotFoundError(f"INFEDITOR_EXE points to a nonexistent file: {p}")
        return p
    p = REPO / "target" / "release" / "infeditor.exe"
    if not p.exists():
        raise FileNotFoundError(
            f"cannot find {p} -- run `cargo build --release` first, or set INFEDITOR_EXE")
    return p


@pytest.fixture(scope="session")
def exe_path() -> Path:
    return _locate_exe()


@pytest.fixture()
def work_dir(tmp_path: Path) -> Path:
    d = tmp_path / "work"
    d.mkdir()
    return d


@pytest.fixture()
def app_factory(exe_path: Path, tmp_path: Path):
    """Build and launch an InfEditor instance; guaranteed to exit and restore the clipboard at teardown.

    * `app_factory()` — minimal instance without files;
    * `app_factory(files=[...])` — launch with files on the command line;
    * `app_factory(env_extra={"INF_SHELL_LOG": "1"})` — diagnostic logging;
    * `app_factory(config_dir=some_dir)` — **reuse** an existing config dir
      (needed by crash-recovery tests: the session.json left by a previous
      abnormal exit lives inside the config dir);
    * `app_factory(handle_recovery=False)` — do **not** auto-dismiss the
      recovery prompt at startup (only crash-recovery tests need this; all
      other tests auto-dismiss a recovery prompt with two-step confirmation).

By default each instance gets a **fresh** config dir, and any unexpected
"recover session" prompt is auto-dismissed at startup. This is not just
cleanliness: recovery sessions are written into the config dir, and test
teardown always calls `kill()` (equivalent to an abnormal exit). Reusing a
directory would make the next instance pop the "recover session" modal
right at startup — it swallows all clicks and manifests as fake failures
like "the maximize button does nothing" (hit during one debugging round,
hence this driver-level safety net).
    """
    created: list[App] = []

    def factory(files: list[Path] | None = None, env_extra: dict | None = None,
                config_dir: Path | None = None,
                handle_recovery: bool = True) -> App:
        cfg = config_dir or (tmp_path / f"config{len(created)}")
        cfg.mkdir(parents=True, exist_ok=True)
        (tmp_path / "work").mkdir(parents=True, exist_ok=True)
        a = App(exe_path, cfg, tmp_path / "work", files=files,
                env_extra=env_extra, screenshot_dir=OUT_DIR)
        # Track before start(): a launch that never finds its window (timeout,
        # early exit) still leaves the process behind, and teardown is the only
        # place that kills it. Appending after start() leaked exactly those.
        created.append(a)
        # handle_recovery=False: crash-recovery tests drive the recovery
        # prompt themselves (by default, startup auto-dismisses any
        # unexpected recovery prompt with a two-step confirmation; see
        # driver.start).
        a.start(handle_recovery=handle_recovery)
        return a

    yield factory

    # Newest first: each instance backs up the clipboard at its first use, so
    # the *oldest* backup is the user's original text — restoring it last
    # leaves the desktop clipboard as we found it.
    for a in reversed(created):
        try:
            if a.main_hwnd:
                a.screenshot("_teardown_last")
        except Exception:
            pass
        try:
            a.quit(force=True)
        except Exception:
            pass
        try:
            a.restore_clipboard()
        except Exception:
            pass


@pytest.fixture()
def app(app_factory):
    """Most common usage: an InfEditor launched without files."""
    return app_factory()


@pytest.hookimpl(hookwrapper=True, tryfirst=True)
def pytest_runtest_makereport(item, call):
    """Auto-screenshot failing tests into tests_py/out/."""
    outcome = yield
    rep = outcome.get_result()
    if rep.when == "call" and rep.failed:
        for val in getattr(item, "funcargs", {}).values():
            candidates = val if isinstance(val, list) else [val]
            for c in candidates:
                if isinstance(c, App) and c.main_hwnd:
                    try:
                        p = c.screenshot(f"FAIL_{item.nodeid.replace('/', '_')}")
                        print(f"\n[failure screenshot] {p}")
                    except Exception:
                        pass


def pytest_collection_modifyitems(config, items):
    """The GUI suite only makes sense on Windows (real windows + SendInput
    keyboard/mouse injection).

    The platform gate lives here instead of a `pytestmark` repeated atop
    every `test_*.py`: miss one file and it fails on other platforms with
    "exe not found" instead of skipping cleanly.
    """
    if sys.platform == "win32":
        return
    reason = "InfEditor GUI tests support Windows only (real windows + keyboard/mouse injection)"
    for item in items:
        item.add_marker(pytest.mark.skip(reason=reason))
