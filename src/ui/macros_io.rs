//! On-disk persistence of macros (Macros → Save Macro… / Run Macro…).
//!
//! # Why on disk
//!
//! The whole point of "save macro" is to let it outlive the session: a
//! recorded [`MacroStep`] sequence is written to a disk file and replayed
//! later from the macro menu.
//!
//! # File format
//!
//! A JSON file (`<name>.imacro`):
//!
//! ```json
//! { "format": "infeditor-macro", "version": 1, "steps": [{"Cmd":"DuplicateLine"}, {"Text":"x"}] }
//! ```
//!
//! Commands are encoded into stable strings via [`cmd_to_wire`] /
//! [`cmd_from_wire`] — **not** `Debug`: `Debug` output is an implementation
//! detail, and a rename would corrupt saved files. Parsing is defensive:
//! unknown commands / bad JSON are always skipped and reported, never a
//! panic — a hand-edited macro file has no right to crash the editor.
//!
//! # Test environment
//!
//! [`config_dir`] is always `None` under `cfg!(test)`: unit tests never
//! touch the user's real `%APPDATA%\InfEditor` (same reason as the guard
//! in `settings::save`).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::textops::{CaseKind, SortKey};
use crate::ui::actions::Cmd;
use crate::ui::recorder::MacroStep;
use crate::ui::{Fmt, Msg};

/// File format identifier; an unrecognised format rejects the whole file.
pub const FORMAT: &str = "infeditor-macro";
/// Current format version.
pub const VERSION: u32 = 1;
/// Extension of macro files.
pub const EXT: &str = "imacro";

// ---------------------------------------------------------------------------
// Directories
// ---------------------------------------------------------------------------

/// App config directory (Windows = `%APPDATA%\InfEditor`); `None` in tests
/// or when it cannot be determined.
///
/// The `INFEDITOR_CONFIG_DIR` env var overrides it wholesale (portable
/// mode); integration tests also use it to isolate session salvage, macros
/// and settings persistence into a temp directory — integration tests link
/// the **normally compiled** library (`cfg!(test)` is false), so without
/// this override they would write to the user's real config directory.
pub fn config_dir() -> Option<PathBuf> {
    if cfg!(test) {
        return None;
    }
    if let Ok(d) = std::env::var("INFEDITOR_CONFIG_DIR") {
        if !d.trim().is_empty() {
            return Some(PathBuf::from(d));
        }
    }
    directories::ProjectDirs::from("", "", "InfEditor").map(|d| d.config_dir().to_path_buf())
}

/// Directory where macros are saved (`macros`).
pub fn macros_dir() -> Option<PathBuf> {
    config_dir().map(|d| d.join("macros"))
}

/// All macro files in the directory, sorted by file name.
///
/// Missing directory / no user directory ⇒ empty list ("no macros at all"
/// is a perfectly normal state, not an error).
pub fn list_scripts(dir: Option<&Path>) -> Vec<PathBuf> {
    let Some(dir) = dir else { return Vec::new() };
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<PathBuf> = rd
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_file() && p.extension().is_some_and(|x| x == EXT))
        .collect();
    out.sort_by_key(|p| p.file_name().map(|n| n.to_os_string()).unwrap_or_default());
    out
}

/// Turn a user-entered name into a safe file name.
///
/// Illegal characters become `_`, an empty name falls back to `宏`, and the
/// result is truncated to 80 characters — Windows reserved names such as
/// `CON`/`NUL` are avoided too (prefixing a `_` suffices).
pub fn sanitize_file_name(name: &str) -> String {
    let trimmed = name.trim();
    let mut s: String = trimmed
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();
    s = s.trim_matches('.').trim().to_string();
    if s.is_empty() {
        s = "宏".to_string();
    }
    const RESERVED: &[&str] = &["CON", "PRN", "AUX", "NUL"];
    if RESERVED.iter().any(|r| s.eq_ignore_ascii_case(r)) {
        s = format!("_{s}");
    }
    let cap = 80;
    if s.chars().count() > cap {
        s = s.chars().take(cap).collect();
    }
    s
}

// ---------------------------------------------------------------------------
// Serialization
// ---------------------------------------------------------------------------

/// One step on disk (the JSON mirror of [`MacroStep`]).
///
/// Externally tagged (serde's default): `{"Cmd":"Undo"}`, `{"Text":"x"}`,
/// `"Backspace"`. Internally tagged (`tag = "step"`) is *not* an option —
/// serde refuses to put a bare string inside it.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub enum WireStep {
    /// String encoding via [`cmd_to_wire`]
    Cmd(String),
    Text(String),
    Backspace,
    Delete,
    Enter,
    Tab,
}

/// Top-level structure of the disk file.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
struct WireFile {
    format: String,
    version: u32,
    steps: Vec<WireStep>,
}

fn step_to_wire(s: &MacroStep) -> Option<WireStep> {
    Some(match s {
        MacroStep::Cmd(c) => WireStep::Cmd(cmd_to_wire(*c)?),
        MacroStep::Text(t) => WireStep::Text(t.clone()),
        MacroStep::Backspace => WireStep::Backspace,
        MacroStep::Delete => WireStep::Delete,
        MacroStep::Enter => WireStep::Enter,
        MacroStep::Tab => WireStep::Tab,
    })
}

pub(crate) fn step_from_wire(w: WireStep) -> Option<MacroStep> {
    Some(match w {
        WireStep::Cmd(s) => MacroStep::Cmd(cmd_from_wire(&s)?),
        WireStep::Text(t) => MacroStep::Text(t),
        WireStep::Backspace => MacroStep::Backspace,
        WireStep::Delete => MacroStep::Delete,
        WireStep::Enter => MacroStep::Enter,
        WireStep::Tab => MacroStep::Tab,
    })
}

/// Write `steps` to `<dir>/<name>.imacro`. Creates `dir` on demand.
///
/// `dir` is `Option` so the head-less test paths (where [`config_dir`] is
/// `None`) surface as a normal `io::Error` instead of touching the real user
/// profile.
pub fn save_macro(dir: Option<&Path>, name: &str, steps: &[MacroStep]) -> std::io::Result<PathBuf> {
    let dir = dir.ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "no user config directory available",
        )
    })?;
    std::fs::create_dir_all(dir)?;
    let file = WireFile {
        format: FORMAT.into(),
        version: VERSION,
        steps: steps.iter().filter_map(step_to_wire).collect(),
    };
    let json = serde_json::to_string_pretty(&file)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    let path = dir.join(format!("{}.{}", sanitize_file_name(name), EXT));
    std::fs::write(&path, json)?;
    Ok(path)
}

/// Read a macro file. A corrupt / foreign / hand-edited file yields `None`
/// plus whatever steps could be salvaged — never a panic.
pub fn load_macro(path: &Path) -> Option<Vec<MacroStep>> {
    let text = std::fs::read_to_string(path).ok()?;
    let file: WireFile = serde_json::from_str(&text).ok()?;
    if file.format != FORMAT || file.version > VERSION {
        return None;
    }
    Some(file.steps.into_iter().filter_map(step_from_wire).collect())
}

/// `list_scripts` + display names, for dialogs and menus.
pub fn display_names(files: &[PathBuf]) -> Vec<String> {
    files
        .iter()
        .map(|p| {
            p.file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| p.to_string_lossy().into_owned())
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Command codec
// ---------------------------------------------------------------------------

fn case_wire(k: CaseKind) -> &'static str {
    match k {
        CaseKind::Upper => "Upper",
        CaseKind::Lower => "Lower",
        CaseKind::Proper => "Proper",
        CaseKind::Sentence => "Sentence",
        CaseKind::Invert => "Invert",
        CaseKind::FullWidth => "FullWidth",
        CaseKind::HalfWidth => "HalfWidth",
    }
}

fn case_from_wire(s: &str) -> Option<CaseKind> {
    Some(match s {
        "Upper" => CaseKind::Upper,
        "Lower" => CaseKind::Lower,
        "Proper" => CaseKind::Proper,
        "Sentence" => CaseKind::Sentence,
        "Invert" => CaseKind::Invert,
        "FullWidth" => CaseKind::FullWidth,
        "HalfWidth" => CaseKind::HalfWidth,
        _ => return None,
    })
}

fn sort_wire(k: SortKey) -> &'static str {
    match k {
        SortKey::Text => "Text",
        SortKey::TextIgnoreCase => "TextIgnoreCase",
        SortKey::Length => "Length",
        SortKey::Integer => "Integer",
        SortKey::DecimalDot => "DecimalDot",
        SortKey::DecimalComma => "DecimalComma",
    }
}

fn sort_from_wire(s: &str) -> Option<SortKey> {
    Some(match s {
        "Text" => SortKey::Text,
        "TextIgnoreCase" => SortKey::TextIgnoreCase,
        "Length" => SortKey::Length,
        "Integer" => SortKey::Integer,
        "DecimalDot" => SortKey::DecimalDot,
        "DecimalComma" => SortKey::DecimalComma,
        _ => return None,
    })
}

/// Intern a charset name back to its `&'static str`.
///
/// `Cmd::SetEncoding` carries a static string, so the codec cannot accept an
/// arbitrary one from disk — a foreign name would not round-trip. The table is
/// exactly the encoding menu's ids (see [`crate::io::encoding::MENU_GROUPS`]),
/// so a menu entry and a macro name cannot drift apart.
fn encoding_from_wire(s: &str) -> Option<&'static str> {
    crate::io::encoding::menu_ids().find(|id| *id == s)
}

fn newline_from_wire(s: &str) -> Option<&'static str> {
    const NAMES: &[&str] = &["CRLF", "LF", "CR"];
    NAMES.iter().find(|n| **n == s).copied()
}

fn lang_from_wire(s: &str) -> Option<&'static str> {
    // `__auto` / `Plain Text` are menu entries rather than highlighter specs, so
    // they join the lookup table here.
    let mut names = vec!["__auto", "Plain Text"];
    names.extend(crate::highlight::names());
    names.into_iter().find(|n| *n == s)
}

/// Stable string encoding of a command. `None` for commands that only make
/// sense in *this* session (they act on dialogs / the event loop) — the
/// recorder already drops them, this is the second net.
pub fn cmd_to_wire(c: Cmd) -> Option<String> {
    use Cmd::*;
    match c {
        OpenRecent(i) => Some(format!("OpenRecent:{i}")),
        Find(a, b) => Some(format!("Find:{}:{}", a as u8, b as u8)),
        Case(k) => Some(format!("Case:{}", case_wire(k))),
        SortLines(k, desc) => Some(format!("SortLines:{}:{}", sort_wire(k), desc as u8)),
        SetEncoding(n) => Some(format!("SetEncoding:{n}")),
        SetNewline(n) => Some(format!("SetNewline:{n}")),
        Lang(n) => Some(format!("Lang:{n}")),
        MarkStyle(style, all) => Some(format!("MarkStyle:{style}:{}", all as u8)),
        MarkClear(all) => Some(format!("MarkClear:{}", all as u8)),
        MacroPlaySaved(i) => Some(format!("MacroPlaySaved:{i}")),
        RunAgain(i) => Some(format!("RunAgain:{i}")),
        // ---- CSV / Markdown toolbar -----------------------------------------
        CsvDelim(d) => Some(format!("CsvDelim:{d}")),
        CsvSort(desc) => Some(format!("CsvSort:{}", desc as u8)),
        MdEdit(act) => Some(format!("MdEdit:{}", md_act_wire(act))),
        // session-only: they act on the live dialog / event loop, not on a document
        RunExec | MacroPlayback | MacroSaveNamed | PreferencesClosed => None,
        // Same for the recovery confirmation box: replaying a "restore session" macro is meaningless.
        RecoveryRestore | RecoveryDiscard => None,
        other => payload_free_name(other),
    }
}

/// The wire name of a payload-free command: its `Debug` spelling, pinned by
/// [`cmd_from_wire`]'s table (and by the round-trip test) so it cannot drift
/// with refactors.
fn payload_free_name(c: Cmd) -> Option<String> {
    let name = match c {
        Cmd::New => "New",
        Cmd::Open => "Open",
        Cmd::OpenWorkspace => "OpenWorkspace",
        Cmd::OpenFolder => "OpenFolder",
        Cmd::Reload => "Reload",
        Cmd::Save => "Save",
        Cmd::SaveAs => "SaveAs",
        Cmd::SaveAll => "SaveAll",
        Cmd::Close => "Close",
        Cmd::CloseAll => "CloseAll",
        Cmd::Quit => "Quit",
        Cmd::ForceQuit => "ForceQuit",
        Cmd::Undo => "Undo",
        Cmd::Redo => "Redo",
        Cmd::Cut => "Cut",
        Cmd::Copy => "Copy",
        Cmd::Paste => "Paste",
        Cmd::Delete => "Delete",
        Cmd::SelectAll => "SelectAll",
        Cmd::CopyAll => "CopyAll",
        Cmd::CopyFileName => "CopyFileName",
        Cmd::CopyFilePath => "CopyFilePath",
        Cmd::BeginEndSelect => "BeginEndSelect",
        Cmd::BeginEndSelectColumn => "BeginEndSelectColumn",
        Cmd::GotoBracket => "GotoBracket",
        Cmd::ToggleAutoClose => "ToggleAutoClose",
        Cmd::BookmarkToggle => "BookmarkToggle",
        Cmd::BookmarkNext => "BookmarkNext",
        Cmd::BookmarkPrev => "BookmarkPrev",
        Cmd::ToggleReadOnly => "ToggleReadOnly",
        Cmd::DuplicateLine => "DuplicateLine",
        Cmd::DeleteDupLine => "DeleteDupLine",
        Cmd::DeleteConsecutiveDupLine => "DeleteConsecutiveDupLine",
        Cmd::SplitLines => "SplitLines",
        Cmd::JoinLines => "JoinLines",
        Cmd::MoveLineUp => "MoveLineUp",
        Cmd::MoveLineDown => "MoveLineDown",
        Cmd::InsertBlankAbove => "InsertBlankAbove",
        Cmd::InsertBlankBelow => "InsertBlankBelow",
        Cmd::ReverseLines => "ReverseLines",
        Cmd::RandomizeLines => "RandomizeLines",
        Cmd::TrimBoth => "TrimBoth",
        Cmd::TrimLeading => "TrimLeading",
        Cmd::TrimTrailing => "TrimTrailing",
        Cmd::EolToSpace => "EolToSpace",
        Cmd::TabToSpace => "TabToSpace",
        Cmd::SpaceToTab => "SpaceToTab",
        Cmd::SpaceToTabLeading => "SpaceToTabLeading",
        Cmd::RemoveEmptyLines => "RemoveEmptyLines",
        Cmd::RemoveEmptyLinesWithBlank => "RemoveEmptyLinesWithBlank",
        Cmd::ToggleLineComment => "ToggleLineComment",
        Cmd::BlockComment => "BlockComment",
        Cmd::BlockUncomment => "BlockUncomment",
        Cmd::InsertNumbering => "InsertNumbering",
        Cmd::UnicodeNfc => "UnicodeNfc",
        Cmd::UnicodeNfd => "UnicodeNfd",
        Cmd::InsertShortDateTime => "InsertShortDateTime",
        Cmd::InsertLongDateTime => "InsertLongDateTime",
        Cmd::Goto => "Goto",
        Cmd::FindNext => "FindNext",
        Cmd::FindPrev => "FindPrev",
        Cmd::ZoomIn => "ZoomIn",
        Cmd::ZoomOut => "ZoomOut",
        Cmd::ZoomReset => "ZoomReset",
        Cmd::ToggleTheme => "ToggleTheme",
        Cmd::ToggleHighlight => "ToggleHighlight",
        Cmd::HighlightAll => "HighlightAll",
        Cmd::WordWrap => "WordWrap",
        Cmd::ShowEol => "ShowEol",
        Cmd::ShowWs => "ShowWs",
        Cmd::ToggleMinimap => "ToggleMinimap",
        Cmd::ToggleSidebar => "ToggleSidebar",
        Cmd::ToggleJsonl => "ToggleJsonl",
        Cmd::JsonlCollapseAll => "JsonlCollapseAll",
        Cmd::JsonlFoldLine => "JsonlFoldLine",
        Cmd::JsonlUnfoldLine => "JsonlUnfoldLine",
        Cmd::ToggleVim => "ToggleVim",
        Cmd::MacroRecord => "MacroRecord",
        Cmd::MacroSave => "MacroSave",
        Cmd::MacroRun => "MacroRun",
        Cmd::MacroDir => "MacroDir",
        Cmd::RunCommand => "RunCommand",
        Cmd::Md5 => "Md5",
        Cmd::Sha1 => "Sha1",
        Cmd::FormatJson => "FormatJson",
        Cmd::FormatXml => "FormatXml",
        Cmd::CsvTable => "CsvTable",
        Cmd::CsvToggleMode => "CsvToggleMode",
        Cmd::CsvToggleHeader => "CsvToggleHeader",
        Cmd::WordCount => "WordCount",
        Cmd::CharCode => "CharCode",
        Cmd::ToggleCaretBlink => "ToggleCaretBlink",
        Cmd::CycleCaretStyle => "CycleCaretStyle",
        Cmd::TabNext => "TabNext",
        Cmd::TabPrev => "TabPrev",
        Cmd::TabClose => "TabClose",
        Cmd::TabCloseAll => "TabCloseAll",
        Cmd::AlwaysOnTop => "AlwaysOnTop",
        Cmd::WindowToggleMaximize => "WindowToggleMaximize",
        Cmd::WindowMinimize => "WindowMinimize",
        Cmd::Preferences => "Preferences",
        Cmd::About => "About",
        Cmd::HideLines => "HideLines",
        Cmd::ShowHiddenLines => "ShowHiddenLines",
        // anything else has a payload or must not round-trip
        _ => return None,
    };
    Some(name.to_string())
}
/// Inverse of [`cmd_to_wire`].
pub fn cmd_from_wire(s: &str) -> Option<Cmd> {
    use Cmd::*;
    // parameterised forms first: their prefix could also be a plain name
    if let Some(rest) = s.strip_prefix("OpenRecent:") {
        return rest.parse().ok().map(OpenRecent);
    }
    if let Some(rest) = s.strip_prefix("Find:") {
        let mut it = rest.split(':');
        let a = it.next()?.parse::<u8>().ok()?;
        let b = it.next()?.parse::<u8>().ok()?;
        if a > 1 || b > 1 || it.next().is_some() {
            return None;
        }
        return Some(Find(a != 0, b != 0));
    }
    if let Some(rest) = s.strip_prefix("Case:") {
        return case_from_wire(rest).map(Case);
    }
    if let Some(rest) = s.strip_prefix("SortLines:") {
        let mut it = rest.split(':');
        let key = sort_from_wire(it.next()?)?;
        let desc = it.next()?.parse::<u8>().ok()? != 0;
        return Some(SortLines(key, desc));
    }
    if let Some(rest) = s.strip_prefix("SetEncoding:") {
        return encoding_from_wire(rest).map(SetEncoding);
    }
    if let Some(rest) = s.strip_prefix("SetNewline:") {
        return newline_from_wire(rest).map(SetNewline);
    }
    if let Some(rest) = s.strip_prefix("Lang:") {
        return lang_from_wire(rest).map(Lang);
    }
    if let Some(rest) = s.strip_prefix("MarkStyle:") {
        let mut it = rest.split(':');
        let style = it.next()?.parse::<u8>().ok()?;
        if !(1..=5).contains(&style) {
            return None;
        }
        let all = it.next()?.parse::<u8>().ok()? != 0;
        return Some(MarkStyle(style, all));
    }
    if let Some(rest) = s.strip_prefix("MarkClear:") {
        return rest.parse::<u8>().ok().map(|v| MarkClear(v != 0));
    }
    if let Some(rest) = s.strip_prefix("MacroPlaySaved:") {
        return rest.parse().ok().map(MacroPlaySaved);
    }
    if let Some(rest) = s.strip_prefix("RunAgain:") {
        return rest.parse().ok().map(RunAgain);
    }
    if let Some(rest) = s.strip_prefix("CsvDelim:") {
        return rest.parse::<u8>().ok().map(CsvDelim);
    }
    if let Some(rest) = s.strip_prefix("CsvSort:") {
        return rest.parse::<u8>().ok().map(|v| CsvSort(v != 0));
    }
    if let Some(rest) = s.strip_prefix("MdEdit:") {
        return md_act_from_wire(rest).map(MdEdit);
    }
    match s {
        "MarkClearAll" => Some(MarkClear(false)),
        "CsvToggleMode" => Some(CsvToggleMode),
        "CsvToggleHeader" => Some(CsvToggleHeader),
        _ => payload_free_from_wire(s),
    }
}

/// Wire format of [`crate::ui::commands::markdown::MdAct`] (human-readable in macro files).
fn md_act_wire(a: crate::ui::commands::markdown::MdAct) -> String {
    use crate::ui::commands::markdown::MdAct::*;
    match a {
        Bold => "Bold".into(),
        Italic => "Italic".into(),
        Strike => "Strike".into(),
        Code => "Code".into(),
        CodeBlock => "CodeBlock".into(),
        Heading(n) => format!("Heading:{n}"),
        Body => "Body".into(),
        Quote => "Quote".into(),
        Bullet => "Bullet".into(),
        Numbered => "Numbered".into(),
        TaskList => "TaskList".into(),
        Link => "Link".into(),
        Image => "Image".into(),
        TableGrid { rows, cols } => format!("TableGrid:{rows}x{cols}"),
        Emoji(i) => format!("Emoji:{i}"),
        Hr => "Hr".into(),
    }
}

fn md_act_from_wire(s: &str) -> Option<crate::ui::commands::markdown::MdAct> {
    use crate::ui::commands::markdown::MdAct::*;
    Some(match s {
        "Bold" => Bold,
        "Italic" => Italic,
        "Strike" => Strike,
        "Code" => Code,
        "CodeBlock" => CodeBlock,
        "Quote" => Quote,
        "Bullet" => Bullet,
        "Numbered" => Numbered,
        "TaskList" => TaskList,
        "Link" => Link,
        "Image" => Image,
        "Body" => Body,
        "Hr" => Hr,
        other => {
            // "Table" = the old fixed 2×2 template (the grid picker records
            // TableGrid:rows×cols now); old macros keep playing back.
            if other == "Table" {
                return Some(TableGrid { rows: 2, cols: 2 });
            }
            if let Some((r, c)) = other.strip_prefix("TableGrid:").and_then(|s| {
                s.split_once('x')
                    .and_then(|(r, c)| Some((r.parse::<u8>().ok()?, c.parse::<u8>().ok()?)))
            }) {
                return Some(TableGrid { rows: r, cols: c });
            }
            if let Some(i) = other
                .strip_prefix("Emoji:")
                .and_then(|s| s.parse::<u8>().ok())
            {
                return Some(Emoji(i));
            }
            let n = other.strip_prefix("Heading:")?.parse::<u8>().ok()?;
            Heading(n)
        }
    })
}

fn payload_free_from_wire(s: &str) -> Option<Cmd> {
    use Cmd::*;
    Some(match s {
        "New" => New,
        "Open" => Open,
        "OpenWorkspace" => OpenWorkspace,
        "OpenFolder" => OpenFolder,
        "Reload" => Reload,
        "Save" => Save,
        "SaveAs" => SaveAs,
        "SaveAll" => SaveAll,
        "Close" => Close,
        "CloseAll" => CloseAll,
        "Quit" => Quit,
        "ForceQuit" => ForceQuit,
        "Undo" => Undo,
        "Redo" => Redo,
        "Cut" => Cut,
        "Copy" => Copy,
        "Paste" => Paste,
        "Delete" => Delete,
        "SelectAll" => SelectAll,
        "CopyAll" => CopyAll,
        "CopyFileName" => CopyFileName,
        "CopyFilePath" => CopyFilePath,
        "BeginEndSelect" => BeginEndSelect,
        "BeginEndSelectColumn" => BeginEndSelectColumn,
        "GotoBracket" => GotoBracket,
        "ToggleAutoClose" => ToggleAutoClose,
        "BookmarkToggle" => BookmarkToggle,
        "BookmarkNext" => BookmarkNext,
        "BookmarkPrev" => BookmarkPrev,
        "ToggleReadOnly" => ToggleReadOnly,
        "DuplicateLine" => DuplicateLine,
        "DeleteDupLine" => DeleteDupLine,
        "DeleteConsecutiveDupLine" => DeleteConsecutiveDupLine,
        "SplitLines" => SplitLines,
        "JoinLines" => JoinLines,
        "MoveLineUp" => MoveLineUp,
        "MoveLineDown" => MoveLineDown,
        "InsertBlankAbove" => InsertBlankAbove,
        "InsertBlankBelow" => InsertBlankBelow,
        "ReverseLines" => ReverseLines,
        "RandomizeLines" => RandomizeLines,
        "TrimBoth" => TrimBoth,
        "TrimLeading" => TrimLeading,
        "TrimTrailing" => TrimTrailing,
        "EolToSpace" => EolToSpace,
        "TabToSpace" => TabToSpace,
        "SpaceToTab" => SpaceToTab,
        "SpaceToTabLeading" => SpaceToTabLeading,
        "RemoveEmptyLines" => RemoveEmptyLines,
        "RemoveEmptyLinesWithBlank" => RemoveEmptyLinesWithBlank,
        "ToggleLineComment" => ToggleLineComment,
        "BlockComment" => BlockComment,
        "BlockUncomment" => BlockUncomment,
        "InsertNumbering" => InsertNumbering,
        "UnicodeNfc" => UnicodeNfc,
        "UnicodeNfd" => UnicodeNfd,
        "InsertShortDateTime" => InsertShortDateTime,
        "InsertLongDateTime" => InsertLongDateTime,
        "Goto" => Goto,
        "FindNext" => FindNext,
        "FindPrev" => FindPrev,
        "ZoomIn" => ZoomIn,
        "ZoomOut" => ZoomOut,
        "ZoomReset" => ZoomReset,
        "ToggleTheme" => ToggleTheme,
        "ToggleHighlight" => ToggleHighlight,
        "HighlightAll" => HighlightAll,
        "WordWrap" => WordWrap,
        "ShowEol" => ShowEol,
        "ShowWs" => ShowWs,
        "ToggleMinimap" => ToggleMinimap,
        "ToggleSidebar" => ToggleSidebar,
        "ToggleJsonl" => ToggleJsonl,
        "JsonlCollapseAll" => JsonlCollapseAll,
        "JsonlFoldLine" => JsonlFoldLine,
        "JsonlUnfoldLine" => JsonlUnfoldLine,
        "ToggleVim" => ToggleVim,
        "MacroRecord" => MacroRecord,
        "MacroSave" => MacroSave,
        "MacroRun" => MacroRun,
        "MacroDir" => MacroDir,
        "RunCommand" => RunCommand,
        "Md5" => Md5,
        "Sha1" => Sha1,
        "FormatJson" => FormatJson,
        "FormatXml" => FormatXml,
        // The inline preview view is gone (2026-10); old macros that recorded
        // the view switch degrade to the nearest surviving view, text.
        "MdPreview" => MdText,
        "CsvTable" => CsvTable,
        "WordCount" => WordCount,
        "CharCode" => CharCode,
        "ToggleCaretBlink" => ToggleCaretBlink,
        "CycleCaretStyle" => CycleCaretStyle,
        "TabNext" => TabNext,
        "TabPrev" => TabPrev,
        "TabClose" => TabClose,
        "TabCloseAll" => TabCloseAll,
        "AlwaysOnTop" => AlwaysOnTop,
        "WindowToggleMaximize" => WindowToggleMaximize,
        "WindowMinimize" => WindowMinimize,
        "Preferences" => Preferences,
        "About" => About,
        "HideLines" => HideLines,
        "ShowHiddenLines" => ShowHiddenLines,
        _ => return None,
    })
}

// ---------------------------------------------------------------------------
// EditorState: macro/run command behaviour
// ---------------------------------------------------------------------------

/// Reveal a path in the platform file manager, creating it first when it is a
/// directory that does not exist yet ("open macro directory" on a fresh
/// profile).
pub(crate) fn reveal_path_for(path: &Path, is_dir: bool, st: &mut crate::ui::state::EditorState) {
    reveal_path_inner(path, is_dir, st)
}

fn reveal_path_inner(path: &Path, is_dir: bool, st: &mut crate::ui::state::EditorState) {
    if is_dir && std::fs::create_dir_all(path).is_err() {
        st.status = Some(
            Fmt::StatusDirCreateFailed {
                path: path.display().to_string(),
            }
            .into(),
        );
        return;
    }
    // Platform differences (explorer /select, open, xdg-open) live in the
    // `platform` crate; file = select it in the file manager, directory =
    // open the directory itself.
    let res = platform::reveal_in_file_manager(path, !is_dir);
    st.status = Some(match res {
        Ok(_) => Fmt::StatusRevealedInFileManager {
            path: path.display().to_string(),
        }
        .into(),
        Err(e) => Fmt::StatusRevealError { err: e.to_string() }.into(),
    });
}

impl crate::ui::state::EditorState {
    /// Macros → Save Macro…: name the most recent macro and write it to disk.
    pub fn macro_save_open(&mut self) {
        if self.is_recording() {
            // The recording buffer is only finalised once recording stops
            // (see the recorder module); saving now would store a half-cut
            // macro — direct the user to stop first.
            self.status = Some(Msg::StatusRecordingSaveBlocked.into());
            return;
        }
        if self.macros.last.is_empty() {
            self.status = Some(Msg::StatusMacroSaveEmpty.into());
            return;
        }
        use crate::ui::actions::Cmd;
        use crate::ui::state::Dialog;
        self.show_dialog(
            Dialog::text(
                Msg::DlgSaveMacroTitle,
                Fmt::DlgSaveMacroBody {
                    steps: self.macros.last.len(),
                },
            )
            .with_input(Msg::DlgSaveMacroHint, Cmd::MacroSaveNamed)
            .primary(Msg::DlgSave, Some(Cmd::MacroSaveNamed))
            .button(Msg::CommonCancel, None),
        );
    }

    /// Macros → Save Macro… input submit: write to disk under the entered name.
    pub fn macro_save_named(&mut self) {
        let name = self
            .dialog_input()
            .trim()
            .trim_end_matches(".imacro")
            .to_string();
        let name = if name.is_empty() {
            format!("宏 {}", self.macro_files.len() + 1)
        } else {
            name
        };
        let steps = self.macros.last.clone();
        match save_macro(macros_dir().as_deref(), &name, &steps) {
            Ok(path) => {
                self.macro_files = list_scripts(macros_dir().as_deref());
                self.status = Some(
                    Fmt::StatusMacroSaved {
                        path: path.display().to_string(),
                    }
                    .into(),
                );
            }
            Err(e) => self.status = Some(Fmt::StatusMacroSaveFailed { err: e.to_string() }.into()),
        }
    }

    /// Macros → Run Macro…: list the macros on disk; tapping one replays it.
    pub fn macro_run_open(&mut self) {
        use crate::ui::actions::Cmd;
        use crate::ui::state::Dialog;
        self.macro_files = list_scripts(macros_dir().as_deref());
        if self.macro_files.is_empty() {
            self.status = Some(Msg::StatusMacroListEmpty.into());
            return;
        }
        let names = display_names(&self.macro_files);
        let dir = macros_dir()
            .map(|d| d.display().to_string())
            .unwrap_or_default();
        let list = names
            .iter()
            .enumerate()
            .map(|(i, n)| format!("{:>2}. {n}", i + 1))
            .collect::<Vec<_>>()
            .join("\n");
        let mut dlg = Dialog::text(
            Msg::DlgRunMacroTitle,
            Fmt::DlgRunMacroBody {
                total: names.len(),
                dir,
                list,
            },
        );
        // Cap of 8 buttons: more would not fit; the list itself is always complete
        for (i, n) in names.iter().take(8).enumerate() {
            // Macro names are user data, not translation copy: the raw passthrough key renders verbatim
            dlg = dlg.button(
                Fmt::CommonRaw { text: n.clone() },
                Some(Cmd::MacroPlaySaved(i)),
            );
        }
        dlg = dlg
            .button(Msg::MacroOpenDir, Some(Cmd::MacroDir))
            .button(Msg::CommonCancel, None);
        self.show_dialog(dlg);
    }

    /// Replay the `i`-th macro on disk (index matches the
    /// [`Self::macro_files`] snapshot).
    pub fn macro_play_saved(&mut self, i: usize) {
        let Some(path) = self.macro_files.get(i).cloned() else {
            self.status = Some(Msg::StatusMacroListStale.into());
            return;
        };
        match load_macro(&path) {
            Some(steps) => {
                let msg = self.macro_play_steps(steps);
                self.status = Some(match msg {
                    // Empty-macro notices pass through as-is; the success message
                    // carries the source path along.
                    i18n::Message::With(Fmt::StatusMacroPlayed { steps }) => {
                        Fmt::StatusMacroPlayedFile {
                            steps,
                            path: path.display().to_string(),
                        }
                        .into()
                    }
                    other => other,
                });
            }
            None => {
                self.status = Some(
                    Fmt::StatusMacroFileUnreadable {
                        path: path.display().to_string(),
                    }
                    .into(),
                );
            }
        }
    }

    /// The "open macro directory" action in the Macros → Save Macro dialog.
    pub fn macro_dir_reveal(&mut self) {
        let Some(dir) = macros_dir() else {
            self.status = Some(Msg::StatusNoConfigDir.into());
            return;
        };
        reveal_path_for(&dir, true, self);
    }

    /// Run → Run…: pop up the command-line input box.
    pub fn run_command_open(&mut self) {
        use crate::ui::actions::Cmd;
        use crate::ui::state::Dialog;
        // On reopen, the previous command becomes the hint text so editing a
        // few characters reruns it in one press
        let dlg = match self.run_recent.first() {
            Some(prev) => Dialog::text(Msg::DlgRun, Msg::DlgRunBody)
                .with_input(Fmt::CommonRaw { text: prev.clone() }, Cmd::RunExec),
            None => Dialog::text(Msg::DlgRun, Msg::DlgRunBody)
                .with_input(Msg::DlgRunPlaceholder, Cmd::RunExec),
        }
        .primary(Msg::DlgRun, Some(Cmd::RunExec))
        .button(Msg::CommonCancel, None);
        self.show_dialog(dlg);
    }

    /// Run → Run… input submit: expand variables, then hand off to the
    /// system shell.
    pub fn run_exec(&mut self) {
        let raw = self.dialog_input();
        let cmd = raw.trim();
        if cmd.is_empty() {
            self.status = Some(Msg::StatusRunEmpty.into());
            return;
        }
        let expanded = self.expand_run_vars(cmd);
        // Record into "recent runs" for one-press reruns from the menu
        // (newest first)
        self.run_recent.retain(|c| c != cmd);
        self.run_recent.insert(0, cmd.to_string());
        self.run_recent.truncate(10);
        self.spawn_shell(&expanded);
    }

    /// Run → the `i`-th entry of recent runs.
    pub fn run_again(&mut self, i: usize) {
        let Some(cmd) = self.run_recent.get(i).cloned() else {
            self.status = Some(Msg::StatusRunListStale.into());
            return;
        };
        let expanded = self.expand_run_vars(&cmd);
        self.spawn_shell(&expanded);
    }

    /// Substitute the `$(...)` variables the run dialog documents.
    pub(crate) fn expand_run_vars(&self, cmd: &str) -> String {
        let doc = self.active_doc();
        let path = doc.meta.path.clone();
        let full = path
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        let dir = path
            .as_ref()
            .and_then(|p| p.parent().map(|d| d.to_string_lossy().into_owned()))
            .unwrap_or_default();
        let file = doc.meta.display_name.clone();
        let stem = std::path::Path::new(&file)
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let ext = std::path::Path::new(&file)
            .extension()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        cmd.replace("$(FULL_CURRENT_PATH)", &full)
            .replace("$(CURRENT_DIRECTORY)", &dir)
            .replace("$(FILE_NAME)", &file)
            .replace("$(NAME)", &stem)
            .replace("$(EXT)", &ext)
    }

    /// Hand a command line to the platform shell (the `cmd` vs `sh` choice
    /// and the working-directory handling both live in
    /// `platform::spawn_shell`).
    fn spawn_shell(&mut self, expanded: &str) {
        let cwd = self
            .active_doc()
            .meta
            .path
            .as_ref()
            .and_then(|p| p.parent().map(|d| d.to_path_buf()));
        match platform::spawn_shell(expanded, cwd.as_deref()) {
            Ok(_) => {
                self.status = Some(
                    Fmt::StatusRunStarted {
                        cmd: expanded.to_string(),
                    }
                    .into(),
                );
            }
            Err(e) => self.status = Some(Fmt::StatusRunFailed { err: e.to_string() }.into()),
        }
    }
}

// `DialogInput` is only constructed here and in `state::dialogs`; the import
// above keeps the builder calls short.

#[cfg(test)]
mod tests {
    use super::*;

    /// Every command the recorder may capture survives the wire round trip —
    /// this is what stops a `Debug`-format rename from silently corrupting
    /// every saved macro.
    #[test]
    fn every_recordable_command_round_trips_through_the_wire() {
        let samples = [
            Cmd::New,
            Cmd::Save,
            Cmd::Undo,
            Cmd::BeginEndSelect,
            Cmd::BeginEndSelectColumn,
            Cmd::Case(CaseKind::Upper),
            Cmd::Case(CaseKind::HalfWidth),
            Cmd::SortLines(SortKey::DecimalComma, true),
            Cmd::TrimBoth,
            Cmd::RemoveEmptyLinesWithBlank,
            Cmd::BlockUncomment,
            Cmd::InsertLongDateTime,
            Cmd::FindNext,
            Cmd::WordWrap,
            Cmd::JsonlUnfoldLine,
            Cmd::SetEncoding("UTF-8 BOM"),
            Cmd::SetEncoding("GBK"),
            Cmd::SetNewline("CR"),
            Cmd::Lang("__auto"),
            Cmd::Lang("Plain Text"),
            Cmd::HideLines,
            Cmd::ShowHiddenLines,
            Cmd::MarkStyle(5, true),
            Cmd::MarkClear(false),
            Cmd::MarkClear(true),
            Cmd::TabNext,
            Cmd::About,
        ];
        for c in samples {
            let wire = cmd_to_wire(c)
                .unwrap_or_else(|| panic!("{c:?} has no wire form"))
                .clone();
            let back =
                cmd_from_wire(&wire).unwrap_or_else(|| panic!("wire {wire} does not parse back"));
            assert_eq!(back, c, "wire {wire} round-trip mismatch");
        }
    }

    /// Dialog/session commands must never be written to (or read from) disk.
    #[test]
    fn session_commands_never_hit_the_wire() {
        for c in [
            Cmd::RunExec,
            Cmd::MacroPlayback,
            Cmd::PreferencesClosed,
            Cmd::MacroSaveNamed,
            Cmd::RecoveryRestore,
            Cmd::RecoveryDiscard,
        ] {
            assert!(cmd_to_wire(c).is_none(), "{c:?} must not be serialised");
        }
    }

    /// An unknown / hostile wire string parses to `None`, never to a panic or
    /// to some arbitrary command.
    #[test]
    fn hostile_wire_strings_are_rejected() {
        for s in [
            "",
            "NotACommand",
            "OpenRecent:banana",
            "Find:2:0",
            "Find:0:1:9",
            "MarkStyle:9:1",
            "SortLines:Nope:1",
            "SetEncoding:KOI8-RU",
            "Lang:::x",
        ] {
            assert!(cmd_from_wire(s).is_none(), "{s:?} must not parse");
        }
    }

    /// A whole file round-trips: commands, typed text and the edit keys.
    #[test]
    fn a_macro_file_round_trips() {
        let dir = std::env::temp_dir().join(format!("infeditor-test-{}", std::process::id()));
        let steps = vec![
            MacroStep::Cmd(Cmd::DuplicateLine),
            MacroStep::Text("foo".into()),
            MacroStep::Backspace,
            MacroStep::Enter,
            MacroStep::Cmd(Cmd::Case(CaseKind::Upper)),
            // not on disk: dropped by the writer, not by the reader
            MacroStep::Cmd(Cmd::RunExec),
        ];
        let path = save_macro(Some(&dir), "测试宏", &steps).expect("save");
        assert_eq!(path.extension().and_then(|e| e.to_str()), Some(EXT));
        let back = load_macro(&path).expect("load");
        assert_eq!(back.len(), steps.len() - 1, "RunExec is dropped on write");
        assert_eq!(back[0], MacroStep::Cmd(Cmd::DuplicateLine));
        assert_eq!(back[1], MacroStep::Text("foo".into()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Names the filesystem rejects (or that would be hostile in a shell) are
    /// sanitised instead of failing the save.
    #[test]
    fn file_names_are_sanitised() {
        assert_eq!(sanitize_file_name("  "), "宏");
        assert_eq!(sanitize_file_name("a/b\\c:d"), "a_b_c_d");
        assert_eq!(sanitize_file_name("CON"), "_CON");
        assert_eq!(sanitize_file_name(".."), "宏");
        assert!(sanitize_file_name(&"长".repeat(200)).chars().count() <= 80);
    }

    /// A corrupt file yields `None` rather than panicking.
    #[test]
    fn a_corrupt_macro_file_is_rejected() {
        let dir = std::env::temp_dir().join(format!("infeditor-test-bad-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("bad.{EXT}"));
        std::fs::write(&path, "{ not json").unwrap();
        assert!(load_macro(&path).is_none());
        std::fs::write(&path, r#"{"format":"other","version":1,"steps":[]}"#).unwrap();
        assert!(load_macro(&path).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The `$(...)` run variables expand from the active document.
    #[test]
    fn run_variables_expand_from_the_document() {
        use crate::ui::state::EditorState;
        let mut st = EditorState::minimal();
        st.tabs[0].doc.meta.path = Some(std::path::PathBuf::from("C:/work/notes/report.md"));
        st.tabs[0].doc.meta.display_name = "report.md".into();
        assert_eq!(
            st.expand_run_vars("prog \"$(FULL_CURRENT_PATH)\" $(EXT)"),
            "prog \"C:/work/notes/report.md\" md"
        );
        assert_eq!(
            st.expand_run_vars("cd $(CURRENT_DIRECTORY) && ./x $(NAME)"),
            "cd C:/work/notes && ./x report"
        );
    }
}
