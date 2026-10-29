//! Find / replace / goto-line commands over [`EditorState`].
//!
//! The search engine works on an immutable
//! [`PieceTable`](crate::buffer::piece_table::PieceTable) snapshot, so "next" /
//! "previous" only ever scan a 16 MB window around the caret and stay
//! responsive on multi-gigabyte files; "find all" collects up to
//! [`FIND_LIMIT`] matches and feeds the results panel.

use crate::buffer::doc::EditOp;
use crate::search::{CompiledFind, FindOptions, Match, SearchMode};
use crate::ui::{Fmt, Msg};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use crate::ui::state::{
    EditorState, FindFocus, SearchJob, SearchJobKind, SearchOutcome, FIND_LIMIT,
};

/// When a document exceeds this many bytes, find-all/replace-all move to a background thread.
///
/// The synchronous path is millisecond-level for a 1MB document; a 4MB regex full scan on the
/// UI thread is tens of milliseconds — already enough to feel frame drops — while a 256MB scan
/// is second-level and must come with progress and cancellation.
pub const ASYNC_SEARCH_THRESHOLD: u64 = 4 * 1024 * 1024;

// ---- GUI smoke --------------------------------------------------------------
// With `INF_FIND_SMOKE=1`, the app opens the find popup automatically ~1.5s after launch
// (for screenshot verification). Generic implementation in [`crate::ui::smoke`].

static SMOKE: crate::ui::smoke::Smoke = crate::ui::smoke::Smoke::new();

pub fn arm_smoke_from_env() {
    SMOKE.arm_from_env("INF_FIND_SMOKE", 1500);
}

pub fn smoke_due() -> bool {
    SMOKE.due()
}

/// Non-destructive probe (exclusive to `needs_idle_poll`, see [`crate::ui::smoke::Smoke::pending`]).
pub fn smoke_pending() -> bool {
    SMOKE.pending()
}

impl EditorState {
    // ---- opening / closing ------------------------------------------------

    /// Open the find dialog. `replace` shows the replacement row, `panel`
    /// asks for the results list.
    ///
    /// On Windows the dialog is a **child window rendered by Blitz** (see
    /// `ui::findwin` / `ui::shell`): it floats over the editor like the old
    /// native dialog but uses the same DOM/CSS surface as the in-page bar.
    /// Where a child window cannot be created the in-page bar takes over;
    /// both share one state machine ([`Self::open_find_inline`] is what the
    /// unit tests drive).
    pub fn open_find(&mut self, replace: bool, panel: bool) {
        // A short selection is the obvious thing to search for, exactly as in
        // a conventional editor.
        if let Some(r) = self.sel_range() {
            if r.end - r.start <= 512 {
                let text = self.active_doc().text_range(r);
                if !text.is_empty() && !text.contains('\n') && !text.contains('\r') {
                    self.find.pattern = text;
                }
            }
        }
        if self.open_find_child(replace, panel) {
            return;
        }
        self.open_find_inline(replace, panel);
    }

    /// Show the find dialog as a Blitz-rendered child window. Returns false
    /// when owned popups don't exist on this platform or the shell bridge is
    /// unavailable (then the caller falls back to the in-page bar).
    fn open_find_child(&mut self, replace: bool, panel: bool) -> bool {
        if !winshell::owned_popups() || !crate::ui::shell::bridge_ready() {
            return false;
        }
        self.enter_find_child_mode(replace, panel);
        self.show_find_child();
        true
    }

    /// Enter find mode with the surface living in the child window.
    ///
    /// The flag goes in **before** [`Self::enter_find_mode`] relayouts: the
    /// popup draws the find UI itself, so the in-page strip
    /// (`Find::bar_height`) must stay 0. Reserving it and flipping the flag
    /// afterwards left a `FIND_ROW_H`-tall unpainted band at the top of the
    /// viewport — `FindBar` renders nothing in child-window mode, so nothing
    /// ever covered it (white in the light theme, the window background in the
    /// dark one). That band was the leftover of the old inline-only bar.
    fn enter_find_child_mode(&mut self, replace: bool, panel: bool) {
        self.find.child_win = true;
        self.enter_find_mode(replace, panel);
    }

    /// Place the popup at its remembered position (default: centered on the main window's upper
    /// area when nothing is remembered) and show it.
    ///
    /// The popup is an owned window; its coordinate system is **screen logical coordinates**
    /// (no longer an offset in the main window's client area).
    fn show_find_child(&mut self) {
        let (dw, dh) = crate::ui::findwin::popup_size(self.find.replace);
        let rect = self
            .find
            .popup_rect
            .filter(|r| r[2] >= dw * 0.5 && r[3] >= dh * 0.5)
            .unwrap_or_else(|| {
                let (mx, my) = winshell::win::frameless::main_window_pos();
                let x = mx + ((self.win.w - dw) / 2.0).max(0.0);
                let y = my + (self.win.h * 0.18).max(0.0);
                [x, y, dw, dh]
            });
        crate::ui::findwin::sync_shared(self);
        crate::ui::shell::send(crate::ui::shell::ShellEvt::PopupShow {
            id: crate::ui::findwin::ID,
            parent: winshell::win::frameless::current_hwnd(),
            x: rect[0],
            y: rect[1],
            w: rect[2],
            h: rect[3],
        });
    }

    /// Open the **in-page** find bar (the fallback surface).
    ///
    /// The unit tests keep using this name.
    pub fn open_find_inline(&mut self, replace: bool, panel: bool) {
        self.enter_find_mode(replace, panel);
    }

    /// Enter find mode: the find surface becomes the keyboard owner.
    fn enter_find_mode(&mut self, replace: bool, panel: bool) {
        self.find.open = true;
        // Find/Replace are two tabs of the same dialog: Ctrl+F switches to
        // "Find", Ctrl+H to "Replace"; the content mode follows the tab.
        self.find.replace = replace;
        if panel {
            self.find.panel = true;
        }
        self.find.stale = true;
        self.find.message = None;
        self.find.focus = FindFocus::Pattern;
        // The find surface owns the keyboard while it is up, so the document
        // greys its selection exactly as it does behind a dialog.
        self.doc_focused = false;
        self.relayout();
    }

    /// Close the dialog (or the in-page bar) and the results panel, and give the
    /// keyboard back to the document.
    pub fn close_find(&mut self) {
        if self.find.child_win {
            crate::ui::shell::send(crate::ui::shell::ShellEvt::PopupHide {
                id: crate::ui::findwin::ID,
            });
            // The popup geometry (written back to popup_rect after drag/resize) is persisted once
            // with the settings. Only write to disk in a real GUI session (the tests' minimal()
            // has no shell bridge).
            if crate::ui::shell::bridge_ready() {
                crate::ui::settings::save(&crate::ui::settings::AppSettings::capture(self));
            }
        }
        self.find.open = false;
        self.find.child_win = false;
        self.find.replace = false;
        self.find.panel = false;
        self.find.goto = None;
        self.find.goto_fresh = false;
        self.find.focus = FindFocus::Doc;
        self.find.cur = None;
        self.find.message = None;
        self.doc_focused = true;
        self.relayout();
    }

    pub fn toggle_find_panel(&mut self) {
        self.find.panel = !self.find.panel;
        self.relayout();
    }

    /// Search → Go to line.
    ///
    /// Stays in-page: it is a single one-field prompt, shown as
    /// its own small dialog precisely because it is *not* the find dialog —
    /// hijacking the find window for it would lose the typed pattern.
    ///
    /// An open find child window is hidden first: two search surfaces at once
    /// is the state where it is genuinely unclear which one a later keystroke
    /// belongs to. Closing the goto box restores the child window (the bar
    /// reopens through `open_find`-equivalent state, so simply re-show it).
    pub fn open_goto(&mut self) {
        if self.find.child_win {
            crate::ui::shell::send(crate::ui::shell::ShellEvt::PopupHide {
                id: crate::ui::findwin::ID,
            });
            self.find.child_win = false;
            self.find.open = false;
        }
        let cur = self.active_doc().line_of_pos(self.caret().pos) + 1;
        self.find.goto = Some(cur.to_string());
        // What is prefilled is the "current line number", semantically in a **selected** state:
        // the user's first character replaces it.
        self.find.goto_fresh = true;
        self.find.focus = FindFocus::Goto;
        self.find.open = true;
        self.find.replace = false;
        self.find.panel = false;
        self.find.message = None;
        self.doc_focused = false;
        self.relayout();
    }

    pub fn set_find_focus(&mut self, f: FindFocus) {
        self.find.focus = f;
        self.doc_focused = f == FindFocus::Doc;
        // Switching focus boxes: the old box's IME composition string is voided (otherwise the
        // composition string would flash in the new box at the switch instant — the composer
        // remount has already broken the IME session, so the string never reaches the screen).
        self.find.preedit.clear();
    }

    /// Find/Replace tab switch (the popup's Find/Replace tabs, two tabs of one dialog). In child
    /// window mode the window height must be grown to the new mode's minimum content: the
    /// replace tab has one more input row than the find tab, and switching directly would clip
    /// the bottom row.
    pub fn set_find_tab(&mut self, replace: bool) {
        if self.find.replace == replace {
            return;
        }
        self.find.replace = replace;
        self.find.focus = FindFocus::Pattern;
        self.relayout();
        if self.find.child_win {
            let (dw, dh) = crate::ui::findwin::popup_size(replace);
            if let Some(r) = self.find.popup_rect.as_mut() {
                if r[2] < dw {
                    r[2] = dw;
                }
                if r[3] < dh {
                    r[3] = dh;
                }
            }
            self.show_find_child();
        }
    }

    // ---- options ----------------------------------------------------------

    /// Any change to the pattern or the flags invalidates the result list.
    pub fn invalidate_find(&mut self) {
        self.find.stale = true;
        self.find.message = None;
    }

    pub fn cycle_find_mode(&mut self) {
        self.find.mode = match self.find.mode {
            SearchMode::Plain => SearchMode::Regex,
            SearchMode::Regex => SearchMode::Esc,
            SearchMode::Esc => SearchMode::Plain,
        };
        self.invalidate_find();
        self.find.message = Some(
            match self.find.mode {
                SearchMode::Plain => Msg::FindModePlain,
                SearchMode::Regex => Msg::FindModeRegex,
                SearchMode::Esc => Msg::FindModeEsc,
            }
            .into(),
        );
    }

    pub fn toggle_find_case(&mut self) {
        self.find.case = !self.find.case;
        self.invalidate_find();
    }

    pub fn toggle_find_word(&mut self) {
        self.find.word = !self.find.word;
        self.invalidate_find();
    }

    /// Direction radio (popup): true = up.
    pub fn set_find_dir(&mut self, up: bool) {
        self.find.up = up;
    }

    /// Wrap-around search toggle.
    pub fn toggle_find_wrap(&mut self) {
        self.find.wrap = !self.find.wrap;
        self.invalidate_find();
    }

    /// Find/replace in-selection toggle.
    pub fn toggle_find_in_sel(&mut self) {
        self.find.in_sel = !self.find.in_sel;
        self.invalidate_find();
    }

    /// Search mode radio (popup); the in-page bar cycles via [`Self::cycle_find_mode`].
    pub fn set_find_mode(&mut self, m: SearchMode) {
        self.find.mode = m;
        self.invalidate_find();
        self.find.message = Some(
            match m {
                SearchMode::Plain => Msg::FindModePlain,
                SearchMode::Regex => Msg::FindModeRegex,
                SearchMode::Esc => Msg::FindModeEsc,
            }
            .into(),
        );
    }

    /// Search range: in-selection = current selection, otherwise the whole document.
    fn search_bounds(&self) -> (u64, u64) {
        match (self.find.in_sel, self.sel_range()) {
            (true, Some(r)) if r.end > r.start => (r.start, r.end),
            _ => (0, self.active_doc().table.len()),
        }
    }

    /// After a successful find, record the pattern in history (most recent first, deduped, ≤30).
    fn record_history(&mut self) {
        if self.find.pattern.is_empty() {
            return;
        }
        if let Some(i) = self
            .find
            .history
            .iter()
            .position(|p| p == &self.find.pattern)
        {
            self.find.history.remove(i);
        }
        self.find.history.insert(0, self.find.pattern.clone());
        self.find.history.truncate(30);
    }

    /// Copy the focused input box to the system clipboard (popup Ctrl+C).
    pub fn find_copy_clip(&mut self) {
        let Some(text) = self.find.field().map(|s| s.to_string()) else {
            return;
        };
        let _clip = platform::clipboard::lock();
        if let Ok(mut cb) = arboard::Clipboard::new() {
            let _ = cb.set_text(text);
        }
    }

    /// Paste from the system clipboard into the focused input box (popup Ctrl+V).
    pub fn find_paste_clip(&mut self) {
        let text = {
            let _clip = platform::clipboard::lock();
            match arboard::Clipboard::new() {
                Ok(mut cb) => cb.get_text().unwrap_or_default(),
                Err(_) => String::new(),
            }
        };
        let text: String = text.chars().filter(|c| *c != '\n' && *c != '\r').collect();
        if !text.is_empty() {
            self.find_insert(&text);
        }
    }

    pub fn find_options(&self) -> FindOptions {
        FindOptions {
            pattern: self.find.pattern.clone(),
            mode: self.find.mode,
            case_sensitive: self.find.case,
            whole_word: self.find.word,
        }
    }

    fn compile(&self) -> Result<CompiledFind, regex::Error> {
        CompiledFind::new(self.find_options())
    }

    /// Compile and report a user-visible error in the bar instead of throwing.
    fn compile_or_msg(&mut self) -> Option<CompiledFind> {
        if self.find.pattern.is_empty() {
            self.find.message = Some(Msg::FindEnterPattern.into());
            return None;
        }
        match self.compile() {
            Ok(cf) => Some(cf),
            Err(e) => {
                self.find.message = Some(Fmt::FindRegexError { err: e.to_string() }.into());
                None
            }
        }
    }

    // ---- navigation -------------------------------------------------------

    /// Select `m` in the active document and scroll it into view.
    fn reveal(&mut self, m: Match) {
        let c = self.caret_mut();
        c.anchor = Some(m.start);
        c.pos = m.end;
        self.scroll_caret_into_view();
    }

    /// Under the wys view, find highlight/scroll acts on the hidden text view, so hits are not
    /// visible — minimal compensation: hint "Nth match (line X)" in the status bar (a full
    /// highlight mapping is deferred). Zero action in non-wys modes.
    fn wys_find_hint(&mut self, nth: Option<usize>, m: Match) {
        if !self.tabs.get(self.active).is_some_and(|t| t.md_wys) {
            return;
        }
        let line = self.active_doc().line_of_pos(m.start);
        self.status = Some(match nth {
            Some(n) => Fmt::StatusFindMatchNthLine {
                n,
                line: (line + 1) as usize,
            }
            .into(),
            None => Fmt::StatusFindMatchLine {
                line: (line + 1) as usize,
            }
            .into(),
        });
    }

    /// Put the caret at the start of `m` (used when a list is being built).
    fn reveal_start(&mut self, m: Match) {
        let c = self.caret_mut();
        c.pos = m.start;
        c.anchor = None;
        self.scroll_caret_into_view();
    }

    /// Live, "type-to-find" reveal: as the pattern changes, select and scroll
    /// to the first match at/after the caret without building the full results
    /// list (that is what `find_all_matches` is for). This is an
    /// incremental search, where the document jumps to each match as you type.
    pub fn find_incremental(&mut self) {
        let Some(cf) = self.compile_or_msg() else {
            return;
        };
        let (m, wrapped) = {
            let doc = self.active_doc();
            let total = doc.table.len();
            let from = self.caret().pos;
            match crate::search::find_next(&doc.table, &cf, from, total) {
                Some(m) => (Some(m), false),
                None => (crate::search::find_next(&doc.table, &cf, 0, total), true),
            }
        };
        match m {
            Some(m) => {
                self.find.message = if wrapped {
                    Some(Msg::FindWrappedTop.into())
                } else {
                    None
                };
                self.reveal(m);
                self.record_history();
                self.status = Some(Fmt::StatusFindMatchByte { pos: m.start }.into());
            }
            None => {
                self.find.message = Some(Msg::FindNoMatch.into());
            }
        }
    }

    pub fn find_next(&mut self) {
        // Direction radio: up = take the "find previous" logic
        if self.find.up {
            return self.find_prev();
        }
        let Some(cf) = self.compile_or_msg() else {
            return;
        };
        let (lo, hi) = self.search_bounds();
        // Walking the collected list is O(1) once find-all has run.
        let (doc_id, gen) = {
            let doc = &self.tab().doc;
            (doc.id, doc.gen)
        };
        if self.find.valid_for(doc_id, gen) {
            let total = self.find.results.len();
            let pos = self.caret().pos;
            let next = match self.find.cur {
                Some(i) => {
                    if self.find.wrap {
                        (i + 1) % total
                    } else if i + 1 < total {
                        i + 1
                    } else {
                        self.find.message = Some(Msg::FindHitEnd.into());
                        return;
                    }
                }
                None => match self.find.results.iter().position(|m| m.start >= pos) {
                    Some(i) => i,
                    None if self.find.wrap => 0,
                    None => {
                        self.find.message = Some(Msg::FindHitEnd.into());
                        return;
                    }
                },
            };
            self.find.cur = Some(next);
            self.find.ensure_visible(next);
            let m = self.find.results[next];
            self.find.message = Some(
                Fmt::FindPos {
                    cur: next + 1,
                    total,
                }
                .into(),
            );
            self.reveal(m);
            self.wys_find_hint(Some(next + 1), m);
            self.record_history();
            return;
        }
        let (m, wrapped) = {
            let doc = self.active_doc();
            let from = self.caret().pos.clamp(lo, hi);
            let m = crate::search::find_next(&doc.table, &cf, from, hi);
            match m {
                Some(m) => (Some(m), false),
                None if self.find.wrap => (crate::search::find_next(&doc.table, &cf, lo, hi), true),
                None => (None, false),
            }
        };
        match m {
            Some(m) => {
                self.find.message = if wrapped {
                    Some(Msg::FindWrappedTop.into())
                } else {
                    None
                };
                self.reveal(m);
                self.sync_cur(m);
                self.record_history();
                self.status = Some(Fmt::StatusFindMatchByte { pos: m.start }.into());
                self.wys_find_hint(None, m);
            }
            None => {
                let msg = if self.find.wrap || self.find.in_sel {
                    Msg::FindNoMatch
                } else {
                    Msg::FindHitEnd
                };
                self.find.message = Some(msg.into());
                self.status = Some(msg.into());
            }
        }
    }

    /// Count (the find dialog's "Count"): how many matches there are,
    /// reported through the find message line and the status bar — without
    /// touching the results panel, the selection or the caret.
    pub fn find_count(&mut self) {
        let Some(cf) = self.compile_or_msg() else {
            return;
        };
        let doc = self.active_doc();
        let (lo, hi) = self.search_bounds();
        if hi - lo >= ASYNC_SEARCH_THRESHOLD {
            self.find.message = Some(Msg::FindCountTooLarge.into());
            self.status = Some(Msg::StatusCountTooLarge.into());
            return;
        }
        let cancel = AtomicBool::new(false);
        // borrow-scope workaround removed: the clone of the whole piece table
        // was only there to end `doc`'s borrow early
        let (matches, truncated) = {
            let res = crate::search::find_all(&doc.table, &cf, lo..hi, FIND_LIMIT, &cancel, |_| {});
            (res.matches.len(), res.truncated)
        };
        self.find.message = Some(
            if truncated {
                Fmt::FindCountCapped { n: matches }
            } else {
                Fmt::FindCount { n: matches }
            }
            .into(),
        );
        self.status = Some(
            if truncated {
                Fmt::StatusFindCountCapped { n: matches }
            } else {
                Fmt::StatusFindCount { n: matches }
            }
            .into(),
        );
    }

    pub fn find_prev(&mut self) {
        let Some(cf) = self.compile_or_msg() else {
            return;
        };
        let (lo, hi) = self.search_bounds();
        // Search *before* the start of the current selection, otherwise a
        // selected match would be returned again (find-previous would stall).
        let from = self
            .sel_range()
            .map(|r| r.start)
            .unwrap_or_else(|| self.caret().pos)
            .clamp(lo, hi);
        let (doc_id, gen) = {
            let doc = &self.tab().doc;
            (doc.id, doc.gen)
        };
        if self.find.valid_for(doc_id, gen) {
            let total = self.find.results.len();
            let prev = match self.find.cur {
                Some(i) => {
                    if self.find.wrap {
                        (i + total - 1) % total
                    } else if i > 0 {
                        i - 1
                    } else {
                        self.find.message = Some(Msg::FindHitStart.into());
                        return;
                    }
                }
                None => {
                    let p = self.find.results.iter().rposition(|m| m.start < from);
                    match p {
                        Some(i) => i,
                        None if self.find.wrap => total - 1,
                        None => {
                            self.find.message = Some(Msg::FindHitStart.into());
                            return;
                        }
                    }
                }
            };
            self.find.cur = Some(prev);
            self.find.ensure_visible(prev);
            let m = self.find.results[prev];
            self.find.message = Some(
                Fmt::FindPos {
                    cur: prev + 1,
                    total,
                }
                .into(),
            );
            self.reveal(m);
            self.wys_find_hint(Some(prev + 1), m);
            self.record_history();
            return;
        }
        let (m, wrapped) = {
            let doc = self.active_doc();
            let m = crate::search::find_prev(&doc.table, &cf, from, lo);
            match m {
                Some(m) => (Some(m), false),
                None if self.find.wrap => (crate::search::find_prev(&doc.table, &cf, hi, lo), true),
                None => (None, false),
            }
        };
        match m {
            Some(m) => {
                self.find.message = if wrapped {
                    Some(Msg::FindWrappedBottom.into())
                } else {
                    None
                };
                self.reveal(m);
                self.sync_cur(m);
                self.record_history();
                self.status = Some(Fmt::StatusFindMatchByte { pos: m.start }.into());
                self.wys_find_hint(None, m);
            }
            None => {
                let msg = if self.find.wrap || self.find.in_sel {
                    Msg::FindNoMatch
                } else {
                    Msg::FindHitStart
                };
                self.find.message = Some(msg.into());
                self.status = Some(msg.into());
            }
        }
    }

    fn sync_cur(&mut self, m: Match) {
        if self.find.results.is_empty() {
            return;
        }
        self.find.cur = self.find.results.iter().position(|x| x.start == m.start);
    }

    /// Collect every match into the results panel.
    ///
    /// Small documents take the synchronous path (millisecond-level; the tests depend on it
    /// too); large documents hand a cloned `PieceTable` snapshot to a background thread, with
    /// progress in the status bar and Esc to cancel; on completion the result is applied back
    /// on the UI thread via a channel (see `poll_search`).
    pub fn find_all_matches(&mut self) {
        let Some(cf) = self.compile_or_msg() else {
            return;
        };
        let (doc_id, gen, caret, snapshot) = {
            let doc = self.active_doc();
            (doc.id, doc.gen, self.caret().pos, doc.table.clone())
        };
        let (lo, hi) = self.search_bounds();
        if hi - lo < ASYNC_SEARCH_THRESHOLD {
            let cancel = AtomicBool::new(false);
            let res = crate::search::find_all(&snapshot, &cf, lo..hi, FIND_LIMIT, &cancel, |_| {});
            self.finish_find_all(res.matches, res.truncated, doc_id, gen, None, caret);
            return;
        }
        self.spawn_search(SearchJobKind::FindAll, cf, snapshot, lo, hi, gen, None);
    }

    /// Common part of background searches: spawn the thread, wire up progress/cancellation.
    ///
    /// `lo..hi` is the search range (in-selection = the selection range, otherwise the whole document).
    #[allow(clippy::too_many_arguments)]
    fn spawn_search(
        &mut self,
        kind: SearchJobKind,
        cf: crate::search::CompiledFind,
        table: crate::buffer::PieceTable,
        lo: u64,
        hi: u64,
        gen: u64,
        replacement: Option<String>,
    ) {
        if self.search_job.is_some() {
            self.status = Some(Msg::StatusSearchInProgress.into());
            return;
        }
        let total = hi - lo;
        let cancel = Arc::new(AtomicBool::new(false));
        let progress = Arc::new(AtomicU64::new(0));
        // Capacity 1 gives the worker backpressure: it computes one batch
        // ahead, never the whole document. The UI applies each batch before
        // the worker can hand over the next one.
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        let (c, p) = (cancel.clone(), progress.clone());
        let spawned = std::thread::Builder::new()
            .name("infeditor-search".into())
            .spawn(move || {
                // Search/replace reads the user's document: a panic must surface as a
                // failed job on the UI thread, not kill the process.
                crate::ui::guard::worker("ui::find", move || match replacement {
                    Some(repl) => {
                        let mut from = lo;
                        let mut delta = 0i64;
                        let mut total_matches = 0usize;
                        loop {
                            if c.load(Ordering::Relaxed) {
                                let _ = tx.send(SearchOutcome::ReplaceDone {
                                    matches: total_matches,
                                    cancelled: true,
                                    gen,
                                });
                                return;
                            }
                            let b = crate::search::build_replace_batch(
                                &table,
                                &cf,
                                from,
                                hi,
                                &repl,
                                delta,
                                crate::search::REPLACE_BATCH_MATCHES,
                                crate::search::REPLACE_BATCH_BYTES,
                                &c,
                                |done| {
                                    p.store(done, Ordering::Relaxed);
                                },
                            );
                            if b.cancelled {
                                let _ = tx.send(SearchOutcome::ReplaceDone {
                                    matches: total_matches,
                                    cancelled: true,
                                    gen,
                                });
                                return;
                            }
                            total_matches += b.matches;
                            delta = b.delta_after;
                            if !b.ops.is_empty()
                                && tx
                                    .send(SearchOutcome::ReplaceBatch {
                                        ops: b.ops,
                                        matches: b.matches,
                                        cancelled: false,
                                        gen,
                                    })
                                    .is_err()
                            {
                                return;
                            }
                            if b.done {
                                break;
                            }
                            from = b.next_from;
                        }
                        let _ = tx.send(SearchOutcome::ReplaceDone {
                            matches: total_matches,
                            cancelled: c.load(Ordering::Relaxed),
                            gen,
                        });
                    }
                    None => {
                        let res =
                            crate::search::find_all(&table, &cf, lo..hi, FIND_LIMIT, &c, |done| {
                                p.store(done, Ordering::Relaxed);
                            });
                        let _ = tx.send(SearchOutcome::FindAll {
                            matches: res.matches,
                            truncated: res.truncated,
                            cancelled: res.cancelled,
                            gen,
                        });
                    }
                });
            });
        if spawned.is_err() {
            // thread exhaustion must not take the editor down: degrade to an
            // error message instead of `expect`-panicking
            self.status = Some(Msg::StatusSearchThreadFailed.into());
            return;
        }
        self.search_job = Some(SearchJob {
            rx,
            progress,
            total,
            cancel,
            kind,
            started: std::time::Instant::now(),
            doc_id: self.tab().doc.id,
            replace_batches: 0,
            replace_matches: 0,
            replace_gen: gen,
        });
        self.status = Some(
            match kind {
                SearchJobKind::FindAll => Msg::StatusFindAllScanning,
                SearchJobKind::ReplaceAll => Msg::StatusReplaceAllScanning,
            }
            .into(),
        );
    }

    /// Shared finish-up of "find all" for the synchronous and background paths.
    fn finish_find_all(
        &mut self,
        matches: Vec<crate::search::Match>,
        truncated: bool,
        doc_id: u64,
        gen: u64,
        secs: Option<String>,
        caret: u64,
    ) {
        let n = matches.len();
        self.find.results = matches;
        self.find.truncated = truncated;
        self.find.stale = false;
        self.find.doc_id = doc_id;
        self.find.gen = gen;
        self.find.panel = true;
        self.find.cur = self
            .find
            .results
            .iter()
            .position(|m| m.start >= caret)
            .or(Some(0));
        if let Some(i) = self.find.cur {
            self.find.ensure_visible(i);
            if let Some(m) = self.find.results.get(i).copied() {
                self.reveal_start(m);
            }
        }
        self.find.message = Some(
            if truncated {
                Fmt::FindCountCappedLimit { n }
            } else {
                Fmt::FindCount { n }
            }
            .into(),
        );
        self.status = Some(match (truncated, secs) {
            (true, Some(s)) => Fmt::StatusFindAllDoneSecsCapped { n, secs: s }.into(),
            (true, None) => Fmt::StatusFindAllDoneCapped { n }.into(),
            (false, Some(s)) => Fmt::StatusFindAllDoneSecs { n, secs: s }.into(),
            (false, None) => Fmt::StatusFindAllDone { n }.into(),
        });
        self.relayout();
    }

    /// Reap progress and completion of background searches. Runs every frame from `poll_opens`.
    ///
    /// In progress: the status bar shows a percentage (the job owns the status bar until it
    /// ends, so user messages cannot push the progress off). Done: FindAll fills the results
    /// panel directly; ReplaceOps is **applied** here — the scan runs in the background, but
    /// the disk-style write-back must happen on the UI thread (PieceTable is not Sync-thread
    /// semantics, and undo must stay contiguous).
    pub fn poll_search(&mut self) {
        let Some(job) = &self.search_job else { return };
        // Check completion first. `Disconnected` (worker panic or sender dropped early) must be
        // distinguished from `Empty`: otherwise the job is never cleared, the status bar sticks
        // at 99%, and every subsequent find/replace is rejected with "a search is already in
        // progress" — not even Esc can recover it.
        match job.rx.try_recv() {
            Ok(outcome) => {
                let job = self.search_job.take().expect("job just read");
                self.handle_search_outcome(job, outcome);
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {
                let done = job.progress.load(Ordering::Relaxed);
                let pct = done
                    .checked_mul(100)
                    .and_then(|n| n.checked_div(job.total))
                    .map_or(0, |n| n.min(99));
                let secs = job.started.elapsed().as_secs();
                self.status = Some(match job.kind {
                    SearchJobKind::FindAll => Fmt::StatusFindAllProgress {
                        pct: pct as usize,
                        secs,
                    }
                    .into(),
                    SearchJobKind::ReplaceAll => Fmt::StatusReplaceAllProgress {
                        pct: pct as usize,
                        secs,
                    }
                    .into(),
                });
            }
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.search_job = None;
                self.status = Some(Msg::StatusSearchThreadDied.into());
            }
        }
    }

    fn handle_search_outcome(&mut self, mut job: SearchJob, outcome: SearchOutcome) {
        // Results belong only to the document that started them: during the scan the user may
        // have switched tabs (or edits advanced the doc's gen). Applying replace results
        // directly would blind-delete stale positions or even write into another document —
        // always discard and notify.
        let (id_now, gen_now) = {
            let t = self.tab();
            (t.doc.id, t.doc.gen)
        };
        if id_now != job.doc_id {
            self.status = Some(Msg::StatusSearchStale.into());
            return;
        }
        match outcome {
            SearchOutcome::FindAll {
                matches,
                truncated,
                cancelled,
                gen,
            } => {
                if cancelled {
                    self.status = Some(Msg::StatusFindAllCancelled.into());
                    return;
                }
                let caret = self.caret().pos;
                let secs = format!("{:.1}", job.started.elapsed().as_millis() as f64 / 1000.0);
                self.finish_find_all(matches, truncated, job.doc_id, gen, Some(secs), caret);
            }
            SearchOutcome::ReplaceBatch {
                ops,
                matches,
                cancelled,
                ..
            } => {
                if cancelled || job.cancel.load(Ordering::Relaxed) {
                    self.status = Some(Msg::StatusReplaceAllCancelled.into());
                    return;
                }
                // The worker's op positions assume exactly the state our own
                // previous batches produced. A user edit in between makes the
                // remaining positions unsafe, so stop instead of blind-writing.
                if gen_now != job.replace_gen {
                    self.status = Some(Msg::StatusReplaceDocChangedStop.into());
                    return;
                }
                let before = self.tab().doc.gen;
                let applied = if job.replace_batches == 0 {
                    self.tab_mut().doc.apply_transaction(ops);
                    true
                } else {
                    self.tab_mut().doc.apply_transaction_continuing(ops)
                };
                if !applied {
                    self.status = Some(Msg::StatusReplaceUndoGroupClosed.into());
                    return;
                }
                job.replace_matches += matches;
                // Only a document that really changed counts as an "applied batch" (the next
                // batch must continue the same undo group). Refresh gen regardless: the next
                // batch's validity check compares against "the state our previous batch produced".
                if self.tab().doc.gen != before {
                    job.replace_batches += 1;
                }
                job.replace_gen = self.tab().doc.gen;
                self.status = Some(
                    Fmt::StatusReplaceAllApplied {
                        n: job.replace_matches,
                    }
                    .into(),
                );
                self.search_job = Some(job);
            }
            SearchOutcome::ReplaceDone {
                matches,
                cancelled,
                gen: _,
            } => {
                if cancelled {
                    // Cancellation is cooperative: batches already applied are
                    // rolled back as one undo step when the document still
                    // matches the state our last batch produced.
                    let rolled = job.replace_batches > 0
                        && gen_now == job.replace_gen
                        && self.tab_mut().doc.undo().is_some();
                    self.find.stale = true;
                    self.status = Some(if rolled {
                        Fmt::StatusReplaceCancelledRolled {
                            n: job.replace_matches,
                        }
                        .into()
                    } else if job.replace_matches > 0 {
                        Fmt::StatusReplaceCancelledKept {
                            n: job.replace_matches,
                        }
                        .into()
                    } else {
                        Msg::StatusReplaceCancelledNone.into()
                    });
                    return;
                }
                if gen_now != job.replace_gen {
                    self.status = Some(
                        Fmt::StatusReplaceDocChangedPartial {
                            n: job.replace_matches,
                        }
                        .into(),
                    );
                    return;
                }
                let n = job.replace_matches.max(matches);
                if n == 0 {
                    self.find.message = Some(Msg::FindNoMatch.into());
                    self.status = Some(Msg::FindNoMatch.into());
                    return;
                }
                // Streaming replace's timing is already visible in the progress message, so
                // nothing more is appended to the result message (Message is a key; rendered
                // strings cannot be concatenated onto it).
                self.finish_replace_stream(n);
            }
        }
    }

    /// Cancel the background search when the user presses Esc.
    ///
    /// Cancellation is **cooperative**: once the flag is set, the worker exits at the next
    /// window boundary and the already-scanned part is discarded. A replace job writes not a
    /// single byte when cancelled.
    pub fn cancel_search(&mut self) -> bool {
        match &self.search_job {
            Some(job) => {
                job.cancel.store(true, Ordering::Relaxed);
                self.status = Some(Msg::StatusCancellingSearch.into());
                true
            }
            None => false,
        }
    }

    /// Jump to a result row from the results panel.
    pub fn goto_result(&mut self, i: usize) {
        let Some(m) = self.find.results.get(i).copied() else {
            return;
        };
        self.find.cur = Some(i);
        self.find.ensure_visible(i);
        self.find.message = Some(
            Fmt::FindPos {
                cur: i + 1,
                total: self.find.results.len(),
            }
            .into(),
        );
        self.reveal(m);
    }

    // ---- replace ----------------------------------------------------------

    /// Replace the match under the caret (or the next one) and advance.
    pub fn replace_one(&mut self) {
        // In wys mode, replacing by editing the piece table directly would diverge from the wys
        // model (the next keystroke silently rolls it back) — disable outright and notify (the
        // reseed approach is deferred until the write-access choke point lands).
        if self.tabs.get(self.active).is_some_and(|t| t.md_wys) {
            self.status = Some(Msg::StatusReplaceWysUnsupported.into());
            return;
        }
        let Some(cf) = self.compile_or_msg() else {
            return;
        };
        let repl = self.find.replacement.clone();
        let sel = self.sel_range();
        let target: Option<Match> = match sel {
            Some(r) if self.matches_at(&cf, r.start, r.end) => Some(Match {
                start: r.start,
                end: r.end,
            }),
            _ => {
                self.find_next();
                self.sel_range().map(|r| Match {
                    start: r.start,
                    end: r.end,
                })
            }
        };
        let Some(m) = target else { return };
        let (deleted, inserted) = {
            let doc = self.active_doc();
            let deleted = doc.table.read_str(m.start..m.end);
            let inserted = crate::search::expand_replacement(&cf, &deleted, &repl);
            (deleted, inserted)
        };
        let inserted_len = inserted.len() as u64;
        self.tab_mut().doc.apply_transaction(vec![EditOp {
            pos: m.start,
            deleted,
            inserted,
        }]);
        let len = self.tab().doc.table.len();
        let c = self.caret_mut();
        // Park the caret *after* the replacement. Searching again from
        // `m.start` re-matched the text just written whenever the replacement
        // begins with the pattern ("a" -> "aa", "foo" -> "foo bar"): every
        // "Replace Next" then replaced the same spot again.
        c.pos = (m.start + inserted_len).min(len);
        c.anchor = None;
        self.find.stale = true;
        self.status = Some(Msg::FindReplacedOne.into());
        self.scroll_caret_into_view();
        // move on to the next occurrence
        self.find_next();
        if self.find.message.as_ref().map(|m| m.key()).as_deref() != Some("Plain:FindNoMatch") {
            self.find.message = Some(Msg::FindReplacedOne.into());
        }
    }

    /// Replace every match in the document as one undoable transaction.
    pub fn replace_all(&mut self) {
        // Same as [`Self::replace_one`] — replace is disabled in wys mode
        if self.tabs.get(self.active).is_some_and(|t| t.md_wys) {
            self.status = Some(Msg::StatusReplaceWysUnsupported.into());
            return;
        }
        let Some(cf) = self.compile_or_msg() else {
            return;
        };
        let repl = self.find.replacement.clone();
        let (gen, snapshot) = {
            let doc = self.active_doc();
            (doc.gen, doc.table.clone())
        };
        let (lo, hi) = self.search_bounds();
        if hi - lo < ASYNC_SEARCH_THRESHOLD {
            let cancel = AtomicBool::new(false);
            let mut from = lo;
            let mut delta = 0i64;
            let mut total = 0usize;
            let mut applied_any = false;
            loop {
                let b = crate::search::build_replace_batch(
                    &snapshot,
                    &cf,
                    from,
                    hi,
                    &repl,
                    delta,
                    crate::search::REPLACE_BATCH_MATCHES,
                    crate::search::REPLACE_BATCH_BYTES,
                    &cancel,
                    |_| {},
                );
                if b.cancelled {
                    break;
                }
                total += b.matches;
                if !b.ops.is_empty() {
                    let before = self.tab().doc.gen;
                    let ok = if applied_any {
                        self.tab_mut().doc.apply_transaction_continuing(b.ops)
                    } else {
                        self.tab_mut().doc.apply_transaction(b.ops);
                        true
                    };
                    if !ok {
                        self.status = Some(Msg::StatusReplaceUndoGroupClosed.into());
                        return;
                    }
                    applied_any |= self.tab().doc.gen != before;
                }
                delta = b.delta_after;
                if b.done {
                    break;
                }
                from = b.next_from;
            }
            if total == 0 {
                self.find.message = Some(Msg::FindNoMatch.into());
                self.status = Some(Msg::FindNoMatch.into());
            } else {
                self.finish_replace_stream(total);
            }
            return;
        }
        self.spawn_search(
            SearchJobKind::ReplaceAll,
            cf,
            snapshot,
            lo,
            hi,
            gen,
            Some(repl),
        );
    }

    /// Finish-up after streaming replace completes: batches are already written back; this only
    /// settles the caret, results and message.
    fn finish_replace_stream(&mut self, n: usize) {
        let len = self.tab().doc.table.len();
        let c = self.caret_mut();
        c.pos = c.pos.min(len);
        c.anchor = None;
        self.find.stale = true;
        self.find.results.clear();
        self.find.cur = None;
        self.find.truncated = false;
        self.find.message = Some(Fmt::FindReplacedAll { n }.into());
        self.status = self.find.message.clone();
        self.scroll_caret_into_view();
    }

    /// Does `cf` match exactly `[start, end)`?
    fn matches_at(&self, cf: &CompiledFind, start: u64, end: u64) -> bool {
        if start >= end {
            return false;
        }
        let text = self.active_doc().text_range(start..end);
        match cf.re.find(&text) {
            Some(m) => m.start() == 0 && m.end() == text.len(),
            None => false,
        }
    }

    // ---- goto line --------------------------------------------------------

    /// Jump to a 1-based line number; out-of-range values clamp.
    pub fn goto_line(&mut self, line: u64) {
        let (total, pos, target) = {
            let doc = self.active_doc();
            let total = doc.line_count().max(1);
            let target = line.clamp(1, total) - 1;
            (total, doc.pos_of_line(target), target)
        };
        let c = self.caret_mut();
        c.pos = pos;
        c.anchor = None;
        self.status = Some(
            Fmt::StatusGotoLine {
                line: (target + 1) as usize,
            }
            .into(),
        );
        // centre the target line so it is obvious where we landed
        let lh = self.metrics.lh.max(1.0);
        self.scroll_top = (target as f64 * lh as f64 - self.view.h as f64 * 0.35).max(0.0);
        self.clamp_scroll();
        self.scroll_caret_into_view();
        let _ = total;
    }

    /// Run the goto box: parse the typed text, jump, then close it.
    pub fn commit_goto(&mut self) -> bool {
        let text = self.find.goto.clone().unwrap_or_default();
        let trimmed = text.trim().to_string();
        if trimmed.is_empty() {
            self.find.message = Some(Msg::GotoEnterLine.into());
            return false;
        }
        let n: u64 = match trimmed.parse() {
            Ok(n) => n,
            Err(_) => {
                self.find.message = Some(Msg::GotoLineNotNumeric.into());
                return false;
            }
        };
        self.find.goto = None;
        self.find.goto_fresh = false;
        self.find.focus = FindFocus::Doc;
        self.doc_focused = true;
        // keep the bar open only when a real search is going on
        if self.find.pattern.is_empty() {
            self.find.open = false;
            self.find.panel = false;
        }
        self.goto_line(n);
        self.relayout();
        true
    }

    // ---- text entry (virtual focus) ---------------------------------------

    /// Insert text into whichever box owns the virtual focus.
    /// Returns false when the keyboard belongs to the document.
    pub fn find_insert(&mut self, s: &str) -> bool {
        if s.is_empty() {
            return true;
        }
        // the boxes are single-line
        let s: String = s.chars().filter(|c| *c != '\n' && *c != '\r').collect();
        if s.is_empty() {
            return true;
        }
        match self.find.focus {
            FindFocus::Doc => false,
            FindFocus::Pattern => {
                self.find.pattern.push_str(&s);
                self.invalidate_find();
                self.find_incremental();
                true
            }
            FindFocus::Replacement => {
                self.find.replacement.push_str(&s);
                true
            }
            FindFocus::Goto => {
                if let Some(g) = self.find.goto.as_mut() {
                    // The prefilled current line number is in a "selected" state: the first
                    // character replaces it, only later characters append (otherwise typing 3
                    // on line 1 would yield "13").
                    if self.find.goto_fresh {
                        self.find.goto_fresh = false;
                        g.clear();
                    }
                    g.push_str(&s);
                }
                true
            }
        }
    }

    /// Backspace inside the focused box.
    pub fn find_backspace(&mut self) -> bool {
        if self.find.focus == FindFocus::Doc {
            return false;
        }
        if self.find.focus == FindFocus::Goto && self.find.goto_fresh {
            // Backspace on the "selected" prefilled line number removes it whole, not just its last digit
            self.find.goto_fresh = false;
            if let Some(g) = self.find.goto.as_mut() {
                g.clear();
            }
            return true;
        }
        if let Some(f) = self.find.field_mut() {
            f.pop();
        }
        if self.find.focus != FindFocus::Goto {
            self.invalidate_find();
            if self.find.focus == FindFocus::Pattern {
                self.find_incremental();
            }
        }
        true
    }

    /// Delete the whole content of the focused box.
    pub fn find_clear_field(&mut self) -> bool {
        if self.find.focus == FindFocus::Doc {
            return false;
        }
        if let Some(f) = self.find.field_mut() {
            f.clear();
        }
        if self.find.focus != FindFocus::Goto {
            self.invalidate_find();
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::state::{FIND_PANEL_H, FIND_ROW_H};
    use crate::ui::testutil::st_of;

    #[test]
    fn incremental_search_reveals_first_match_as_you_type() {
        // typing into the pattern box jumps to the first match live
        let mut st = st_of("foo bar baz foo");
        st.open_find_inline(false, false);
        assert!(st.find_insert("f"));
        assert!(st.find_insert("o"));
        assert!(st.find_insert("o"));
        assert_eq!(st.sel_range(), Some(0..3), "first match selected live");

        // a different pattern jumps straight to its own first match
        let mut st2 = st_of("foo bar baz foo");
        st2.open_find_inline(false, false);
        assert!(st2.find_insert("baz"));
        assert_eq!(st2.sel_range(), Some(8..11), "jumps to the baz match");

        // a pattern with no match reports it instead of panicking
        let mut st3 = st_of("abc");
        st3.open_find_inline(false, false);
        assert!(st3.find_insert("zzz"));
        assert_eq!(
            st3.find.message.as_ref().map(|m| m.key()),
            Some("Plain:FindNoMatch".to_string())
        );
    }

    #[test]
    fn find_next_selects_and_wraps() {
        let mut st = st_of("foo bar foo");
        st.open_find_inline(false, false);
        st.find.pattern = "foo".into();
        st.find_next();
        assert_eq!(st.sel_range(), Some(0..3));
        st.find_next();
        assert_eq!(st.sel_range(), Some(8..11));
        // wraps around
        st.find_next();
        assert_eq!(st.sel_range(), Some(0..3));
    }

    #[test]
    fn find_prev_walks_backwards() {
        let mut st = st_of("foo bar foo");
        st.open_find_inline(false, false);
        st.find.pattern = "foo".into();
        st.find_prev();
        assert_eq!(st.sel_range(), Some(8..11), "from the end first");
        st.find_prev();
        assert_eq!(st.sel_range(), Some(0..3));
    }

    #[test]
    fn find_all_fills_panel() {
        let mut st = st_of("a\nb a\nc a b a\n");
        st.open_find_inline(false, true);
        st.find.pattern = "a".into();
        st.find_all_matches();
        assert_eq!(st.find.results.len(), 4);
        assert!(st.find.panel);
        assert!(!st.find.stale);
        assert_eq!(st.find.cur, Some(0));
        // navigating uses the collected list
        st.find_next();
        assert_eq!(st.find.cur, Some(1));
        assert_eq!(
            st.find.message.as_ref().map(|m| m.key()),
            Some("With:FindPos".to_string())
        );
    }

    #[test]
    fn find_all_tracks_current_position() {
        let mut st = st_of("a\na\na");
        st.set_caret(4, false);
        st.find.pattern = "a".into();
        st.find_all_matches();
        assert_eq!(st.find.cur, Some(2), "first match at/after the caret");
    }

    #[test]
    fn replace_all_is_one_undo_step() {
        let mut st = st_of("one two one two one");
        st.open_find_inline(true, false);
        st.find.pattern = "one".into();
        st.find.replacement = "1".into();
        st.replace_all();
        assert_eq!(st.active_doc().read_all(), "1 two 1 two 1");
        assert_eq!(
            st.find.message.as_ref().map(|m| m.key()),
            Some("With:FindReplacedAll".to_string())
        );
        st.undo();
        assert_eq!(st.active_doc().read_all(), "one two one two one");
    }

    #[test]
    fn replace_all_regex_backreference() {
        let mut st = st_of("john smith\njane doe");
        st.open_find_inline(true, false);
        st.find.pattern = r"(\w+) (\w+)".into();
        st.find.mode = SearchMode::Regex;
        st.find.replacement = "$2, $1".into();
        st.replace_all();
        assert_eq!(st.active_doc().read_all(), "smith, john\ndoe, jane");
    }

    #[test]
    fn replace_one_advances() {
        let mut st = st_of("x x x");
        st.open_find_inline(true, false);
        st.find.pattern = "x".into();
        st.find.replacement = "y".into();
        st.replace_one();
        assert_eq!(st.active_doc().read_all(), "y x x");
        assert_eq!(st.sel_range(), Some(2..3), "the next match is selected");
        st.replace_one();
        assert_eq!(st.active_doc().read_all(), "y y x");
    }

    #[test]
    fn case_and_whole_word_flags() {
        let mut st = st_of("Cat cat category");
        st.find.pattern = "cat".into();
        st.find_all_matches();
        assert_eq!(st.find.results.len(), 3, "case-insensitive by default");
        st.find.case = true;
        st.find_all_matches();
        assert_eq!(st.find.results.len(), 2);
        st.find.word = true;
        st.find_all_matches();
        assert_eq!(st.find.results.len(), 1);
    }

    #[test]
    fn bad_regex_reports_instead_of_panicking() {
        let mut st = st_of("abc");
        st.find.pattern = "[".into();
        st.find.mode = SearchMode::Regex;
        st.find_next();
        assert!(st
            .find
            .message
            .as_ref()
            .map(|m| m.key())
            .unwrap_or_default()
            .contains("FindRegexError"));
        st.find_all_matches();
        assert!(st.find.results.is_empty());
    }

    #[test]
    fn goto_line_clamps_and_centres() {
        let mut st = st_of("l1\nl2\nl3\nl4\nl5");
        st.goto_line(3);
        assert_eq!(st.active_doc().line_of_pos(st.caret().pos), 2);
        st.goto_line(999);
        assert_eq!(st.active_doc().line_of_pos(st.caret().pos), 4);
        st.goto_line(0);
        assert_eq!(st.active_doc().line_of_pos(st.caret().pos), 0);
    }

    #[test]
    fn commit_goto_parses() {
        let mut st = st_of("a\nb\nc");
        st.open_goto();
        st.find.goto = Some("2".into());
        assert!(st.commit_goto());
        assert_eq!(st.active_doc().line_of_pos(st.caret().pos), 1);
        st.open_goto();
        st.find.goto = Some("x".into());
        assert!(!st.commit_goto());
        assert_eq!(
            st.find.message.as_ref().map(|m| m.key()),
            Some("Plain:GotoLineNotNumeric".to_string())
        );
    }

    /// Ctrl+G prefills the current line number, and the prefilled content is semantically
    /// **selected**: the first character replaces it, later characters append.
    ///
    /// Without this rule, pressing Ctrl+G on line 1 and typing `3` yields `13` —
    /// jumping to the last line while the user expected "go to line 3". Unit tests cannot
    /// see this defect (they assign `find.goto` directly); a GUI automation case hit it first.
    #[test]
    fn goto_box_replaces_prefilled_line_number() {
        let mut st = st_of("a\nb\nc\nd");
        st.open_goto();
        assert_eq!(
            st.find.goto.as_deref(),
            Some("1"),
            "must prefill the current line number"
        );
        assert!(st.find.goto_fresh);

        // first character replaces the prefilled value
        assert!(st.find_insert("3"));
        assert_eq!(st.find.goto.as_deref(), Some("3"));

        // later characters append (multi-digit line numbers type normally)
        assert!(st.find_insert("2"));
        assert_eq!(st.find.goto.as_deref(), Some("32"));

        assert!(st.commit_goto());
        assert_eq!(st.active_doc().line_of_pos(st.caret().pos), 3); // clamped to the last line

        // backspace removes the "selected" prefilled value whole, not just its last digit
        st.open_goto();
        assert!(st.find_backspace());
        assert_eq!(st.find.goto.as_deref(), Some(""));
        assert!(st.find_insert("2"));
        assert_eq!(st.find.goto.as_deref(), Some("2"));
        assert!(st.commit_goto());
        assert_eq!(st.active_doc().line_of_pos(st.caret().pos), 1);

        // the flag must be reset after closing the box, not leak into the next one
        st.open_goto();
        st.close_find();
        assert!(!st.find.goto_fresh);
    }

    #[test]
    fn virtual_focus_routes_text() {
        let mut st = st_of("text");
        st.open_find_inline(false, false);
        assert!(st.find_insert("ab"));
        assert_eq!(st.find.pattern, "ab");
        assert_eq!(st.active_doc().read_all(), "text", "document untouched");
        assert!(st.find_backspace());
        assert_eq!(st.find.pattern, "a");
        // hand the keyboard back to the document
        st.set_find_focus(FindFocus::Doc);
        assert!(!st.find_insert("z"));
        st.insert_text("z");
        assert_eq!(st.active_doc().read_all(), "ztext");
    }

    #[test]
    fn editing_invalidates_results() {
        let mut st = st_of("a a a");
        st.find.pattern = "a".into();
        st.find_all_matches();
        assert!(!st.find.stale);
        st.insert_text("b");
        st.invalidate_find();
        assert!(st.find.stale);
    }

    /// Child-window mode must not reserve the in-page strip: doing so left an
    /// unpainted `FIND_ROW_H` band above the text after Ctrl+F (the leftover of
    /// the old inline-only bar — reported as "a white row at the top of the
    /// editor"). The failure mode is an ordering one, so the test drives the
    /// same helper `open_find_child` uses instead of setting the flag itself.
    #[test]
    fn child_window_find_reserves_no_inline_strip() {
        let mut st = st_of("x");
        st.set_window_size(1200.0, 800.0);
        let (y0, h0) = (st.view.y, st.view.h);
        st.enter_find_child_mode(false, false);
        assert_eq!(st.find.bar_height(), 0.0, "the popup draws the bar itself");
        assert!(
            (st.view.y - y0).abs() < 0.001,
            "the viewport must start right under the tab strip: {} vs {y0}",
            st.view.y
        );
        assert!((st.view.h - h0).abs() < 0.001, "and keep its full height");
        st.close_find();
        assert!((st.view.y - y0).abs() < 0.001);
    }

    #[test]
    fn bar_geometry_shrinks_viewport() {
        let mut st = st_of("x");
        st.set_window_size(1200.0, 800.0);
        let base = st.view.h;
        st.open_find_inline(false, false);
        assert!(
            st.view.h < base - FIND_ROW_H + 0.001,
            "{} vs {}",
            st.view.h,
            base
        );
        assert!((st.view.y - (crate::ui::state::CHROME_TOP + FIND_ROW_H)).abs() < 0.001);
        st.find.results.push(Match { start: 0, end: 1 });
        st.find.panel = true;
        st.relayout();
        assert!((st.view.h - (base - FIND_ROW_H - FIND_PANEL_H)).abs() < 0.001);
        st.close_find();
        assert!((st.view.h - base).abs() < 0.001);
    }
}

#[cfg(test)]
mod async_search_tests {
    use super::*;
    use crate::ui::testutil::st_of;
    use std::time::Duration;

    /// Build a document exceeding [`ASYNC_SEARCH_THRESHOLD`] (about 5.3MB).
    fn big_doc(st: &mut EditorState) {
        let row = "hello world this is a reasonably long line of text\n";
        let text = row.repeat(120_000);
        st.insert_text(&text);
    }

    fn wait_done(st: &mut EditorState, ms: u64) -> bool {
        for _ in 0..(ms / 5) {
            st.poll_search();
            if st.search_job.is_none() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        st.poll_search();
        st.search_job.is_none()
    }

    /// "Find all" on a large document goes to the background: progress shows first, and results
    /// land in the panel as usual on completion.
    #[test]
    fn find_all_runs_in_background_for_large_documents() {
        let mut st = st_of("");
        big_doc(&mut st);
        st.open_find_inline(false, false);
        st.find.pattern = "hello".into();
        st.find_all_matches();
        assert!(
            st.search_job.is_some(),
            "over the threshold must go to the background"
        );
        assert!(
            st.status
                .as_ref()
                .map(|m| m.key())
                .unwrap_or_default()
                .contains("StatusFindAllScanning"),
            "{}",
            st.status_text()
        );

        assert!(
            wait_done(&mut st, 5000),
            "the background search should finish"
        );
        assert!(
            st.status
                .as_ref()
                .map(|m| m.key())
                .unwrap_or_default()
                .contains("StatusFindAll"),
            "{}",
            st.status_text()
        );
        assert!(!st.find.results.is_empty());
        assert!(st.find.panel);
        assert!(
            !st.find.stale,
            "document untouched, results should be fresh"
        );
        // the results panel is navigable
        st.find_next();
        assert!(st.find.cur.is_some());
    }

    /// "Replace all" on a large document: background scan, one transaction written back on
    /// completion, one undo step.
    ///
    /// Match count is kept within FIND_LIMIT (one hello every 5 lines ≈ 26k matches), so the
    /// assertion is "complete replacement", not capped-truncation semantics.
    #[test]
    fn replace_all_scans_in_background_and_applies_on_completion() {
        let mut st = st_of("");
        // ~10.4MB, above the 4MB threshold; 26_000 hellos < FIND_LIMIT (50k)
        for i in 0..130_000 {
            let row = if i % 5 == 0 {
                "hello mark on this line with some padding text\n"
            } else {
                "plain filler line with plenty of other words here\n"
            };
            st.insert_text(row);
        }
        assert!(st.active_doc().table.len() > super::ASYNC_SEARCH_THRESHOLD);
        let before = st.active_doc().table.len();
        st.open_find_inline(true, false);
        st.find.pattern = "hello".into();
        st.find.replacement = "bye".into();
        st.replace_all();
        assert!(
            st.search_job.is_some(),
            "the replace scan should go to the background"
        );

        assert!(wait_done(&mut st, 8000));
        assert!(
            st.status
                .as_ref()
                .map(|m| m.key())
                .unwrap_or_default()
                .contains("Replaced"),
            "{}",
            st.status_text()
        );
        assert_eq!(
            st.active_doc().read_all().find("hello"),
            None,
            "replace should complete fully"
        );
        // hello(5) → bye(3): 26_000 matches × -2 each
        assert_eq!(st.active_doc().table.len(), before - 26_000 * 2);
        st.undo();
        assert_eq!(
            st.active_doc().table.len(),
            before,
            "replace-all is one undo step"
        );
    }

    /// Esc cancels: results are discarded, the document is untouched.
    #[test]
    fn cancel_discards_results_without_touching_the_document() {
        let mut st = st_of("");
        big_doc(&mut st);
        let before = st.active_doc().read_all();
        st.open_find_inline(false, false);
        st.find.pattern = "hello".into();
        st.find_all_matches();
        assert!(
            st.cancel_search(),
            "should report that cancellation was accepted"
        );
        assert!(wait_done(&mut st, 5000));
        assert!(
            st.status
                .as_ref()
                .map(|m| m.key())
                .unwrap_or_default()
                .contains("Cancel"),
            "{}",
            st.status_text()
        );
        assert!(
            st.find.results.is_empty(),
            "cancelled results must not enter the panel"
        );
        assert_eq!(
            st.active_doc().read_all(),
            before,
            "cancellation must not touch the document"
        );
    }

    /// Small documents stay on the synchronous path (tests and existing behavior depend on it).
    #[test]
    fn small_documents_stay_synchronous() {
        let mut st = st_of("a b a b a");
        st.open_find_inline(false, false);
        st.find.pattern = "a".into();
        st.find_all_matches();
        assert!(
            st.search_job.is_none(),
            "small documents must not spawn a thread"
        );
        assert_eq!(st.find.results.len(), 3);
    }
}
