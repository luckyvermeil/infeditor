//! Persistence (JSON) of app settings and the recent-files list.
//!
//! # Why it is worth doing
//!
//! Settings like theme, font size and whitespace display were reset on every
//! launch, so users effectively reconfigured the editor every day — the most
//! visible gap in "finishedness". This module writes all settings into one
//! JSON file and reads it back at startup.
//!
//! # Design
//!
//! * Storage location: the `ProjectDirs` config directory (Windows =
//!   `%APPDATA%\InfEditor`), fully separate from user documents;
//! * Reads and writes are both **best-effort**: full disk, permission errors
//!   and corrupt JSON all fall back to defaults; the editor must never fail
//!   to start because of a settings file — a settings file has no right to
//!   become a startup failure;
//! * Disk writes happen only when settings actually change (`persist_settings`
//!   fires at the tail of command dispatch, by command type), avoiding
//!   per-frame writes; the file is tiny, one write takes microseconds.
//!
//! `EditorState::minimal()` (the test entry point) does **not** load settings,
//! keeping unit tests unaffected by the local machine's configuration; only
//! the real-startup `EditorState::default()` reads it once.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::ui::state::{CaretStyle, EditorState};
use crate::ui::Fmt;
use i18n::Message as StatusLine;

/// A persistable snapshot of the app settings.
///
/// Fields are **flat scalars**, not nested structures: backwards compatibility
/// is simplest — new fields get `#[serde(default)]` and old files read as
/// before; renaming/removing fields does not affect the rest.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct AppSettings {
    #[serde(default = "default_true")]
    pub highlight_on: bool,
    #[serde(default)]
    pub highlight_all: bool,
    #[serde(default)]
    pub dark: bool,
    #[serde(default)]
    pub wrap: bool,
    #[serde(default)]
    pub show_eol: bool,
    #[serde(default)]
    pub show_ws: bool,
    /// Code minimap on the right
    #[serde(default = "default_true")]
    pub minimap: bool,
    /// JSON Lines preview (click the line-start marker to expand the JSON
    /// tree in place).
    ///
    /// `Option`: `None` = the user never chose, use the per-document-type
    /// default (on for JSON Lines documents); `Some(v)` = the user chose
    /// explicitly, and JSON Lines documents always follow it afterwards. Old
    /// files with `true/false` still deserialize as `Some`.
    #[serde(default)]
    pub jsonl: Option<bool>,
    #[serde(default = "default_true")]
    pub caret_blink: bool,
    /// 0 line / 1 block / 2 underline
    #[serde(default)]
    pub caret_style: u8,
    #[serde(default = "default_font")]
    pub font_size: f32,
    #[serde(default)]
    pub vim: bool,
    /// Auto-close brackets/quotes while typing
    #[serde(default = "default_true")]
    pub auto_close: bool,
    /// Recently opened files, newest last (same order as `EditorState::recent`)
    #[serde(default)]
    pub recent: Vec<String>,
    /// Find popup geometry (screen logical coords x, y, w, h), None = default
    /// centered at startup
    #[serde(default)]
    pub find_popup: Option<[f32; 4]>,
    /// Preferences popup geometry (screen logical coords x, y, w, h)
    #[serde(default)]
    pub prefs_popup: Option<[f32; 4]>,
    /// Last opened workspace folder (folder sidebar); None = never opened
    #[serde(default)]
    pub ws_path: Option<String>,
    /// Folder sidebar visibility (meaningless without a workspace)
    #[serde(default)]
    pub ws_show: bool,
    /// UI language, stored as `Locale::tag()` (`"zh-CN"` / `"en-US"`).
    ///
    /// `None` = never chosen: the startup probe runs once (see [`resolve_locale`]).
    /// **Never a numeric index** — adding an enum variant would repoint an old
    /// file's `1` at another language.
    #[serde(default)]
    pub locale: Option<String>,
}

fn default_true() -> bool {
    true
}
fn default_font() -> f32 {
    crate::ui::state::DEFAULT_FONT_SIZE
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            highlight_on: true,
            highlight_all: false,
            dark: false,
            wrap: false,
            show_eol: false,
            show_ws: false,
            minimap: true,
            jsonl: None,
            caret_blink: true,
            caret_style: 0,
            font_size: crate::ui::state::DEFAULT_FONT_SIZE,
            vim: false,
            auto_close: true,
            recent: Vec::new(),
            find_popup: None,
            prefs_popup: None,
            ws_path: None,
            ws_show: false,
            locale: None,
        }
    }
}

impl AppSettings {
    /// Capture current settings from the editor state.
    pub fn capture(st: &EditorState) -> Self {
        Self {
            highlight_on: st.highlight_on,
            highlight_all: st.highlight_all,
            dark: st.dark,
            wrap: st.wrap,
            show_eol: st.show_eol,
            show_ws: st.show_ws,
            minimap: st.minimap,
            // Use `get` instead of indexing: capture is called at the tail of
            // command dispatch; if some future path leaves tabs briefly empty
            // (the mid-state of closing the last tab), a bare index would
            // panic the whole window, while "no active tab" already has a
            // sensible answer here (None).
            jsonl: match st.tabs.get(st.active) {
                Some(t) if t.spec.name == "JSON Lines" => Some(t.jsonl.enabled),
                _ => None,
            },
            caret_blink: st.caret_blink,
            caret_style: match st.caret_style {
                CaretStyle::Line => 0,
                CaretStyle::Block => 1,
                CaretStyle::Underline => 2,
            },
            font_size: st.font_size,
            vim: st.vim.enabled,
            auto_close: st.auto_close,
            recent: st.recent.iter().cloned().collect(),
            find_popup: st.find.popup_rect,
            prefs_popup: st.prefs.rect,
            ws_path: st
                .ws_root
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned()),
            ws_show: st.ws_show,
            locale: Some(st.locale.tag().to_string()),
        }
    }

    /// Apply to the editor state.
    ///
    /// A font-size change requires rebuilding metrics (line height/char width
    /// follow the font size); same path as `zoom`.
    pub fn apply(&self, st: &mut EditorState) {
        st.highlight_on = self.highlight_on;
        st.highlight_all = self.highlight_all;
        st.dark = self.dark;
        st.wrap = self.wrap;
        st.show_eol = self.show_eol;
        st.show_ws = self.show_ws;
        st.minimap = self.minimap;
        // jsonl is the "expand the preview by default for JSON Lines documents"
        // preference and only means something for JSON Lines documents. At
        // startup the active tab is a fresh unnamed Plain Text tab; applying the
        // preference directly would turn that tab into a JSONL preview view —
        // so only record it here and let [`EditorState::adopt_view_prefs`]
        // apply it when a JSON Lines document is actually opened.
        st.jsonl_pref = self.jsonl;
        st.caret_blink = self.caret_blink;
        st.caret_style = match self.caret_style {
            1 => CaretStyle::Block,
            2 => CaretStyle::Underline,
            _ => CaretStyle::Line,
        };
        st.vim.enabled = self.vim;
        st.auto_close = self.auto_close;
        st.find.popup_rect = self.find_popup;
        st.prefs.rect = self.prefs_popup;
        // Workspace: set visibility first, then restore the directory tree
        // (`ws_restore` does not touch ws_show — a sidebar the user collapsed
        // must not be force-expanded at startup). Silently dropped when the
        // folder no longer exists.
        st.ws_show = self.ws_show;
        if let Some(p) = &self.ws_path {
            st.ws_restore(PathBuf::from(p));
        }
        if (st.font_size - self.font_size).abs() > f32::EPSILON
            && crate::ui::state::font_size_ok(self.font_size)
        {
            st.font_size = self.font_size;
            st.metrics = crate::ui::metrics::metrics_for_family("Consolas", st.font_size);
        }
        // A recognized tag wins, otherwise probe the environment. The process
        // mirror is synced once here; it serves libraries without an
        // `EditorState`, and `set_locale` is the runtime switch.
        let locale = resolve_locale(self.locale.as_deref());
        st.locale = locale;
        i18n::set_current(locale);
        st.rerender_derived_names();
        // Merge recent files into the existing list (keeping "newest last");
        // existing entries move to the front.
        for p in &self.recent {
            st.push_recent(p.clone());
        }
    }
}

/// Startup language: explicit setting → `INFEDITOR_LANG` → system UI language →
/// default.
///
/// The order is the priority:
///
/// * an explicit choice outranks everything — a user on an English system may
///   still want a Chinese UI;
/// * the environment variable covers setups with no settings UI (automation,
///   portable use, synced config);
/// * the system language is only a first-launch guess, and an unrecognized one
///   falls through to the default rather than guessing wrong.
pub fn resolve_locale(persisted: Option<&str>) -> i18n::Locale {
    if let Some(loc) = persisted.and_then(i18n::Locale::from_tag) {
        return loc;
    }
    if let Some(loc) = std::env::var("INFEDITOR_LANG")
        .ok()
        .as_deref()
        .and_then(i18n::Locale::from_tag)
    {
        return loc;
    }
    system_locale_hint().unwrap_or(i18n::Locale::DEFAULT)
}

/// System UI language. Only used for the first-launch guess; `None` if unknown.
/// The OS query itself lives in `platform::locale_tag` (Win32 LANGID mapping /
/// POSIX `LC_ALL`-`LANG`).
fn system_locale_hint() -> Option<i18n::Locale> {
    platform::locale_tag().and_then(|ref tag| i18n::Locale::from_tag(tag))
}

/// Settings file path; `None` when the user directory is unavailable (both
/// read and write skip).
///
/// Must go through [`crate::ui::macros_io::config_dir`] (honors the
/// `INFEDITOR_CONFIG_DIR` override + always `None` under `cfg!(test)`):
/// going straight to `ProjectDirs` would let tests that write settings
/// persist test state into the user's real
/// `%APPDATA%\InfEditor\settings.json`.
pub fn settings_path() -> Option<PathBuf> {
    super::macros_io::config_dir().map(|d| d.join("settings.json"))
}

/// Load settings; silently use defaults when the file does not exist.
pub fn load() -> AppSettings {
    load_with_status().0
}

/// Peek only the dark flag (used by the startup skeleton screen).
///
/// The main window's **clear color and skeleton screen** must both know
/// dark/light before the full load (font metrics / restore scan / command-line
/// files, see the skeleton boot sequence) — otherwise a dark-theme user's
/// first screen is white. This does one small-file read + parse with no
/// restore/persist side effects; the full load is still done by
/// [`load_with_status`] (`EditorState::default`) after the skeleton frame.
pub fn peek_dark() -> bool {
    settings_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|t| serde_json::from_str::<AppSettings>(&t).ok())
        .map(|s| s.dark)
        .unwrap_or(false)
}

/// Load settings, renaming the original file to preserve it when corrupt.
///
/// The second return value is a status-bar hint: a corrupt settings file is
/// **no longer silently wiped**. The old implementation did
/// `unwrap_or_default()` directly, so the next settings command's disk write
/// permanently lost all user preferences without a trace. Here the file is
/// first backed up as `settings.corrupt-<pid>-<ms>.json`, and the original is
/// rebuilt by the next `save`; even if the backup fails, the hint is still
/// returned.
pub fn load_with_status() -> (AppSettings, Option<StatusLine>) {
    let Some(path) = settings_path() else {
        return (AppSettings::default(), None);
    };
    load_from_path(&path)
}

fn load_from_path(path: &Path) -> (AppSettings, Option<StatusLine>) {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return (AppSettings::default(), None)
        }
        Err(e) => {
            return (
                AppSettings::default(),
                Some(Fmt::StatusSettingsUnreadable { err: e.to_string() }.into()),
            )
        }
    };
    match serde_json::from_str::<AppSettings>(&text) {
        Ok(s) => (s, None),
        Err(e) => {
            let backup = backup_corrupt(path);
            let msg = match &backup {
                Some(p) => Fmt::StatusSettingsCorruptBackup {
                    err: e.to_string(),
                    path: p.display().to_string(),
                }
                .into(),
                None => Fmt::StatusSettingsCorrupt { err: e.to_string() }.into(),
            };
            (AppSettings::default(), Some(msg))
        }
    }
}

/// Rename the corrupt settings file to preserve it; returns the backup path,
/// or `None` on failure.
fn backup_corrupt(path: &Path) -> Option<PathBuf> {
    use std::time::{SystemTime, UNIX_EPOCH};
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let backup = path.with_file_name(format!(
        "settings.corrupt-{}-{}.json",
        std::process::id(),
        ms
    ));
    std::fs::rename(path, &backup).ok().map(|_| backup)
}

/// Write to disk. Best-effort: failures are silent (settings are not worth
/// interrupting the user).
pub fn save(s: &AppSettings) {
    // A test binary must never touch the user's real settings file. `run()`
    // persists after every settings command, so the headless command test —
    // which runs each one exactly once against a fresh `minimal()` state —
    // used to write a file where *every* toggle sits at its flipped value
    // (highlight off, whitespace glyphs on, wrap on, ...). That is not a
    // setting the user ever chose; it is an artefact of running the tests.
    if cfg!(test) {
        return;
    }
    let Some(path) = settings_path() else { return };
    let Some(parent) = path.parent() else { return };
    if std::fs::create_dir_all(parent).is_err() {
        return;
    }
    if let Ok(json) = serde_json::to_string_pretty(s) {
        // atomic write: a process killed mid-write must not leave a truncated
        // settings.json behind (which would reset every user preference)
        //
        // The temp name carries the pid: when two instances save at the same
        // time and share `settings.json.tmp`, A's tmp is renamed away by B
        // first, so A's rename fails and deletes B's freshly written file —
        // both sides lose their settings (B11).
        let tmp = path.with_extension(format!("json.{}.tmp", std::process::id()));
        // Clean the temp file on *either* failure: the old `&&` short-circuited
        // on a failed write and left a truncated `settings.json.<pid>.tmp`
        // behind, the very artifact this atomic write is meant to avoid.
        if let Err(_e) = std::fs::write(&tmp, &json).and_then(|()| std::fs::rename(&tmp, &path)) {
            let _ = std::fs::remove_file(&tmp);
        }
    }
}

impl EditorState {
    /// Read and apply settings at startup (called once, only in `default()`).
    pub fn load_settings(&mut self) {
        let (s, warning) = load_with_status();
        s.apply(self);
        self.status = warning;
    }

    /// Write the current settings to disk.
    ///
    /// Triggered by command dispatch after settings-type commands (see
    /// `actions::run`); each write is a full rewrite, because the file is tiny
    /// and tracking "which fields changed" is not worth it.
    pub fn persist_settings(&mut self) {
        save(&AppSettings::capture(self));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buffer::TextDocument;
    use crate::ui::state::Caret;

    /// Serialize round trip: capture → save → read → apply; fields match one
    /// by one.
    #[test]
    fn settings_round_trip_through_json() {
        let mut st = EditorState::minimal();
        st.dark = true;
        st.font_size = 18.0;
        st.caret_style = CaretStyle::Underline;
        st.show_ws = true;
        st.vim.enabled = true;
        st.push_recent("C:\\a.txt".into());
        st.push_recent("C:\\b.txt".into());

        let s = AppSettings::capture(&st);
        let json = serde_json::to_string(&s).unwrap();
        let back: AppSettings = serde_json::from_str(&json).unwrap();

        let mut st2 = EditorState::minimal();
        back.apply(&mut st2);
        assert!(st2.dark);
        assert_eq!(st2.font_size, 18.0);
        assert_eq!(st2.caret_style, CaretStyle::Underline);
        assert!(st2.show_ws);
        assert!(st2.vim.enabled);
        assert_eq!(
            st2.recent.iter().cloned().collect::<Vec<_>>(),
            vec!["C:\\a.txt".to_string(), "C:\\b.txt".to_string()]
        );
    }

    /// The JSON Lines preview preference must genuinely take effect: the old impl
    /// applied it at startup to a blank Plain Text tab (blocked by the type guard),
    /// so opening a .jsonl afterwards always used the default — the setting was
    /// inert. The preference now lives on the state and is applied by document
    /// type when a document lands.
    #[test]
    fn jsonl_preference_applies_to_jsonl_documents_only() {
        let mut st = EditorState::minimal();
        let spec =
            crate::highlight::for_path_and_first_line(Some(std::path::Path::new("x.jsonl")), "");
        assert_eq!(spec.name, "JSON Lines");
        st.tabs[0] = crate::ui::state::Tab::with_spec(TextDocument::empty(), spec);
        assert!(
            st.tabs[0].jsonl.enabled,
            "JSON Lines documents enable preview by default"
        );

        // Plain Text tag unaffected
        st.jsonl_pref = Some(false);
        st.adopt_view_prefs(0);
        assert!(!st.tabs[0].jsonl.enabled);

        // After the user explicitly enables it, capture records Some(true);
        // new documents follow it after apply
        st.tabs[0].jsonl.enabled = true;
        let saved = AppSettings::capture(&st);
        assert_eq!(saved.jsonl, Some(true));
        let mut st2 = EditorState::minimal();
        saved.apply(&mut st2);
        assert_eq!(st2.jsonl_pref, Some(true));

        // Before opening (blank Plain Text tag) apply must not change Plain Text to JSONL view
        assert!(!st2.tabs[0].jsonl.enabled);
    }

    /// Workspace fields round trip: capture → apply restores the directory
    /// tree; a collapsed sidebar stays collapsed; a vanished folder is
    /// silently dropped.
    #[test]
    fn workspace_fields_round_trip_and_restore() {
        let d = std::env::temp_dir().join(format!("infeditor-ws-settings-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("a.txt"), "a").unwrap();

        let mut st = EditorState::minimal();
        st.ws_enter(d.clone());
        let s = AppSettings::capture(&st);
        assert_eq!(s.ws_path.as_deref(), Some(d.to_string_lossy().as_ref()));
        assert!(s.ws_show);

        // Normal restore: tree ready, sidebar expanded
        let mut st2 = EditorState::minimal();
        s.apply(&mut st2);
        assert!(st2.ws_root.is_some());
        assert!(st2.ws_show);
        assert_eq!(st2.ws_rows.len(), 1);
        assert_eq!(st2.ws_rows[0].name, "a.txt");

        // The user collapsed the sidebar: restore keeps it collapsed
        let mut st3 = EditorState::minimal();
        let s_hidden = AppSettings {
            ws_show: false,
            ..s.clone()
        };
        s_hidden.apply(&mut st3);
        assert!(st3.ws_root.is_some());
        assert!(!st3.ws_show);

        // Folder gone: silently dropped, no root left behind
        let mut st4 = EditorState::minimal();
        let s_gone = AppSettings {
            ws_path: Some("Z:/no/such/dir".into()),
            ws_show: true,
            ..AppSettings::default()
        };
        s_gone.apply(&mut st4);
        assert!(st4.ws_root.is_none());
        assert!(!st4.ws_show);

        let _ = std::fs::remove_dir_all(&d);
    }

    /// A corrupt settings file must fall back to defaults — a settings file
    /// has no right to keep the editor from starting; but the original file
    /// must be backed up first, and the status bar can tell the user what
    /// happened.
    #[test]
    fn a_corrupt_file_is_backed_up_and_reported() {
        let dir = std::env::temp_dir().join(format!(
            "infeditor-settings-corrupt-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.json");
        std::fs::write(&path, b"{ not json").unwrap();

        let (s, warning) = load_from_path(&path);
        assert_eq!(s, AppSettings::default());
        assert!(s.highlight_on, "highlight defaults to on");
        assert_eq!(s.font_size, crate::ui::state::DEFAULT_FONT_SIZE);
        let warning = warning.expect("corrupt file must produce a warning");
        let warning = i18n::current().tr(warning).into_owned();
        assert!(warning.contains("已备份"), "{warning}");
        assert!(
            !path.exists(),
            "corrupt file should be renamed, not left in place to be overwritten"
        );
        let backup = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .find(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with("settings.corrupt-")
            })
            .expect("backup file should exist");
        assert_eq!(std::fs::read(backup.path()).unwrap(), b"{ not json");

        // A missing file is a normal state: defaults, no warning.
        let (missing, warning) = load_from_path(&dir.join("missing.json"));
        assert_eq!(missing, AppSettings::default());
        assert!(warning.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Old settings files missing fields: `#[serde(default)]` must fill them
    /// in without error.
    #[test]
    fn a_partial_file_keeps_its_fields_and_defaults_the_rest() {
        let s: AppSettings = serde_json::from_str(r#"{"dark": true}"#).unwrap();
        assert!(s.dark);
        assert!(s.highlight_on, "missing fields fall back to defaults");
        assert!(s.recent.is_empty());
    }

    /// File values with a font size below the readable floor must be rejected,
    /// so a broken config cannot shrink the UI into an unreadable mess.
    #[test]
    fn an_absurd_font_size_is_ignored_on_apply() {
        let mut st = EditorState::minimal();
        let s = AppSettings {
            font_size: 1.0,
            ..AppSettings::default()
        };
        s.apply(&mut st);
        assert_ne!(st.font_size, 1.0);
    }

    /// Recent files dedup + cap, and apply merges the persisted list back.
    #[test]
    fn recent_files_merge_without_duplicates() {
        let mut st = EditorState::minimal();
        st.tabs[0].doc = TextDocument::from("x");
        st.tabs[0].caret = Caret::default();
        st.push_recent("C:\\a.txt".into());
        st.push_recent("C:\\a.txt".into());
        assert_eq!(st.recent.len(), 1, "duplicate paths are recorded only once");
        let s = AppSettings {
            recent: vec!["C:\\b.txt".into()],
            ..Default::default()
        };
        s.apply(&mut st);
        let got: Vec<&String> = st.recent.iter().collect();
        assert_eq!(got, [&"C:\\a.txt".to_string(), &"C:\\b.txt".to_string()]);
    }
}
