//! The command vocabulary shared by the menu bar, the editor context menu and
//! the keyboard map, plus the editor's menu tree.
//!
//! Having one [`Cmd`] enum means a menu entry, a context-menu entry and (later)
//! a key binding can never drift apart: they all name the same command, and
//! [`run`] is the single place that decides what it does.

use std::borrow::Cow;

use crate::textops::{CaseKind, SortKey};
use crate::ui::state::{CaretStyle, EditorState};
use crate::ui::{Fmt, Locale, Msg};

/// Every command the editor understands.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Cmd {
    // ---- File ---------------------------------------------------------------
    New,
    Open,
    OpenFolder,
    /// File → Open Folder...: pick a directory as the workspace, sidebar shows its tree
    /// (see `state::workspace`)
    OpenWorkspace,
    /// File → Recent Files, item `i` (index into `EditorState::recent`)
    OpenRecent(usize),
    Reload,
    Save,
    SaveAs,
    SaveAll,
    Close,
    CloseAll,
    Quit,
    /// Quit *without* asking again — the Quit button of the unsaved-changes
    /// confirmation (`Cmd::Quit` only asks the question).
    ForceQuit,
    /// Close confirmation → Save (two-phase close, see `EditorState::close_doc`)
    CloseSave,
    /// Close confirmation → Don't save
    CloseDiscard,
    /// Close confirmation → Cancel
    CloseCancel,
    /// Reload confirmation → discard in-memory changes and reload (two-phase, see `EditorState::reload_active`)
    ReloadConfirm,
    /// Reload confirmation → Cancel
    ReloadCancel,
    /// Startup recovery dialog → Restore (crash rescue, see [`crate::ui::recovery`])
    RecoveryRestore,
    /// Startup recovery dialog → Discard snapshot (asks for a second confirmation first; deletes nothing yet)
    RecoveryDiscard,
    /// Snapshot discard confirmation → confirm discard (actually deletes)
    RecoveryDiscardConfirm,
    /// Snapshot discard confirmation → Cancel (back to the recovery dialog)
    RecoveryDiscardCancel,

    // ---- Edit ---------------------------------------------------------------
    Undo,
    Redo,
    Cut,
    Copy,
    Paste,
    Delete,
    SelectAll,
    CopyAll,
    CopyFileName,
    CopyFilePath,
    /// Edit → Begin/End Select
    BeginEndSelect,
    /// Edit → Go to matching bracket (Ctrl+P)
    GotoBracket,
    /// Settings → Auto-close brackets while typing
    ToggleAutoClose,
    /// Edit → Toggle line bookmark (Ctrl+F2)
    BookmarkToggle,
    /// Edit → Next bookmark (F2)
    BookmarkNext,
    /// Edit → Previous bookmark (Shift+F2)
    BookmarkPrev,
    /// View → Read-only mode: intercept all document modification operations (prevent accidental changes when reading large files)
    ToggleReadOnly,
    /// View → Folder sidebar (Ctrl+B): show/hide the workspace directory tree
    ToggleSidebar,
    /// View → Document outline: switch sidebar to outline panel (Markdown heading tree levels, see
    /// `state::outline` / `components::outline`)
    ToggleOutline,

    // ---- Case conversion ----------------------------------------------------
    Case(CaseKind),

    // ---- Line operations ----------------------------------------------------
    DuplicateLine,
    DeleteDupLine,
    DeleteConsecutiveDupLine,
    SplitLines,
    JoinLines,
    MoveLineUp,
    MoveLineDown,
    InsertBlankAbove,
    InsertBlankBelow,
    ReverseLines,
    RandomizeLines,
    /// `(key, descending)`
    SortLines(SortKey, bool),

    // ---- Whitespace operations ----------------------------------------------
    TrimBoth,
    TrimLeading,
    TrimTrailing,
    EolToSpace,
    TabToSpace,
    SpaceToTab,
    SpaceToTabLeading,
    RemoveEmptyLines,
    RemoveEmptyLinesWithBlank,

    // ---- Comments ---------------------------------------------------------------
    ToggleLineComment,
    BlockComment,
    BlockUncomment,

    /// Edit → Insert → Number... (start value/increment dialog)
    InsertNumbering,
    /// Encoding → Unicode normalization → NFC (precomposed)
    UnicodeNfc,
    /// Encoding → Unicode normalization → NFD (decomposed)
    UnicodeNfd,

    // ---- Insert -------------------------------------------------------------
    InsertShortDateTime,
    InsertLongDateTime,

    // ---- Find ---------------------------------------------------------------
    /// find bar: `(with replacement row, with results panel)`
    Find(bool, bool),
    Goto,
    FindNext,
    FindPrev,

    // ---- View ---------------------------------------------------------------
    ZoomIn,
    ZoomOut,
    ZoomReset,
    ToggleTheme,
    ToggleHighlight,
    HighlightAll,
    WordWrap,
    ShowEol,
    ShowWs,
    /// Minimap on the right edge
    ToggleMinimap,
    /// JSON Lines preview (click line head marker to inline expand JSON tree)
    ToggleJsonl,
    /// Collapse all expanded JSONL lines
    JsonlCollapseAll,
    /// Collapse current caret JSONL line/node (Ctrl+Alt+[)
    JsonlFoldLine,
    /// Expand current caret JSONL line (Ctrl+Alt])
    JsonlUnfoldLine,

    /// JSON tree view preview (whole-document colored foldable tree, directly replaces text field)
    JsonPreview,
    /// Collapse all containers of the JSON tree (root only)
    JsonCollapseAll,
    /// Expand all containers of the JSON tree (full tree)
    JsonExpandAll,

    // ---- Encoding -----------------------------------------------------------
    /// charset name, e.g. `UTF-8`, `UTF-8 BOM`, `GBK`
    SetEncoding(&'static str),
    /// `CRLF` / `LF` / `CR`
    SetNewline(&'static str),

    // ---- Language -----------------------------------------------------------
    /// language by display name, `__auto` = re-detect
    Lang(&'static str),

    // ---- Input mode ---------------------------------------------------------
    /// Vim-style modal editing (View → Vim mode)
    ToggleVim,

    // ---- Macro --------------------------------------------------------------
    /// Macro → Start/stop recording (same command; label flips with the state)
    MacroRecord,
    /// Macro → Play recording
    MacroPlayback,
    /// Macro → Save macro...: opens a name input box (submit = [`Cmd::MacroSaveNamed`])
    MacroSave,
    /// Submit of the save-macro dialog: writes the last recorded macro to disk under the entered name
    MacroSaveNamed,
    /// Macro → Run macro...: lists macros on disk (submit button = [`Cmd::MacroPlaySaved`])
    MacroRun,
    /// Replay the `i`-th macro on disk (index matches the `EditorState::macro_files` snapshot)
    MacroPlaySaved(usize),
    /// "Open macro folder" in the save-macro dialog
    MacroDir,

    // ---- Run ----------------------------------------------------------------
    /// Run → Run... (F5): opens a command-line input box (submit = [`Cmd::RunExec`])
    RunCommand,
    /// Submit of the run dialog: expands `$(FULL_CURRENT_PATH)` etc., then hands off to the system shell
    RunExec,
    /// Run → the `i`-th recently run command (index matches `EditorState::run_recent`)
    RunAgain(usize),

    // ---- Marks (editor right-click → Style...) ------------------------------
    /// Apply style `style` (1..=5) to selection/word: `all` = all occurrences in document,
    /// `false` = only the one under the caret
    MarkStyle(u8, bool),
    /// Clear marks: `true` = all, `false` = the ones at selection/word
    MarkClear(bool),

    // ---- Selection / view (editor right-click) ------------------------------
    /// Edit → Begin/End Select (column mode): rectangular selection; copy/cut/replace proceed line by line by column
    BeginEndSelectColumn,
    /// Edit → Hide lines: hide the lines covered by the selection (current line if none)
    HideLines,
    /// Edit → Show all hidden lines
    ShowHiddenLines,
    /// Editor right-click → Search with browser: search the selection/word in the default browser
    WebSearch,

    // ---- Tools --------------------------------------------------------------
    /// MD5 digest of the selection / document (shown in an in-app dialog)
    Md5,
    /// SHA-1 digest
    Sha1,
    /// JSON formatting (serde pretty printer)
    FormatJson,
    /// XML formatting (tag indentation)
    FormatXml,
    /// Markdown document view: WYSIWYG edit toggle (crates/wysiwyg component,
    /// view directly replaces the text field; see `ui::components::mdws`)
    MdWys,
    /// Plain Text view button (special command): leave the document view and go back to plain text view
    /// — previously the "text view" under wys was bound to the preview toggle, so clicking it landed in preview.
    MdText,
    /// CSV/TSV table view (View menu): enable table mode + CSV toolbar
    CsvTable,
    /// CSV toolbar: table mode ↔ text mode
    CsvToggleMode,
    /// CSV toolbar/menu: delimiter (`,` `\t` `;` `|`)
    CsvDelim(u8),
    /// CSV toolbar/menu: first row is the header row
    CsvToggleHeader,
    /// CSV toolbar: sort by selected column (`true` = descending)
    CsvSort(bool),
    /// CSV table: insert blank row above/below the selected row (no selection: above = top, below = append at end)
    CsvRowAbove,
    CsvRowBelow,
    /// CSV table: delete the selected rows
    CsvRowDel,
    /// CSV table: insert blank column left/right of the selected column
    CsvColLeft,
    CsvColRight,
    /// CSV table: delete the selected columns
    CsvColDel,
    /// Markdown toolbar editing commands (bold/heading/list…)
    MdEdit(crate::ui::commands::markdown::MdAct),
    /// WYSIWYG toolbar commands (icon toolbar → wys editor). `Cmd` is
    /// `Copy`; `wysiwyg::Cmd` variants carrying a `String` (InsertText/Paste)
    /// cannot get in, so they go through this Copy subset; emoji/table grids
    /// are expanded on the mdws side. See
    /// `ui::components::mdws::toolbar_apply`.
    WysTool(WysTool),
    /// WYSIWYG toolbar: undo / redo (wys's own undo stack).
    WysUndo,
    WysRedo,
    /// Submit of the insert-link/image dialog: reads the URL from
    /// `dialog_input()` and expands it into `WysCmd::Link(url)` /
    /// `WysCmd::Image(url)` respectively (empty URL cancels).
    WysLinkSubmit,
    WysImageSubmit,
    /// WYSIWYG toolbar more▾: export HTML / export Markdown.
    MdExportHtml,
    MdExportMd,
    /// Tools → Word count (chars/words/lines/bytes)
    WordCount,
    /// Tools → Character code value (code point hex/decimal)
    CharCode,

    // ---- Caret --------------------------------------------------------------
    /// Settings → Caret → Blink
    ToggleCaretBlink,
    /// Settings → Caret → style (line/block/underline, cycles)
    CycleCaretStyle,

    // ---- Window -------------------------------------------------------------
    TabNext,
    TabPrev,
    TabClose,
    TabCloseAll,
    /// Window menu → Always on top: keep the window above every other one.
    ///
    /// Not a `Todo`: it is a real `HWND_TOPMOST` toggle (see
    /// [`winshell::win::frameless::set_topmost`]) and the title-bar right-click menu
    /// shows a check mark for it.
    AlwaysOnTop,
    // ---- Window system menu (title-bar right-click) -------------------------
    /// Restore / Maximize: the same command, whichever direction the state says.
    WindowToggleMaximize,
    WindowMinimize,
    /// Start the system move loop (`WM_NCLBUTTONDOWN` + `HTCAPTION`).
    WindowMove,
    /// Start the system resize loop (`WM_NCLBUTTONDOWN` + `HTBOTTOMRIGHT`).
    WindowResize,

    // ---- Help / Settings ----------------------------------------------------
    /// Settings → Preferences → Interface language. Applies immediately and is
    /// persisted. A session preference like the theme: read-only mode allows it,
    /// macro recording skips it.
    SetLocale(i18n::Locale),
    /// Settings → Preferences...: the settings panel dialog
    Preferences,
    /// Settings → Shortcuts...: the accelerator list dialog
    ShortcutHelp,
    /// Settings → Close: the preferences popup's own close button.
    ///
    /// Not a menu command — it only exists so the popup can report "the user
    /// closed it" through the same command channel as the real commands (see
    /// [`crate::ui::prefswin`]), and land in one place that hides the window.
    PreferencesClosed,

    About,
}

/// The `Copy` subset of the WYSIWYG toolbar commands.
///
/// [`Cmd`] derives `Copy`; `wysiwyg::Cmd` variants carrying a `String`
/// (InsertText / Paste) cannot get in — emoji and table grids are expanded into
/// the corresponding commands on the mdws side. Semantics match the same-named
/// `wysiwyg::Cmd` variants (see crates/wysiwyg `edit.rs`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum WysTool {
    Wrap(wysiwyg::WrapKind),
    /// Inline link: opens an input box to collect the URL (selection text is no longer used as the URL).
    Link,
    /// Insert image: URL input box → `![](url)` template.
    Image,
    SetHead(u8),
    ToggleList {
        ordered: bool,
    },
    ToggleQuote,
    ToggleTask,
    ToggleCodeBlock,
    InsertHr,
    InsertBefore,
    InsertAfter,
    InsertTable {
        rows: u8,
        cols: u8,
    },
    /// Table operations: row/column add/remove on the table containing the focused cell.
    Table(wysiwyg::TableCmd),
    /// Table column alignment (0=default 1=left 2=center 3=right; no kernel command, mdws edits directly).
    Align(u8),
    Indent,
    Outdent,
    /// Item `i` of the emoji panel (index into `mdws::EMOJIS`).
    Emoji(u8),
}

/// One entry of a menu: a command, a separator, or a submenu.
///
/// `label` is a `Cow` so the handful of entries whose text depends on the
/// current state (theme, highlight switches, the language list) can be built at
/// render time while everything else stays a borrowed `&'static str`.
pub enum Item<'a> {
    Cmd {
        label: Cow<'a, str>,
        accel: Cow<'a, str>,
        cmd: Cmd,
    },
    Sep,
    Sub {
        label: Cow<'a, str>,
        items: Vec<Item<'a>>,
    },
}

impl<'a> Item<'a> {
    pub fn cmd(label: impl Into<Cow<'a, str>>, accel: impl Into<Cow<'a, str>>, cmd: Cmd) -> Self {
        Item::Cmd {
            label: label.into(),
            accel: accel.into(),
            cmd,
        }
    }

    /// A command entry whose accelerator text comes from
    /// [`crate::ui::shortcuts::BINDINGS`] — the same table the keyboard reads.
    ///
    /// This is the only constructor the menu tree should use: it makes it
    /// impossible for a menu to advertise a shortcut the editor does not
    /// implement, which is exactly the bug that let 16 of 25 advertised
    /// accelerators do nothing.
    pub fn bind(label: impl Into<Cow<'a, str>>, cmd: Cmd) -> Self {
        Item::Cmd {
            label: label.into(),
            accel: Cow::Owned(crate::ui::shortcuts::accel_of(cmd)),
            cmd,
        }
    }

    pub fn sub(label: impl Into<Cow<'a, str>>, items: Vec<Item<'a>>) -> Self {
        Item::Sub {
            label: label.into(),
            items,
        }
    }

    pub fn sep() -> Self {
        Item::Sep
    }

    pub fn label(&self) -> &str {
        match self {
            Item::Cmd { label, .. } | Item::Sub { label, .. } => label.as_ref(),
            Item::Sep => "",
        }
    }

    /// Accelerator text of a command entry, `""` for separators and submenus.
    pub fn accel(&self) -> &str {
        match self {
            Item::Cmd { accel, .. } => accel.as_ref(),
            _ => "",
        }
    }

    /// The command an entry runs, if any.
    pub fn cmd_of(&self) -> Option<Cmd> {
        match self {
            Item::Cmd { cmd, .. } => Some(*cmd),
            _ => None,
        }
    }
}

/// The twelve top-level menu captions: **identity**, not text.
///
/// The twelve top-level menu captions: identity, not text.
///
/// Three "i-th item" conventions used to have to agree — `CAPTION_ACCELS[i]`,
/// the literal caption at index i of `menu_bar_items`, and
/// `MenuUi::open: Option<String>`. They could disagree with no compile error,
/// and the symptom was visible only on screen: a translated caption no longer
/// matched the open state, so the panel rendered empty.
///
/// Now there is one source: order = the index in [`Self::ALL`] (the bar renders
/// in that order, `inf-cap-{i}` uses the same i), and the mnemonic and the label
/// hang off the variant.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Caption {
    File,
    Edit,
    Search,
    View,
    Encoding,
    Language,
    Settings,
    Tools,
    Macro,
    Run,
    Window,
    Help,
}

impl Caption {
    /// Render order of the menu bar (also mnemonic order and `inf-cap-{i}`).
    pub const ALL: [Caption; 12] = [
        Caption::File,
        Caption::Edit,
        Caption::Search,
        Caption::View,
        Caption::Encoding,
        Caption::Language,
        Caption::Settings,
        Caption::Tools,
        Caption::Macro,
        Caption::Run,
        Caption::Window,
        Caption::Help,
    ];

    /// Position in the menu bar.
    pub fn index(self) -> usize {
        self as usize
    }

    /// Caption text in the caller's locale.
    pub fn msg(self) -> crate::ui::Msg {
        use crate::ui::Msg as M;
        match self {
            Caption::File => M::MenuFile,
            Caption::Edit => M::MenuEdit,
            Caption::Search => M::MenuSearch,
            Caption::View => M::MenuView,
            Caption::Encoding => M::MenuEncoding,
            Caption::Language => M::MenuLanguage,
            Caption::Settings => M::MenuSettings,
            Caption::Tools => M::MenuTools,
            Caption::Macro => M::MenuMacro,
            Caption::Run => M::MenuRun,
            Caption::Window => M::MenuWindow,
            Caption::Help => M::MenuHelp,
        }
    }

    /// <kbd>Alt</kbd> mnemonic, lower case.
    ///
    /// The keys Windows programs conventionally use for these twelve captions,
    /// so they are worth matching exactly. Independent of the label, therefore
    /// stable across languages.
    pub fn accel(self) -> char {
        match self {
            Caption::File => 'f',
            Caption::Edit => 'e',
            Caption::Search => 's',
            Caption::View => 'v',
            Caption::Encoding => 'm',
            Caption::Language => 'l',
            Caption::Settings => 't',
            Caption::Tools => 'o',
            Caption::Macro => 'r',
            Caption::Run => 'u',
            Caption::Window => 'w',
            Caption::Help => 'h',
        }
    }

    /// Looks a caption up by its mnemonic.
    pub fn by_accel(ch: char) -> Option<Caption> {
        let ch = ch.to_ascii_lowercase();
        Caption::ALL.into_iter().find(|c| c.accel() == ch)
    }
}

/// Mnemonic of the caption at index `i`, `None` when out of range.
///
/// The index shape survives because `inf-cap-{i}` is the shared coordinate of
/// the shell probe and the layout measurement; the meaning lives in
/// [`Caption::ALL`], this is only its projection.
pub fn caption_accel(index: usize) -> Option<char> {
    Caption::ALL.get(index).map(|c| c.accel())
}

// ---------------------------------------------------------------------------
// The open menu, flattened
// ---------------------------------------------------------------------------

/// One rendered line of an open dropdown.
///
/// Flattened out of the shared tree so the renderer, the keyboard cursor and the
/// hit test all agree on what "row 7" is.
#[derive(Clone)]
pub struct MenuRow {
    pub label: String,
    pub accel: String,
    /// `None` for separators and submenu captions
    pub cmd: Option<Cmd>,
    /// a submenu caption: activating it opens the fly-out instead of running
    pub header: bool,
    pub sep: bool,
    /// Whether this row **is** a checkable command (even if currently unchecked). The menu uses
    /// this to decide whether to reserve a check column on the left: when no row in the whole
    /// menu is checkable, labels align fully to the left.
    pub checkable: bool,
    /// toggle commands render a check mark
    pub checked: bool,
}

/// Whether `cmd` is a "checkable" command (a toggle / the selected item of a radio group).
///
/// This and [`is_checked`] are two halves of the same list: `is_checked` answers
/// "is it checked right now", this one answers "does it have a check box". The menu's
/// check-column layout only trusts this — otherwise a run of toggles that are all off
/// would make the check column vanish, and turning one on would pop the whole column
/// back in, making the labels jump left and right.
pub fn is_toggle(cmd: Cmd) -> bool {
    matches!(
        cmd,
        Cmd::BeginEndSelectColumn
            | Cmd::ToggleTheme
            | Cmd::ToggleHighlight
            | Cmd::HighlightAll
            | Cmd::WordWrap
            | Cmd::ShowEol
            | Cmd::ShowWs
            | Cmd::ToggleMinimap
            | Cmd::ToggleJsonl
            | Cmd::JsonPreview
            | Cmd::ToggleCaretBlink
            | Cmd::SetNewline(_)
            | Cmd::SetEncoding(_)
            | Cmd::Lang(_)
            | Cmd::AlwaysOnTop
            | Cmd::CsvTable
            | Cmd::CsvToggleMode
            | Cmd::CsvDelim(_)
            | Cmd::CsvToggleHeader
            | Cmd::MdWys
            | Cmd::ToggleSidebar
            | Cmd::ToggleOutline
            | Cmd::ToggleAutoClose
            | Cmd::ToggleVim
            | Cmd::ToggleReadOnly
    )
}

/// Current document's encoding id in the encoding menu (unique checkmark state).
///
/// Takes the first hit in the display order of [`crate::io::encoding::MENU_GROUPS`]:
/// "ANSI" is the system code page; on Chinese Windows it equals GBK, so both hit the same encoding
/// —letting the more specific encoding listed first win is what keeps the radio group to a single check.
///
/// Encodings absent from the menu (detection may report EUC-KR etc.) return `None`, i.e. nothing is checked:
/// checking a wrong one (say UTF-8) is more misleading than checking none.
fn current_encoding_id(st: &EditorState) -> Option<&'static str> {
    let doc = st.active_doc();
    // Untitled document (meta.encoding = None) is UTF-8 no BOM.
    let enc = doc.meta.encoding.unwrap_or(encoding_rs::UTF_8);
    let bom = doc.meta.had_bom;
    crate::io::encoding::menu_ids().find(|id| {
        crate::io::encoding::menu_choice(id).is_some_and(|(want, want_bom)| {
            // Only UTF-8 is split into "with BOM / without BOM" entries in the menu; UTF-16
            // has no separate BOM variant, so opening without a BOM still maps to that one entry.
            enc == want && (want != encoding_rs::UTF_8 || bom == want_bom)
        })
    })
}

/// Which toggles are currently on. Drives the check marks shown in
/// View / Encoding / Language.
pub fn is_checked(st: &EditorState, cmd: Cmd) -> bool {
    match cmd {
        Cmd::BeginEndSelectColumn => st.col_anchor.is_some(),
        Cmd::ToggleTheme => !st.dark,
        Cmd::ToggleHighlight => st.highlight_on,
        Cmd::HighlightAll => st.highlight_all,
        Cmd::WordWrap => st.wrap,
        Cmd::ShowEol => st.show_eol,
        Cmd::ShowWs => st.show_ws,
        Cmd::ToggleMinimap => st.minimap,
        Cmd::ToggleJsonl => st.tabs[st.active].jsonl.enabled,
        Cmd::JsonPreview => st.tabs[st.active].json.enabled,
        Cmd::ToggleCaretBlink => st.caret_blink,
        // What the menu must mark is the line ending that would "actually be written on save":
        // if the user never picked one it is the detected value; if they did, it is their choice
        // — `TextDocument::newline` is exactly the save-path criterion; honoring only
        // `user_newline` would leave all three entries unchecked right after opening a file.
        Cmd::SetNewline(nl) => st.active_doc().newline().display() == nl,
        Cmd::SetEncoding(id) => current_encoding_id(st) == Some(id),
        Cmd::Lang(name) => name != "__auto" && st.spec().name == name,
        Cmd::AlwaysOnTop => st.win.topmost,
        Cmd::CsvTable => st.csv_table_active(),
        Cmd::CsvToggleMode => st.tabs.get(st.active).is_some_and(|t| t.csv.table),
        Cmd::CsvDelim(d) => st.tabs.get(st.active).is_some_and(|t| t.csv.delim == d),
        Cmd::CsvToggleHeader => st.tabs.get(st.active).is_some_and(|t| t.csv.header),
        Cmd::MdWys => st.tabs.get(st.active).is_some_and(|t| t.md_wys),
        Cmd::ToggleSidebar => st.ws_show && st.ws_root.is_some() && !st.outline_panel_shown(),
        Cmd::ToggleAutoClose => st.auto_close,
        Cmd::ToggleOutline => st.outline_panel_shown(),
        Cmd::ToggleVim => st.vim.enabled,
        Cmd::ToggleReadOnly => st.read_only,
        _ => false,
    }
}

/// The title bar's right-click menu — the window's own "system" menu.
///
/// Windows puts Restore / Move / Size / Minimize / Maximize / Close on a right-click of
/// the caption; a frameless window has no caption, so the gesture has to be
/// re-created. This menu adds Always-on-top on top of the stock items: it is the
/// one window-level thing a text editor actually wants (a scratch file you keep
/// visible while working in another).
///
/// `Close` is deliberately **absent**: the editor has to ask about unsaved
/// changes first, and a native popup cannot. Leaving it out is what
/// [`Cmd::Quit`] (which asks) is for, and the ✕ button still closes the window.
///
/// `Maximize` becomes `Restore` while maximized, and both are greyed out in the state
/// where they do nothing, exactly as a real caption does.
pub fn window_system_items(st: &EditorState) -> Vec<Item<'static>> {
    let loc = st.locale();
    let zoomed = st.win.maximized;
    vec![
        Item::cmd(loc.tr(Msg::WinRestore), "", Cmd::WindowToggleMaximize),
        Item::cmd(loc.tr(Msg::WinMove), "", Cmd::WindowMove),
        Item::cmd(loc.tr(Msg::WinSize), "", Cmd::WindowResize),
        Item::sep(),
        Item::cmd(loc.tr(Msg::WinMinimize), "", Cmd::WindowMinimize),
        Item::cmd(
            if zoomed {
                loc.tr(Msg::WinMinimizeToTray)
            } else {
                loc.tr(Msg::WinMaximize)
            },
            "",
            Cmd::WindowToggleMaximize,
        ),
        Item::sep(),
        Item::cmd(
            if st.win.topmost {
                loc.tr(Msg::WinUnpin)
            } else {
                loc.tr(Msg::WinPin)
            },
            "",
            Cmd::AlwaysOnTop,
        ),
    ]
}

/// Open the caption whose access key is `ch` (from <kbd>Alt</kbd>+letter).
///
/// Returns whether a menu was opened. The anchor is the caption's left edge,
/// which the menu bar records when it renders — see
/// [`crate::ui::components::titlebar`] — so this does not have to measure text.
pub fn open_caption_by_accel(st: &mut EditorState, ch: char) -> bool {
    let Some(cap) = Caption::by_accel(ch) else {
        return false;
    };
    let anchor = crate::ui::components::titlebar::caption_x(st, cap);
    st.menu_open_from_alt(cap, anchor);
    true
}

/// Flatten the open caption's items into drawable rows.
///
/// Submenus are **not** expanded here: they fly out as a separate
/// panel one level deeper, and a nested panel needs its own stacking order and
/// its own hover state. Use [`submenu_rows`] for the fly-out's contents.
pub fn menu_rows(st: &EditorState) -> Vec<MenuRow> {
    let mut out: Vec<MenuRow> = Vec::new();
    let Some(open) = st.menu.open else {
        return out;
    };
    // The open state is a caption identity, so index the tree directly instead
    // of matching label strings.
    if let Some(Item::Sub { items, .. }) = menu_bar_items(st).into_iter().nth(open.index()) {
        flatten_rows(&items, st, &mut out);
    }
    out
}

/// One line of Settings → Shortcuts.
pub struct ShortcutRow {
    /// canonical key text, e.g. `Ctrl+Shift+S`
    pub accel: String,
    /// the menu entry it runs, e.g. `File / Save As...`
    pub label: String,
}

/// Every accelerator the editor implements, paired with the menu entry it
/// belongs to.
///
/// Built from the **binding table** (which owns the keys) joined with the menu
/// tree (which owns the wording), so the list cannot drift from either: a key
/// that nothing can reach, or a menu entry whose key does nothing, would be
/// visible here immediately.
pub fn shortcut_rows() -> Vec<ShortcutRow> {
    let st = EditorState::minimal();
    let mut found: Vec<(Cmd, String)> = Vec::new();
    for top in menu_bar_items(&st) {
        if let Item::Sub { label, items } = top {
            collect_shortcut_paths(&items, &label.clone(), &mut found);
        }
    }

    let mut rows: Vec<ShortcutRow> = crate::ui::shortcuts::BINDINGS
        .iter()
        .map(|b| {
            let label = found
                .iter()
                .find(|(cmd, _)| *cmd == b.cmd)
                .map(|(_, path)| path.clone())
                .unwrap_or_else(|| format!("{:?}", b.cmd));
            ShortcutRow {
                accel: b.chord.label(),
                label,
            }
        })
        .collect();
    rows.sort_by(|a, b| a.accel.cmp(&b.accel));
    rows
}

/// Walk a submenu collecting `label` for every command that advertises a key.
fn collect_shortcut_paths(items: &[Item<'static>], path: &str, out: &mut Vec<(Cmd, String)>) {
    for it in items {
        match it {
            Item::Cmd { label, accel, cmd } => {
                if !accel.is_empty() {
                    out.push((*cmd, format!("{path} / {label}")));
                }
            }
            Item::Sub { label, items } => {
                collect_shortcut_paths(items, &format!("{path} / {label}"), out)
            }
            Item::Sep => {}
        }
    }
}

/// The children of `caption`, if it is a submenu of the open caption.
///
/// `caption` is the fly-out's label **in the current language**. It comes from
/// the menu row in the same frame, so both sides of the comparison share one
/// source; switching languages closes the menu (`EditorState::set_locale`), so a
/// stale cross-language label cannot survive.
pub fn submenu_rows(st: &EditorState, top: Caption) -> Vec<MenuRow> {
    let mut out: Vec<MenuRow> = Vec::new();
    let Some(caption) = st.menu.sub.as_deref() else {
        return out;
    };
    if let Some(Item::Sub { items, .. }) = menu_bar_items(st).into_iter().nth(top.index()) {
        find_sub(&items, caption, st, &mut out);
    }
    out
}

fn find_sub(
    items: &[Item<'static>],
    caption: &str,
    st: &EditorState,
    out: &mut Vec<MenuRow>,
) -> bool {
    for it in items {
        match it {
            Item::Sub { label, items } if label == caption => {
                flatten_rows(items, st, out);
                return true;
            }
            Item::Sub { items, .. } if find_sub(items, caption, st, out) => {
                return true;
            }
            _ => {}
        }
    }
    false
}

/// Flatten one menu level's entries into drawable rows.
///
/// Flattens **one level only**: nested submenus are separate panels and are not
/// expanded here, so rows carry no "depth" (indentation is each panel's own
/// business, see `components::menu::row_indent`).
fn flatten_rows(items: &[Item<'static>], st: &EditorState, out: &mut Vec<MenuRow>) {
    for it in items {
        match it {
            Item::Sep => out.push(MenuRow {
                label: String::new(),
                accel: String::new(),
                cmd: None,
                header: false,
                sep: true,
                checkable: false,
                checked: false,
            }),
            Item::Cmd { label, accel, cmd } => {
                if label.is_empty() {
                    continue;
                }
                out.push(MenuRow {
                    label: label.to_string(),
                    accel: accel.to_string(),
                    cmd: Some(*cmd),
                    header: false,
                    sep: false,
                    checkable: is_toggle(*cmd),
                    checked: is_checked(st, *cmd),
                });
            }
            Item::Sub { label, .. } => out.push(MenuRow {
                label: label.to_string(),
                accel: String::new(),
                cmd: None,
                header: true,
                sep: false,
                checkable: false,
                checked: false,
            }),
        }
    }
}

// ---------------------------------------------------------------------------
// The editor menu tree
// ---------------------------------------------------------------------------

/// Edit → Line operations → Sort: six keys × ascending/descending, plus the two
/// non-key sorts.
fn sort_items(loc: Locale) -> Vec<Item<'static>> {
    // The suffix carries "ascending / descending" and its position differs per
    // language, so the twelve combinations are separate entries, not a format.
    const KEYS: &[(Msg, Msg, SortKey)] = &[
        (Msg::SortByTextAsc, Msg::SortByTextDesc, SortKey::Text),
        (
            Msg::SortByTextCiAsc,
            Msg::SortByTextCiDesc,
            SortKey::TextIgnoreCase,
        ),
        (Msg::SortByLengthAsc, Msg::SortByLengthDesc, SortKey::Length),
        (Msg::SortByIntAsc, Msg::SortByIntDesc, SortKey::Integer),
        (
            Msg::SortByDecDotAsc,
            Msg::SortByDecDotDesc,
            SortKey::DecimalDot,
        ),
        (
            Msg::SortByDecCommaAsc,
            Msg::SortByDecCommaDesc,
            SortKey::DecimalComma,
        ),
    ];
    let mut v = Vec::with_capacity(KEYS.len() * 2 + 4);
    for (asc, desc, key) in KEYS {
        v.push(Item::cmd(loc.tr(*asc), "", Cmd::SortLines(*key, false)));
        v.push(Item::cmd(loc.tr(*desc), "", Cmd::SortLines(*key, true)));
    }
    v.push(Item::sep());
    v.push(Item::cmd(loc.tr(Msg::SortReverse), "", Cmd::ReverseLines));
    v.push(Item::cmd(loc.tr(Msg::SortRandom), "", Cmd::RandomizeLines));
    v
}

/// Edit → Line operations.
fn line_op_items(loc: Locale) -> Vec<Item<'static>> {
    vec![
        Item::bind(loc.tr(Msg::LineDuplicate), Cmd::DuplicateLine),
        Item::sep(),
        Item::cmd(loc.tr(Msg::LineDeleteDup), "", Cmd::DeleteDupLine),
        Item::cmd(
            loc.tr(Msg::LineDeleteConsecutiveDup),
            "",
            Cmd::DeleteConsecutiveDupLine,
        ),
        Item::sep(),
        Item::cmd(loc.tr(Msg::LineSplit), "", Cmd::SplitLines),
        Item::bind(loc.tr(Msg::LineJoin), Cmd::JoinLines),
        Item::sep(),
        Item::bind(loc.tr(Msg::LineMoveUp), Cmd::MoveLineUp),
        Item::bind(loc.tr(Msg::LineMoveDown), Cmd::MoveLineDown),
        Item::sep(),
        Item::bind(loc.tr(Msg::LineBlankAbove), Cmd::InsertBlankAbove),
        Item::bind(loc.tr(Msg::LineBlankBelow), Cmd::InsertBlankBelow),
        Item::sep(),
        Item::sub(loc.tr(Msg::LineSort), sort_items(loc)),
    ]
}

/// Edit → Whitespace operations.
fn blank_op_items(loc: Locale) -> Vec<Item<'static>> {
    vec![
        Item::cmd(loc.tr(Msg::BlankTrimLeading), "", Cmd::TrimLeading),
        Item::cmd(loc.tr(Msg::BlankTrimTrailing), "", Cmd::TrimTrailing),
        Item::cmd(loc.tr(Msg::BlankTrimBoth), "", Cmd::TrimBoth),
        Item::sep(),
        Item::cmd(loc.tr(Msg::BlankEolToSpace), "", Cmd::EolToSpace),
        Item::sep(),
        Item::cmd(loc.tr(Msg::BlankTabToSpace), "", Cmd::TabToSpace),
        Item::cmd(loc.tr(Msg::BlankSpaceToTab), "", Cmd::SpaceToTab),
        Item::cmd(
            loc.tr(Msg::BlankSpaceToTabLeading),
            "",
            Cmd::SpaceToTabLeading,
        ),
        Item::sep(),
        Item::cmd(loc.tr(Msg::BlankRemoveEmpty), "", Cmd::RemoveEmptyLines),
        Item::cmd(
            loc.tr(Msg::BlankRemoveBlank),
            "",
            Cmd::RemoveEmptyLinesWithBlank,
        ),
    ]
}

/// Edit → Toggle line/block comments.
fn comment_items(loc: Locale) -> Vec<Item<'static>> {
    vec![
        Item::bind(loc.tr(Msg::CommentToggleLine), Cmd::ToggleLineComment),
        Item::sep(),
        Item::bind(loc.tr(Msg::CommentBlock), Cmd::BlockComment),
        Item::bind(loc.tr(Msg::CommentUnblock), Cmd::BlockUncomment),
    ]
}

/// Edit → Case conversion.
fn case_items(loc: Locale) -> Vec<Item<'static>> {
    vec![
        Item::bind(loc.tr(Msg::CaseUpper), Cmd::Case(CaseKind::Upper)),
        Item::bind(loc.tr(Msg::CaseLower), Cmd::Case(CaseKind::Lower)),
        Item::sep(),
        Item::cmd(loc.tr(Msg::CaseProper), "", Cmd::Case(CaseKind::Proper)),
        Item::cmd(loc.tr(Msg::CaseSentence), "", Cmd::Case(CaseKind::Sentence)),
        Item::cmd(loc.tr(Msg::CaseInvert), "", Cmd::Case(CaseKind::Invert)),
        Item::sep(),
        Item::cmd(
            loc.tr(Msg::CaseFullWidth),
            "",
            Cmd::Case(CaseKind::FullWidth),
        ),
        Item::cmd(
            loc.tr(Msg::CaseHalfWidth),
            "",
            Cmd::Case(CaseKind::HalfWidth),
        ),
    ]
}

/// Edit → Insert.
fn insert_items(loc: Locale) -> Vec<Item<'static>> {
    vec![
        Item::bind(loc.tr(Msg::InsertShortDateTime), Cmd::InsertShortDateTime),
        Item::cmd(loc.tr(Msg::InsertLongDateTime), "", Cmd::InsertLongDateTime),
        Item::sep(),
        Item::cmd(loc.tr(Msg::InsertNumbering), "", Cmd::InsertNumbering),
    ]
}

/// Encoding → Character encoding.
///
/// Encoding and newline names are **proper nouns** and stay out of the catalog:
/// they read the same in every language, and translating them would hide the
/// name the user is looking for.
///
/// Entries come from [`crate::io::encoding::MENU_GROUPS`] (the same table as the
/// check states, `set_encoding` and macro parsing); this only draws the separators.
fn encoding_items() -> Vec<Item<'static>> {
    let mut out = Vec::new();
    for (i, group) in crate::io::encoding::MENU_GROUPS.iter().enumerate() {
        if i > 0 {
            out.push(Item::sep());
        }
        out.extend(
            group
                .iter()
                .map(|(id, label)| Item::cmd(*label, "", Cmd::SetEncoding(id))),
        );
    }
    out
}

/// Encoding → end-of-line. Same rule as [`encoding_items`].
fn newline_items() -> Vec<Item<'static>> {
    vec![
        Item::cmd("Windows (CR LF)", "", Cmd::SetNewline("CRLF")),
        Item::cmd("Unix (LF)", "", Cmd::SetNewline("LF")),
        Item::cmd("Mac (CR)", "", Cmd::SetNewline("CR")),
    ]
}

/// The entries in a right-click on the **editor**.
///
/// Everything is expressed in terms of [`Cmd`], so the menu is *drawn* by a
/// native popup but its behaviour is the editor's own.
///
/// Shares catalog keys with the menu bar: one command, one wording.
pub fn editor_context_items(loc: Locale) -> Vec<Item<'static>> {
    vec![
        Item::bind(loc.tr(Msg::MenuCut), Cmd::Cut),
        Item::bind(loc.tr(Msg::MenuCopy), Cmd::Copy),
        Item::bind(loc.tr(Msg::MenuPaste), Cmd::Paste),
        Item::bind(loc.tr(Msg::MenuDelete), Cmd::Delete),
        Item::sep(),
        Item::bind(loc.tr(Msg::MenuSelectAll), Cmd::SelectAll),
        Item::sep(),
        Item::bind(loc.tr(Msg::CtxBeginEndSelect), Cmd::BeginEndSelect),
        Item::cmd(
            loc.tr(Msg::CtxBeginEndSelectColumn),
            "",
            Cmd::BeginEndSelectColumn,
        ),
        Item::sep(),
        Item::sub(loc.tr(Msg::CtxMarkAllStyles), {
            // A token is styled in one of five colours; one entry per
            // palette slot, and `all` marks every occurrence in the document.
            vec![
                Item::cmd(loc.tr(Msg::CtxMarkStyle1), "", Cmd::MarkStyle(1, true)),
                Item::cmd(loc.tr(Msg::CtxMarkStyle2), "", Cmd::MarkStyle(2, true)),
                Item::cmd(loc.tr(Msg::CtxMarkStyle3), "", Cmd::MarkStyle(3, true)),
                Item::cmd(loc.tr(Msg::CtxMarkStyle4), "", Cmd::MarkStyle(4, true)),
                Item::cmd(loc.tr(Msg::CtxMarkStyle5), "", Cmd::MarkStyle(5, true)),
            ]
        }),
        Item::sub(loc.tr(Msg::CtxMarkCurStyle), {
            vec![
                Item::cmd(loc.tr(Msg::CtxMarkStyle1), "", Cmd::MarkStyle(1, false)),
                Item::cmd(loc.tr(Msg::CtxMarkStyle2), "", Cmd::MarkStyle(2, false)),
                Item::cmd(loc.tr(Msg::CtxMarkStyle3), "", Cmd::MarkStyle(3, false)),
                Item::cmd(loc.tr(Msg::CtxMarkStyle4), "", Cmd::MarkStyle(4, false)),
                Item::cmd(loc.tr(Msg::CtxMarkStyle5), "", Cmd::MarkStyle(5, false)),
            ]
        }),
        Item::sub(loc.tr(Msg::CtxMarkClear), {
            vec![
                Item::cmd(loc.tr(Msg::CtxMarkClearCurrent), "", Cmd::MarkClear(false)),
                Item::cmd(loc.tr(Msg::CtxMarkClearAll), "", Cmd::MarkClear(true)),
            ]
        }),
        Item::sep(),
        Item::bind(loc.tr(Msg::CaseUpper), Cmd::Case(CaseKind::Upper)),
        Item::bind(loc.tr(Msg::CaseLower), Cmd::Case(CaseKind::Lower)),
        Item::sep(),
        Item::cmd(loc.tr(Msg::RevealInFolder), "", Cmd::OpenFolder),
        Item::cmd(loc.tr(Msg::CtxWebSearch), "", Cmd::WebSearch),
        Item::sep(),
        Item::bind(loc.tr(Msg::CommentToggleLine), Cmd::ToggleLineComment),
        Item::bind(loc.tr(Msg::CommentBlock), Cmd::BlockComment),
        Item::bind(loc.tr(Msg::CommentUnblock), Cmd::BlockUncomment),
        Item::sep(),
        Item::cmd(loc.tr(Msg::CtxHideLines), "", Cmd::HideLines),
        Item::cmd(loc.tr(Msg::CtxShowHidden), "", Cmd::ShowHiddenLines),
        // JSONL preview / Collapse all not in right-click menu: they are auto-enabled by file type
        // (.jsonl). The toolbar's 'preview view / collapse all' is the correct entry, same as the
        // view menu's. Same for CSV/JSON items.
    ]
}

/// The menu-bar tree, i.e. one submenu per top-level caption.
///
/// Returns `Item<'static>` (the only runtime-dependent labels are `Cow::Owned`)
/// so a caller may hold the tree across a `Signal::read()` guard.
///
/// Every label goes through `loc.tr(…)`; static ones borrow `&'static str`, so
/// rebuilding the whole tree per frame allocates nothing. Caption order is
/// [`Caption::ALL`] and `menu_rows` indexes by it.
pub fn menu_bar_items(st: &EditorState) -> Vec<Item<'static>> {
    // Order = [`Caption::ALL`]. `menu_rows` indexes the tree, so a mismatch
    // would not panic — it would open the wrong submenu under a caption.
    let items = vec![
        file_menu(st),
        edit_menu(st.locale()),
        search_menu(st.locale()),
        view_menu(st),
        encoding_menu(st.locale()),
        language_menu(st),
        settings_menu(st.locale()),
        tools_menu(st.locale()),
        macro_menu(st),
        run_menu(st),
        window_menu(st),
        help_menu(st.locale()),
    ];
    debug_assert_eq!(items.len(), Caption::ALL.len());
    items
}

/// Caption text of a top-level menu (one place, so a menu never gets two names).
fn caption_label(loc: Locale, c: Caption) -> Cow<'static, str> {
    loc.tr(c.msg())
}

/// File menu.
///
/// The recent-files submenu is not kept as a placeholder when the list is empty:
/// a forever-empty submenu only makes the menu look unfinished.
fn file_menu(st: &EditorState) -> Item<'static> {
    let loc = st.locale();
    let mut items: Vec<Item<'static>> = vec![
        Item::bind(loc.tr(Msg::MenuNew), Cmd::New),
        Item::bind(loc.tr(Msg::MenuOpen), Cmd::Open),
        Item::bind(loc.tr(Msg::MenuOpenFolder), Cmd::OpenWorkspace),
        Item::cmd(loc.tr(Msg::RevealInFolder), "", Cmd::OpenFolder),
        Item::bind(loc.tr(Msg::MenuReload), Cmd::Reload),
    ];
    if !st.recent.is_empty() {
        let recent: Vec<Item<'static>> = st
            .recent
            .iter()
            .rev()
            .enumerate()
            .map(|(i, p)| Item::cmd(p.clone(), "", Cmd::OpenRecent(st.recent.len() - 1 - i)))
            .collect();
        items.push(Item::sep());
        items.push(Item::sub(loc.tr(Msg::MenuRecentFiles), recent));
    }
    items.push(Item::sep());
    items.push(Item::bind(loc.tr(Msg::MenuSave), Cmd::Save));
    items.push(Item::bind(loc.tr(Msg::MenuSaveAs), Cmd::SaveAs));
    items.push(Item::cmd(loc.tr(Msg::MenuSaveAll), "", Cmd::SaveAll));
    items.push(Item::sep());
    items.push(Item::bind(loc.tr(Msg::MenuClose), Cmd::Close));
    items.push(Item::cmd(loc.tr(Msg::MenuCloseAll), "", Cmd::CloseAll));
    items.push(Item::bind(loc.tr(Msg::MenuQuit), Cmd::Quit));
    Item::sub(caption_label(loc, Caption::File), items)
}

/// Language menu: one entry per registered highlighter, current one bulleted.
fn language_menu(st: &EditorState) -> Item<'static> {
    let loc = st.locale();
    let current = st.spec().name;
    let mut items: Vec<Item<'static>> = vec![
        Item::cmd(loc.tr(Msg::LangAuto), "", Cmd::Lang("__auto")),
        Item::cmd(loc.tr(Msg::LangPlain), "", Cmd::Lang("Plain Text")),
        Item::sep(),
    ];
    for name in crate::highlight::names() {
        if name == "Plain Text" {
            continue;
        }
        let shown = crate::ui::lang_label(loc, name);
        let label = if name == current {
            format!("● {shown}")
        } else {
            shown.into_owned()
        };
        items.push(Item::cmd(label, "", Cmd::Lang(name)));
    }
    Item::sub(caption_label(loc, Caption::Language), items)
}

/// Edit menu. Everything here targets the text document, so the wys model
/// intercepts the clipboard/undo subset before dispatch (see [`wys_intercept`]).
fn edit_menu(loc: Locale) -> Item<'static> {
    let items = vec![
        Item::bind(loc.tr(Msg::MenuUndo), Cmd::Undo),
        Item::bind(loc.tr(Msg::MenuRedo), Cmd::Redo),
        Item::sep(),
        Item::bind(loc.tr(Msg::MenuCut), Cmd::Cut),
        Item::bind(loc.tr(Msg::MenuCopy), Cmd::Copy),
        Item::bind(loc.tr(Msg::MenuPaste), Cmd::Paste),
        Item::bind(loc.tr(Msg::MenuDelete), Cmd::Delete),
        Item::sep(),
        Item::bind(loc.tr(Msg::MenuSelectAll), Cmd::SelectAll),
        Item::bind(loc.tr(Msg::MenuGotoBracket), Cmd::GotoBracket),
        Item::sub(
            loc.tr(Msg::MenuBookmarks),
            vec![
                Item::bind(loc.tr(Msg::MenuBookmarkToggle), Cmd::BookmarkToggle),
                Item::bind(loc.tr(Msg::MenuBookmarkNext), Cmd::BookmarkNext),
                Item::bind(loc.tr(Msg::MenuBookmarkPrev), Cmd::BookmarkPrev),
            ],
        ),
        Item::cmd(loc.tr(Msg::MenuCopyAll), "", Cmd::CopyAll),
        Item::cmd(loc.tr(Msg::MenuCopyFileName), "", Cmd::CopyFileName),
        Item::cmd(loc.tr(Msg::MenuCopyFilePath), "", Cmd::CopyFilePath),
        Item::sep(),
        Item::sub(loc.tr(Msg::MenuCase), case_items(loc)),
        Item::sub(loc.tr(Msg::MenuLineOps), line_op_items(loc)),
        Item::sub(loc.tr(Msg::MenuBlankOps), blank_op_items(loc)),
        Item::sub(loc.tr(Msg::MenuCommentOps), comment_items(loc)),
        Item::sub(loc.tr(Msg::MenuInsert), insert_items(loc)),
    ];
    Item::sub(caption_label(loc, Caption::Edit), items)
}

/// Search menu, plus the encoding-independent "go to line" entry.
fn search_menu(loc: Locale) -> Item<'static> {
    let items = vec![
        Item::bind(loc.tr(Msg::MenuFind), Cmd::Find(false, false)),
        Item::bind(loc.tr(Msg::MenuReplace), Cmd::Find(true, false)),
        Item::bind(loc.tr(Msg::MenuFindInFiles), Cmd::Find(false, true)),
        Item::sep(),
        Item::bind(loc.tr(Msg::MenuFindNext), Cmd::FindNext),
        Item::bind(loc.tr(Msg::MenuFindPrev), Cmd::FindPrev),
        Item::sep(),
        Item::bind(loc.tr(Msg::MenuGotoLine), Cmd::Goto),
    ];
    Item::sub(caption_label(loc, Caption::Search), items)
}

/// View menu. The toggles read as the *action* they will perform ("to dark
/// theme"), not as the current state — the check mark carries the state.
fn view_menu(st: &EditorState) -> Item<'static> {
    let loc = st.locale();
    let theme = if st.dark {
        loc.tr(Msg::ViewToLightTheme)
    } else {
        loc.tr(Msg::ViewToDarkTheme)
    };
    let highlight = if st.highlight_on {
        loc.tr(Msg::ViewHighlightOff)
    } else {
        loc.tr(Msg::ViewHighlightOn)
    };
    let highlight_all = if st.highlight_all {
        loc.tr(Msg::ViewHighlightAllOff)
    } else {
        loc.tr(Msg::ViewHighlightAllOn)
    };
    let vim = if st.vim.enabled {
        loc.tr(Msg::ViewVimOff)
    } else {
        loc.tr(Msg::ViewVimOn)
    };
    let read_only = if st.read_only {
        loc.tr(Msg::ViewReadOnlyOn)
    } else {
        loc.tr(Msg::ViewReadOnly)
    };

    let items = vec![
        Item::cmd(theme, "", Cmd::ToggleTheme),
        Item::bind(loc.tr(Msg::ViewZoomIn), Cmd::ZoomIn),
        Item::bind(loc.tr(Msg::ViewZoomOut), Cmd::ZoomOut),
        Item::bind(loc.tr(Msg::ViewZoomReset), Cmd::ZoomReset),
        Item::sep(),
        Item::cmd(highlight, "", Cmd::ToggleHighlight),
        Item::cmd(highlight_all, "", Cmd::HighlightAll),
        Item::sep(),
        Item::cmd(loc.tr(Msg::ViewWordWrap), "", Cmd::WordWrap),
        Item::cmd(loc.tr(Msg::ViewShowEol), "", Cmd::ShowEol),
        Item::cmd(loc.tr(Msg::ViewShowWs), "", Cmd::ShowWs),
        Item::sep(),
        Item::cmd(vim, "", Cmd::ToggleVim),
        Item::sep(),
        Item::cmd(loc.tr(Msg::ViewMinimap), "", Cmd::ToggleMinimap),
        Item::bind(loc.tr(Msg::ViewSidebar), Cmd::ToggleSidebar),
        // Document outline (sidebar panel): Markdown heading-level tree;
        // clicking jumps to the matching source line/block (see state::outline).
        Item::cmd(loc.tr(Msg::ViewOutline), "", Cmd::ToggleOutline),
        // CSV / JSONL / JSON view toggles don't enter the menu: they are auto-enabled by file
        // type (opening .csv/.json/.jsonl switches the view + toolbar automatically); manual
        // menu triggering for other file types would only create broken states. Entries:
        // the toolbar (toolbar.rs, appears per document type) + shortcuts
        // (Ctrl+Alt+[ / ], shortcuts.rs).
        Item::cmd(read_only, "", Cmd::ToggleReadOnly),
    ];
    Item::sub(caption_label(loc, Caption::View), items)
}

/// Encoding menu: charset, newline, and the two Unicode normalisation forms.
fn encoding_menu(loc: Locale) -> Item<'static> {
    let items = vec![
        Item::sub(loc.tr(Msg::EncodingCharset), encoding_items()),
        Item::sub(loc.tr(Msg::EncodingNewline), newline_items()),
        Item::sep(),
        Item::sub(
            loc.tr(Msg::EncodingUnicode),
            vec![
                Item::cmd(loc.tr(Msg::EncodingNfc), "", Cmd::UnicodeNfc),
                Item::cmd(loc.tr(Msg::EncodingNfd), "", Cmd::UnicodeNfd),
            ],
        ),
    ];
    Item::sub(caption_label(loc, Caption::Encoding), items)
}

/// Settings menu: caret, auto-close, and the two dialog entries.
fn settings_menu(loc: Locale) -> Item<'static> {
    let items = vec![
        Item::sub(
            loc.tr(Msg::SettingsCursor),
            vec![
                Item::bind(loc.tr(Msg::SettingsCaretBlink), Cmd::ToggleCaretBlink),
                Item::bind(loc.tr(Msg::SettingsCaretStyle), Cmd::CycleCaretStyle),
            ],
        ),
        Item::cmd(loc.tr(Msg::SettingsAutoClose), "", Cmd::ToggleAutoClose),
        Item::cmd(loc.tr(Msg::SettingsPreferences), "", Cmd::Preferences),
        Item::cmd(loc.tr(Msg::SettingsShortcuts), "", Cmd::ShortcutHelp),
    ];
    Item::sub(caption_label(loc, Caption::Settings), items)
}

/// Tools menu.
fn tools_menu(loc: Locale) -> Item<'static> {
    let items = vec![
        Item::cmd(loc.tr(Msg::ToolsMd5), "", Cmd::Md5),
        Item::cmd(loc.tr(Msg::ToolsSha1), "", Cmd::Sha1),
        Item::sep(),
        Item::cmd(loc.tr(Msg::ToolsFormatJson), "", Cmd::FormatJson),
        Item::cmd(loc.tr(Msg::ToolsFormatXml), "", Cmd::FormatXml),
        // Markdown preview not in the menu: auto-enabled per file type (opening .md makes the
        // toolbar show the 'preview view / text view' toggle, same for CSV/JSONL views).
        Item::sep(),
        Item::cmd(loc.tr(Msg::ToolsWordCount), "", Cmd::WordCount),
        Item::cmd(loc.tr(Msg::ToolsCharCode), "", Cmd::CharCode),
    ];
    Item::sub(caption_label(loc, Caption::Tools), items)
}

/// Macro menu: the record entry doubles as the stop entry while recording.
fn macro_menu(st: &EditorState) -> Item<'static> {
    let loc = st.locale();
    let record = if st.is_recording() {
        loc.tr(Msg::MacroRecordStop)
    } else {
        loc.tr(Msg::MacroRecordStart)
    };
    let items = vec![
        Item::cmd(record, "", Cmd::MacroRecord),
        Item::cmd(loc.tr(Msg::MacroPlayback), "", Cmd::MacroPlayback),
        Item::sep(),
        Item::cmd(loc.tr(Msg::MacroSave), "", Cmd::MacroSave),
        Item::cmd(loc.tr(Msg::MacroRun), "", Cmd::MacroRun),
        Item::cmd(loc.tr(Msg::MacroOpenDir), "", Cmd::MacroDir),
    ];
    Item::sub(caption_label(loc, Caption::Macro), items)
}

/// Run menu: F5 opens the command-line box; commands run this session are
/// listed below for one-click reruns (variables like `$(FULL_CURRENT_PATH)` are
/// expanded on submit, see macros_io).
fn run_menu(st: &EditorState) -> Item<'static> {
    let loc = st.locale();
    let mut items = vec![Item::bind(loc.tr(Msg::RunCommand), Cmd::RunCommand)];
    for (i, c) in st.run_recent.iter().take(6).enumerate() {
        items.push(Item::cmd(c.clone(), "", Cmd::RunAgain(i)));
    }
    Item::sub(caption_label(loc, Caption::Run), items)
}

/// Window menu.
fn window_menu(st: &EditorState) -> Item<'static> {
    let loc = st.locale();
    // Also on the title bar's right-click menu; listed here too so it is
    // discoverable without knowing the gesture.
    let pin = if st.win.topmost {
        loc.tr(Msg::WindowUnpin)
    } else {
        loc.tr(Msg::WindowPin)
    };
    let items = vec![
        Item::bind(loc.tr(Msg::WindowNextTab), Cmd::TabNext),
        Item::bind(loc.tr(Msg::WindowPrevTab), Cmd::TabPrev),
        Item::sep(),
        Item::bind(loc.tr(Msg::WindowCloseTab), Cmd::TabClose),
        Item::cmd(loc.tr(Msg::WindowCloseAllTabs), "", Cmd::TabCloseAll),
        Item::sep(),
        Item::cmd(pin, "", Cmd::AlwaysOnTop),
    ];
    Item::sub(caption_label(loc, Caption::Window), items)
}

/// Help menu.
fn help_menu(loc: Locale) -> Item<'static> {
    let items = vec![Item::cmd(loc.tr(Msg::DlgAboutTitle), "", Cmd::About)];
    Item::sub(caption_label(loc, Caption::Help), items)
}

// ---------------------------------------------------------------------------
// Dispatch
// ---------------------------------------------------------------------------

/// Execute a command. This is the single entry point shared by the menu bar,
/// the native editor context menu and the keyboard.
///
/// This is also the **macro recording hook**: a recordable command is captured
/// here (see [`crate::ui::recorder`]) after it has run, so anything reachable
/// from the menus is automatically macro-able. The hook is a no-op unless a
/// recording is in progress.
pub fn run(st: &mut EditorState, cmd: Cmd) {
    if intercept(st, cmd) {
        return;
    }
    dispatch(st, cmd);
    st.menu.open = None;
    // Settings commands changed user preferences; persist to disk immediately (the file is tiny,
    // microsecond-level).
    if is_settings_cmd(cmd) {
        st.persist_settings();
    }
    // Macro recording hook: the command has taken effect, now record it (see the recorder module
    // docs). Placed after menu_close so macros never embed menu-state toggles.
    st.record_cmd(cmd);
}

/// Commands that must never reach [`dispatch`]: blocked by the current mode, or
/// owned by another model.
///
/// Returns `true` when `cmd` is finished with (either refused with a status
/// message, or handled against the wys model / preview selection). Everything
/// that returns `false` is dispatched against the text document.
fn intercept(st: &mut EditorState, cmd: Cmd) -> bool {
    // Read-only mode: editing commands are blocked here (typing/backspace have their own guard at
    // the `commands.rs` entry; the two layers together cover menu, shortcuts, right-click, Vim and
    // IME paths).
    if st.read_only && cmd_edits_doc(cmd) {
        st.status = Some(crate::ui::commands::READONLY_MSG.into());
        return true;
    }
    if st.tabs.get(st.active).is_some_and(|t| t.md_wys) && wys_intercept(st, cmd) {
        return true;
    }
    // The macro commands themselves must not be captured (a recorded
    // "stop recording" would cut every future replay short), so they are
    // handled here and fall through to the hook only as ordinary,
    // filtered commands.
    match cmd {
        Cmd::MacroRecord => st.status = st.macro_toggle_record(),
        Cmd::MacroPlayback => st.status = Some(st.macro_play()),
        _ => return false,
    }
    true
}

/// wys mode write-access choke point.
///
/// In wys view the true owner of the content is the wys model; any command that edits the piece
/// table directly is **silently rolled back** by the next sync and leaves junk steps on the text
/// undo stack. Consolidate here: clipboard/delete/select-all/undo/redo are **rerouted to the wys
/// model** (same semantics as the shortcut path); the remaining editing commands get a status
/// bar hint that they are unavailable; save/find/export etc. pass through as usual via the
/// `cmd_edits_doc` allow list.
fn wys_intercept(st: &mut EditorState, cmd: Cmd) -> bool {
    use crate::ui::components::mdws;
    // Flush debounced changes first: find/copy-all/word-count read the piece table.
    if !matches!(
        cmd,
        Cmd::WysTool(_) | Cmd::WysUndo | Cmd::WysRedo | Cmd::WysLinkSubmit | Cmd::WysImageSubmit
    ) {
        mdws::flush_if_pending(st);
    }
    match cmd {
        Cmd::Copy => mdws::wys_copy(st),
        Cmd::Cut => mdws::wys_cut(st),
        Cmd::Paste => mdws::wys_paste(st),
        Cmd::Delete => mdws::wys_delete_sel(st),
        Cmd::SelectAll => mdws::wys_select_all(st),
        Cmd::Undo => mdws::undo(st),
        Cmd::Redo => mdws::redo(st),
        Cmd::WysTool(_)
        | Cmd::WysUndo
        | Cmd::WysRedo
        | Cmd::WysLinkSubmit
        | Cmd::WysImageSubmit => {
            return false;
        }
        _ if cmd_edits_doc(cmd) => {
            st.status = Some(Msg::StatusWysUnavailable.into());
        }
        _ => return false,
    }
    true
}

/// Echo the new state of an on/off setting to the status bar.
///
/// Every toggle reports "now on" / "now off" with its own pair of messages; the
/// flag itself is flipped by the caller so it stays a plain assignment there.
fn report_flag(st: &mut EditorState, on: bool, on_msg: Msg, off_msg: Msg) {
    st.status = Some(if on { on_msg } else { off_msg }.into());
}

/// The dispatch table.
///
/// Deliberately one exhaustive `match`: a new [`Cmd`] variant must fail to
/// compile until it is wired up, which a per-domain split with `_ => {}`
/// fall-throughs would silently lose.
fn dispatch(st: &mut EditorState, cmd: Cmd) {
    match cmd {
        // ---- File ---------------------------------------------------------
        Cmd::New => st.new_doc(),
        Cmd::Open => st.open_dialog(),
        Cmd::OpenWorkspace => st.ws_open_folder_dialog(),
        Cmd::OpenFolder => st.reveal_in_explorer(),
        Cmd::OpenRecent(i) => st.open_recent(i),
        Cmd::Reload => st.reload_active(),
        Cmd::Save => st.save_active(),
        Cmd::SaveAs => st.save_active_as(),
        Cmd::SaveAll => {
            for i in 0..st.tabs.len() {
                if st.tabs[i].doc.is_dirty() {
                    st.save_doc(i);
                }
            }
        }
        Cmd::Close => {
            let i = st.active;
            st.close_doc(i);
        }
        Cmd::CloseAll => st.close_all_sweep(),
        Cmd::Quit => st.request_quit(),
        Cmd::ForceQuit => st.quit_requested = true,
        Cmd::CloseSave => st.finish_close(Some(true)),
        Cmd::CloseDiscard => st.finish_close(Some(false)),
        Cmd::CloseCancel => st.finish_close(None),
        Cmd::ReloadConfirm => st.finish_reload(true),
        Cmd::ReloadCancel => st.finish_reload(false),
        Cmd::RecoveryRestore => st.restore_recovery(),
        Cmd::RecoveryDiscard => st.discard_recovery(),
        Cmd::RecoveryDiscardConfirm => st.discard_recovery_confirmed(),
        Cmd::RecoveryDiscardCancel => st.discard_recovery_cancel(),

        // ---- Edit ---------------------------------------------------------
        Cmd::Undo => st.undo(),
        Cmd::Redo => st.redo(),
        Cmd::Cut => st.cut(),
        Cmd::Copy => st.copy(),
        Cmd::Paste => st.paste(),
        Cmd::Delete => st.delete_fwd(),
        Cmd::SelectAll => st.select_all(),
        Cmd::CopyAll => st.copy_all(),
        Cmd::CopyFileName => st.copy_file_name(),
        Cmd::CopyFilePath => st.copy_file_path(),
        Cmd::BeginEndSelect => st.begin_end_select(),
        Cmd::GotoBracket => st.goto_matching_bracket(),
        Cmd::ToggleReadOnly => {
            st.read_only = !st.read_only;
            report_flag(
                st,
                st.read_only,
                Msg::StatusReadOnlyOn,
                Msg::StatusReadOnlyOff,
            );
        }
        Cmd::ToggleSidebar => st.toggle_sidebar(),
        Cmd::ToggleOutline => st.toggle_outline(),
        Cmd::BookmarkToggle => st.toggle_bookmark(),
        Cmd::BookmarkNext => st.goto_bookmark(1),
        Cmd::BookmarkPrev => st.goto_bookmark(-1),
        Cmd::ToggleAutoClose => {
            st.auto_close = !st.auto_close;
            report_flag(
                st,
                st.auto_close,
                Msg::StatusAutoCloseOn,
                Msg::StatusAutoCloseOff,
            );
        }

        // ---- Case ---------------------------------------------------------
        Cmd::Case(k) => st.convert_case(k),

        // ---- Line operations ----------------------------------------------
        Cmd::DuplicateLine => st.duplicate_lines(),
        Cmd::DeleteDupLine => st.remove_duplicate_lines(false),
        Cmd::DeleteConsecutiveDupLine => st.remove_duplicate_lines(true),
        Cmd::SplitLines => st.split_lines(),
        Cmd::JoinLines => st.join_lines(),
        Cmd::MoveLineUp => st.move_lines(-1),
        Cmd::MoveLineDown => st.move_lines(1),
        Cmd::InsertBlankAbove => st.insert_blank_line(true),
        Cmd::InsertBlankBelow => st.insert_blank_line(false),
        Cmd::ReverseLines => st.reverse_lines(),
        Cmd::RandomizeLines => st.randomize_lines(),
        Cmd::SortLines(key, desc) => st.sort_lines(key, desc),

        // ---- Whitespace operations ----------------------------------------
        Cmd::TrimBoth => st.trim_lines(true, true),
        Cmd::TrimLeading => st.trim_lines(true, false),
        Cmd::TrimTrailing => st.trim_lines(false, true),
        Cmd::EolToSpace => st.eol_to_space(),
        Cmd::TabToSpace => st.tabs_to_spaces(),
        Cmd::SpaceToTab => st.spaces_to_tabs(false),
        Cmd::SpaceToTabLeading => st.spaces_to_tabs(true),
        Cmd::RemoveEmptyLines => st.remove_empty_lines(false),
        Cmd::RemoveEmptyLinesWithBlank => st.remove_empty_lines(true),

        // ---- Comments ------------------------------------------------------
        Cmd::ToggleLineComment => st.toggle_line_comment(),
        Cmd::BlockComment => st.block_comment(),
        Cmd::BlockUncomment => st.block_uncomment(),

        // ---- Insert --------------------------------------------------------
        Cmd::InsertShortDateTime => st.insert_datetime(false),
        Cmd::InsertLongDateTime => st.insert_datetime(true),
        Cmd::InsertNumbering => {
            // On platforms without an owned popup window (open_numbering_window returns false), hint in the status bar.
            if !st.open_numbering_window() {
                st.status = Some(Msg::StatusNumberingNoWindow.into());
            }
        }

        // ---- Encoding helpers ----------------------------------------------
        Cmd::UnicodeNfc => st.unicode_normalize(false),
        Cmd::UnicodeNfd => st.unicode_normalize(true),

        // ---- Find ----------------------------------------------------------
        Cmd::Find(replace, panel) => st.open_find(replace, panel),
        Cmd::Goto => st.open_goto(),
        Cmd::FindNext => st.find_next(),
        Cmd::FindPrev => st.find_prev(),

        // ---- View ----------------------------------------------------------
        Cmd::ZoomIn => st.zoom(1.0),
        Cmd::ZoomOut => st.zoom(-1.0),
        Cmd::ZoomReset => st.reset_zoom(),
        Cmd::ToggleTheme => st.toggle_theme(),
        Cmd::ToggleHighlight => st.toggle_highlight(),
        Cmd::HighlightAll => {
            st.highlight_all = !st.highlight_all;
            report_flag(
                st,
                st.highlight_all,
                Msg::StatusHighlightAllOn,
                Msg::StatusHighlightAllOff,
            );
        }
        Cmd::ShowEol => {
            st.show_eol = !st.show_eol;
            report_flag(st, st.show_eol, Msg::StatusShowEolOn, Msg::StatusShowEolOff);
        }
        Cmd::ShowWs => {
            st.show_ws = !st.show_ws;
            report_flag(st, st.show_ws, Msg::StatusShowWsOn, Msg::StatusShowWsOff);
        }
        Cmd::WordWrap => {
            st.wrap = !st.wrap;
            report_flag(st, st.wrap, Msg::StatusWrapOn, Msg::StatusWrapOff);
            // With wrap on there is no horizontal overflow, so the old horizontal scroll amount
            // must be zeroed; after turning it off, re-clamp against the real content width.
            st.clamp_scroll();
        }
        Cmd::ToggleMinimap => st.toggle_minimap(),
        Cmd::ToggleJsonl => st.toggle_jsonl(),
        Cmd::JsonlCollapseAll => st.jsonl_collapse_all(),
        Cmd::JsonPreview => st.json_toggle_preview(),
        Cmd::JsonCollapseAll => st.json_collapse_all(),
        Cmd::JsonExpandAll => st.json_expand_all(),
        Cmd::JsonlFoldLine => st.jsonl_fold_caret_line(),
        Cmd::JsonlUnfoldLine => st.jsonl_unfold_caret_line(),

        // ---- Encoding ------------------------------------------------------
        Cmd::SetEncoding(name) => st.set_encoding(name),
        Cmd::SetNewline(name) => st.set_newline(name),

        // ---- Language ------------------------------------------------------
        Cmd::Lang(name) => {
            if name == "__auto" {
                st.redetect_language();
            } else {
                st.set_language_by_name(name);
            }
        }

        // ---- Input mode ----------------------------------------------------
        Cmd::ToggleVim => st.toggle_vim(),

        // ---- Tools ---------------------------------------------------------
        Cmd::Md5 => st.md5_digest(),
        Cmd::Sha1 => st.sha1_digest(),
        Cmd::FormatJson => st.format_json(),
        Cmd::FormatXml => st.format_xml(),
        Cmd::MdWys => st.md_toggle_wys(),
        Cmd::MdText => st.md_show_text(),
        Cmd::CsvTable => st.csv_toggle_table(),
        Cmd::CsvToggleMode => st.csv_toggle_mode(),
        Cmd::CsvDelim(d) => st.csv_set_delim(d),
        Cmd::CsvToggleHeader => st.csv_toggle_header(),
        Cmd::CsvRowAbove => st.csv_insert_row(true),
        Cmd::CsvRowBelow => st.csv_insert_row(false),
        Cmd::CsvRowDel => st.csv_delete_row(),
        Cmd::CsvColLeft => st.csv_insert_col(true),
        Cmd::CsvColRight => st.csv_insert_col(false),
        Cmd::CsvColDel => st.csv_delete_col(),
        // View lens, not an edit: a refused sort (empty / truncated table) only
        // sets a status message
        Cmd::CsvSort(desc) => {
            st.csv_sort(desc);
        }
        Cmd::MdEdit(act) => st.md_apply(act),
        Cmd::WysTool(t) => crate::ui::components::mdws::toolbar_apply(st, t),
        Cmd::WysUndo => crate::ui::components::mdws::toolbar_undo(st),
        Cmd::WysRedo => crate::ui::components::mdws::toolbar_redo(st),
        Cmd::WysLinkSubmit => {
            let url = st.dialog_input();
            crate::ui::components::mdws::apply_link(st, &url);
        }
        Cmd::WysImageSubmit => {
            let url = st.dialog_input();
            crate::ui::components::mdws::apply_image(st, &url);
        }
        Cmd::MdExportHtml => crate::ui::components::mdws::export_html(st),
        Cmd::MdExportMd => crate::ui::components::mdws::export_md(st),
        Cmd::WordCount => st.word_count(),
        Cmd::CharCode => st.char_code_value(),

        // ---- Caret ---------------------------------------------------------
        Cmd::ToggleCaretBlink => {
            st.caret_blink = !st.caret_blink;
            report_flag(
                st,
                st.caret_blink,
                Msg::StatusCaretBlinkOn,
                Msg::StatusCaretBlinkOff,
            );
        }
        Cmd::CycleCaretStyle => {
            st.caret_style = match st.caret_style {
                CaretStyle::Line => CaretStyle::Block,
                CaretStyle::Block => CaretStyle::Underline,
                CaretStyle::Underline => CaretStyle::Line,
            };
            st.status = Some(
                Fmt::StatusCaretStyle {
                    style: st.tr(st.caret_style.msg()).into_owned(),
                }
                .into(),
            );
        }

        // ---- Window --------------------------------------------------------
        Cmd::TabNext => st.activate_next_tab(1),
        Cmd::TabPrev => st.activate_next_tab(-1),
        Cmd::TabClose => {
            let i = st.active;
            st.close_doc(i);
        }
        Cmd::TabCloseAll => st.close_all_sweep(),
        // The window-system commands go straight to Win32: these are OS gestures
        // (the move/size modal loops), not editor state, so routing them through
        // `EditorState` would mean borrowing it across a modal loop. See
        // `winshell::win::frameless`.
        Cmd::AlwaysOnTop => {
            let next = !st.win.topmost;
            if winshell::win::frameless::set_topmost(next) {
                st.win.topmost = next;
                report_flag(st, next, Msg::StatusTopmostOn, Msg::StatusTopmostOff);
            } else {
                st.status = Some(Msg::StatusTopmostFailed.into());
            }
        }
        Cmd::WindowToggleMaximize => st.toggle_window_maximized(),
        Cmd::WindowMinimize => winshell::win::frameless::minimize(),
        Cmd::WindowMove => winshell::win::frameless::start_system_move(),
        Cmd::WindowResize => winshell::win::frameless::start_system_resize(),

        Cmd::Preferences => st.open_preferences(),
        Cmd::PreferencesClosed => st.close_prefs_window(),
        Cmd::SetLocale(locale) => st.set_locale(locale),
        Cmd::ShortcutHelp => st.open_shortcut_help(),
        Cmd::About => st.about(),

        // ---- Macros (disk) / Run (see ui::macros_io) ----------------------
        Cmd::MacroSave => st.macro_save_open(),
        Cmd::MacroSaveNamed => st.macro_save_named(),
        Cmd::MacroRun => st.macro_run_open(),
        Cmd::MacroPlaySaved(i) => st.macro_play_saved(i),
        Cmd::MacroDir => st.macro_dir_reveal(),
        Cmd::RunCommand => st.run_command_open(),
        Cmd::RunExec => st.run_exec(),
        Cmd::RunAgain(i) => st.run_again(i),

        // ---- Marks / column select / hide lines / web search --------------
        Cmd::MarkStyle(style, all) => st.mark_style(style, all),
        Cmd::MarkClear(all) => st.mark_clear(all),
        Cmd::BeginEndSelectColumn => st.begin_end_select_column(),
        Cmd::HideLines => st.hide_lines(),
        Cmd::ShowHiddenLines => st.show_hidden_lines(),
        Cmd::WebSearch => st.web_search(),
        // Already handled at the top of this function (recording/playback must not be recorded
        // themselves); this arm exists only for exhaustiveness.
        Cmd::MacroRecord | Cmd::MacroPlayback => {}
    }
}

/// Whether this command would modify the document in read-only mode.
///
/// Deriving it from a deny list is too easy to get wrong (new commands would be
/// allowed by default), so it lists the **allowed** directions:
/// query/view/navigation/search/window commands. Any new command is blocked by
/// read-only mode by default and must be added here explicitly to be allowed —
/// for the "prevent accidental edits" goal, over-blocking is preferable.
fn cmd_edits_doc(cmd: Cmd) -> bool {
    !matches!(
        cmd,
        Cmd::New
            | Cmd::Open
            | Cmd::OpenWorkspace
            | Cmd::OpenRecent(_)
            | Cmd::OpenFolder
            // Reload is a natural operation in read-only mode (reading large files without
            // accidental edits, Ctrl+R returns to the on-disk version); dirty-document discard
            // protection is handled by reload's own confirmation.
            | Cmd::Reload
            | Cmd::Save
            | Cmd::SaveAs
            | Cmd::SaveAll
            | Cmd::Close
            | Cmd::CloseAll
            | Cmd::Quit
            | Cmd::ForceQuit
            | Cmd::CloseSave
            | Cmd::CloseDiscard
            | Cmd::CloseCancel
            | Cmd::ReloadConfirm
            | Cmd::ReloadCancel
            | Cmd::RecoveryRestore
            | Cmd::RecoveryDiscard
            | Cmd::RecoveryDiscardConfirm
            | Cmd::RecoveryDiscardCancel
            | Cmd::Copy
            | Cmd::CopyAll
            | Cmd::CopyFileName
            | Cmd::CopyFilePath
            | Cmd::SelectAll
            | Cmd::BeginEndSelect
            | Cmd::Find(..)
            | Cmd::FindNext
            | Cmd::FindPrev
            | Cmd::Goto
            | Cmd::GotoBracket
            | Cmd::BookmarkToggle
            | Cmd::BookmarkNext
            | Cmd::BookmarkPrev
            | Cmd::ZoomIn
            | Cmd::ZoomOut
            | Cmd::ZoomReset
            | Cmd::MdExportHtml
            | Cmd::MdExportMd
            | Cmd::ToggleTheme
            | Cmd::ToggleHighlight
            | Cmd::HighlightAll
            | Cmd::WordWrap
            | Cmd::ShowEol
            | Cmd::ShowWs
            | Cmd::ToggleMinimap
            | Cmd::ToggleJsonl
            | Cmd::JsonlCollapseAll
            | Cmd::JsonlFoldLine
            | Cmd::JsonlUnfoldLine
            | Cmd::JsonPreview
            | Cmd::JsonCollapseAll
            | Cmd::JsonExpandAll
            | Cmd::ToggleCaretBlink
            | Cmd::CycleCaretStyle
            | Cmd::ToggleReadOnly
            | Cmd::ToggleSidebar
            | Cmd::ToggleOutline
            | Cmd::ToggleAutoClose
            | Cmd::SetEncoding(_)
            | Cmd::SetNewline(_)
            | Cmd::Lang(_)
            | Cmd::ToggleVim
            | Cmd::TabNext
            | Cmd::TabPrev
            | Cmd::TabClose
            | Cmd::TabCloseAll
            | Cmd::AlwaysOnTop
            | Cmd::WindowToggleMaximize
            | Cmd::WindowMinimize
            | Cmd::WindowMove
            | Cmd::WindowResize
            | Cmd::Preferences
            | Cmd::PreferencesClosed
            | Cmd::SetLocale(_)
            | Cmd::ShortcutHelp
            | Cmd::About
            | Cmd::Md5
            | Cmd::Sha1
            | Cmd::MdWys
            | Cmd::MdText
            | Cmd::CsvTable
            | Cmd::CsvToggleMode
            | Cmd::CsvDelim(_)
            | Cmd::CsvToggleHeader
            // Sorting a table is a view lens, not a document edit: it reorders
            // the parse cache only (rows still save in file order), so a
            // read-only document can be sorted like a read-only text can be
            // searched.
            | Cmd::CsvSort(_)
            | Cmd::WordCount
            | Cmd::CharCode
            | Cmd::MacroRecord
            | Cmd::MacroPlayback
            | Cmd::MacroSave
            | Cmd::MacroSaveNamed
            | Cmd::MacroRun
            | Cmd::MacroPlaySaved(_)
            | Cmd::MacroDir
            | Cmd::RunCommand
            | Cmd::RunExec
            | Cmd::RunAgain(_)
            | Cmd::WebSearch
            | Cmd::MarkStyle(..)
            | Cmd::MarkClear(_)
            | Cmd::BeginEndSelectColumn
            | Cmd::HideLines
            | Cmd::ShowHiddenLines
    )
}

/// Whether this command changed a user setting that needs persisting.
///
/// Explicit listing instead of "all commands written to disk": opening/closing documents shouldn't
/// touch settings files, and typing even less so.
fn is_settings_cmd(cmd: Cmd) -> bool {
    matches!(
        cmd,
        Cmd::ToggleTheme
            | Cmd::ZoomIn
            | Cmd::ZoomOut
            | Cmd::ZoomReset
            | Cmd::ToggleCaretBlink
            | Cmd::CycleCaretStyle
            | Cmd::ToggleHighlight
            | Cmd::HighlightAll
            | Cmd::WordWrap
            | Cmd::ShowEol
            | Cmd::ShowWs
            | Cmd::ToggleMinimap
            | Cmd::ToggleJsonl
            | Cmd::ToggleVim
            | Cmd::ToggleAutoClose
            | Cmd::SetEncoding(_)
            | Cmd::SetNewline(_)
            | Cmd::Lang(_)
            | Cmd::Open
            | Cmd::OpenWorkspace
            | Cmd::OpenRecent(_)
            | Cmd::ToggleSidebar
            | Cmd::SetLocale(_)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `is_toggle` must cover every command type for which `is_checked` can return true:
    /// miss one, and that menu's check column pops up out of nowhere once it is checked
    /// (labels jumping left and right).
    #[test]
    fn every_checkable_command_is_marked_as_toggle() {
        for cmd in [
            Cmd::ToggleTheme,
            Cmd::WordWrap,
            Cmd::ShowEol,
            Cmd::ShowWs,
            Cmd::ToggleMinimap,
            Cmd::ToggleCaretBlink,
            Cmd::SetNewline("LF"),
            Cmd::SetEncoding("UTF-8"),
            Cmd::Lang("__auto"),
            Cmd::CsvDelim(b','),
            Cmd::ToggleSidebar,
            Cmd::ToggleOutline,
            Cmd::ToggleVim,
            Cmd::ToggleReadOnly,
        ] {
            assert!(is_toggle(cmd), "{cmd:?} must count as checkable");
        }
        // plain commands have no check box
        for cmd in [Cmd::Undo, Cmd::Copy, Cmd::Quit, Cmd::Find(false, false)] {
            assert!(!is_toggle(cmd), "{cmd:?} must not have a check box");
        }
    }

    /// Regression: Vim mode / Read-only mode in the View menu used to be missing from
    /// `is_toggle`; opening the menu showed two plain entries — the toggles had neither
    /// a check box nor any indication of whether they were on or off.
    #[test]
    fn view_menu_gives_vim_and_read_only_a_check_box() {
        let mut st = EditorState::minimal();
        st.vim.enabled = true;
        st.read_only = true;
        st.menu_toggle(Caption::View, 0.0);
        let rows = menu_rows(&st);
        for cmd in [Cmd::ToggleVim, Cmd::ToggleReadOnly] {
            let row = rows
                .iter()
                .find(|r| r.cmd == Some(cmd))
                .unwrap_or_else(|| panic!("the View menu must contain {cmd:?}"));
            assert!(row.checkable, "{cmd:?} must carry a check box");
            assert!(row.checked, "{cmd:?} must be checked while on");
        }
    }

    /// Regression: Encoding → Character encoding is a radio group; the whole group had
    /// neither check boxes nor a marker for the current item. Now every entry has a box,
    /// and exactly **one** entry — the current encoding — is checked.
    #[test]
    fn encoding_menu_checks_exactly_the_current_encoding() {
        fn checked_ids(st: &EditorState) -> Vec<&'static str> {
            let mut ids: Vec<&'static str> = submenu_rows(st, Caption::Encoding)
                .into_iter()
                .filter(|r| r.checked)
                .filter_map(|r| match r.cmd {
                    Some(Cmd::SetEncoding(id)) => Some(id),
                    _ => None,
                })
                .collect();
            ids.sort_unstable();
            ids
        }

        let mut st = EditorState::minimal();
        st.menu_toggle(Caption::Encoding, 0.0);
        let charset = st.tr(Msg::EncodingCharset).into_owned();
        st.menu.sub = Some(charset);

        let rows = submenu_rows(&st, Caption::Encoding);
        let enc_rows: Vec<&MenuRow> = rows
            .iter()
            .filter(|r| matches!(r.cmd, Some(Cmd::SetEncoding(_))))
            .collect();
        assert_eq!(
            enc_rows.len(),
            crate::io::encoding::menu_ids().count(),
            "menu entries must cover the whole encoding table"
        );
        assert!(
            enc_rows.iter().all(|r| r.checkable),
            "encoding entries are all checkable (radio group)"
        );

        // Untitled document = UTF-8 no BOM
        assert_eq!(checked_ids(&st), vec!["UTF-8"]);
        // the check follows the selection, and there is still exactly one
        st.set_encoding("GBK");
        assert_eq!(checked_ids(&st), vec!["GBK"]);
        st.set_encoding("UTF-8 BOM");
        assert_eq!(checked_ids(&st), vec!["UTF-8 BOM"]);
        st.set_encoding("UTF-16LE");
        assert_eq!(checked_ids(&st), vec!["UTF-16LE"]);
        // ANSI equals GBK on Chinese Windows: only one checkmark can win (the specific encoding prevails)
        st.set_encoding("ANSI");
        assert_eq!(
            checked_ids(&st).len(),
            1,
            "a radio group must not check two entries at once (ANSI and the specific encoding it resolves to)"
        );
    }

    /// Encoding → Newline is likewise a radio group: even when the user never picked one,
    /// the entry that would "actually be written on save" must be checked — otherwise all
    /// three entries are empty right after opening a file and the current line ending is invisible.
    #[test]
    fn newline_menu_checks_the_effective_newline() {
        fn checked(st: &EditorState) -> Vec<&'static str> {
            submenu_rows(st, Caption::Encoding)
                .into_iter()
                .filter(|r| r.checked)
                .filter_map(|r| match r.cmd {
                    Some(Cmd::SetNewline(nl)) => Some(nl),
                    _ => None,
                })
                .collect()
        }

        let mut st = EditorState::minimal();
        st.tabs[0].doc = crate::buffer::TextDocument::from("a\r\nb\r\n");
        st.menu_toggle(Caption::Encoding, 0.0);
        let newline = st.tr(Msg::EncodingNewline).into_owned();
        st.menu.sub = Some(newline);

        assert_eq!(
            checked(&st),
            vec!["CRLF"],
            "without a user choice, check the detected line ending"
        );
        st.set_newline("LF");
        assert_eq!(
            checked(&st),
            vec!["LF"],
            "after a user choice, check the user's selection"
        );
    }

    /// Commands that touch no OS dialog, so they can be exercised head-less.
    /// Anything that opens a file picker / message box is deliberately absent:
    /// the smoke test must never block on a modal window.
    ///
    /// `Find` / `Goto` are here because without a registered window (`current_hwnd`
    /// is `0` under `cargo test`) they take the in-page path and touch nothing but
    /// state — which is exactly the branch worth smoke-testing.
    const HEADLESS: &[Cmd] = &[
        Cmd::New,
        Cmd::Undo,
        Cmd::Redo,
        Cmd::Cut,
        Cmd::Copy,
        Cmd::Paste,
        Cmd::Delete,
        Cmd::SelectAll,
        Cmd::CopyAll,
        Cmd::CopyFileName,
        Cmd::CopyFilePath,
        Cmd::BeginEndSelect,
        Cmd::Case(CaseKind::Upper),
        Cmd::Case(CaseKind::Lower),
        Cmd::Case(CaseKind::Proper),
        Cmd::Case(CaseKind::Sentence),
        Cmd::Case(CaseKind::Invert),
        Cmd::Case(CaseKind::FullWidth),
        Cmd::Case(CaseKind::HalfWidth),
        Cmd::UnicodeNfc,
        Cmd::UnicodeNfd,
        Cmd::InsertNumbering,
        Cmd::DuplicateLine,
        Cmd::DeleteDupLine,
        Cmd::DeleteConsecutiveDupLine,
        Cmd::SplitLines,
        Cmd::JoinLines,
        Cmd::MoveLineUp,
        Cmd::MoveLineDown,
        Cmd::InsertBlankAbove,
        Cmd::InsertBlankBelow,
        Cmd::ReverseLines,
        Cmd::RandomizeLines,
        Cmd::SortLines(SortKey::Text, false),
        Cmd::SortLines(SortKey::Text, true),
        Cmd::SortLines(SortKey::Length, false),
        Cmd::SortLines(SortKey::Integer, true),
        Cmd::SortLines(SortKey::DecimalDot, false),
        Cmd::SortLines(SortKey::DecimalComma, true),
        Cmd::TrimBoth,
        Cmd::TrimLeading,
        Cmd::TrimTrailing,
        Cmd::EolToSpace,
        Cmd::TabToSpace,
        Cmd::SpaceToTab,
        Cmd::SpaceToTabLeading,
        Cmd::RemoveEmptyLines,
        Cmd::RemoveEmptyLinesWithBlank,
        Cmd::ToggleLineComment,
        Cmd::BlockComment,
        Cmd::BlockUncomment,
        Cmd::InsertShortDateTime,
        Cmd::InsertLongDateTime,
        Cmd::Find(false, false),
        Cmd::Goto,
        Cmd::FindNext,
        Cmd::FindPrev,
        Cmd::ZoomIn,
        Cmd::ZoomOut,
        Cmd::ZoomReset,
        Cmd::ToggleTheme,
        Cmd::ToggleHighlight,
        Cmd::HighlightAll,
        Cmd::WordWrap,
        Cmd::ShowEol,
        Cmd::ShowWs,
        Cmd::ToggleMinimap,
        Cmd::ToggleSidebar,
        Cmd::ToggleOutline,
        Cmd::ToggleJsonl,
        Cmd::JsonlCollapseAll,
        Cmd::JsonlFoldLine,
        Cmd::JsonlUnfoldLine,
        Cmd::ToggleCaretBlink,
        Cmd::CycleCaretStyle,
        Cmd::ToggleVim,
        Cmd::SetEncoding("UTF-8"),
        Cmd::SetEncoding("GBK"),
        Cmd::SetEncoding("ANSI"),
        Cmd::SetNewline("CRLF"),
        Cmd::SetNewline("LF"),
        Cmd::SetNewline("CR"),
        Cmd::Lang("__auto"),
        Cmd::Lang("Plain Text"),
        Cmd::TabNext,
        Cmd::TabPrev,
        Cmd::WordCount,
        Cmd::CharCode,
        // ---- newly menu-implemented (macros/run/marks/column select/hide lines) ----
        Cmd::BeginEndSelectColumn,
        Cmd::MarkStyle(1, true),
        Cmd::MarkStyle(2, true),
        Cmd::MarkStyle(3, true),
        Cmd::MarkStyle(4, true),
        Cmd::MarkStyle(5, true),
        Cmd::MarkStyle(1, false),
        Cmd::MarkStyle(2, false),
        Cmd::MarkStyle(3, false),
        Cmd::MarkStyle(4, false),
        Cmd::MarkStyle(5, false),
        Cmd::MarkClear(false),
        Cmd::MarkClear(true),
        Cmd::HideLines,
        Cmd::ShowHiddenLines,
        Cmd::MacroSave,
        Cmd::MacroSaveNamed,
        Cmd::MacroRun,
        Cmd::MacroPlaySaved(0),
        Cmd::MacroDir,
        Cmd::RunCommand,
        Cmd::RunExec,
        Cmd::RunAgain(0),
    ];

    /// Two passes: an empty document, then a document with content and a
    /// selection, so the line/blank operations actually do work rather than
    /// returning early.
    #[test]
    fn headless_commands_do_not_panic() {
        let mut st = EditorState::minimal();
        for c in HEADLESS {
            run(&mut st, *c);
        }
        let mut st = EditorState::minimal();
        st.active_doc_mut()
            .insert_text(0, "b\n\n  a  \n10\n2\nb\n/**/\n");
        st.select_all();
        for c in HEADLESS {
            run(&mut st, *c);
        }
        // and once more with only a caret, so the "no selection" branches run
        let mut st = EditorState::minimal();
        st.active_doc_mut().insert_text(0, "b\n\n  a  \n10\n2\nb\n");
        st.clear_selection();
        st.set_caret(3, false);
        for c in HEADLESS {
            run(&mut st, *c);
        }
    }

    /// The right-click menu must carry its full set of entries.
    #[test]
    fn context_menu_has_full_entries() {
        let items = editor_context_items(crate::ui::Locale::DEFAULT);
        let mut labels = Vec::new();
        collect_labels(&items, &mut labels);
        for want in [
            "剪切",
            "复制",
            "粘贴",
            "删除",
            "全选",
            "开始/结束选择",
            "开始/结束选择（列模式）",
            "将所有标记样式化",
            "将当前标记样式化",
            "清除样式",
            "转为大写",
            "转为小写",
            "打开文件所在目录",
            "用浏览器搜索",
            "切换单行注释",
            "设置块注释",
            "取消块注释",
            "隐藏行",
        ] {
            assert!(
                labels.iter().any(|l| l == want),
                "missing context entry: {want}"
            );
        }
    }

    /// The menu bar must cover all top-level captions.
    #[test]
    fn menu_bar_has_all_captions() {
        let st = EditorState::minimal();
        let items = menu_bar_items(&st);
        let caps: Vec<String> = items.iter().map(|i| i.label().to_string()).collect();
        for want in [
            "文件", "编辑", "搜索", "视图", "编码", "语言", "设置", "工具", "宏", "运行", "窗口",
            "帮助",
        ] {
            assert!(
                caps.iter().any(|c| c == want),
                "missing menu caption: {want}"
            );
        }
    }

    /// The right-click menu must never expose a command that is neither
    /// exercised by [`HEADLESS`] nor explicitly known to be untestable head-less
    /// (a shell launch).
    ///
    /// This is what keeps `editor_context_items` honest: adding an entry without
    /// a head-less smoke path fails here instead of panicking in front of a user.
    /// The regression guard for the bug this table was built to kill: a menu
    /// entry must never advertise an accelerator the keyboard cannot produce.
    ///
    /// Before the table existed, 16 of the 25 advertised shortcuts were dead —
    /// `Ctrl+J`, `Ctrl+D`, `F3`, `Ctrl+Shift+U` and so on. Because labels now
    /// *come from* the binding table, an unimplemented shortcut cannot be typed
    /// into the tree at all; this test pins that invariant from the other side,
    /// so adding a hardcoded label back is caught immediately.
    #[test]
    fn no_menu_entry_advertises_an_unimplemented_shortcut() {
        fn walk(items: &[Item<'_>], out: &mut Vec<(String, String, Cmd)>) {
            for it in items {
                match it {
                    Item::Cmd { label, accel, cmd } => {
                        if !accel.is_empty() {
                            out.push((label.to_string(), accel.to_string(), *cmd));
                            assert_eq!(
                                accel.as_ref(),
                                crate::ui::shortcuts::accel_of(*cmd),
                                "`{}` advertises `{accel}` but the binding table has no such binding",
                                label
                            );
                        }
                    }
                    Item::Sub { items, .. } => walk(items, out),
                    Item::Sep => {}
                }
            }
        }
        let st = EditorState::minimal();
        let mut seen = Vec::new();
        walk(&menu_bar_items(&st), &mut seen);
        walk(&editor_context_items(crate::ui::Locale::DEFAULT), &mut seen);

        assert!(
            seen.len() >= 30,
            "expected a full menu, got {} entries",
            seen.len()
        );
        // and every binding in the table must be reachable from some menu, so
        // the table cannot accumulate bindings the user cannot discover. A
        // command may carry several chords (Redo = Ctrl+Y and Ctrl+Shift+Z),
        // so reachability is checked by **command**, while the displayed
        // accelerator stays the first chord (`accel_of`) — already asserted
        // by the equality check above.
        //
        // Exemption: file-type-scoped keyboard commands. JSONL line collapse/expand
        // (Ctrl+Alt+[ / ]) lives in the JSONL toolbar context (open a .jsonl for
        // context); the View menu does not advertise it (round-16 user decision:
        // file-type view features stay out of the menu). Register and explain the
        // entry context here when adding bindings of this kind.
        const MENU_EXEMPT: &[Cmd] = &[Cmd::JsonlFoldLine, Cmd::JsonlUnfoldLine];
        let cmds: Vec<Cmd> = seen.iter().map(|(_, _, c)| *c).collect();
        for b in crate::ui::shortcuts::BINDINGS {
            assert!(
                cmds.contains(&b.cmd) || MENU_EXEMPT.contains(&b.cmd),
                "binding {} ({:?}) is implemented but not shown in any menu",
                b.chord.label(),
                b.cmd
            );
        }
    }

    #[test]
    fn context_menu_commands_are_covered() {
        let items = editor_context_items(crate::ui::Locale::DEFAULT);
        let mut cmds = Vec::new();
        collect_cmds(&items, &mut cmds);
        assert!(!cmds.is_empty(), "context menu is empty");

        for c in &cmds {
            let covered = HEADLESS.contains(c) || matches!(c, Cmd::OpenFolder | Cmd::WebSearch);
            assert!(covered, "context-menu command has no head-less test: {c:?}");
        }
    }

    /// Settings → Caret carries the caret controls and they actually mutate state.
    #[test]
    fn caret_menu_and_commands() {
        // 1. the submenu exists and is wired to the right commands
        let st = EditorState::minimal();
        let menu = menu_bar_items(&st);
        let settings = find_sub(&menu, "设置").expect("Settings menu missing");
        let caret = find_sub(settings, "光标").expect("Caret submenu missing");
        let mut blink = false;
        let mut style = false;
        for it in caret {
            if let Item::Cmd { cmd, .. } = it {
                match cmd {
                    Cmd::ToggleCaretBlink => blink = true,
                    Cmd::CycleCaretStyle => style = true,
                    _ => {}
                }
            }
        }
        assert!(blink, "Settings→Caret is missing Blink (ToggleCaretBlink)");
        assert!(style, "Settings→Caret is missing Style (CycleCaretStyle)");

        // 2. running the commands mutates state
        let mut st = EditorState::minimal();
        let before = st.caret_blink;
        run(&mut st, Cmd::ToggleCaretBlink);
        assert_ne!(
            st.caret_blink, before,
            "ToggleCaretBlink did not flip caret_blink"
        );

        let before_style = st.caret_style;
        run(&mut st, Cmd::CycleCaretStyle);
        assert_ne!(
            st.caret_style, before_style,
            "CycleCaretStyle did not change caret_style"
        );
    }

    fn find_sub<'a, 'b>(items: &'a [Item<'b>], label: &str) -> Option<&'a [Item<'b>]> {
        for it in items {
            if let Item::Sub { label: l, items } = it {
                if l == label {
                    return Some(items);
                }
            }
        }
        None
    }

    /// A command may not appear twice in the context menu: the native popup maps
    /// its id back to a `Cmd` by index, so a duplicate would be ambiguous.
    #[test]
    fn context_menu_commands_are_unique() {
        let items = editor_context_items(crate::ui::Locale::DEFAULT);
        let mut cmds = Vec::new();
        collect_cmds(&items, &mut cmds);

        let mut seen: Vec<Cmd> = Vec::new();
        for c in cmds {
            assert!(!seen.contains(&c), "duplicate context-menu command: {c:?}");
            seen.push(c);
        }
    }

    fn collect_cmds(items: &[Item<'_>], out: &mut Vec<Cmd>) {
        for it in items {
            match it {
                Item::Cmd { cmd, .. } => out.push(*cmd),
                Item::Sub { items, .. } => collect_cmds(items, out),
                Item::Sep => {}
            }
        }
    }

    fn collect_labels(items: &[Item<'_>], out: &mut Vec<String>) {
        for it in items {
            match it {
                Item::Cmd { label, .. } => out.push(label.to_string()),
                Item::Sub { label, items } => {
                    out.push(label.to_string());
                    collect_labels(items, out);
                }
                Item::Sep => {}
            }
        }
    }

    /// <kbd>Alt</kbd>+<kbd>F</kbd> must open File and nothing else.
    ///
    /// <kbd>Alt</kbd>+letter must open the caption that owns that mnemonic —
    /// and it must be *that* caption, not merely the one at the same position.
    ///
    /// The identity is [`Caption`] itself, so this pins the two directions:
    /// `Caption::File` is what the state records, and its label is what the
    /// tree renders at index 0. A reordered `menu_bar_items` still fails here
    /// (the label at index `cap.index()` would belong to another caption).
    #[test]
    fn alt_letter_opens_the_matching_caption() {
        let mut st = EditorState::minimal();
        let caps = menu_bar_items(&st);
        assert_eq!(
            Caption::ALL.len(),
            caps.len(),
            "the mnemonic table has one entry per top-level caption"
        );
        for cap in Caption::ALL {
            st.menu_close();
            let ch = cap.accel();
            assert!(
                open_caption_by_accel(&mut st, ch),
                "Alt+{ch} matched nothing"
            );
            assert_eq!(
                st.menu.open,
                Some(cap),
                "Alt+{ch} opened the wrong caption ({cap:?})"
            );
            assert_eq!(
                caps[cap.index()].label(),
                st.tr(cap.msg()),
                "caption {cap:?}'s render position/wording does not match its identity"
            );
        }
    }

    /// An unknown letter must not open anything, and must not be treated as a
    /// caption either — otherwise a stray <kbd>Alt</kbd>+<kbd>Z</kbd> would
    /// silently open Search.
    #[test]
    fn an_unknown_accel_opens_nothing() {
        let mut st = EditorState::minimal();
        assert!(!open_caption_by_accel(&mut st, 'z'));
        assert!(!open_caption_by_accel(&mut st, '1'));
        assert!(st.menu.open.is_none());
    }

    /// Every mnemonic must be a plain ASCII letter, and unique — two captions
    /// sharing a key makes one of them unreachable.
    ///
    /// The key does **not** have to appear in the (Chinese) label: while
    /// <kbd>Alt</kbd> is held the caption renders it as `文件(F)`, so the key is
    /// visible to the user even though the label itself has no Latin character
    /// to underline.
    #[test]
    fn accels_are_unique_ascii_letters() {
        let mut seen = std::collections::HashSet::new();
        for cap in Caption::ALL {
            let c = cap.accel();
            assert!(
                c.is_ascii_alphabetic(),
                "mnemonic {c:?} of {cap:?} is not an ASCII letter"
            );
            assert!(seen.insert(c), "duplicate mnemonic: {c} ({cap:?})");
        }
    }

    /// Every caption must have exactly one access key, and there must be exactly
    /// as many captions as keys — a caption added without a key would be
    /// reachable only by mouse, which is the regression this guards.
    #[test]
    fn every_caption_has_exactly_one_access_key() {
        let st = EditorState::minimal();
        let caps = menu_bar_items(&st);
        assert_eq!(
            Caption::ALL.len(),
            caps.len(),
            "the menu tree must map one-to-one to Caption::ALL (order = index)"
        );
        for (i, item) in caps.iter().enumerate() {
            assert!(
                caption_accel(i).is_some(),
                "caption {:?} has no access key",
                item.label()
            );
        }
    }

    /// The title-bar right-click menu must offer a real Always-on-top toggle and must
    /// not offer Close — closing has to ask about unsaved changes first, which a
    /// native popup cannot do.
    #[test]
    fn the_window_system_menu_offers_topmost_but_not_close() {
        let mut st = EditorState::minimal();
        let items = window_system_items(&st);
        assert!(
            items.iter().any(|i| i.cmd_of() == Some(Cmd::AlwaysOnTop)),
            "Always-on-top missing from the title-bar menu"
        );
        assert!(
            !items.iter().any(|i| i.cmd_of() == Some(Cmd::Quit)),
            "Close must not be on the native menu: it cannot ask about unsaved work"
        );
        // the label flips with the state
        let before = items
            .iter()
            .find(|i| i.cmd_of() == Some(Cmd::AlwaysOnTop))
            .unwrap()
            .label()
            .to_string();
        st.win.topmost = true;
        let after = window_system_items(&st)
            .into_iter()
            .find(|i| i.cmd_of() == Some(Cmd::AlwaysOnTop))
            .unwrap()
            .label()
            .to_string();
        assert_ne!(
            before, after,
            "the Always-on-top label must reflect the current state"
        );
        assert!(after.contains("取消置顶"), "{after}");
    }

    /// Always-on-top is a toggle, so the menu must draw a check mark for it.
    #[test]
    fn topmost_renders_a_check_mark() {
        let mut st = EditorState::minimal();
        assert!(!is_checked(&st, Cmd::AlwaysOnTop));
        st.win.topmost = true;
        assert!(is_checked(&st, Cmd::AlwaysOnTop));
    }

    /// View commands never touch the document: read-only mode must allow them, and the
    /// WYS view must not report "temporarily unavailable". History: `WordWrap` /
    /// `ToggleMinimap` / JSONL / JSON tree switching were once outside the allowed list,
    /// so with read-only on (reading large files without accidental edits) users could not
    /// even fold JSON or toggle the minimap.
    #[test]
    fn view_only_commands_are_allowed_in_read_only_mode() {
        for cmd in [
            Cmd::WordWrap,
            Cmd::ToggleMinimap,
            Cmd::ToggleJsonl,
            Cmd::JsonlCollapseAll,
            Cmd::JsonlFoldLine,
            Cmd::JsonlUnfoldLine,
            Cmd::JsonPreview,
            Cmd::JsonCollapseAll,
            Cmd::JsonExpandAll,
        ] {
            assert!(!cmd_edits_doc(cmd), "{cmd:?} must not count as editing");
        }
        let mut st = EditorState::minimal();
        st.read_only = true;
        assert!(!st.wrap);
        run(&mut st, Cmd::WordWrap);
        assert!(st.wrap, "read-only mode must not block view toggles");
    }

    /// **Read-only mode guard rail**: iterate over every head-less command, run each one
    /// in read-only state, and assert that the original document's body and generation
    /// are unchanged.
    ///
    /// This is more reliable than hand-ticking the negatively maintained `cmd_edits_doc`
    /// list: any new command that really edits the document but was never registered turns
    /// red here immediately. "The active tab changed" caused by Close/New is not a
    /// violation — the original document is found again by id before comparing.
    #[test]
    fn read_only_mode_never_mutates_the_document() {
        for c in HEADLESS {
            let mut st = EditorState::minimal();
            st.active_doc_mut()
                .insert_text(0, "b\n\n  a  \n10\n2\nb\n/**/\n");
            st.select_all();
            st.read_only = true;
            let id = st.active_doc().id;
            let before = st.active_doc().read_all();
            let gen = st.active_doc().gen;

            run(&mut st, *c);

            let Some(tab) = st.tabs.iter().find(|t| t.doc.id == id) else {
                continue; // document was closed (Close/CloseAll/Quit…): nothing to compare
            };
            assert_eq!(
                tab.doc.gen, gen,
                "{c:?} modified the document in read-only mode (generation changed)"
            );
            assert_eq!(
                tab.doc.read_all(),
                before,
                "{c:?} modified the body in read-only mode"
            );
        }
    }

    /// `Cmd::CsvSort` sorts the **view**: dispatch must leave the document, its
    /// dirty flag and the undo stack alone, and read-only mode must not block it
    /// (`cmd_edits_doc` lists it with the view commands).
    ///
    /// This is the command path every entry point shares — a header click, a
    /// replayed macro and the menu all land here — so the invariant is asserted
    /// once, at the choke point, rather than per button.
    #[test]
    fn csv_sort_command_is_a_view_lens() {
        use crate::ui::testutil::st_of;
        let src = "k,v\nc,3\nb,2\n";
        let mut st = st_of(src);
        st.tab_mut().csv.enabled = true;
        st.tab_mut().csv.table = true;
        st.tab_mut().csv.header = true;
        let serial = st.tabs[0].doc.edit_serial();

        run(&mut st, Cmd::CsvSort(false));
        assert_eq!(
            st.tab().csv.sort_col,
            Some(0),
            "the lens column is recorded"
        );
        let (rows, _, _, _) = st.csv_rows_ensure();
        assert_eq!(rows[1][0], "b", "the view is ordered by column 0");
        assert_eq!(rows[2][0], "c");
        assert_eq!(
            st.active_doc().read_all(),
            src,
            "the document text is untouched"
        );
        assert_eq!(st.tabs[0].doc.edit_serial(), serial);
        assert!(!st.tabs[0].doc.can_undo(), "no undo step");
        assert!(!st.tabs[0].doc.is_dirty(), "not a modification");

        // Read-only mode: a lens is not an edit, so intercept must let it through
        st.read_only = true;
        run(&mut st, Cmd::CsvSort(true));
        assert!(st.tab().csv.sort_desc);
        let (rows, _, _, _) = st.csv_rows_ensure();
        assert_eq!(rows[1][0], "c", "descending through the same lens");
        assert_eq!(st.active_doc().read_all(), src);
    }

    /// `active` must stay inside `0..tabs.len()` after **every** command.
    ///
    /// `tab()` / `tab_mut()` use bare indexing (see the invariant on
    /// `EditorState::active`): out of range is not an error code, it is a
    /// panicking window, and menus, session restore and background opens all
    /// arrive with "last frame's" index. The existing read-only sweep already
    /// runs every head-less command, but it never touches `tab()` afterwards —
    /// this one does, and also asserts the range explicitly so the failure
    /// message names the command instead of a slice index.
    ///
    /// The close/quit commands are swept separately: they are deliberately
    /// absent from [`HEADLESS`] (they end the session), and they are exactly the
    /// ones that move `active`.
    #[test]
    fn active_tab_stays_in_range_after_every_command() {
        const LIFECYCLE: &[Cmd] = &[
            Cmd::Close,
            Cmd::CloseAll,
            Cmd::Quit,
            Cmd::TabClose,
            Cmd::TabCloseAll,
            Cmd::TabNext,
            Cmd::TabPrev,
        ];
        for c in HEADLESS.iter().chain(LIFECYCLE) {
            let mut st = EditorState::minimal();
            // Two tabs: switching and closing commands need something to act on.
            // The documents are clean, so the two-phase close does not stop at a
            // confirmation dialog and the tab really is removed.
            st.new_doc();

            run(&mut st, *c);

            if st.tabs.is_empty() {
                continue; // Close/CloseAll/Quit: nothing to index, by definition
            }
            assert!(
                st.active < st.tabs.len(),
                "{c:?} left active = {} with {} tab(s)",
                st.active,
                st.tabs.len()
            );
            // The accessors every frame and keypress calls.
            let _ = st.tab().doc.total_bytes();
            st.tab_mut().md_wys = st.tab().md_wys;
        }
    }
}
