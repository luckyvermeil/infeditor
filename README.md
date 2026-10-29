# InfEditor

**Open huge files. Edit Markdown, JSONL and CSV the way they deserve.**

[![ci](https://github.com/luckyvermeil/infeditor/actions/workflows/ci.yml/badge.svg?branch=master)](https://github.com/luckyvermeil/infeditor/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
![Rust 1.85+](https://img.shields.io/badge/rust-1.85%2B-orange.svg)
![Platforms](https://img.shields.io/badge/platform-Windows%20%C2%B7%20Linux%20%C2%B7%20macOS-lightgrey.svg)

A text editor written from scratch in Rust — kernel and UI stack alike. The kernel is original:
piece table, sparse line index, encoding detection, regex search, syntax highlighting. So is the
interface: [Blitz](https://github.com/dioxuslabs/blitz) through Dioxus Native lays out and paints
every pixel. No Electron, no WebView, no native widget toolkit.

Windows is the first-class platform: frameless window with a merged title/menu bar, native
right-click menu, owned popup windows, touch-keyboard avoidance, Snap layouts. Linux and macOS
run the same core, with the OS-specific affordances degraded to in-page equivalents.

> **中文说明**：[README.zh-CN.md](README.zh-CN.md)

**Contents** — [At a glance](#at-a-glance) · [Screenshots](#screenshots) · [Features](#features) ·
[What it is not](#what-it-is-not) · [Shortcuts](#shortcuts-worth-memorizing) ·
[Platforms](#platform-support) · [Install](#install) · [Build](#build-from-source) ·
[Architecture](#how-it-is-put-together) · [Tests](#tests) · [License](#license) ·
[Contributing](#contributing)

## At a glance

| If you want to… | InfEditor gives you |
| --- | --- |
| Open a 500GB log today, not tomorrow | memory-mapped reads above 64 MB, progressive open (first screen paints first), background search |
| Actually read a CSV | Excel-style table view: edit cells in place, drag column widths, click a header to sort |
| See what a JSONL dump contains | click a line's gutter and it unfolds into a tree, right where it sits |
| Write a paper and its code samples | WYSIWYG Markdown with LaTeX / chemistry formulas, next to 28-language syntax highlighting |
| Keep your hands on the keyboard | one shortcut table drives both the menu labels and the key handler — plus full Vim mode |
| Survive a power cut | a session journal restores tabs, carets and **unsaved** buffers on the next launch |

## Screenshots

**Markdown — WYSIWYG document view.** Headings, inline formatting, formulas and the outline
sidebar, on `samples/wys-all-features.md`.

![Markdown document view with outline sidebar](assets/screenshot1.png)

**JSONL — unfolded in place.** Every line keeps its text; clicking the
gutter marker expands that one record into a tree.

![JSONL preview with an expanded record](assets/screenshot2.png)

**CSV — the table view, with rows and columns as first-class objects.** Header row, delimiter
switch, column sorting and in-place cell editing.

![CSV table view with header row and row/column actions](assets/screenshot3.png)

## Features

The sections are ordered by what InfEditor is for: files too big to open, data files you need to
see, prose you need to write — then everything else.

### Large files

- Large UTF-8 files (64 MB and up) are **memory-mapped** instead of copied into the heap. From
  128 MB they also open **progressively**: the first screen appears while a background thread
  finishes indexing, and the line count reads as an estimate until it does. Every open happens
  off the UI thread, with progress in the status bar.
- Above 4 MiB, syntax highlighting and the minimap switch themselves off (both can be turned
  back on), and search moves to a background thread.
- The minimap marks find matches across the whole document, not just the visible screen.
- Own scrollbars: drag, page, arrow, plus a progressive horizontal scan for very wide lines.

### Data files

- **CSV / TSV** — a real table view with Excel semantics. Double-click a cell to edit it (every
  keystroke is written back to the document), Enter moves down, Tab moves right, Esc restores;
  drag a header edge to resize a column; click a header to cycle ascending → descending → off.
  Insert and delete rows and columns, toggle "first row is header", and switch the delimiter
  between `,` `\t` `;` `|`. Sorting reorders the **view only** — the document keeps its byte
  order, so a sorted file saves unsorted and a read-only file can still be sorted; cell and
  structural edits write back with RFC 4180 quoting (minimal quotes on output).
- **JSON** — the whole document becomes a colored, collapsible tree (`{…} 12 items` when
  collapsed), with expand-all / collapse-all and a pretty-printer. JSON documents open straight
  into the tree view; unparseable or oversized files fall back to text with an explanation
  rather than a hang.
- **JSONL / NDJSON** — each line keeps its text; click the gutter marker to unfold that one line
  into a tree, `Ctrl+Alt+[` / `Ctrl+Alt+]` folds and unfolds the line under the caret. Parsing
  limits (4 MiB per line, 64 levels deep, 20k nodes) exist so one pathological record cannot
  take the window down.
- The view toggles appear in the toolbar only when the document type calls for them. CSV/TSV,
  JSONL and JSON open straight into their data view; an unparseable or oversized (8 MB+) JSON
  stays in text — the tree is a read-only preview, and fixing broken JSON needs the text view.
  The toolbar switches back and forth either way.

### Markdown

- **Two views**: a document view (WYSIWYG — you edit the formatted result, not the markup) and
  the Markdown source view. Markdown documents open in the document view (oversized files open
  as source); switching never loses the document; the WYSIWYG model writes back
  into the same text buffer, so dirty flags, autosave, crash recovery and `Ctrl+S` all keep
  working.
- **Formulas**: `$…$` and `$$…$$` are rendered to self-contained SVG by a pure-Rust LaTeX
  engine — no network, no browser, glyphs as paths with an embedded math font. `\ce{…}`
  chemistry, `\boxed`, `gathered` and friends are supported, and LaTeX that Markdown has
  escaped (`x\_n`, `\\geq`) is repaired before it reaches the renderer.
- **Structure editing**: headings, bold/italic/strikethrough, inline code, links, images,
  ordered / unordered / task lists, quotes, code blocks, tables (with column alignment and
  row/column add-remove), horizontal rules, emoji — from the toolbar or by typing.
- **A model, not contenteditable.** The document is a block tree; the DOM is derived from it and
  never written back. Cursor positions, table cells and undo all work on the model, and saving
  goes through one serializer instead of whatever the HTML parser happened to leave behind.
- **Outline sidebar** lists headings; click to jump.
- **Export** to HTML (formulas included as inline SVG) or back to Markdown.
- Remote images load over HTTPS; relative image paths resolve against the document's folder.

### Editing

- **Tabs, drag-and-drop, command line.** Drop files on the window, or run
  `infeditor notes.md src/main.rs` — each path becomes a tab. Dropping a file onto
  `infeditor.exe` in Explorer works the same way: if the editor is already running, that
  launch hands the file to the open window and exits (one editor per user, so two
  processes never fight over the same session journal).
- **Selection that matches the mouse.** Double-click selects a word, triple-click selects a
  line, and `Ctrl+Shift+B` drops a selection anchor: move the caret, press again, and the range
  between the two points is selected without dragging. The right-click menu also offers a
  rectangular block mode, where copy, cut and typing apply column by column.
- **The small things you notice after a day.** Auto-closing brackets and quotes, `Ctrl+P` to
  jump to the matching bracket, line bookmarks (`Ctrl+F2` / `F2` / `Shift+F2`), and a 10,000-step
  undo history.
- **Five-color marks.** Right-click → Style paints the selection (or the word under the caret)
  in one of five colors, either just here or every occurrence in the document. Marks carry the
  text they were created on, so after an edit they disappear instead of highlighting the wrong
  characters.
- **Hide what you don't need.** Select lines → Hide lines; the rest of the document reflows as
  if they were never there. Show all hidden lines brings them back.
- **Read-only mode** for when you only came to look — every modifying command is intercepted.
- **IME works.** Chinese / Japanese / Korean input goes through the system IME into the
  document; there is no "type into a hidden box and hope" mirroring step.
- Caret styles (line / block / underline, blinking on or off), font zoom, word wrap, visible
  EOL and whitespace markers.

### Files and encodings

- **Open and forget about encodings.** Detection goes BOM → strict UTF-8 → statistical guess
  (chardetng), so a GBK file from an old Windows box, a Big5 subtitle or a Shift-JIS log opens
  as readable text instead of mojibake. The result is shown in the status bar.
- **Switch and re-save** between UTF-8, UTF-8 with BOM, UTF-16 LE / BE, GBK, Big5, Shift-JIS,
  Windows-1252 and ANSI (the system code page). Newlines are detected and switchable between
  CRLF, LF and CR — independently of the encoding.
- Save, save as, save all, reload from disk, and a recent-files list that survives restarts.

### Code

- Syntax highlighting for **28 languages** plus plain text: C/C++, C#, Rust, Python,
  JavaScript, TypeScript, Java, Go, Lua, PHP, Ruby, Perl, SQL, HTML, XML, CSS, YAML, TOML, INI,
  Markdown, PowerShell, Shell, Batch, Diff, CSV, JSON, JSON Lines and log files.
  Language is detected from the extension and can be overridden from the **Language** menu.
- Line and block comments (`Ctrl+Q`, `Ctrl+Shift+Q`, `Ctrl+Shift+Alt+Q`), tab ↔ space
  conversion, trim leading / trailing whitespace.
- **Vim mode**: normal / insert / visual / command-line, toggled from the View menu.
- **Macros**: record, replay, name and save to disk, then rerun from the Macro menu or open the
  macro folder to edit the files by hand.
- **F5 runs a command** in the system shell with the current document expanded into
  `$(FULL_CURRENT_PATH)`, `$(CURRENT_DIRECTORY)`, `$(FILE_NAME)`, `$(NAME)` and `$(EXT)` — handy
  for "compile this file and show me the output". Recent commands are one click away.

### Find, replace, navigate

- `Ctrl+F` / `Ctrl+H`, with three pattern modes: **plain**, **regex**, **escape** (`\n`, `\t`,
  `\x41`…), plus match-case and whole-word toggles.
- **Incremental**: matches highlight and the view jumps while you type.
- The find UI is a separate window on Windows and a dockable in-page bar elsewhere: replace,
  replace-all, match count, a history dropdown, and a results panel that lists every hit. A
  short selection pre-fills the pattern.
- `F3` / `Shift+F3` for next / previous; `Ctrl+G` for go-to-line.

### The text toolbox

Case conversion, full-width ↔ half-width, Unicode NFC / NFD normalization, sort (plain,
case-insensitive, by length, by the first number on the line, by decimal), dedupe, reverse,
randomize, join / split lines, duplicate / delete duplicate lines, move lines up and down,
remove empty lines, trim (both / leading / trailing), line breaks to spaces, insert numbering,
insert date and time, MD5 / SHA-1 of the selection or the whole document, word count
(characters / words / lines / bytes) and the code point value of any character.

### Window and interface

- **Frameless**, with the title bar and menu bar merged into one strip: drag, resize from any
  edge, double-click to maximize, Snap layouts, always-on-top, and the system menu on
  right-click of the title bar.
- **12 menus** — File, Edit, Search, View, Encoding, Language, Settings, Tools, Macro, Run,
  Window, Help — with Alt mnemonics, and a native Win32 context menu that can extend past the
  window edges.
- **Popups are real windows on Windows** — find / preferences / numbering / about can be moved
  to another monitor while you keep editing. On Linux and macOS they render as in-page dialogs.
- **Workspace sidebar** (`Ctrl+Shift+O` opens a folder): directory tree, refresh, and a document
  outline panel for Markdown.
- **Light and dark themes** for the whole UI, not just the text.
- **Ten interface languages** — 简体中文, English, 日本語, 한국어, Français, Deutsch, Español,
  Português, Italiano, العربية — switched live from Preferences. Arabic strings ship; right-to-
  left layout is still on the list.
- Status bar: cursor position, selection size, line count, encoding, newline style, language,
  read-only and recording indicators.
- Settings persist between runs: theme, wrap, EOL/whitespace markers, minimap, caret style and
  blink, font size, Vim mode, auto-close, recent files, workspace folder and UI language.

### Not losing work

- **Crash recovery.** A session journal (`%APPDATA%\InfEditor\session\` on Windows) records open
  tabs, caret and scroll positions and full snapshots of dirty buffers a few seconds after you
  stop typing. After a power cut or a killed process, the next launch offers to restore —
  including content you never saved. Your original files are never touched.
- **External modification detection.** If a file changed on disk since you opened it, saving
  asks before overwriting.
- **Two-phase close.** Closing with unsaved changes offers Save / Don't save / Cancel, one
  document at a time.
- **Panics don't have to be fatal.** Worker threads and the window procedure run behind
  firewalls: a panic there is logged and the window keeps working. Every panic — swallowed or
  not — lands in `%APPDATA%\InfEditor\crash.log` with a backtrace and a `--- recovered` marker,
  so "it just closed" turns into a report someone can act on.
## What it is not

No plugin API, no language server or code completion, no embedded terminal, no Git panel.
InfEditor aims to be an unusually solid text editor for large and structured files, not an IDE.

## Shortcuts worth memorizing

| Keys | Action |
| --- | --- |
| `Ctrl+N` / `Ctrl+O` / `Ctrl+Shift+O` | new file / open file / open folder |
| `Ctrl+S` / `Ctrl+Alt+S` | save / save as |
| `Ctrl+F` / `Ctrl+H` / `Ctrl+G` | find / replace / go to line |
| `F3` / `Shift+F3` | next / previous match |
| `Ctrl+P` | jump to matching bracket |
| `Ctrl+F2` / `F2` / `Shift+F2` | toggle / next / previous bookmark |
| `Ctrl+D` / `Ctrl+J` | duplicate line / join lines |
| `Ctrl+Shift+↑` / `Ctrl+Shift+↓` | move line up / down |
| `Ctrl+Shift+B` | set a selection anchor (press again to select the range) |
| `Ctrl+Q` / `Ctrl+Shift+Q` | line comment / block comment |
| `Ctrl+B` | workspace sidebar |
| `Ctrl+Tab` / `Ctrl+W` | next tab / close document |
| `Ctrl+=` / `Ctrl+-` / `Ctrl+0` | zoom in / out / reset |
| `F5` | run a command |

Settings → Keyboard Shortcuts… lists the whole table; it is generated from the same declaration
the key handler uses, so a menu can never advertise a shortcut that does not fire.

## Platform support

| Capability | Windows | Linux / macOS |
| --- | --- | --- |
| Editor core (piece table, search, encodings, CSV / JSONL, Markdown) | ✔ | ✔ |
| Frameless window: drag / resize / maximize / always-on-top | native (`WM_NCHITTEST`, Snap) | winit gestures (`drag_window`; in-page edge strips drive `drag_resize_window`) |
| Context menu | native `TrackPopupMenu` | in-page overlay (submenus flattened to section headers) |
| Find / preferences / about popups | owned child windows (Blitz-rendered) | in-page dialogs |
| File dialogs | native (rfd / Win32) | Linux: xdg-desktop-portal; macOS: native panels |
| File association + Explorer context menu | ✔ (HKCU classes) | not applicable (menu entry hidden) |
| Touch keyboard avoidance / TabTip | ✔ | not applicable |

Linux builds need no system development packages — including per-crate builds such as
`cargo test -p wysiwyg`: fonts are discovered through fontconfig loaded at runtime, TLS is
rustls, and the portal dialogs are pure Rust.

Two affordances still lag on the experimental platforms: **Insert Numbering** needs an owned
popup window, so Linux/macOS report it as unavailable instead of opening an in-page form; and
**Run (F5)** hands the command to `sh -c` in the editor's own session, so its output is only
visible when the editor was started from a terminal.

## Install

Prebuilt packages are published on every `v*` tag — see
[Releases](https://github.com/luckyvermeil/infeditor/releases):

| Artifact | Target | What it is |
| --- | --- | --- |
| `InfEditor-<version>-x64-setup.exe` | Windows x64 | NSIS wizard, with an Add/Remove entry |
| `InfEditor-<version>-<arch>.msi` | Windows x64 / arm64 | WiX installer: per-machine, start-menu and desktop shortcuts, in-place major upgrade |
| `InfEditor-<tag>-<target>.zip` | Windows x64 / arm64 | portable executable — unzip and run |
| `infeditor_<version>_amd64.deb` | Linux amd64 | Debian package (experimental port) |

`<tag>` is the git tag (`v0.1.0`); `<version>` is the bare numeric version the
installers require (`0.1.0`). A `-symbols.zip` ships beside each Windows build —
keep the PDB next to the executable if you want `crash.log` backtraces to resolve
to `file:line`.

The Windows installers ask where to install, then offer the common file
associations: the same per-user (HKCU) `Open with` entries the editor writes from
Preferences → File Types. They never take over a default handler. Silent installs
skip the associations with `msiexec /i InfEditor.msi /qn REMOVE=FileAssociations`.
Missed the page the first time? On the MSI it stays reachable — Apps & features →
InfEditor → Modify, then Change on the maintenance page, reopens it with the
checkbox showing the current state.
One thing the installers do not do is install a runtime: the binary links the
dynamic MSVC CRT, so a machine without the VC++ redistributable needs
`vcruntime140.dll` once.

## Build from source

Requirements: **Windows 10+** (or Linux / macOS for the experimental build), **Rust 1.85+**,
and **Python 3** (stylo's build script needs it).

```bash
cargo run                 # development build
cargo build --release     # optimized: opt-level 3, thin LTO, line tables only
```

Point the build at your Python once, in a `.cargo/config.toml` that stays out of git:

```toml
[env]
PYTHON3 = "python"
```

The first build compiles the whole Blitz tree and takes a while. Blitz is redirected to the
local `vendor/blitz` copy by the `[patch]` section in the workspace root: that copy carries
sixteen fixes that have not landed upstream yet — accessible names for interactive elements
(screen readers saw empty ones), hit-testing that respects `overflow` clipping, selection
painting, pointer/click focus and touch-slop behaviour, IME commit handling, stale ids in the
cached paint tree. [`vendor/blitz/INFEDITOR-PATCH.md`](vendor/blitz/INFEDITOR-PATCH.md) lists
each patch with its problem, its change and how to remove it once upstream ships the fix.

Build the MSI locally with `packaging/windows/build-msi.ps1` — it writes
`InfEditor-<version>-<arch>.msi` into `dist/`, where every package lands (MSI, NSIS setup,
portable zips, DEB; needs the `wix` dotnet tool, which the script pins to v6.0.2).

## How it is put together

```
src/
├── buffer/       piece table, sparse line index, document, undo
├── io/           encoding detection, open (incl. progressive), save
├── search/       find / replace engine (snapshot-based, backgroundable)
├── textops.rs    pure text transforms: case, width, Unicode, sort / dedupe
├── format/       JSON / XML formatting, CSV parsing
├── highlight/    language definitions, line lexer, color themes
├── jsonl.rs      JSONL / NDJSON parsing and flattening
├── digest.rs     MD5 / SHA-1
└── ui/           Blitz layer: components, popups, state, commands
crates/           reusable sub-crates (below)
tests/            headless integration tests
tests_py/         GUI end-to-end tests (not part of the normal dev loop)
samples/          sample code and Markdown fixtures
```

Dependencies point one way only: `buffer` / `io` / `search` / `textops` / `format` /
`highlight` / `jsonl` are pure logic with no UI knowledge; `ui` sits on top of them and the
sub-crates. Platform differences are concentrated in `crates/platform`; the few Win32-specific
touch points in the UI (native context menu, frameless window hooks) are marked with
`#[cfg(windows)]` at the call site.

| Crate | Purpose |
| --- | --- |
| [crates/winshell](crates/winshell/README.md) | frameless window layer (Win32 and winit behind one facade) + Blitz multi-window popup framework |
| [crates/platform](crates/platform/) | the single home for OS differences: locale, code page, shared file opens, local time, file manager, shell, file association |
| [crates/wysiwyg](crates/wysiwyg/README.md) | model-driven WYSIWYG Markdown editor component, reusable on its own |
| [crates/ui-kit](crates/ui-kit/README.md) | business-agnostic UI component library for Blitz |
| [crates/i18n](crates/i18n/README.md) | compile-time message catalog (`Msg` / `Fmt`), ten locales, zero dependencies |

What the renderer can and cannot do today is documented upstream: Blitz's
[HTML / CSS support matrix](https://blitz.is/status/css).

## Tests

```bash
cargo test --workspace      # headless: kernel, components, integration
cargo clippy --workspace    # static checks
```

The GUI end-to-end suite drives a real window with real keyboard and mouse input. Build the
release binary first and keep the suite out of the normal dev loop:

```bash
cargo build --release
pytest
```

See [tests_py/README.md](tests_py/README.md).

[FEATURES.md](FEATURES.md) tracks what is done and what is next, feature by feature.

## License

This project (excluding `vendor/`) is licensed under **MIT**; see [LICENSE](LICENSE).

`vendor/blitz` is a local copy of upstream Blitz (MIT OR Apache-2.0, with `stylo_taffy`
additionally under MPL-2.0). Its license and copyright belong to its respective authors and are
not covered by this project's LICENSE.

## Contributing

Issues and pull requests are very welcome. Found a bug, have a feature idea, or want to fix
something yourself? Open an issue to report it or discuss it first, then send a PR. The code is
split into small, single-purpose crates (see [How it is put together](#how-it-is-put-together)),
so most changes stay localized and are easy to review.
