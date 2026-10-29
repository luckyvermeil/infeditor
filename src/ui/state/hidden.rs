//! Hidden lines (editor right-click → Hide lines / Show all hidden lines)
//! and the screen-row mapping.
//!
//! # Row spaces
//!
//! A document's "screen row count" is no longer the line count: hiding a line
//! removes a row. JSONL expansion introduces **virtual rows** too (see
//! [`super::jsonl`]), so three spaces must be aligned:
//!
//! ```text
//! doc lines  --jsonl.vstart-->  JSONL rows  --this module-->  screen rows
//! ```
//!
//! The conversion depends on one thing only: each hidden line's position in
//! JSONL row space (`vstarts`, ascending). `screen row r → JSONL row` is
//! "adding the hidden rows back one by one"; the reverse subtracts them. The
//! two directions are inverse operations — pure functions, unit-testable
//! (see end of file).
//!
//! # Cache
//!
//! `vstarts` is taken from `jsonl.vstart(hidden line)` and changes with the
//! document (its id and generation), line count and expansion state, so
//! [`EditorState::refresh_hidden`] validates invalidation by the key
//! `(doc_id, gen, lines, jsonl_total, jsonl_open, hidden_len)` — in the common
//! case (no hidden lines) it is one HashMap lookup then return, matching the
//! performance contract of `jsonl::refresh`. The document id is part of the
//! key because `gen` starts at zero for every document: two tabs with the
//! same line count and one hidden line each used to share one cache slot.
//!
//! # Interaction with JSONL
//!
//! Hiding a line first collapses that line's expanded block: otherwise the
//! hidden doc line would still carry a string of virtual child rows and the
//! row counts would no longer match. Conversely, refresh guarantees each
//! frame that "hidden rows have no expanded block".

use crate::ui::state::EditorState;

use super::{Fmt, Msg};

/// Invalidation key of the `vstarts` snapshot.
type HiddenKey = (u64, u64, u64, u64, usize, usize);

/// Hidden JSONL row position cache (see module comment).
#[derive(Clone, Debug)]
pub struct HiddenCache {
    key: HiddenKey,
    /// Position of each hidden line in JSONL row space, ascending
    vstarts: Vec<u64>,
}

impl Default for HiddenCache {
    fn default() -> Self {
        Self {
            key: (u64::MAX, u64::MAX, 0, 0, 0, 0),
            vstarts: Vec::new(),
        }
    }
}

impl EditorState {
    // ---- refresh ---------------------------------------------------------------

    /// Called once per frame at the top of `viewport::build` (alongside
    /// `jsonl::refresh`).
    ///
    /// Common case (no hidden lines): one HashMap lookup then return. Key
    /// changed (edit / expand / hide changes) ⇒ drop out-of-range line
    /// numbers, collapse expanded blocks on hidden lines, rebuild `vstarts`.
    pub fn refresh_hidden(&mut self) {
        let (id, gen, lines) = {
            let d = self.active_doc();
            (d.id, d.gen, d.line_count().max(1))
        };
        // Out-of-range line numbers (document shrank) are dropped outright
        if let Some(set) = self.hidden.get_mut(&id) {
            set.retain(|&l| l < lines);
            if set.is_empty() {
                self.hidden.remove(&id);
            }
        }
        // Hidden rows must not have expanded blocks (see module comment)
        if self.hidden.contains_key(&id) {
            let set = &self.hidden[&id];
            let tab = &mut self.tabs[self.active];
            tab.jsonl.open.retain(|line, _| !set.contains(line));
        }
        let Some(set) = self.hidden.get(&id) else {
            self.hidden_cache = None;
            return;
        };
        let hidden_len = set.len();
        let tab = &self.tabs[self.active];
        let jv_total = tab.jsonl.total_rows(lines);
        let jv_open = tab.jsonl.open.len();
        let key = (id, gen, lines, jv_total, jv_open, hidden_len);
        if let Some(c) = &self.hidden_cache {
            if c.key == key {
                return;
            }
        }
        let mut vstarts: Vec<u64> = set.iter().map(|&l| tab.jsonl.vstart(l)).collect();
        vstarts.sort_unstable();
        self.hidden_cache = Some(HiddenCache { key, vstarts });
    }

    /// Rebuild the cache (used by command entries: `refresh_hidden`'s key
    /// excludes the *content* of `hidden`, only the count, and swapping
    /// "hide line 3" for "hide line 8" may not change the count).
    fn rebuild_hidden_cache(&mut self) {
        self.hidden_cache = None;
        self.refresh_hidden();
    }

    /// Read-only probe of whether [`Self::refresh_hidden`] would do real work
    /// (used by pre-render healing). The conditions must be verbatim
    /// identical to that method's early-return/work branches, or healing
    /// diverges.
    /// "A change in the *content* of `hidden`" is not covered here — it is
    /// invalidated explicitly by the command-entry `rebuild_hidden_cache`
    /// (this method's key only counts, it cannot detect content changes).
    pub fn hidden_stale(&self) -> bool {
        let id = self.active_doc().id;
        let lines = self.active_doc().line_count().max(1);
        let Some(set) = self.hidden.get(&id) else {
            // Common case: no hidden lines ⇒ the cache should be None
            return self.hidden_cache.is_some();
        };
        if set.is_empty() {
            return true; // refresh removes the empty set
        }
        if set.iter().any(|&l| l >= lines) {
            return true; // refresh drops out-of-range line numbers
        }
        let tab = &self.tabs[self.active];
        let jv_total = tab.jsonl.total_rows(lines);
        let key = (
            self.active_doc().id,
            self.active_doc().gen,
            lines,
            jv_total,
            tab.jsonl.open.len(),
            set.len(),
        );
        !self.hidden_cache.as_ref().is_some_and(|c| c.key == key)
    }

    // ---- queries ---------------------------------------------------------------

    /// Number of hidden lines in the current document.
    pub fn hidden_count(&self) -> u64 {
        let id = self.active_doc().id;
        self.hidden.get(&id).map(|s| s.len()).unwrap_or(0) as u64
    }

    /// Whether this document line is hidden.
    pub fn line_hidden(&self, line: u64) -> bool {
        let id = self.active_doc().id;
        self.hidden.get(&id).is_some_and(|s| s.contains(&line))
    }

    /// `vstarts` from the cache (empty when stale ⇒ the mapping degenerates
    /// to identity).
    fn hidden_vstarts(&self) -> &[u64] {
        self.hidden_cache
            .as_ref()
            .map(|c| c.vstarts.as_slice())
            .unwrap_or(&[])
    }

    // ---- conversions between the three spaces -------------------------------------------------

    /// Total screen rows = JSONL rows − hidden rows (used for the scroll
    /// bound).
    ///
    /// With the whole-document JSON tree view on, the screen no longer shows
    /// doc lines at all — everything is virtual rows expanded by the tree —
    /// so [`crate::ui::state::json::JsonView::total_rows`] is used directly.
    pub fn screen_rows(&self) -> u64 {
        let tab = &self.tabs[self.active];
        let lines = tab.doc.line_count().max(1);
        if tab.json.enabled {
            return tab.json.total_rows(lines);
        }
        tab.jsonl
            .total_rows(lines)
            .saturating_sub(self.hidden_count())
    }

    /// Count of entries in `vstarts` that are `<= v` (how many rows are
    /// hidden before JSONL row v).
    fn hidden_up_to(&self, v: u64) -> u64 {
        let vh = self.hidden_vstarts();
        vh.partition_point(|&x| x <= v) as u64
    }

    /// Start of document line L in **screen row** space (caret positioning,
    /// scroll bound).
    ///
    /// When L itself is hidden the return value is where it would sit if
    /// visible — callers should first move the caret out of hidden territory
    /// with [`Self::nearest_visible_line`].
    pub fn screen_vstart(&self, line: u64) -> u64 {
        let tab = &self.tabs[self.active];
        let v = tab.jsonl.vstart(line);
        v.saturating_sub(self.hidden_up_to(v))
    }

    /// Screen row r → JSONL row (inverse operation, see module comment).
    ///
    /// Binary-searches "the r-th unhidden JSONL row": `f(v) = v - count(hidden < v)`
    /// is monotonically non-decreasing; the target is the smallest `v` with
    /// `f(v) >= r`, then skip plateaus landing on hidden positions. The old
    /// implementation advanced linearly per hidden row, called once per row
    /// per frame in the viewport — with tens of thousands of hidden lines,
    /// scrolling was O(visible × hidden); binary search brings that down to
    /// O(log² h).
    pub fn jsonl_row_of(&self, r: u64) -> u64 {
        let vh = self.hidden_vstarts();
        if vh.is_empty() {
            return r;
        }
        let h = vh.len() as u64;
        let before = |v: u64| vh.partition_point(|&x| x < v) as u64;
        let mut lo = r;
        let mut hi = r.saturating_add(h);
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if mid - before(mid) >= r {
                hi = mid;
            } else {
                lo = mid + 1;
            }
        }
        let mut row = lo;
        while vh.binary_search(&row).is_ok() {
            row += 1;
        }
        row
    }

    /// The visible line nearest `line` (down first, then up; back to the
    /// original line when everything is hidden).
    pub fn nearest_visible_line(&self, line: u64) -> u64 {
        if !self.line_hidden(line) {
            return line;
        }
        let lines = self.active_doc().line_count();
        let mut down = line + 1;
        while down < lines && self.line_hidden(down) {
            down += 1;
        }
        if down < lines {
            return down;
        }
        let mut up = line;
        while up > 0 && self.line_hidden(up - 1) {
            up -= 1;
        }
        if up > 0 {
            up - 1
        } else if !self.line_hidden(0) {
            0
        } else {
            line
        }
    }

    // ---- commands ---------------------------------------------------------------

    /// Right-click → Hide lines: hide the lines covered by the selection (or
    /// the current line if nothing is selected).
    pub fn hide_lines(&mut self) {
        // Unavailable in the JSON tree view: screen_rows does not subtract
        // hidden rows for the tree view while the hidden set still takes
        // effect → the reveal_caret heal moves the caret to "the nearest
        // visible line", yet on screen it is the tree view with nothing
        // hidden, so the caret appears to jump at random.
        if self.tab().json.enabled {
            self.status = Some(Msg::StatusHideLinesJsonView.into());
            return;
        }
        let (first, last) = match self.sel_range() {
            Some(r) if r.start < r.end => (
                self.active_doc().line_of_pos(r.start),
                self.active_doc().line_of_pos(r.end.saturating_sub(1)),
            ),
            _ => {
                let l = self.active_doc().line_of_pos(self.caret().pos);
                (l, l)
            }
        };
        let id = self.active_doc().id;
        let set = self.hidden.entry(id).or_default();
        let mut added = 0u64;
        for l in first..=last {
            if set.insert(l) {
                added += 1;
            }
        }
        self.rebuild_hidden_cache();
        if added == 0 {
            self.status = Some(Msg::StatusLinesAlreadyHidden.into());
            return;
        }
        self.clamp_scroll();
        let total = self.hidden_count();
        self.status = Some(
            Fmt::StatusHidLines {
                n: added as usize,
                total: total as usize,
            }
            .into(),
        );
    }

    /// Right-click → Show all hidden lines.
    pub fn show_hidden_lines(&mut self) {
        let id = self.active_doc().id;
        let n = self.hidden.remove(&id).map(|s| s.len()).unwrap_or(0);
        self.rebuild_hidden_cache();
        self.clamp_scroll();
        self.status = Some(if n == 0 {
            Msg::StatusNoHiddenLines.into()
        } else {
            Fmt::StatusShowedLines { n }.into()
        });
    }

    /// Move the caret to the nearest visible line when it lands on a hidden
    /// one (preserving the column).
    ///
    /// Called every frame by `viewport::build` (same place as
    /// `sanitize_carets`), so "arrow keys walking into hidden territory" and
    /// "find jumping into it" are corrected in place.
    pub fn reveal_caret(&mut self) {
        // Caret and anchor must each be judged independently; early-return
        // only when neither needs moving. Checking only the caret leaves
        // "caret visible, anchor on a hidden line" satisfying the heal probe
        // (heal probes both) without entering the fix below, so needs_heal is
        // always true → infinite render loop.
        let pos = self.caret().pos;
        let anchor = self.caret().anchor;
        let caret_hidden = {
            let line = self.active_doc().line_of_pos(pos);
            self.line_hidden(line)
        };
        let anchor_hidden = anchor.is_some_and(|a| {
            let al = self.active_doc().line_of_pos(a);
            self.line_hidden(al)
        });
        if !caret_hidden && !anchor_hidden {
            return;
        }
        // caret: hidden line → nearest visible line, same column
        let mut new_pos = pos;
        if caret_hidden {
            let line = self.active_doc().line_of_pos(pos);
            let target = self.nearest_visible_line(line);
            if target != line {
                let col = pos.saturating_sub(self.active_doc().pos_of_line(line));
                let start = self.active_doc().pos_of_line(target);
                let len = self
                    .active_doc()
                    .line_span(target)
                    .end
                    .saturating_sub(start);
                new_pos = start + col.min(len);
            }
        }
        // anchor: likewise moved to a visible line in the same column, keeping the selection shape
        let new_anchor = anchor.map(|a| {
            let al = self.active_doc().line_of_pos(a);
            if !self.line_hidden(al) {
                return a;
            }
            let at = self.nearest_visible_line(al);
            if at == al {
                return a;
            }
            let acol = a.saturating_sub(self.active_doc().pos_of_line(al));
            let ast = self.active_doc().pos_of_line(at);
            let alen = self.active_doc().line_span(at).end.saturating_sub(ast);
            ast + acol.min(alen)
        });
        if new_pos == pos && new_anchor == anchor {
            return;
        }
        let c = self.caret_mut();
        c.pos = new_pos;
        c.anchor = new_anchor;
        // Pull the caret back into view after moving it (otherwise a caret
        // that "walked into hidden territory and jumped out" may stop
        // offscreen)
        self.scroll_caret_into_view();
    }
}

#[cfg(test)]
mod tests {
    use crate::ui::state::Caret;
    use crate::ui::testutil::st_of;

    #[test]
    fn hiding_a_line_shrinks_the_screen_row_count() {
        let mut st = st_of("a\nb\nc\nd\n");
        assert_eq!(st.screen_rows(), 4);
        st.set_caret(4, false); // line 2 (c)
        st.hide_lines();
        assert_eq!(st.screen_rows(), 3);
        assert!(st.line_hidden(2));
        st.show_hidden_lines();
        assert_eq!(st.screen_rows(), 4);
        assert!(!st.line_hidden(2));
    }

    #[test]
    fn hiding_a_selection_hides_every_line_it_touches() {
        let mut st = st_of("a\nb\nc\nd\ne\n");
        st.set_caret(2, false); // start of line 1
        st.set_caret(8, true); // select into the middle of line 3
        st.hide_lines();
        assert_eq!(st.hidden_count(), 3, "lines 1..=3");
    }

    #[test]
    fn screen_and_jsonl_rows_round_trip() {
        let mut st = st_of("a\nb\nc\nd\ne\nf\n");
        st.hidden
            .entry(st.active_doc().id)
            .or_default()
            .extend([1, 3]);
        st.rebuild_hidden_cache();

        // screen rows = skipping hidden rows
        for (screen, jsonl) in [(0u64, 0u64), (1, 2), (2, 4), (3, 5)] {
            assert_eq!(st.jsonl_row_of(screen), jsonl, "screen {screen}");
            assert_eq!(st.screen_vstart(jsonl), screen, "jsonl {jsonl}");
        }
        assert_eq!(st.screen_rows(), 4);
    }

    /// The `vstarts` cache is a single slot; two tabs with the same shape and
    /// generation but a different hidden line must not share it.
    #[test]
    fn hidden_cache_is_scoped_to_the_document() {
        let mut st = st_of("a\nb\nc\n");
        st.hidden.entry(st.active_doc().id).or_default().insert(1);
        st.rebuild_hidden_cache();
        assert_eq!(st.screen_vstart(1), 0, "row 0 is hidden");

        let mut other = crate::buffer::TextDocument::empty();
        other.insert_text(0, "x\ny\nz\n");
        st.tabs.push(crate::ui::state::Tab::new(other));
        st.active = st.tabs.len() - 1;
        st.hidden.entry(st.active_doc().id).or_default().insert(2);
        st.refresh_hidden();
        assert_eq!(
            st.screen_vstart(1),
            1,
            "the second document hides line 2, not line 1"
        );
    }

    #[test]
    fn a_caret_on_a_hidden_line_jumps_to_the_next_visible_one() {
        let mut st = st_of("a\nb\nc\n");
        st.set_caret(4, false); // line 2
        st.hide_lines();
        assert!(st.line_hidden(2));
        st.reveal_caret();
        assert_eq!(
            st.active_doc().line_of_pos(st.caret().pos),
            1,
            "line 2 is hidden, the caret should land on line 1"
        );
    }

    #[test]
    fn a_visible_caret_with_a_hidden_anchor_still_gets_healed() {
        // Regression: caret visible, anchor on a hidden line. The probe
        // (heal.rs checks both caret and anchor) held while the old
        // reveal_caret checked only the caret and returned early — needs_heal
        // always true, the render loop self-perpetuating forever. After the
        // fix the anchor must be moved out independently.
        let mut st = st_of("a\nb\nc\n");
        st.hidden
            .entry(st.active_doc().id)
            .or_default()
            .extend([1, 2]);
        st.rebuild_hidden_cache();
        // Hand-build "caret line 0 (visible) + anchor middle of line 2
        // (hidden)": arriving within the same frame the hiding takes effect
        // via Shift+↓ is exactly this state.
        st.tabs[st.active].caret = Caret {
            pos: 0,
            anchor: Some(4),
        };
        assert!(!st.line_hidden(0));
        assert!(st.line_hidden(2));

        st.reveal_caret();
        assert_eq!(
            st.active_doc().line_of_pos(st.caret().pos),
            0,
            "the caret is visible and must not be moved"
        );
        let anchor = st.caret().anchor.expect("the anchor should be kept");
        assert!(
            !st.line_hidden(st.active_doc().line_of_pos(anchor)),
            "the anchor was moved out of the hidden line"
        );
        // Convergence: running it again after the fix is idempotent (no change)
        let before = (st.caret().pos, st.caret().anchor);
        st.reveal_caret();
        assert_eq!((st.caret().pos, st.caret().anchor), before);
    }

    #[test]
    fn hiding_keeps_out_of_range_lines_out() {
        let mut st = st_of("a\nb\n");
        st.hidden
            .entry(st.active_doc().id)
            .or_default()
            .extend([0, 5, 99]);
        st.refresh_hidden();
        let set = &st.hidden[&st.active_doc().id];
        assert!(
            set.contains(&0) && set.len() == 1,
            "out-of-range line numbers were dropped"
        );
    }

    /// The binary mapping must match the old linear semantics item by item
    /// (exhaustively over all hidden subsets).
    #[test]
    fn jsonl_row_of_matches_linear_mapping() {
        let n = 8u64;
        let mut st = st_of(&"x\n".repeat(n as usize));
        for mask in 0u32..(1 << n) {
            let hidden: Vec<u64> = (0..n).filter(|i| mask & (1 << i) != 0).collect();
            st.hidden
                .insert(st.active_doc().id, hidden.iter().copied().collect());
            st.hidden_cache = None;
            st.refresh_hidden();
            let visible = n - hidden.len() as u64;
            for r in 0..visible {
                let mut want = r;
                for &v in &hidden {
                    if v > want {
                        break;
                    }
                    want += 1;
                }
                assert_eq!(st.jsonl_row_of(r), want, "hidden={hidden:?} r={r}");
            }
        }
    }

    #[test]
    fn json_preview_switches_screen_rows_to_tree_total() {
        let mut st = st_of(r#"{"a":[1,2],"b":{"c":3}}"#);
        // Text mode: single-line document = 1 screen row
        assert_eq!(st.screen_rows(), 1, "text mode: 1 line");
        // Whole-document JSON tree preview on: screen rows = expanded tree rows (> doc lines)
        st.tabs[0].json.enabled = true;
        let doc = st.tabs[0].doc.clone();
        let loc = st.locale;
        st.tabs[0].json.refresh(&doc, loc);
        let tree_rows = st.tabs[0].json.total_rows(st.tabs[0].doc.line_count());
        assert!(
            tree_rows > 1,
            "the tree should expand into multiple rows: {tree_rows}"
        );
        assert_eq!(st.screen_rows(), tree_rows);
        // Row count falls back after collapsing all
        st.tabs[0].json.collapse_all();
        assert!(st.screen_rows() < tree_rows);
        assert!(st.screen_rows() >= 1);
    }
}
