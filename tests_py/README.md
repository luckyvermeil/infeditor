# InfEditor GUI end-to-end tests

These tests drive a **real InfEditor process**: they launch the release executable, inject real
keyboard and mouse input through `SendInput`, read the result back from the clipboard, the OS
window title and the files on disk, walk the accessibility tree over UIA, and leave screenshots
behind. They cover what `cargo test` cannot — window and focus behaviour, text input (including
CJK), self-drawn controls, modal dialogs and rendering.

The suite is **not part of CI**: it needs an interactive desktop, a foreground window and the
system clipboard.

## Running

Requirements: Windows 10+ (the driver is Win32-bound), a release build, and Python 3 with the
packages in `tests_py/requirements.txt`.

```bash
cargo build --release                        # the tests look for target/release/infeditor.exe
pip install -r tests_py/requirements.txt
python -m pytest tests_py -v
```

While iterating on one area:

```bash
python -m pytest tests_py/test_find.py -v
```

Point the suite at another binary:

```bash
INFEDITOR_EXE=path/to/infeditor.exe python -m pytest tests_py
```

**Run serially.** Every test shares the real desktop focus and the system clipboard, so `-n`
(pytest-xdist) interleaves two windows and turns assertions into coin flips.

**UI language: Simplified Chinese.** Window titles and dialog buttons are asserted as localized
text (`未命名` "Untitled", `保存` / `不保存` / `取消` "Save / Don't save / Cancel", `恢复` /
`放弃快照` "Restore / Discard snapshot"), and the driver clicks dialog buttons *by label*.
`App.start` therefore pins `INFEDITOR_LANG=zh-CN` by default (each test also gets a fresh config
dir), so the suite behaves the same on an English host. Export the variable only when you
deliberately want another language — the assertions assume Chinese labels.

```bash
INFEDITOR_LANG=zh-CN python -m pytest tests_py   # already the driver default
```

## Layout

| Path | Contents |
| --- | --- |
| `infed/driver.py` | process and window control, input injection, clipboard oracles, dialog geometry, screenshots |
| `infed/keys.py` | `SendInput` wrapper: virtual-key typing and key combos |
| `infed/clipboard.py` | clipboard read/write (`CF_UNICODETEXT`, with retries) |
| `conftest.py` | fixtures: executable lookup, per-test directories, app lifecycle, failure screenshots, Windows-only gate |
| `out/` | screenshots: `FAIL_<nodeid>.png` on failure, `_teardown_last.png` at teardown |

## Coverage

| Area | Tests | What they pin |
| --- | --- | --- |
| Launch and windows | `test_smoke` · `test_window` · `test_view` | real launch, title, self-drawn window buttons, zoom round-trips |
| Accessibility | `test_a11y` | UIA tree (accesskit is activated lazily), keyboard-only menu operation |
| Editing and documents | `test_editing` · `test_files` · `test_close_dialog` | typing incl. CJK/emoji, undo/redo, paste, save round-trip, tabs, dirty-close confirmation |
| Find, replace, go to | `test_find` · `test_replace` · `test_goto` | standalone find/replace window, F3 walking, in-page goto bar |
| Markdown and WYSIWYG | `test_markdown_features` · `test_wys_all_features` · `test_wys_spaces` | view-switch matrix and status text, toolbar commands, byte-identical sample round-trip, caret across formula/image atoms, table editing, space glyphs |
| Data and workspace | `test_csvtable` · `test_sidebar` | Excel-style cell editing, directory tree, `Ctrl+B`, restart restore |
| Crash recovery | `test_recovery` | abnormal exit → prompt → restore and discard paths |
| Regressions | `test_regressions` | GUI-only fixes that cargo tests cannot reach |

## Fixtures

| Fixture | Use |
| --- | --- |
| `exe_path` | session-scoped executable lookup (`INFEDITOR_EXE`, else `target/release/infeditor.exe`) |
| `work_dir` | a fresh directory for test files, under `tmp_path` |
| `app` | one instance with no files — the common case |
| `app_factory` | build instances with options; every instance is killed and its clipboard restored at teardown |

`app_factory` arguments:

- `files=[...]` — pass paths on the command line (one tab each);
- `env_extra={...}` — extra environment, e.g. `{"INF_FIND_SMOKE": "1"}`;
- `config_dir=...` — **reuse** a config directory (needed by recovery tests: `session.json` from
  the previous abnormal exit lives there);
- `handle_recovery=False` — do not auto-dismiss a startup recovery prompt; recovery tests drive
  it themselves.

## Isolation and safety

- Each instance gets its own `INFEDITOR_CONFIG_DIR`, so the real `settings.json`, macros and
  rescue sessions are never read or written.
- Test files live under pytest's `tmp_path`.
- The clipboard is backed up before the suite first writes to or reads from it (including the
  CJK paste channel) and restored at teardown, so the original text survives the run.
- Instances never outlive a test: `quit()` sends `WM_CLOSE` and falls back to `kill()`; teardown
  kills outright.
- The driver zeroes Windows' foreground-lock timeout (`SPI_SETFOREGROUNDLOCKTIMEOUT`) so
  `SetForegroundWindow` succeeds; the previous value is restored when the pytest process exits.
- `INFEDITOR_IME=0` and `INF_UI_PROBE=1` are injected by default (see the table below).
- A failing test screenshots the live window into `out/` (`FAIL_<nodeid>.png`).

## Oracles, in order of trust

1. **Clipboard** — `app.doc_text()` (`Ctrl+A` → `Ctrl+C`) or `app.copy_selection()`. The only
   lossless view of what the editor actually holds; line endings are normalized to `\n`.
2. **Window title** — file name, dirty marker and `[n/m]` tab counter all live there.
3. **Files on disk** — after a save, the bytes are the only trustworthy source for encoding and
   line endings.
4. **UIA names** — status-bar text and the accessibility tree. Good for "which view am I in";
   useless for "which button did I click" (see the field notes).
5. **Pixels** — `mean_diff()` on screen grabs, only where nothing else can tell (zoom, ink
   presence). Prefer round-trips over absolute images.

## Environment variables

| Variable | Effect |
| --- | --- |
| `INFEDITOR_EXE` | executable to test (default `target/release/infeditor.exe`) |
| `INFEDITOR_CONFIG_DIR` | config directory; the fixture points it at a per-test temp dir |
| `INFEDITOR_LANG` | UI language; the driver pins `zh-CN` (assertions assume Chinese labels) |
| `INFEDITOR_IME=0` | disable IME so a real input method cannot swallow injected keys |
| `INF_UI_PROBE=1` | app writes self-drawn dialog button geometry and labels to `shell.log` |
| `INF_SHELL_LOG=1` | verbose shell log; also the oracle when the screen cannot be captured |
| `INF_FIND_SMOKE=1` | open the find window ~1.5 s after launch (smoke test only) |
| `INF_WS_SMOKE=<dir>` | open `<dir>` as the workspace folder at startup (sidebar tests) |
| `INF_WYS_LOG=1` | WYSIWYG component debug log (`shell.log`) |

`shell.log` lives in the config directory, and the driver truncates it at every launch — probe
lines (`[dialog]`, `[frame]`, `[timing]`) from a previous run would otherwise skew the geometry you
read.

## Field notes

- **Focus.** winit windows are not in the Win32 focus model (`GetGUIThreadInfo` is always empty),
  so `SetForegroundWindow` alone sometimes drops keys silently. `focus_soft()` uses pywinauto's
  `set_focus()`; `focus_click()` adds a real click, which is the only reliable way to land
  keyboard focus. Leave ~0.3 s after the click before typing.
- **Input.** The app only accepts virtual-key events: `KEYEVENTF_UNICODE` and hand-made `WM_CHAR`
  are ignored. ASCII is typed key by key; Chinese and emoji go through the clipboard plus
  `Ctrl+V`, because IME commit events cannot be injected from outside. The app runs with
  `INFEDITOR_IME=0` so a live IME cannot intercept the synthetic keys.
- **Find dialog.** `Ctrl+F` on Windows opens a separate top-level window that takes focus itself.
  Type into it with `type_text(..., focus=False)`, or the text lands in the document. Read the
  selection **before** pressing Esc: closing the find UI clears it.
- **DPI and coordinates.** pywinauto switches the process to per-monitor DPI aware at import time;
  `infed/__init__.py` pins the level even earlier, so every coordinate in the suite is a physical
  pixel. Convert CSS to screen through `app.dpi_scale()` — never multiply by the display scale
  yourself.
- **Screenshots.** `snapshot()` / `screenshot()` are screen grabs, not `PrintWindow` (a wgpu
  surface comes back black). The window must be foreground, and an always-on-top window can pollute
  the image. Under a locked screen you only capture the lock screen; switch those assertions to
  the `shell.log` oracle.
- **Dragging.** The app sets `user-select:none` on text leaves and paints the selection itself, so
  Blitz's native selection never starts. A drag must deliver a real pointer-move sequence (2 px
  threshold): use `drag_at_css()`, which steps 14 intermediate positions. A single teleport reads
  as two separate clicks.
- **Multi-click.** Double and triple click are counted inside the app (500 ms window, 8 px slop),
  not by the OS. Use `double_click_at_css()` / `triple_click_at_css()`; hand-rolled clicks that
  drift farther apart count as separate clicks.
- **Pixel diffs.** `mean_diff()` returns the mean per-pixel difference: caret blink and font
  antialiasing stay below ~1, a real change scores above 10, so tests threshold at 3. Blank
  regions are identical before and after (pure white in the light theme) — confirm the box has
  content first. When reading `session.json`, Windows paths are `\\`-escaped: `json.loads` before
  comparing.
- **Config directory reuse.** A killed instance leaves a recovery session; the next launch shows a
  modal that swallows every click and looks like "the button does nothing". `app_factory` gives
  each instance a fresh directory and auto-dismisses an unexpected recovery prompt.
- **Toolbar.** Buttons are self-drawn; their UIA names are always present, which makes them a
  vacuous oracle. Click by computed CSS position (`md_toolbar_x()`) and assert the status text the
  action produces (`已切换至…视图`, "switched to … view"). `test_regressions.py` derives the CSV
  band from the same constants as `toolbar::layout` (`_toolbar_x`, pinned on the Rust side by
  `toolbar::tests::csv_toolbar_button_positions`) and passes the expected status text to
  `_click_toolbar_button`, so a drifted coordinate fails instead of passing silently.
- **Geometric clicks prove they landed.** A click on a self-drawn control is pure geometry, so a
  wrong constant degrades into "nothing happened" — and any assertion of the form "the document is
  still clean" then holds either way. That is a *false pass*, not a flaky test: it never turns red.
  Two rules keep it out: `App.click_at_css` (and the double/triple/drag/popup variants) **refuses**
  a point outside the target's client area, and every coordinate click is followed by a positive
  oracle (a status text, a title change, disk bytes) that only holds if the app saw the input.
  The audit found one test that had been passing this way for months, with its toolbar click
  landing outside the window: accesskit had reported a 0×0 rect for the button.
- **UIA blind spots.** accesskit builds its tree lazily and not every rendered node is named —
  inline SVG formulas and some rich-text nodes can come back without a name. Fall back to region
  ink/chroma counts and poll: the first frames after a view switch can be blank.
- **Self-drawn dialogs.** No HWND, no controls. Geometry and labels come from the app's own probe
  (`INF_UI_PROBE=1` → `[dialog] centers=… labels=…`), and `click_dialog_button(i)` /
  `click_dialog_label(text)` drive it. Never guess by colour or coordinates: clicking the wrong
  button in a modal can turn "cancel" into "discard unsaved work".
- **Line endings.** `Path.write_text` translates `\n` to `\r\n` on Windows. Write LF fixtures with
  `write_bytes`, or "the file is unchanged after a save" compares CRLF against LF.

## Writing new tests

- Type with `app.type_text()` and `app.keys()`. Do not call `pywinauto.keyboard` directly: it has
  no focus control and bypasses the ASCII/Unicode channel split.
- `app.doc_text()` leaves the whole document selected. Collapse the selection (`Ctrl+End` or a
  click) before typing, or the next character replaces everything.
- Opening several files loads them in parallel; the active tab depends on completion order. Assert
  "two tabs" and switch with `Ctrl+Tab` instead of pinning an order.
- Prefer behaviour oracles. Where only pixels can tell (zoom), assert a round-trip — changed, then
  changed back — not an absolute image.
- Every geometric click needs a **positive** follow-up assertion. "Nothing bad happened" (no dirty
  marker, unchanged disk) passes just as well when the click never reached the app; pair it with
  something the action writes and nothing else does (status text, title, a changed cell).
- Assert the **specific** message of the guard under test, and satisfy its preconditions first. A
  command usually has several refusals; a missing precondition (e.g. no CSV selection) is answered
  by an *earlier* guard, so "the refused operation left the file alone" holds for a code path the
  test never meant to exercise. `csv_insert_col` is the live example: it needs a column selection,
  and without one it says "先选中一列（点击列标）" long before the quote guard runs.
- **Never inspect a directory that the process CWD also writes into.** `App.start` runs the app
  with `cwd=work_dir`, and other software writes there too: measured on this machine, the GPU
  driver creates `NVIDIA Corporation/umdlogs` in the CWD within a second of the first frame.
  Any test that reads the CWD as content (the workspace sidebar does) must keep its root one
  level down (`WS_SUB`) **and** assert the visible rows before clicking — an unexpected first row
  silently shifts every row-indexed click, which reads as "the sidebar stopped working".
- Markdown fixtures must be in canonical form (no blank line between adjacent lists, `|---|`
  separator rows). A non-canonical document is normalised on the first view switch and marked
  dirty, so a "browse and save" test would fail for the wrong reason.
- Keep a test on its own instance unless the states chain naturally: every `app_factory()` call is
  a full process launch.
