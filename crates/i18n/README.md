# i18n: UI text framework and migration plan

The UI language lives in `EditorState::locale` (`i18n::Locale`); the message keys
live in `src/keys.rs` (one row = one variant) and the wording lives in
`src/locales/<lang>.rs` — **one file per language**, nothing else carries either.

```rust
span { "{st.tr(Msg::StatusReadOnly)}" }                 // fixed text, borrows &'static str
span { "{st.tr(Fmt::StatusLines { lines, noun })}" }    // with arguments, one format!
```

## 1. Why it needed real work

About 600 user-visible strings were spread over 58k lines. The problem was not
"nothing is translated" but three structural defects — fixing only the words would
have turned Chinese bugs into English bugs:

| Defect | Where | Effect |
| --- | --- | --- |
| **Text used as identity** | `Cmd::Lang("纯文本")`, `LangSpec.name`, `MenuUi::open: Option<String>`, `CAPTION_ACCELS[i]` | Once a caption or language name is translated, the open state no longer matches itself, a menu flies out the wrong submenu, and persisted `meta.file_type` plus `by_name()` stop resolving |
| **Rendered text stored in state** | `Dialog.title: String`, `st.status: String`, `doc.meta.display_name` | A dialog, status message or tab name open during a switch stays in the old language — and it is no longer "text" but "data", so nobody thinks to recompute it |
| **Duplicated wording** | `"打开所在目录"` vs `"打开文件所在目录"`; two copies of `"样式 1..5"`; cut/copy/paste in three places | One edit misses two sites; translators pick different words for the same command |

The framework fixes the first two with types (`Locale` and `Msg` are different
things; `Fmt` keeps arguments as fields and renders late) and the third with one
table (one command, one key).

## 2. Framework

```
        crates/i18n  (no dependencies, no build script, no proc macro)
        ┌────────────────────────────────────────────────┐
callers →│ Locale::tr(impl Text) → Cow<'static, str>       │
        │   ├── Msg   (no arguments) ── borrows            │
        │   └── Fmt   (arguments)    ── one format!        │
        │            ↑                                     │
        │   keys.rs: one row = one variant (key + fields)  │
        │   locales/<lang>.rs: one exhaustive match each   │
        └────────────────────────────────────────────────┘
```

| Type | Role | Property |
| --- | --- | --- |
| `Locale` | Language identity (`ZhCn` / `EnUs`) | Persisted as `tag()` (`"zh-CN"`), never as an index |
| `Msg` | Fixed text: menu entries, buttons, titles | Renders `Cow::Borrowed` — **no allocation**; `get(loc)` for APIs that demand `&'static str` (window titles, registry) |
| `Fmt` | Text with arguments: status bar, dialog bodies, questions | Arguments are fields, so each locale writes its own word order |
| `Text` | Shared trait | One spelling at call sites, monomorphized, no vtable |
| `i18n::current()` | Process-level mirror | For code without `EditorState` (library internals, crash log, registry); the only writer is `EditorState::set_locale` |

### Three guarantees, all from the compiler

1. **No missing translation.** Every `locales/<lang>.rs` is an exhaustive `match`
   over the whole enum, and the per-locale dispatcher in `locales/mod.rs` is an
   exhaustive `match` over `Locale` — a missing row or an unwired locale stops
   the build.
2. **No typo'd placeholder.** Each locale of a `Fmt` is a `format!` *literal* whose
   placeholders are the destructured field names. `{lien}` is a "cannot find value"
   error — not a panic, not literal braces on screen.
3. **No allocation for fixed text.** The menu rebuilds a few hundred labels per
   frame and every one of them is borrowed; only rows with numbers `format!`.

## 3. Usage

```rust
// 1. With an EditorState (the normal path in UI code)
let label = st.tr(Msg::MenuSave);                    // Cow<'static, str>
let text  = st.tr(Fmt::StatusLines { lines, noun });

// 2. With only a Locale (pure functions, tests, menu builders)
fn items(loc: Locale) -> Vec<Item<'static>> { … loc.tr(Msg::CaseUpper) … }

// 3. Without state (library internals, registry, crash log, window titles)
i18n::current().tr(Msg::CommonCancel);               // Cow
Msg::PrefsTitle.get(i18n::current());                // &'static str
```

Do **not** call `i18n::current()` in UI code: it bypasses `Signal<EditorState>`, so
a language switch would not redraw. It is a mirror for one-shot effects outside the
reactive tree.

## 4. Multiple windows

Title bar, menu, status bar and dialogs live under the main window's
`Signal<EditorState>`, so a `locale` change redraws them in the same frame. Find,
Preferences and About are separate vdoms and follow their `Snap` mirror
(`PrefsSnap` already carries `locale`) — a new window must add it, or it stays in
the language it opened with.

`EditorState::set_locale` does four things: write the field, update the mirror,
**close the menu**, recompute derived names. Closing is required: the open state is
language-neutral, but the fly-out is located by row position.

## 5. Adding a language

1. Add a `Locale` variant with `tag()` and `native_name()` (a self-name, never
   translated), to `Locale::ALL` (the picker and the tests pick it up), and to
   `from_tag` (startup detection and `INFEDITOR_LANG`).
2. Add the variant to the two dispatcher `match`es in `locales/mod.rs`.
3. Copy `locales/en_us.rs` to `locales/<tag>.rs` and translate the literals.
   The file is an exhaustive `match`, so the compiler enforces completeness:
   a language ships only when every row is filled — there is no fallback path
   to half-ship through.

## 6. Known limits (decisions, not oversights)

| Behaviour | Reason | Follow-up |
| --- | --- | --- |
| English plurals | Chinese has no inflection; a `{}` template would freeze word order | A few count messages compute one fragment at the call site with `i18n::plural(n, "line", "lines")` (see `DlgQuitBody`). Full plural rules only when a language needs them |
| Encoding and newline names are not translated | `UTF-16 LE`, `GBK`, `Big5`, `Windows (CR LF)` are proper nouns | Translating them would hide the name the user is looking for — same rule as language self-names |
| English labels are wider | Same font size, different advance widths: the menu bar estimates caption width per character class, the toolbar adapts per label | Already adaptive; a Latin-first font stack for English is S6 polish |
| Registry text (`MUIVerb`, `ApplicationDescription`) | Explorer caches it, so a switch needs re-registration | **Done (S5)**: written with `i18n::current()`; replay registration after `Cmd::SetLocale` if live switching is wanted |
| File dialog titles | Drawn by the OS, passed in per call | **Done (S5)**: taken from `st.tr(…)` |
| An open dialog keeps its language | `Dialog` holds a snapshot (`title: String`, `body: Text(String)`) | **Done (S4)**: `Dialog.title` / `DialogButton::label` / `DialogBody::Text` hold `i18n::Message` and render on draw |
| System title bar (window title) | `winshell::Popup::title()` is read at window creation | Preferences already returns `Msg::get(current())`, so it updates on the next open; live updates need the popup side to call `set_title` |
| Untitled placeholder name | It is **document state** (save-as default, tab, title bar read it) | Handled by semantics: no path and a name matching the placeholder in *any* language → recomposed on switch; a name the user chose is never touched (`rerender_derived_names`) |
| Arabic catalog is translated, the layout is still LTR | RTL mirroring is a rendering-level feature (direction, alignment, ellipsis side), not a catalog one | Flip per-view `direction: rtl` when RTL support is wanted; the text is already correct |

## 7. Migration plan

Execute in this order. For each file: move text into `catalog.rs` → call `tr(…)` →
`cargo test` → **lower the ratchet baseline**.

### S0 Framework (done)

- `crates/i18n`: `Locale`, `Msg`, `Fmt`, `Text`, process mirror, 220 rows;
- `EditorState.locale` + `tr()` + `set_locale()`; `Cmd::SetLocale` (a settings
  command: macros skip it, read-only allows it);
- `AppSettings.locale` (a tag) with startup resolution: setting →
  `INFEDITOR_LANG` → system UI language → default;
- Preferences: a Language page (radios, self-names); `PrefsSnap.locale`;
- `tests/cjk_ratchet.rs` and `tests/i18n.rs`.

### S1 De-stringify identity (done)

| Change | Files |
| --- | --- |
| `Caption` replaces `CAPTION_ACCELS` + string captions; `MenuUi::open: Option<Caption>`; `caption_x(st, cap)` indexes directly | `actions.rs`, `state/menu.rs`, `state/mod.rs`, `components/{titlebar,menu}.rs` |
| Every menu tree label via `loc.tr(…)`; one key per command (`RevealInFolder`, `CtxMarkStyle1..5`) | `actions.rs` (257 → 23) |
| Status bar: position, selection, line count, macro steps, read-only, opening, modified, four tooltips | `components/status.rs` (17 → 0) |
| Quit / About / Preferences titles | `state/docs.rs`, `prefswin.rs` |

### S2 Command feedback: `st.status: String` → `Option<Fmt>` (~200 rows)

Today it is `st.status = format!("已标记第 {} 行", line + 1)` in 215 places. Each
becomes `Some(Fmt::BookmarkSet { line })` and the status bar renders it — which also
means the message follows a language switch, and state no longer holds formatted
strings.

Files (remaining count): `commands/csv.rs`(42), `commands/edit.rs`(24),
`commands/markdown.rs`(15), `commands/json.rs`(11), `commands/lines.rs`(9),
`commands/bookmarks.rs`(5), `commands/{brackets,mod}.rs`(2), `state/opens.rs`(20),
`state/docs.rs`(30), `state/mod.rs`(10), `state/marks.rs`(9),
`state/workspace.rs`(9), `state/hidden.rs`(5), `state/outline.rs`(4),
`state/{json,jsonl,dialogs}.rs`(5), `actions.rs`(23). Replace `on_off(v)` with
`Msg::CommonOn` / `CommonOff` on the way.

### S3 Find/replace and toolbars (209 rows)

`find.rs`(66) result panel and messages, `findwin.rs`(25) popup, `findbar.rs`(19),
`components/toolbar.rs`(99) including `MenuKind::tip`. Toolbar button widths are
already label-adaptive, so longer English labels need no layout change.

### S4 Dialogs and remaining views (~140 rows)

`components/dialog.rs`(16) first: make `Dialog.title`, `DialogButton::label` and
`DialogBody::Text` hold `Fmt`, otherwise every new dialog adds another
render-at-open snapshot. Then `recovery.rs`(11), `assoc.rs`(17), `aboutwin.rs`(7),
`numwin.rs`(5), `macros_io.rs`(8), `mdws.rs`(11), `sidebar.rs`(5), `titlebar.rs`(5),
`outline.rs`(4), `settings.rs`(3), `view/{tabs,viewport,composer,editor}`(12),
`csvtable.rs`(1), `recorder.rs`(5), `shortcuts.rs`(1), `vim.rs`(4),
`vim_bridge.rs`(2), `mdnet.rs`(4), `jsonl.rs`(17), `highlight/{code,theme}.rs`(4),
`io/file.rs`(2), `main.rs`(1).

### S5 Libraries and OS surfaces

- `crates/wysiwyg`: `md.rs`(3), `render.rs`(2), `preview.rs`(1) — messages and
  placeholders. These functions cannot see `EditorState`: use `i18n::current()`, or
  pass `locale` through the `Wysiwyg` props (consistent with receiving `Theme`).
- `crates/ui-kit`: `theme.rs`(2), `modal.rs`(1) — a component library should not
  ship wording; turn them into required props with empty defaults.
- `crates/winshell`: `shell/mod.rs`(2) — window class name and default title.
- Registry text (`assoc.rs`), file dialog titles, `frameless::set_title` call sites,
  crash log (`main.rs`).

### S6 Polish

- A Latin-first UI font stack when the locale is English;
- Recheck the window width where the twelve English captions start overflowing into
  the more-button;
- Turn tests that assert Chinese wording into tests that assert keys
  (`Msg::key()` / `Cmd`), so wording becomes editable;
- Live window-title updates on a language switch.

## 8. Tooling and acceptance

**Ratchet** (`tests/cjk_ratchet.rs`): counts user-visible hard-coded Chinese per
file (comments and `#[cfg(test)]` excluded). It may only go down; currently 596.

```bash
cargo test -p i18n                       # framework + ratchet
INF_I18N_BASELINE=1 cargo test -p i18n --test cjk_ratchet -- --nocapture   # re-baseline
cargo test                               # everything, including tests/i18n.rs
```

**Per stage**:

1. that file's baseline equals the measured value (or leaves the table);
2. headless tests pass in both languages (`tests/i18n.rs` shows the pattern);
3. a manual look at narrow-window overflow (English labels are wider).

**Release gate**: `en-US` is not shippable before S3 — an English menu over a
Chinese toolbar is worse than a Chinese UI. The ratchet reaching 0 is what
"translated" means.

## 9. Anti-patterns (reject in review)

- UI text written under `src/` — the ratchet fails;
- storing the result of `st.tr(…)` in an `EditorState` field, unless it is a
  **derived** name with a recompute path — it freezes into that language;
- matching, comparing or keying on display text — `Caption`, `Msg` and `Cmd` are
  identities;
- two keys for one UI element — the wordings drift apart;
- positional `{}` in a `Fmt` — word order breaks in the other language, use `{name}`.
