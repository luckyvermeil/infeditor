//! Find-bar state: virtual focus, pattern/replacement, the all-matches
//! results list and the goto-line box.

use super::{FIND_PANEL_H, FIND_ROW_H};

/// Which field of the find bar owns the keyboard.
///
/// The editor has exactly one real DOM focus target (the IME composer), so the
/// find bar uses *virtual* focus: keystrokes are routed by this enum instead of
/// moving DOM focus (blitz panics when `set_focus` runs inside a dioxus task).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FindFocus {
    /// keyboard belongs to the document
    Doc,
    Pattern,
    Replacement,
    Goto,
}

/// State of the find/replace bar, its results and the goto-line box.
#[derive(Clone)]
pub struct FindState {
    /// bar visible
    pub open: bool,
    /// The find bar shows as a separate child window (Blitz-rendered, see
    /// `ui::findwin` / `ui::shell`), floating above the editor and taking no
    /// layout space
    pub child_win: bool,
    /// second row (replacement) visible
    pub replace: bool,
    /// virtual focus owner
    pub focus: FindFocus,
    pub pattern: String,
    pub replacement: String,
    pub mode: crate::search::SearchMode,
    pub case: bool,
    pub word: bool,
    /// all matches of the last Find All (sorted, non-overlapping)
    pub results: Vec<crate::search::Match>,
    /// index into `results` of the current match
    pub cur: Option<usize>,
    /// the result list hit `FIND_LIMIT`
    pub truncated: bool,
    /// results are from an older document generation
    pub stale: bool,
    /// results panel visible
    pub panel: bool,
    /// index of the first row shown in the results panel
    pub panel_top: usize,
    /// message shown on the right of the bar
    pub message: Option<i18n::Message>,
    /// goto-line box (`Some` = open), holds the typed text
    pub goto: Option<String>,
    /// The "current line number" pre-filled in the goto box has not been
    /// overwritten by user input yet.
    ///
    /// Ctrl+G pre-fills the current line number **selected**: the user's
    /// first character should **replace** it, not append. True selection
    /// state is not modeled for this single-line box, so this flag stands
    /// in — the first insert/backspace clears it first. Without it, Ctrl+G
    /// on line 1 then typing 3 yields "13" (jump to the last line); GUI
    /// automation caught this defect (see `tests_py/test_goto.py`).
    pub goto_fresh: bool,
    /// document generation the results were produced for
    pub gen: u64,
    /// document identity the results were produced for. `gen` restarts at 0
    /// for every document, so generation alone could not tell two tabs apart:
    /// after Find All in tab A, switching to a freshly opened tab B (same gen)
    /// kept the results "valid" and painted A's byte offsets over B's text.
    pub doc_id: u64,
    // ---- find dialog extensions (find-dialog semantics) ----
    /// Direction radio: true = up (decides which way "Find Next" goes)
    pub up: bool,
    /// Wrap search: loop back after reaching the end
    pub wrap: bool,
    /// Search/replace only within the current selection
    pub in_sel: bool,
    /// Search history (newest first, deduplicated, ≤30, per session)
    pub history: Vec<String>,
    /// Dialog geometry (screen logical coords x, y, w, h; written back after drag/resize, persisted with settings)
    pub popup_rect: Option<[f32; 4]>,
    /// IME composition string in the focused in-page find box (pinyin
    /// pre-edit, candidate window not yet committed). editor.rs's
    /// WindowEvent::Ime(Preedit) writes it while the find box captures the
    /// keyboard; cleared on commit/clear. Rendered as an accent-colored
    /// composition string — the composer textarea is invisible, and without
    /// mirroring it, "typing pinyin gives no feedback".
    pub preedit: String,
}

impl Default for FindState {
    fn default() -> Self {
        FindState {
            open: false,
            child_win: false,
            replace: false,
            focus: FindFocus::Doc,
            pattern: String::new(),
            replacement: String::new(),
            mode: crate::search::SearchMode::Plain,
            doc_id: 0,
            case: false,
            word: false,
            results: Vec::new(),
            cur: None,
            truncated: false,
            stale: false,
            panel: false,
            panel_top: 0,
            message: None,
            goto: None,
            goto_fresh: false,
            gen: 0,
            up: false,
            wrap: true,
            in_sel: false,
            history: Vec::new(),
            popup_rect: None,
            preedit: String::new(),
        }
    }
}

impl FindState {
    /// Height of the docked bar (0 when closed or shown as a floating
    /// child window — the child window overlays the editor instead of
    /// taking layout space).
    pub fn bar_height(&self) -> f32 {
        if !self.open || self.child_win {
            0.0
        } else if self.replace {
            FIND_ROW_H * 2.0
        } else {
            FIND_ROW_H
        }
    }

    /// Height of the results panel (0 when hidden).
    pub fn panel_height(&self) -> f32 {
        if self.open && self.panel && !self.results.is_empty() {
            FIND_PANEL_H
        } else {
            0.0
        }
    }

    /// True while the keyboard is captured by one of the boxes.
    ///
    /// The find bar uses *virtual* focus: while a box owns the keyboard the
    /// composer must not intercept anything.
    pub fn captured(&self) -> bool {
        self.focus != FindFocus::Doc
    }

    /// Results may only be *used* (painted, navigated, listed) while they still
    /// describe the current document. `gen` is bumped by every edit, so this
    /// cannot be forgotten the way a manual `stale = true` can — and stale byte
    /// offsets can point past the end of a shrunk buffer.
    pub fn valid_for(&self, doc_id: u64, gen: u64) -> bool {
        !self.results.is_empty() && !self.stale && self.gen == gen && self.doc_id == doc_id
    }

    /// How many result rows fit in the results panel.
    pub fn rows_per_page(&self) -> usize {
        (((FIND_PANEL_H - 24.0) / 18.0).floor() as usize).max(1)
    }

    /// Scroll the results panel so row `i` is visible.
    pub fn ensure_visible(&mut self, i: usize) {
        let n = self.rows_per_page();
        let total = self.results.len();
        // Second branch: when the window has run past the end of the results
        // (`panel_top + n > total`), push i to the window's first row (the
        // end clamp below backs it up). The original condition had an extra
        // `n`, which cancels out and is independent of i — clearly a typo.
        if i < self.panel_top || self.panel_top + n > total {
            self.panel_top = i;
        } else if i + 1 > self.panel_top + n {
            self.panel_top = i + 1 - n;
        }
        let max = total.saturating_sub(n);
        if self.panel_top > max {
            self.panel_top = max;
        }
    }

    /// The mutable text of the currently focused box.
    pub fn field_mut(&mut self) -> Option<&mut String> {
        match self.focus {
            FindFocus::Pattern => Some(&mut self.pattern),
            FindFocus::Replacement => Some(&mut self.replacement),
            FindFocus::Goto => self.goto.as_mut(),
            FindFocus::Doc => None,
        }
    }

    /// The text of the currently focused box.
    pub fn field(&self) -> Option<&str> {
        match self.focus {
            FindFocus::Pattern => Some(&self.pattern),
            FindFocus::Replacement => Some(&self.replacement),
            FindFocus::Goto => self.goto.as_deref(),
            FindFocus::Doc => None,
        }
    }
}
