//! Render-path healing before drawing.
//!
//! `viewport::build` is pure-read rendering: the derived state it depends on
//! (JSONL/JSON tree caches, hidden-row mapping, horizontal width scan,
//! out-of-range caret clamping) is synchronized once **before** rendering by
//! [`EditorState::self_heal`]. These syncs used to live at the top of `build`
//! — but build must take `&mut` via `Signal::write()`, whose guard
//! **unconditionally mark_dirty**s on drop, so every render self-dirtied and
//! the whole app always rendered one extra round.
//!
//! The component is now two-phase:
//!
//! ```ignore
//! if state.read().needs_heal() {
//!     state.write().self_heal(); // mark_dirty → next frame (a duplicate frame with identical content)
//! }
//! let vm = { let st = state.read(); build(&st) };
//! ```
//!
//! Convergence: every probe condition of [`EditorState::needs_heal`] is
//! **verbatim identical to the early-return branch of the corresponding fix**
//! (the fast paths of `stale`/`hidden_stale`/`pump_h_extent`), so after healing
//! runs the probe must turn false — except the horizontal scan, which by
//! design keeps advancing toward `done` (a finite number of times) while
//! unfinished. In the steady idle state with the scan finished, the render
//! path does zero writes and zero mark_dirty.

/// Rows scanned per healing step by the horizontal width scan (the former
/// `viewport::H_SCAN_BUDGET`).
///
/// 256 rows is ~1–2 ms even with CJK and tab stops, and the scan itself is
/// invisible; a 100k-line document converges within seconds, and the visible
/// window is folded per frame as a fallback anyway.
pub const H_SCAN_BUDGET: usize = 256;

use super::json::JsonView;
use super::jsonl::RowRef;
use super::{EditorState, PAD_TOP};

impl EditorState {
    /// The visible document line range (including JSONL fold expansion and
    /// hidden-row shrinkage).
    ///
    /// `viewport::build` and the heal probes share this one computation —
    /// implementing it twice would drift sooner or later.
    pub fn visible_doc_range(&self) -> (u64, u64) {
        if !self.wrap_enabled() {
            return self.visible_doc_range_screen();
        }
        let lh = self.metrics.lh.max(1.0);
        let text_h = self.text_h().max(40.0);
        let mut first_f = (self.scroll_top - PAD_TOP as f64) / lh as f64;
        if !first_f.is_finite() || first_f < 0.0 {
            first_f = 0.0;
        }
        let first = first_f.floor() as u64;
        let visible = ((text_h / lh).ceil() as u64).max(1) + 2;
        let last = (first + visible).min(self.visual_rows());
        let tab = &self.tabs[self.active];
        let mut lo = u64::MAX;
        let mut hi = 0u64;
        for v in first..last {
            let (s, _) = self.screen_of_visual(v);
            if let RowRef::Doc(l) = tab.jsonl.row_at(self.jsonl_row_of(s)) {
                lo = lo.min(l);
                hi = hi.max(l + 1);
            }
        }
        let lines = tab.doc.line_count().max(1);
        if hi == 0 {
            (first.min(lines), first.min(lines))
        } else {
            (lo, hi.min(lines))
        }
    }

    /// Visible document line range ignoring soft wrap (used by wrap refresh
    /// to avoid recursion).
    pub fn visible_doc_range_screen(&self) -> (u64, u64) {
        let lh = self.metrics.lh.max(1.0);
        let text_h = self.text_h().max(40.0);
        let mut first_f = (self.scroll_top - PAD_TOP as f64) / lh as f64;
        if !first_f.is_finite() || first_f < 0.0 {
            first_f = 0.0;
        }
        let first = first_f.floor() as u64;
        let visible = ((text_h / lh).ceil() as u64).max(1) + 2;
        let last = (first + visible).min(self.screen_rows());
        let tab = &self.tabs[self.active];
        let mut lo = u64::MAX;
        let mut hi = 0u64;
        for r in first..last {
            if let RowRef::Doc(l) = tab.jsonl.row_at(self.jsonl_row_of(r)) {
                lo = lo.min(l);
                hi = hi.max(l + 1);
            }
        }
        let lines = tab.doc.line_count().max(1);
        if hi == 0 {
            (first.min(lines), first.min(lines))
        } else {
            (lo, hi.min(lines))
        }
    }

    /// Fold the visible lines to the horizontal width and record the
    /// **measured range** into `h_extent.visible` (the probe key for "was the
    /// folding done", see [`Self::needs_heal`]).
    ///
    /// The range leaves one screen of slack on both sides. Scrolling happens
    /// line by line: measuring only the current screen would make the probe
    /// true again on every scrolled line — the render path would write a
    /// signal (`mark_dirty`) and rescan a screen once per line, turning one
    /// wheel tick into two full renders (on big files this is exactly the
    /// source of "scroll once, stutter twice"). With one screen of slack,
    /// healing happens once per scrolled screen, amortizing the scan across
    /// frames.
    pub fn measure_visible(&mut self, first: u64, last: u64) {
        let n = last.saturating_sub(first).max(1);
        let lines = self.tab().doc.line_count();
        let lo = first.saturating_sub(n);
        let hi = last.saturating_add(n).min(lines.max(last));
        self.measure_lines(lo, hi);
        let id = self.tab().doc.id;
        self.h_extent.visible = Some((id, lo, hi));
    }

    /// Whether healing is needed before rendering. **Every condition must be
    /// verbatim identical to the early-return branch of the corresponding fix
    /// in `self_heal`**: a probe too loose ⇒ infinite re-rendering while
    /// idle; too tight ⇒ missed healing.
    pub fn needs_heal(&self) -> bool {
        let d = self.active_doc();
        let c = self.caret();
        // sanitize_carets: caret/anchor out of range
        if c.pos > d.table.len() || c.anchor.is_some_and(|a| a > d.table.len()) {
            return true;
        }
        // JSONL / JSON tree cache generations
        let tab = &self.tabs[self.active];
        if tab.jsonl.stale(d) || tab.json.stale(d, self.locale) || self.hidden_stale() {
            return true;
        }
        // reveal_caret: caret/anchor on a hidden line and movable out
        let hidden_caret = |pos: u64| -> bool {
            let line = self.active_doc().line_of_pos(pos);
            self.line_hidden(line) && self.nearest_visible_line(line) != line
        };
        if hidden_caret(c.pos) || c.anchor.is_some_and(hidden_caret) {
            return true;
        }
        // One-time notice for oversized JSON
        if self.json_warn_pending() {
            return true;
        }
        // Soft-wrap index expired (including unmeasured visible lines)
        if self.wrap_stale() {
            return true;
        }
        // pump_h_extent: document/generation switch must reset and resample.
        //
        // **"Scan unfinished" is NOT here**: healing writes a signal during
        // rendering, so treating it as a probe condition equals "heal every
        // frame" — a 300k-line document at 256 rows/frame takes 1172 frames,
        // and after opening a big file the window spins at 60fps for nearly
        // 20 seconds (measured idle CPU 76%), competing with scrolling for
        // frames. The scan instead advances **only on frames that exist
        // anyway** (the `pump_h_extent` in `self_heal`; scrolling/editing
        // triggers frames), and huge documents are capped outright by
        // `H_SCAN_LINES` (see `sample_h_extent`).
        if self.h_extent.doc_id != d.id || self.h_extent.gen != d.gen {
            return true;
        }
        // measure_visible: the visible range escaped the last folded window
        // (the window has one screen of slack, see `measure_visible` —
        // scrolling line by line must not heal once per line)
        let (doc_first, doc_last) = self.visible_doc_range();
        match self.h_extent.visible {
            Some((id, lo, hi)) => id != d.id || doc_first < lo || doc_last > hi,
            None => true,
        }
    }

    /// Whether the oversized-JSON document still owes its one-time notice
    /// (matches the early-return condition of `json_auto_warn_too_large`).
    fn json_warn_pending(&self) -> bool {
        let tab = &self.tabs[self.active];
        tab.spec.name == "JSON"
            && !tab.json.enabled
            && !tab.json.notified_too_large
            && JsonView::is_too_large(&tab.doc)
    }

    /// All pre-render healing. Each step has an O(1) fast path (key
    /// comparison), so event handlers can also call it directly after
    /// mutating state, folding the healing cost into the same write.
    ///
    /// Order-sensitive: cache refreshes (2/3) must precede caret correction
    /// (3/4) and the visible range (5) — the latter build on the fresh
    /// caches.
    pub fn self_heal(&mut self) {
        // 1) clamp out-of-range carets
        self.sanitize_carets();
        // 2) JSONL / JSON tree cache sync (active tab, same scope as the old build)
        {
            let loc = self.locale;
            let tab = self.tab_mut();
            tab.jsonl.refresh(&tab.doc);
            tab.json.refresh(&tab.doc, loc);
        }
        // 3) hidden-row mapping + in-place correction of carets on hidden rows
        self.refresh_hidden();
        self.reveal_caret();
        // 4) one-time oversized-JSON notice
        self.json_auto_warn_too_large();
        // 5) soft wrap: after the hidden-row mapping, before the visible range
        self.refresh_wrap();
        // 6) horizontal width: progressive scan advance + visible-line folding
        self.pump_h_extent(H_SCAN_BUDGET);
        let (doc_first, doc_last) = self.visible_doc_range();
        self.measure_visible(doc_first, doc_last);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::testutil::st_of_view;

    /// Heal in a loop until convergence, with a round-count guard.
    fn heal_to_convergence(st: &mut EditorState, cap: usize) -> usize {
        let mut rounds = 0;
        while st.needs_heal() {
            st.self_heal();
            rounds += 1;
            assert!(
                rounds <= cap,
                "healing does not converge (still advancing after {rounds} rounds)"
            );
        }
        rounds
    }

    /// **Convergence guard**: after `self_heal` runs, `needs_heal` must turn
    /// false. If probe and fix ever drift, the render loop self-perpetuates
    /// forever (mark_dirty every frame) — this test welds that regression
    /// path shut. The horizontal width scan advances over multiple rounds
    /// toward done by design while unfinished.
    #[test]
    fn self_heal_converges_for_common_states() {
        for text in ["", "single", "a\nb\nc\n", "中文行\nwith \"tokens\"\n"] {
            let mut st = st_of_view(text);
            assert!(
                st.needs_heal(),
                "a new document/view should need healing: {text:?}"
            );
            heal_to_convergence(&mut st, 64);
            assert!(!st.needs_heal(), "must converge after healing: {text:?}");
            // Idempotent: another round is still a no-op
            st.self_heal();
            assert!(!st.needs_heal(), "healing must be idempotent: {text:?}");
        }
    }

    /// Large document: the horizontal scan advances over multiple "frames"
    /// (heals) and finishes; it still converges after an edit.
    ///
    /// Scan does not auto-trigger recheck (see `Self::needs_heal` comment), so here we explicitly
    /// heal repeatedly to simulate consecutive frames.
    #[test]
    fn self_heal_scans_to_done_then_converges_again_after_edit() {
        let mut text = String::new();
        for i in 0..5000 {
            text.push_str(&format!("line {i} with some width\n"));
        }
        let mut st = st_of_view(&text);
        for _ in 0..64 {
            if st.h_extent.done {
                break;
            }
            st.self_heal();
        }
        assert!(st.h_extent.done, "the scan should eventually finish");
        heal_to_convergence(&mut st, 8);
        assert!(!st.needs_heal(), "should converge after the scan finishes");
        st.insert_text("x");
        heal_to_convergence(&mut st, 8);
        assert!(
            !st.needs_heal(),
            "healing should still converge after an edit"
        );
    }

    /// Huge document: after cross-line sampling it caps outright and must
    /// **not** demand healing every frame because "the scan is unfinished".
    ///
    /// That probe once pinned the idle render loop at 60fps: 300k lines ×
    /// 256 rows/frame = 1172 frames, burning nearly 20 seconds of CPU after
    /// opening a big file (measured 76% of one core).
    #[test]
    fn huge_document_does_not_ask_for_a_frame_per_line() {
        // The cap itself lives in `commands::edit` (a private constant); only
        // its magnitude matters here: 120k lines far exceeds any sane
        // "scan line by line" range.
        let lines = 120_000u64;
        let text = "x\n".repeat(lines as usize);
        let mut st = st_of_view(&text);
        st.self_heal();
        assert!(
            st.h_extent.done,
            "beyond H_SCAN_LINES should cap via sampling"
        );
        assert!(
            !st.needs_heal(),
            "even a huge document must not heal every frame"
        );
    }

    /// Scrolling line by line must **not** trigger healing per line (each
    /// heal is a render-path signal write = one extra full render). The
    /// window has one screen of slack: refolding is only needed after
    /// scrolling past a screen.
    #[test]
    fn scrolling_a_few_lines_does_not_heal_every_line() {
        let mut text = String::new();
        for i in 0..400 {
            text.push_str(&format!("line {i} with some width\n"));
        }
        let mut st = st_of_view(&text);
        heal_to_convergence(&mut st, 64);
        assert!(!st.needs_heal());
        // Scroll two lines: still inside the measured window
        st.scroll_top += st.metrics.lh as f64 * 2.0;
        assert!(
            !st.needs_heal(),
            "line-by-line scrolling must not heal once per line"
        );
        // Scroll past a screen: the visible range escapes the window, must refold
        st.scroll_top += st.text_h() as f64 * 1.5;
        assert!(
            st.needs_heal(),
            "visible lines must be refolded after leaving the measured window"
        );
        heal_to_convergence(&mut st, 8);
        assert!(!st.needs_heal());
    }

    /// Healing must also converge with word wrap on (wrap index scan and
    /// visible range do not recurse into each other).
    #[test]
    fn self_heal_converges_with_word_wrap() {
        let mut st = st_of_view("aaa bbb ccc ddd eee fff\ntail\n");
        st.view.w = 180.0;
        st.wrap = true;
        assert!(st.needs_heal());
        heal_to_convergence(&mut st, 64);
        assert!(!st.needs_heal(), "healing should converge with wrap on");
        assert!(st.visual_rows() > st.screen_rows());
    }

    /// Out-of-range caret (after undo shrinks the document) → healing clamps
    /// and converges.
    #[test]
    fn self_heal_clamps_out_of_range_caret() {
        let mut st = st_of_view("abc");
        st.tabs[0].caret.pos = 999;
        st.tabs[0].caret.anchor = Some(500);
        assert!(st.needs_heal());
        st.self_heal();
        assert_eq!(st.caret().pos, 3);
        assert_eq!(st.caret().anchor, Some(3));
        assert!(!st.needs_heal());
    }

    /// Caret on a hidden line → reveal moves it to the nearest visible line
    /// and converges (the probe's `nearest_visible_line != line` condition
    /// must match `reveal_caret`).
    #[test]
    fn self_heal_reveals_caret_on_hidden_line() {
        let mut st = st_of_view("a\nb\nc\nd\n");
        st.set_caret(4, false); // line 2 (c)
        st.hide_lines();
        assert!(st.line_hidden(2), "hiding line 2 should take effect");
        // Manually put the caret back on a hidden line (e.g. find jumps)
        st.tabs[0].caret.pos = 4;
        assert!(
            st.needs_heal(),
            "a caret on a hidden line should need healing"
        );
        st.self_heal();
        let caret_line = st.active_doc().line_of_pos(st.caret().pos);
        assert!(
            !st.line_hidden(caret_line),
            "after healing the caret should land on a visible line"
        );
        assert!(!st.needs_heal(), "should converge after reveal");
    }
}
