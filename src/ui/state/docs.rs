//! Document & tab management: open/save/reload, recents, encoding/newline
//! choice and the file-level menu actions (Save As / Quit / About / Help).

use std::path::PathBuf;

use crate::buffer::TextDocument;
use crate::highlight::LangSpec;
use crate::ui::{Fmt, Msg};

use super::{Dialog, EditorState, OpenIntent, Tab, WakeFn};

/// Status line for a finished save.
///
/// `lossy` = the target encoding had to replace characters while writing;
/// `lossy_open` = the document was opened with replacement characters, so this
/// write overwrites bytes that were never decodable. `to_new_path` selects the
/// "saved to <name>" wording used by Save As. Both save paths go through here:
/// Save As used to drop the `SaveReport` on the floor and report a plain
/// success, so a lossy write looked clean.
fn save_status(
    name: String,
    bytes: u64,
    lossy: bool,
    lossy_open: bool,
    to_new_path: bool,
) -> i18n::Message {
    match (lossy, lossy_open) {
        (false, false) if to_new_path => Fmt::StatusSavedTo { name }.into(),
        (false, false) => Fmt::StatusSaved { name, bytes }.into(),
        (true, false) => Fmt::StatusSavedLossy { name, bytes }.into(),
        (false, true) => Fmt::StatusSavedLossyOpen { name, bytes }.into(),
        (true, true) => Fmt::StatusSavedLossyBoth { name, bytes }.into(),
    }
}

impl EditorState {
    // ---- documents --------------------------------------------------------

    /// Open a file as a tab (or focus it when already open).
    ///
    /// Synchronous; used for the command line, where there is nothing to stay
    /// responsive for. Everything interactive goes through
    /// [`Self::open_path_async`].
    pub fn open_path(&mut self, path: PathBuf) {
        if let Some(i) = self
            .tabs
            .iter()
            .position(|t| same_path(t.doc.meta.path.as_deref(), path.as_path()))
        {
            self.set_active_tab(i);
            return;
        }
        match crate::io::open_file(&path, |_, _| {}) {
            Ok(opened) => self.insert_opened(opened, &path, None),
            Err(e) => {
                self.status = Some(
                    Fmt::StatusOpenFailed {
                        path: path.display().to_string(),
                        err: e.to_string(),
                    }
                    .into(),
                );
            }
        }
    }

    /// Give the state a way to wake the window. Called once from the app root;
    /// without it, opens fall back to running inline (which is what the tests
    /// rely on). Any file named on the command line is started here, so the
    /// first frame is painted before a huge file is read.
    pub fn set_waker(&mut self, wake: WakeFn) {
        self.wake = Some(wake.clone());
        // Single-instance hand-off (`ui::single`): the watcher thread needs a
        // way to poke the window, and this is the one place where that exists.
        // Idempotent — `set_waker` is called twice (skeleton hook + boot swap).
        crate::ui::single::spawn_watcher(wake);
        self.flush_startup_paths();
    }

    pub fn new_doc(&mut self) {
        let mut doc = TextDocument::empty();
        let n = self.tabs.len() + 1;
        doc.meta.display_name = self.tr(i18n::Fmt::DocUntitledN { n }).into_owned();
        self.tabs.push(Tab::with_spec(doc, LangSpec::plain()));
        self.set_active_tab(self.tabs.len() - 1);
        self.scroll_top = 0.0;
        self.scroll_left = 0.0;
    }

    /// Switch the active tab.
    ///
    /// The **only** command/menu-driven write point for `active` in the whole
    /// project (the others are `activate_tab_clean` for rebuilt tab
    /// sets and `remove_tab`'s post-removal clamp, which immediately
    /// calls `guard_seed` itself), so both invariants live here:
    ///
    /// * Cross-tab content pollution: before switching, flush the old tab's
    ///   wys edits back into the piece table; after switching, validate/reseed
    ///   the global wys singleton — any content mismatch (a stale singleton
    ///   left over across tabs) triggers a reseed, so A's content can never
    ///   end up in B.
    /// * `active` must stay within `0..tabs.len()`: `tab()`/`tab_mut()` are on
    ///   the hot path of every frame render and every keypress, and use bare
    ///   indexing (out of range panics the whole window). Menus, restore logs
    ///   and background opens all arrive with "last frame's" index, so out of
    ///   range is the norm, not the exception.
    ///
    /// This function used to live in `commands::markdown` (`md_wys` was its
    /// first caller), which put the core tab-switch entry on the wrong layer
    /// of the file tree and made the markdown command module a mandatory
    /// dependency of every tab switch.
    pub fn set_active_tab(&mut self, i: usize) {
        if i >= self.tabs.len() {
            return;
        }
        crate::ui::components::mdws::flush_pending(self);
        self.activate_tab_clean(i);
    }

    /// Switch to tab `i` **without** flushing the wys singleton.
    ///
    /// Callers are paths that have just rebuilt the tab contents (session
    /// restore, in-place reload): the singleton still holds the pre-rebuild
    /// document, and flushing it back would overwrite what was just loaded.
    /// The pending flag is dropped and [`guard_seed`](crate::ui::components::mdws::guard_seed)
    /// re-seeds from the new document instead.
    pub(crate) fn activate_tab_clean(&mut self, i: usize) {
        if i >= self.tabs.len() {
            return;
        }
        crate::ui::components::mdws::discard_pending();
        self.active = i;
        // In-progress editing gestures belong to the document that started
        // them: a column-selection anchor is a raw byte offset with no doc id,
        // so carrying it into another tab painted a rectangle at a random
        // position and the next column edit rewrote that file. Scroll offset
        // and drag state are equally per-document. `activate_next_tab` used to
        // do this by hand; the tab-bar click path (`tabs.rs`) did not.
        self.scroll_top = 0.0;
        self.scroll_left = 0.0;
        self.sel_anchor = None;
        self.drag = false;
        self.col_anchor = None;
        self.sb_end();
        self.mm_end();
        crate::ui::components::mdws::guard_seed(self);
        self.normalize_side_panel();
    }

    /// Drop every auxiliary map entry keyed by a document id once that
    /// document is gone (closed / replaced by a reload).
    ///
    /// Without this, bookmarks, mtime baselines, highlight marks, hidden-line
    /// sets and recovery generations accumulated for the whole session — the
    /// maps are never consulted again because document ids are unique, so the
    /// only symptom was unbounded growth.
    pub(super) fn forget_doc(&mut self, id: u64) {
        self.bookmarks.remove(&id);
        self.mtimes.remove(&id);
        self.hl_marks.remove(&id);
        self.hidden.remove(&id);
        self.recovery_backup_gens.remove(&id);
    }

    /// The outline serves Markdown only: when the current document doesn't
    /// support it, hand the side panel back to the explorer (collapse it when
    /// there is no workspace) instead of leaving an "outline is
    /// Markdown-only" empty panel.
    ///
    /// Runs after every tab switch and close — both paths can change the
    /// active document's type.
    fn normalize_side_panel(&mut self) {
        if self.side_panel != crate::ui::state::SidePanel::Outline
            || !self.ws_show
            || self.outline_supported()
        {
            return;
        }
        if self.ws_root.is_some() {
            self.side_panel = crate::ui::state::SidePanel::Workspace;
        } else {
            self.ws_show = false;
        }
        self.relayout();
    }

    /// Apply the persisted "per document type" view preferences to a tab that
    /// was just opened/restored.
    ///
    /// Currently only JSON Lines preview: the preference cannot go into
    /// `Tab::with_spec` (that is pure per-tab initialization with no access
    /// to global settings), nor be applied to a blank tab at startup (will
    /// change Plain Text tag to JSONL view). So it is unified at the three
    /// entry points where a document actually lands: open, reload, restore
    /// session.
    pub fn adopt_view_prefs(&mut self, idx: usize) {
        let pref = self.jsonl_pref;
        let Some(tab) = self.tabs.get_mut(idx) else {
            return;
        };
        if tab.spec.name == "JSON Lines" {
            if let Some(pref) = pref {
                tab.jsonl.enabled = pref;
            }
        }
    }

    /// Close tab `i`. Returns false when cancelled by the user.
    ///
    /// A dirty document asks with the app's **own** modal dialog (two-phase):
    /// a native `rfd::MessageDialog` never becomes visible on top of the
    /// frameless blitz window — the editor would look hung with no way to
    /// answer (same reason `request_quit` is self-drawn). The answer arrives
    /// as [`Cmd::CloseSave`](crate::ui::actions::Cmd::CloseSave) /
    /// [`Cmd::CloseDiscard`](crate::ui::actions::Cmd::CloseDiscard) /
    /// [`Cmd::CloseCancel`](crate::ui::actions::Cmd::CloseCancel)
    /// and completes the close in [`Self::finish_close`].
    pub fn close_doc(&mut self, i: usize) -> bool {
        if i >= self.tabs.len() {
            return true;
        }
        // wys edits pending sync must go into the piece table first: the
        // dirty check / save prompt reads it
        crate::ui::components::mdws::flush_pending(self);
        if self.tabs[i].doc.is_dirty() {
            // Store the **doc id**, not the index: while the confirmation
            // dialog is pending, a finished background open can trigger
            // drop_scratch_tab, removing tab 0 and shifting every index
            // left — saving/closing by the old index would touch a different
            // tab (the just-opened clean document gets saved and closed, the
            // dirty one stays on screen). finish_close re-locates by id.
            self.pending_close = Some(self.tabs[i].doc.id);
            use crate::ui::actions::Cmd;
            self.show_dialog(
                Dialog::text(
                    Msg::DlgCloseTitle,
                    Fmt::DlgCloseBody {
                        name: self.tabs[i].doc.meta.display_name.clone(),
                    },
                )
                .primary(Msg::DlgSave, Some(Cmd::CloseSave))
                .button(Msg::DlgCloseDontSave, Some(Cmd::CloseDiscard))
                .button(Msg::CommonCancel, Some(Cmd::CloseCancel))
                .confirm(),
            );
            return false;
        }
        self.remove_tab(i);
        true
    }

    /// The removal half of [`Self::close_doc`] — runs once the (possibly
    /// dialog-mediated) decision to close has been made.
    fn remove_tab(&mut self, i: usize) {
        if i >= self.tabs.len() {
            return;
        }
        let closed = self.tabs[i].doc.id;
        self.tabs.remove(i);
        self.forget_doc(closed);
        if i == self.active {
            self.external_hint = false;
        }
        if self.tabs.is_empty() {
            self.new_doc();
        }
        // Closing a tab to the *left* of the active one shifts every index
        // down by one. The upper clamp alone left `active` pointing at the tab
        // after the one being edited, so the next keystrokes — and the next
        // save — went to a different file.
        if i < self.active {
            self.active -= 1;
        }
        if self.active >= self.tabs.len() {
            self.active = self.tabs.len() - 1;
        }
        // The focused tab may have wys open — the global singleton still
        // holds the closed tab's content; reseed on any content mismatch
        // (no flush: close_doc/finish_close already flushed).
        crate::ui::components::mdws::guard_seed(self);
        // The newly focused tab must not inherit the closed tab's scroll
        // position or in-progress column selection
        self.scroll_top = 0.0;
        self.scroll_left = 0.0;
        self.col_anchor = None;
        self.sel_anchor = None;
        self.normalize_side_panel();
    }

    /// Answer of the close-document confirmation (`save`: `Some(true)` =
    /// save, `Some(false)` = don't save, `None` = cancel). Continues a
    /// close-all sweep when one is in progress.
    pub fn finish_close(&mut self, save: Option<bool>) {
        let Some(id) = self.pending_close.take() else {
            return;
        };
        self.close_dialog();
        // Re-locate by the doc id captured at confirmation time: tabs may
        // have been moved/removed in between (scratch tab recycling after a
        // background open finishes, macros, multiple instances, ...). If not
        // found, stop silently — never act on a stale index against another
        // tab.
        let Some(i) = self.tabs.iter().position(|t| t.doc.id == id) else {
            self.close_all_pending = false;
            return;
        };
        match save {
            None => {
                self.close_all_pending = false;
                return;
            }
            Some(true) => {
                if !self.save_doc(i) {
                    // Save was cancelled or failed: keep the tab, abort the sweep
                    self.close_all_pending = false;
                    return;
                }
            }
            Some(false) => {}
        }
        self.remove_tab(i);
        if self.close_all_pending {
            self.close_all_sweep();
        }
    }

    /// Close all: close every tab, prompting once per dirty document.
    pub fn close_all_sweep(&mut self) {
        self.close_all_pending = true;
        let mut i = self.tabs.len();
        while i > 0 {
            i -= 1;
            if !self.close_doc(i) {
                return; // confirmation pending; finish_close continues the sweep
            }
        }
        self.close_all_pending = false;
    }

    /// Save tab `i` (asks for a path when the document is untitled).
    pub fn save_doc(&mut self, i: usize) -> bool {
        if i >= self.tabs.len() {
            return false;
        }
        // Only the path *resolution* differs between Save and Save As; the write
        // itself goes through `save_doc_to`, the single tail. They used to be two
        // separate copies, and Save's copy recorded `meta.path` without
        // `meta.display_name` — so a document saved from "Untitled" kept showing
        // that placeholder in the tab and the window title while the file on disk
        // already had its real name (Save As, which had the other copy, was fine).
        let (path, new_path) = match self.tabs[i].doc.meta.path.clone() {
            Some(p) => (p, false),
            None => {
                let suggested = self.tabs[i].doc.meta.display_name.clone();
                match self.pick_save_path(&suggested) {
                    Some(p) => (p, true),
                    None => return false,
                }
            }
        };
        self.save_doc_to(i, path, new_path)
    }

    pub fn save_active(&mut self) {
        let i = self.active;
        self.save_doc(i);
    }

    /// Ask the user for a target path (`None` = cancelled).
    ///
    /// One dialog for both entry points that need a name — Save on an untitled
    /// document and Save As — so the title and the suggested file name cannot
    /// drift apart. Split out of [`Self::save_doc`] for the state-layer tests: a
    /// native modal cannot be answered headlessly, so `test_dialog` queues the
    /// answer instead.
    fn pick_save_path(&self, suggested: &str) -> Option<PathBuf> {
        #[cfg(test)]
        if let Some(answer) = test_dialog::take_save_answer() {
            return answer;
        }
        // Owner decoration lives in `winshell::dialog::owned` (only
        // Windows has owner semantics; other platforms return as-is).
        winshell::dialog::owned(
            rfd::FileDialog::new()
                .set_title(self.tr(Msg::DlgSaveAsTitle))
                .set_file_name(suggested),
        )
        .save_file()
    }

    pub fn open_dialog(&mut self) {
        let dlg = winshell::dialog::owned(
            rfd::FileDialog::new().set_title(self.tr(Msg::DlgOpenFileTitle)),
        );
        if let Some(p) = dlg.pick_file() {
            self.open_path_async(p);
        }
    }

    pub fn push_recent(&mut self, path: String) {
        self.recent.retain(|p| p != &path);
        if self.recent.len() >= 10 {
            self.recent.pop_front();
        }
        self.recent.push_back(path);
    }

    /// Open entry `i` of "File → Recent Files".
    ///
    /// Out of range (the list changed) is the norm, not the exception — the
    /// menu renders last frame's list — so silently ignore with a status
    /// hint; never panic.
    pub fn open_recent(&mut self, i: usize) {
        let Some(path) = self.recent.get(i).cloned() else {
            self.status = Some(Msg::StatusRecentGone.into());
            return;
        };
        let p = PathBuf::from(&path);
        if !p.exists() {
            self.status = Some(Fmt::StatusFileMissing { path }.into());
            return;
        }
        self.open_path_async(p);
    }

    /// Window → Next / Previous Tab.
    pub fn activate_next_tab(&mut self, step: i64) {
        let n = self.tabs.len() as i64;
        if n < 2 {
            return;
        }
        let mut i = (self.active as i64 + step) % n;
        if i < 0 {
            i += n;
        }
        // `set_active_tab` discards the previous document's scroll offset,
        // selection anchor and column-selection gesture.
        self.set_active_tab(i as usize);
        // The new tab may show/hide the CSV·Markdown toolbars; the viewport
        // must re-yield
        self.relayout();
    }

    // ---- encoding / newline ----------------------------------------------

    /// Encoding → Character encoding. The choice applies to the **next save**; the buffer
    /// stays in memory as UTF-8 as long as it is open.
    pub fn set_encoding(&mut self, name: &str) {
        // Shares the same tab table as the encoding menu / check state
        // (`io::encoding::MENU_GROUPS`).
        let Some((enc, bom)) = crate::io::encoding::menu_choice(name) else {
            self.status = Some(
                Fmt::StatusUnknownEncoding {
                    name: name.to_string(),
                }
                .into(),
            );
            return;
        };
        let i = self.active;
        self.tabs[i].doc.meta.encoding = Some(enc);
        self.tabs[i].doc.meta.had_bom = bom;
        self.status = Some(
            Fmt::StatusEncodingSet {
                name: name.to_string(),
            }
            .into(),
        );
    }

    /// Encoding → Line endings. Also applies to the next save.
    pub fn set_newline(&mut self, name: &str) {
        let nl = match crate::buffer::Newline::from_display(name) {
            Some(nl) => nl,
            None => {
                self.status = Some(
                    Fmt::StatusUnknownNewline {
                        name: name.to_string(),
                    }
                    .into(),
                );
                return;
            }
        };
        let i = self.active;
        self.tabs[i].doc.meta.user_newline = Some(nl);
        self.status = Some(
            Fmt::StatusNewlineSet {
                name: name.to_string(),
            }
            .into(),
        );
    }

    // ---- file-level actions ----------------------------------------------

    /// File → Save As. Always asks for a path, even when the document has one.
    pub fn save_active_as(&mut self) {
        let i = self.active;
        let name = self.tabs[i].doc.meta.display_name.clone();
        if let Some(p) = self.pick_save_path(&name) {
            // No pre-assignment: `save_doc_to` only records the path after a
            // successful save, so a failed write never leaves the document
            // permanently pointing at a file that was never written
            self.save_doc_to(i, p, true);
        }
    }

    /// Write tab `i` to `path` — the single write tail of Save / Save As.
    ///
    /// Both landing spots are set here (`meta.path` **and** `meta.display_name`)
    /// so no caller can save a file and leave the tab showing its old name; that
    /// is exactly what a second copy of this code got wrong (Save recorded the
    /// path only, and the tab stayed "Untitled" until the next Save As).
    ///
    /// `new_path` selects nothing but the status wording: a save that has just
    /// chosen its target reads as "saved to <name>", a routine overwrite keeps
    /// the size report.
    fn save_doc_to(&mut self, i: usize, path: PathBuf, new_path: bool) -> bool {
        // wys edits pending sync land in the piece table first, so what gets
        // saved is what the user sees. Here rather than in the callers: Save As
        // skipped it and could write the pre-edit text.
        crate::ui::components::mdws::flush_pending(self);
        let opts = self.tabs[i].doc.save_options();
        // Captured before the landing spots below are rewritten: "did the
        // document's *type* change along with its name" is decided by these
        // two (see the adoption step in the Ok branch).
        let was_untitled = self.tabs[i].doc.meta.path.is_none();
        let old_ext = extension_of(self.tabs[i].doc.meta.path.as_deref());
        match crate::io::save_file(&self.tabs[i].doc, &path, &opts) {
            Ok(report) => {
                // The file name is the tab title: a path without a file name
                // (`..`, a root) keeps whatever the tab already showed.
                let name = path
                    .file_name()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| self.tabs[i].doc.meta.display_name.clone());
                // The file was opened with lenient decoding: the U+FFFD in the
                // buffer would replace illegal bytes on disk. The post-save state
                // must spell this out, not just report "saved".
                let lossy_open = self.tabs[i].doc.meta.lossy_utf8;
                self.tabs[i].doc.meta.path = Some(path.clone());
                self.tabs[i].doc.meta.display_name = name.clone();
                self.tabs[i].doc.mark_saved();
                self.note_mtime_of(i);
                // The type follows the name. An untitled buffer has no path,
                // so its language could only ever be the "no evidence" Plain
                // Text; saving it as `notes.md` must leave it looking exactly
                // like the same file opened from disk (Markdown highlighting,
                // Markdown/CSV toolbars, outline). Save As to a different
                // extension (`a.txt` → `a.csv`) is the same decision made
                // later, so it goes through the same adoption step.
                if was_untitled || old_ext != extension_of(Some(&path)) {
                    self.adopt_type_of(i);
                }
                self.push_recent(path.to_string_lossy().into_owned());
                self.status = Some(save_status(
                    name,
                    report.bytes_written,
                    report.lossy,
                    lossy_open,
                    new_path,
                ));
                true
            }
            Err(e) => {
                self.status = Some(Fmt::StatusSaveFailed { err: e.to_string() }.into());
                false
            }
        }
    }

    /// Re-detect tab `i`'s language from the path it was just saved to and
    /// apply the per-type view defaults — the same landing state an open of
    /// that file produces ([`Tab::new`] + [`Self::adopt_view_prefs`]).
    ///
    /// Detection here is **path-first, content-second**, unlike
    /// [`detect_spec`]'s deliberate "byte-zero file is Plain Text": the user
    /// just typed the extension, so it is evidence, not a guess. Without that,
    /// saving an empty untitled document as `.md` would land back on Plain
    /// Text.
    fn adopt_type_of(&mut self, i: usize) {
        if i >= self.tabs.len() {
            return;
        }
        let first = if self.tabs[i].doc.line_count() > 0 {
            self.tabs[i].doc.line_text(0)
        } else {
            String::new()
        };
        let spec = crate::highlight::for_path_and_first_line(
            self.tabs[i].doc.meta.path.as_deref(),
            &first,
        );
        self.set_language_at(i, spec);
        // The document view is Markdown-only: a tab that *was* Markdown in WYS
        // mode and has just been saved as `.txt`/`.csv`/… must fall back to the
        // text view, or the WYS shell would keep rendering a non-Markdown file
        // (and its editing keys would keep going to the wys model).
        if self.tabs[i].md_wys && spec.name != "Markdown" {
            self.tabs[i].md_wys = false;
        }
        // The JSONL preview's default comes from the language, not from the
        // tab's construction-time spec; the per-type user preference (if any)
        // is applied on top, exactly as on open.
        self.tabs[i].jsonl = crate::ui::state::jsonl::JsonlView::for_spec(spec.name);
        self.adopt_view_prefs(i);
    }

    /// File → Reload: throw away the in-memory edits and re-read the file.
    ///
    /// Goes through the worker thread like opening a file, and swaps the
    /// document into the *same* tab when the result arrives (unless that tab
    /// was closed in the meantime, in which case it becomes a new tab).
    pub fn reload_active(&mut self) {
        // Commit debounced wys edits first: they are still only in the global
        // singleton, so the dirty check below would miss them and the reload
        // would silently discard what the user just typed.
        crate::ui::components::mdws::flush_pending(self);
        // Confirm first for dirty documents: reloading discards in-memory
        // edits, and silently dropping them is data loss
        if self.active_doc().is_dirty() {
            let id = self.tabs[self.active].doc.id;
            use crate::ui::actions::Cmd;
            self.pending_reload = Some(id);
            self.show_dialog(
                Dialog::text(
                    Msg::DlgReload,
                    Fmt::DlgReloadBody {
                        name: self.active_doc().meta.display_name.clone(),
                    },
                )
                .primary(Msg::DlgReload, Some(Cmd::ReloadConfirm))
                .button(Msg::CommonCancel, Some(Cmd::ReloadCancel))
                .confirm(),
            );
            return;
        }
        self.reload_active_now();
    }

    fn reload_active_now(&mut self) {
        let Some(path) = self.active_doc().meta.path.clone() else {
            self.status = Some(Msg::StatusNoPathReload.into());
            return;
        };
        let index = self.active;
        let doc_id = self.tabs[index].doc.id;
        self.open_async(path, OpenIntent::Replace { index, doc_id });
    }

    /// Verdict of the reload confirmation (same two-phase pattern as the
    /// close confirmation).
    pub fn finish_reload(&mut self, reload: bool) {
        let Some(id) = self.pending_reload.take() else {
            return;
        };
        self.close_dialog();
        if reload && self.tabs.iter().any(|t| t.doc.id == id) && self.active_doc().id == id {
            self.reload_active_now();
        }
    }

    /// Edit → Open file's containing folder / File → Open containing folder.
    pub fn reveal_in_explorer(&mut self) {
        let Some(path) = self.active_doc().meta.path.clone() else {
            self.status = Some(Msg::StatusNoFilePath.into());
            return;
        };
        // Platform differences (explorer /select, open -R, xdg-open) live in
        // the `platform` crate.
        let res = platform::reveal_in_file_manager(&path, true);
        match res {
            Ok(_) => {
                self.status = Some(
                    Fmt::StatusRevealed {
                        path: path.display().to_string(),
                    }
                    .into(),
                )
            }
            Err(e) => self.status = Some(Fmt::StatusRevealFailed { err: e.to_string() }.into()),
        }
    }

    /// Editor right-click → Search in browser: selection first, else the
    /// word under the caret.
    ///
    /// Opens the search results page in the system default browser (same
    /// shell path as "reveal in file manager"; failures land in the status
    /// bar instead of failing silently).
    pub fn web_search(&mut self) {
        let query = {
            let sel = match self.sel_range() {
                Some(r) if r.start < r.end => self.active_doc().text_range(r),
                _ => {
                    let r = self.word_range_at(self.caret().pos);
                    if r.start < r.end {
                        self.active_doc().text_range(r)
                    } else {
                        String::new()
                    }
                }
            };
            sel.trim().to_string()
        };
        if query.is_empty() {
            self.status = Some(Msg::StatusNothingToSearch.into());
            return;
        }
        // Keep only the first line: a search box takes one query; multi-line
        // queries get rejected by search engines
        let first_line = query
            .lines()
            .next()
            .unwrap_or(&query)
            .chars()
            .take(200)
            .collect::<String>();
        let url = format!(
            "https://www.bing.com/search?q={}",
            percent_encode(&first_line)
        );
        let res = platform::open_url(&url);
        self.status = Some(match res {
            Ok(_) => Fmt::StatusSearched { query: first_line }.into(),
            Err(e) => Fmt::StatusBrowserFailed { err: e.to_string() }.into(),
        });
    }

    /// File → Quit. Asks about unsaved documents once, then sets
    /// [`EditorState::quit_requested`]; the window layer performs the close,
    /// because only it can reach the event loop.
    ///
    /// The question is asked with the app's **own** modal dialog. A native
    /// message box (`rfd::MessageDialog`) never becomes visible on top of the
    /// frameless blitz window — the same reason every other dialog here is
    /// self-drawn (see [`crate::ui::components::dialog`]) — so the editor would
    /// look hung with no way to answer.
    pub fn request_quit(&mut self) {
        // wys edits pending sync go into the piece table first: the
        // dirty-document count reads it
        crate::ui::components::mdws::flush_pending(self);
        let dirty = self.tabs.iter().filter(|t| t.doc.is_dirty()).count();
        if dirty == 0 {
            self.quit_requested = true;
            return;
        }
        use crate::ui::actions::Cmd;
        // The dialog holds message keys and renders at draw time, so an open
        // confirmation follows a language switch (the old render-at-open
        // snapshot was a known limitation, see crates/i18n/README.md).
        self.show_dialog(
            Dialog::text(Msg::DlgQuitTitle, Fmt::DlgQuitBody { dirty })
                .primary(Msg::DlgQuitConfirm, Some(Cmd::ForceQuit))
                .button(Msg::CommonCancel, None)
                .confirm(),
        );
    }

    /// Help → About.
    ///
    /// Prefers a standalone owned popup window ([`crate::ui::aboutwin`], same
    /// shape as the find popup); falls back to an in-page dialog when the
    /// shell bridge is unavailable (no windowing environment).
    pub fn about(&mut self) {
        if self.open_about_window() {
            self.status = Some(Msg::DlgAboutTitle.into());
            return;
        }
        let doc = self.active_doc();
        let info = Fmt::DlgAboutBody {
            version: env!("CARGO_PKG_VERSION"),
            name: doc.meta.display_name.clone(),
            lines: doc.line_count() as usize,
            bytes: doc.table.len() as usize,
            syntax: self.spec().name.to_string(),
        };
        self.show_dialog(Dialog::text(Msg::DlgAboutTitle, info));
        self.status = Some(Msg::DlgAboutTitle.into());
    }

    /// Settings → Preferences.
    ///
    /// A Blitz-rendered owned popup window ([`crate::ui::prefswin`]) where
    /// possible, movable and resizable like a native preferences dialog.
    /// The in-page panel stays as the fallback for platforms with no shell
    /// bridge.
    pub fn open_preferences(&mut self) {
        // On platforms without owned popup windows, open_prefs_window returns
        // false and we fall back to the in-page dialog.
        if self.open_prefs_window() {
            self.status = Some(Msg::PrefsTitle.into());
            return;
        }
        self.show_dialog(Dialog::prefs());
        self.status = Some(Msg::PrefsTitle.into());
    }

    /// Settings → Shortcuts: every accelerator the editor actually implements.
    ///
    /// Built from [`crate::ui::actions::shortcut_rows`], which walks the same
    /// menu tree the menu bar renders — so the list can never advertise a key
    /// that does nothing (the bug that made the old hand-written table
    /// worthless).
    pub fn open_shortcut_help(&mut self) {
        let rows: Vec<Vec<String>> = crate::ui::actions::shortcut_rows()
            .into_iter()
            .map(|r| vec![r.accel, r.label])
            .collect();
        self.show_dialog(Dialog::table(
            Msg::DlgShortcutsTitle,
            vec![
                self.tr(Msg::DlgHeaderKey).into_owned(),
                self.tr(Msg::DlgHeaderCmd).into_owned(),
            ],
            rows,
        ));
        self.status = Some(Msg::StatusShortcutHelp.into());
    }
}

/// Percent-encode for a query string: unreserved characters pass through,
/// everything else becomes `%XX` over the UTF-8 bytes. A hand-rolled encoder
/// because the alternative — shipping a URL crate for one call site — is a
/// dependency the project does not otherwise need.
fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Path equality that survives Windows quirks: `C:\a.txt`, `c:/a.txt` and
/// `C:\A.TXT` are the same file. Plain equality elsewhere.
pub fn same_path(a: Option<&std::path::Path>, b: &std::path::Path) -> bool {
    let Some(a) = a else {
        return false;
    };
    // Case/separator rules are a platform difference (Windows insensitive,
    // POSIX sensitive), centralized in `platform::paths_equal`.
    platform::paths_equal(a, b)
}

/// Lower-cased extension of a path (`None` for "no path" / "no extension").
///
/// Case-insensitive on purpose: `A.MD` and `a.md` are the same file type on
/// every platform this editor ships on, and the answer is only ever compared
/// with another answer from this function.
fn extension_of(path: Option<&std::path::Path>) -> Option<String> {
    path?
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
}

/// Test-only answers for the "pick a save path" dialog.
///
/// Two state-layer entry points must ask the OS for a path (Save on an untitled
/// document, Save As). A native dialog is a modal loop on this very thread: a
/// headless test that reached it would hang instead of failing, and `rfd` cannot
/// be driven from a test. The answer is queued here instead; `pick_save_path`
/// consumes it before touching `rfd`.
///
/// Thread-local on purpose: the unit tests of this module share one process and
/// run in parallel, so a global queue would hand one test's answer to another.
#[cfg(test)]
mod test_dialog {
    use std::cell::RefCell;
    use std::path::PathBuf;

    thread_local! {
        /// `None` = nothing queued (fall through to the real dialog),
        /// `Some(None)` = the user cancelled, `Some(Some(p))` = picked `p`.
        static SAVE_ANSWER: RefCell<Option<Option<PathBuf>>> = const { RefCell::new(None) };
    }

    /// Queue the answer the next save dialog gives.
    pub(super) fn answer_next_save(path: Option<PathBuf>) {
        SAVE_ANSWER.with(|a| *a.borrow_mut() = Some(path));
    }

    /// Take the queued answer (one-shot: a second save asks for a fresh one).
    pub(super) fn take_save_answer() -> Option<Option<PathBuf>> {
        SAVE_ANSWER.with(|a| a.borrow_mut().take())
    }
}

#[cfg(test)]
mod tests {
    use super::{percent_encode, test_dialog};
    use crate::ui::state::EditorState;

    /// After cancelling the close-all confirmation with Esc / ×,
    /// `pending_close` and `close_all_pending` must reset together: otherwise
    /// the next time a dirty document is closed with "Save", `finish_close`
    /// sweeps away the remaining tabs (a single-tab close turns into
    /// close-all).
    #[test]
    fn cancelling_the_close_prompt_aborts_the_pending_sweep() {
        let mut st = EditorState::minimal();
        st.new_doc();
        st.tabs[0].doc.insert_text(0, "a");
        st.tabs[1].doc.insert_text(0, "b");
        assert_eq!(st.tabs.len(), 2);

        st.close_all_sweep();
        assert!(
            st.pending_close.is_some(),
            "dirty document must raise the confirmation dialog"
        );
        assert!(st.close_all_pending, "sweep flag should be set");

        st.close_dialog(); // Esc / title bar ×
        assert!(
            st.pending_close.is_none(),
            "pending doc id must be cleared after cancel"
        );
        assert!(
            !st.close_all_pending,
            "sweep flag must be cleared after cancel"
        );

        // Then close one tab: only that one; must not sweep the rest
        assert!(!st.close_doc(0), "dirty document is still two-phase");
        st.finish_close(Some(false));
        assert_eq!(st.tabs.len(), 1, "only the confirmed one may be closed");
        assert!(
            st.dialog.is_none(),
            "no second confirmation dialog should appear"
        );
        assert!(st.pending_close.is_none());
    }

    #[test]
    fn closing_a_tab_drops_its_per_document_state() {
        let mut st = EditorState::minimal();
        st.new_doc();
        let id = st.tabs[0].doc.id;
        st.bookmarks.entry(id).or_default().insert(2);
        st.mtimes.insert(id, std::time::SystemTime::now());
        st.hl_marks.entry(id).or_default();
        st.hidden.entry(id).or_default().insert(1);
        st.recovery_backup_gens.insert(id, 7);

        assert!(st.close_doc(0), "a clean document closes directly");
        assert!(!st.bookmarks.contains_key(&id));
        assert!(!st.mtimes.contains_key(&id));
        assert!(!st.hl_marks.contains_key(&id));
        assert!(!st.hidden.contains_key(&id));
        assert!(!st.recovery_backup_gens.contains_key(&id));
    }

    #[test]
    fn percent_encode_keeps_unreserved_and_escapes_the_rest() {
        assert_eq!(percent_encode("a_b-c.d~e"), "a_b-c.d~e");
        assert_eq!(percent_encode("c++"), "c%2B%2B");
        assert_eq!(percent_encode("中文 q"), "%E4%B8%AD%E6%96%87%20q");
        assert_eq!(percent_encode("a&b=c"), "a%26b%3Dc");
    }

    /// Outline only serves Markdown: when switching from Markdown (with
    /// outline panel open) to a non-Markdown tab, the sidebar must be handed
    /// back/collapsed, leaving no empty panel.
    #[test]
    fn switching_to_a_non_markdown_tab_closes_the_outline_panel() {
        let mut st = EditorState::minimal();
        st.set_language_by_name("Markdown");
        st.toggle_outline();
        assert!(
            st.outline_panel_shown(),
            "Markdown documents can open the outline"
        );

        st.new_doc(); // New tag is Plain Text
        st.set_active_tab(1);
        assert!(
            !st.outline_panel_shown(),
            "a non-Markdown tab must not keep the outline panel"
        );
        assert!(
            !st.ws_show,
            "collapse the sidebar entirely when there is no workspace"
        );
        assert_eq!(st.sidebar_w(), 0.0);
    }

    /// Scratch directory for the save tests (unit tests have no `tests/common`;
    /// the pid + a counter keep parallel tests from deleting each other's files).
    fn scratch_dir(tag: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static SEQ: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "infeditor-save-{}-{tag}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Save on an untitled document is a Save As in disguise: once the dialog
    /// answers with a path, the tab must carry **both** the path and the file
    /// name. It used to record only the path, so the tab — and the window title,
    /// which renders `display_name` — kept showing "Untitled" while the file was
    /// already on disk under its real name.
    #[test]
    fn saving_an_untitled_document_adopts_the_picked_name() {
        let dir = scratch_dir("adopt-name");
        let target = dir.join("报告.md");
        let mut st = EditorState::minimal();
        st.tabs[0].doc.insert_text(0, "# 标题\n");
        assert!(st.tabs[0].doc.meta.path.is_none(), "starts untitled");
        assert_ne!(st.tabs[0].doc.meta.display_name, "报告.md");

        test_dialog::answer_next_save(Some(target.clone()));
        assert!(st.save_doc(0), "the save must succeed");

        assert_eq!(
            st.tabs[0].doc.meta.path.as_deref(),
            Some(target.as_path()),
            "the picked path must land in the document"
        );
        assert_eq!(
            st.tabs[0].doc.meta.display_name, "报告.md",
            "the tab title must become the file name, not stay the placeholder"
        );
        assert!(!st.tabs[0].doc.is_dirty());
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "# 标题\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Both entry points go through one write tail, so Save As must keep
    /// adopting the picked name (it always did — this pins the merge).
    #[test]
    fn save_as_adopts_the_picked_name_of_an_already_saved_document() {
        let dir = scratch_dir("save-as-name");
        let first = dir.join("旧名.txt");
        let second = dir.join("新名.txt");
        let mut st = EditorState::minimal();
        st.tabs[0].doc.insert_text(0, "内容\n");

        test_dialog::answer_next_save(Some(first.clone()));
        assert!(st.save_doc(0));
        assert_eq!(st.tabs[0].doc.meta.display_name, "旧名.txt");

        test_dialog::answer_next_save(Some(second.clone()));
        st.save_active_as();
        assert_eq!(st.tabs[0].doc.meta.path.as_deref(), Some(second.as_path()));
        assert_eq!(st.tabs[0].doc.meta.display_name, "新名.txt");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Saving an untitled document as `*.md` must adopt the type that name
    /// implies.
    ///
    /// The tab was created without a path, so its language could only ever be
    /// the "no evidence" Plain Text; it used to stay that way for the rest of
    /// the session — the editor wrote a Markdown file and then kept showing it
    /// as plain text (no Markdown toolbar, no outline, no highlighting).
    #[test]
    fn saving_an_untitled_document_as_markdown_adopts_the_language() {
        let dir = scratch_dir("save-as-md");
        let target = dir.join("笔记.md");
        let mut st = EditorState::minimal();
        st.tabs[0].doc.insert_text(0, "# 标题\n");
        assert_eq!(st.tabs[0].spec.name, "Plain Text", "starts as plain text");

        test_dialog::answer_next_save(Some(target.clone()));
        assert!(st.save_doc(0));

        assert_eq!(st.tabs[0].spec.name, "Markdown", "type follows the name");
        assert_eq!(st.tabs[0].doc.meta.file_type, "Markdown");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Save As to a **different** extension re-detects the type; saving under
    /// the same one does not — a language picked by hand from the Syntax menu
    /// must survive a plain Save.
    #[test]
    fn save_as_retargets_the_language_only_when_the_extension_changes() {
        let dir = scratch_dir("save-as-ext");
        let csv = dir.join("数据.csv");
        let txt = dir.join("数据.txt");
        let mut st = EditorState::minimal();
        st.tabs[0].doc.insert_text(0, "a,b\n1,2\n");

        test_dialog::answer_next_save(Some(txt.clone()));
        assert!(st.save_doc(0));
        assert_eq!(st.tabs[0].spec.name, "Plain Text", ".txt stays plain");

        // Save As to .csv: the type follows the new name (table toolbar on).
        test_dialog::answer_next_save(Some(csv.clone()));
        st.save_active_as();
        assert_eq!(st.tabs[0].spec.name, "CSV", "type follows the extension");

        // A hand-picked language, then a plain Save to the same path: the
        // choice stays — nothing about the name changed.
        st.set_language(crate::highlight::by_name("Rust"));
        assert!(st.save_doc(0));
        assert_eq!(
            st.tabs[0].spec.name, "Rust",
            "same extension keeps the pick"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The document view belongs to Markdown only: adopting a non-Markdown type
    /// drops back to the text view instead of rendering a `.txt` through the
    /// Markdown document shell.
    ///
    /// Calls the adoption step directly: a real save goes through
    /// `mdws::flush_pending`, which needs a live Dioxus runtime (the other tests
    /// in this module start with `md_wys == false` and therefore never take that
    /// branch). What is pinned here is the guard, not the plumbing.
    #[test]
    fn adopting_a_non_markdown_type_leaves_the_document_view() {
        let dir = scratch_dir("save-as-wys");
        let mut st = EditorState::minimal();
        st.tabs[0].doc.insert_text(0, "# 标题\n");
        st.tabs[0].doc.meta.path = Some(dir.join("笔记.md"));
        st.adopt_type_of(0);
        assert_eq!(st.tabs[0].spec.name, "Markdown");

        st.tabs[0].md_wys = true;
        st.tabs[0].doc.meta.path = Some(dir.join("笔记.txt"));
        st.adopt_type_of(0);
        assert_eq!(st.tabs[0].spec.name, "Plain Text");
        assert!(
            !st.tabs[0].md_wys,
            "a non-Markdown file must not stay in the document view"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Cancelling the dialog is not a save: no path, no rename, still dirty —
    /// nothing may look saved when nothing was written.
    #[test]
    fn cancelling_the_save_dialog_leaves_the_document_untitled_and_dirty() {
        let mut st = EditorState::minimal();
        st.tabs[0].doc.insert_text(0, "x");
        let before = st.tabs[0].doc.meta.display_name.clone();

        test_dialog::answer_next_save(None);
        assert!(!st.save_doc(0), "cancel must report 'not saved'");

        assert!(st.tabs[0].doc.meta.path.is_none(), "no path on cancel");
        assert_eq!(st.tabs[0].doc.meta.display_name, before);
        assert!(st.tabs[0].doc.is_dirty(), "still unsaved work");
    }
}
