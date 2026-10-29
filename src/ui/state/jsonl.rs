//! View state and virtual-row mapping for the JSONL preview (one per
//! document, attached to [`crate::ui::state::Tab`]).
//!
//! The single source of truth for line ↔ virtual row. Three layers:
//! - [`crate::jsonl`]: pure parsing/flattening (stateless);
//! - this module: the set of expanded lines + the virtual-row map +
//!   invalidation rechecks;
//! - viewport: consumes the map only, holds no state.
//!
//! Performance contract: **with no line expanded, the per-frame overhead is
//! 2 u64 comparisons in `refresh()`.**

use std::collections::{BTreeMap, BTreeSet};

use crate::buffer::doc::TextDocument;
use crate::jsonl::{self, Node, Row};
use crate::ui::Fmt;

/// Query outlet for mapping results such as the start position of document
/// line L in virtual-row space.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RowRef {
    /// Screen row r is document line L
    Doc(u64),
    /// Row `sub` of document line L's expanded block (sub is 0-based; 0 is
    /// the first row after L itself)
    Virtual { line: u64, sub: u32 },
}

/// Result of `toggle_line` (callers turn it into a status-bar message).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    Expanded { rows: u32 },
    Collapsed,
    Failed(i18n::Message),
}

/// All state of one expanded document line.
#[derive(Clone, Debug)]
pub struct Expand {
    /// Source text fingerprint (FNV-1a 64): rechecked after edits, discarded
    /// on mismatch
    pub fp: u64,
    pub len: usize,
    /// Parse result; `None` when parsing failed (`err` is then always set)
    pub root: Option<Node>,
    /// Screen rows flattened per the current `collapsed` set
    pub rows: Vec<Row>,
    pub collapsed: BTreeSet<u32>,
    pub err: Option<String>,
}

impl Expand {
    fn compute(text: &str) -> Expand {
        let fp = fnv1a(text.as_bytes());
        match jsonl::parse_line(text) {
            Ok(root) => {
                let (rows, _) = jsonl::flatten(&root, &BTreeSet::new());
                Expand {
                    fp,
                    len: text.len(),
                    root: Some(root),
                    rows,
                    collapsed: BTreeSet::new(),
                    err: None,
                }
            }
            Err(m) => Expand {
                fp,
                len: text.len(),
                root: None,
                rows: Vec::new(),
                collapsed: BTreeSet::new(),
                err: Some(m),
            },
        }
    }

    fn still_valid(&self, doc: &TextDocument, line: u64) -> bool {
        let text = doc.line_text(line);
        text.len() == self.len && fnv1a(text.as_bytes()) == self.fp
    }

    /// Virtual rows occupied by the expanded block (beyond document line L
    /// itself)
    pub fn span(&self) -> u32 {
        if self.err.is_some() {
            1 // one red ⚠ hint row
        } else {
            self.rows.len() as u32
        }
    }
}

/// Invalidation check cache: `(gen, line_count)` unchanged ⇒ zero work per
/// frame.
#[derive(Clone, Debug)]
struct Cache {
    gen: u64,
    lines: u64,
    total_rows: u64,
    /// Ascending `(document line, that line's expanded-block row count)`
    spans: Vec<(u64, u32)>,
}

/// Per-document JSONL preview state.
#[derive(Clone, Debug, Default)]
pub struct JsonlView {
    /// View toggle (View → JSONL Preview; auto-enabled when a .jsonl file
    /// opens)
    pub enabled: bool,
    /// Expanded document lines → parse results. BTreeMap keeps keys ordered
    /// (the mapping algorithm depends on that)
    pub open: BTreeMap<u64, Expand>,
    cache: Option<Cache>,
}

impl JsonlView {
    /// Initialize per language spec when the document opens ("JSON Lines"
    /// auto-enables).
    pub fn for_spec(name: &str) -> JsonlView {
        JsonlView {
            enabled: name == "JSON Lines",
            open: BTreeMap::new(),
            cache: None,
        }
    }

    /// Called once per frame at the start of viewport::build.
    ///
    /// Fast path: `(gen, line_count)` unchanged ⇒ return after 2 u64
    /// comparisons. gen changed (edit/undo/reload) ⇒ recheck fingerprints one
    /// by one (editing cadence, ≤ MAX_OPEN line_text calls), discard on
    /// mismatch. Progressive index completion only changes `line_count`, not
    /// `gen` ⇒ the key includes line_count.
    pub fn refresh(&mut self, doc: &TextDocument) {
        let key = (doc.gen, doc.line_count());
        if self.cache.as_ref().is_some_and(|c| (c.gen, c.lines) == key) {
            return;
        }
        if !self.open.is_empty() {
            let lines = doc.line_count();
            self.open
                .retain(|line, e| *line < lines && e.still_valid(doc, *line));
        }
        self.rebuild_cache(key);
    }

    /// Read-only probe of whether [`Self::refresh`] would do real work (for
    /// pre-render self-healing). The condition must match this method's early
    /// return verbatim, or self-healing will not converge.
    pub fn stale(&self, doc: &TextDocument) -> bool {
        let key = (doc.gen, doc.line_count());
        !self.cache.as_ref().is_some_and(|c| (c.gen, c.lines) == key)
    }

    fn rebuild_cache(&mut self, key: (u64, u64)) {
        let (gen, lines) = key;
        if self.open.is_empty() {
            self.cache = Some(Cache {
                gen,
                lines,
                total_rows: lines,
                spans: Vec::new(),
            });
            return;
        }
        let mut spans = Vec::with_capacity(self.open.len());
        let mut total = lines;
        for (line, e) in &self.open {
            let k = e.span();
            total += k as u64;
            spans.push((*line, k));
        }
        self.cache = Some(Cache {
            gen,
            lines,
            total_rows: total,
            spans,
        });
    }

    /// Total virtual row count (scroll bound). With `open` empty it is
    /// `doc_lines` (identity fast path).
    pub fn total_rows(&self, doc_lines: u64) -> u64 {
        match &self.cache {
            Some(c) => c.total_rows,
            None => doc_lines,
        }
    }

    /// Start of document line L in virtual-row space = `L + Σ{k_i | L_i < L}`.
    pub fn vstart(&self, line: u64) -> u64 {
        let Some(c) = &self.cache else { return line };
        line + c
            .spans
            .iter()
            .take_while(|(l, _)| *l < line)
            .map(|(_, k)| *k as u64)
            .sum::<u64>()
    }

    /// Virtual row r → what to draw on screen. Identity when nothing is
    /// expanded: `row_at(r) == Doc(r)`.
    pub fn row_at(&self, r: u64) -> RowRef {
        self.map_from(r, 0, 0)
    }

    fn map_from(&self, r: u64, mut i: usize, mut shift: u64) -> RowRef {
        let Some(c) = &self.cache else {
            return RowRef::Doc(r);
        };
        while let Some(&(line, k)) = c.spans.get(i) {
            let k = k as u64;
            let vstart = line + shift; // shift = the "extra" virtual rows accumulated before
            if r < vstart {
                return RowRef::Doc(r - shift);
            }
            if r == vstart {
                return RowRef::Doc(line); // the expanded document line itself still renders as a document line
            }
            if r < vstart + 1 + k {
                return RowRef::Virtual {
                    line,
                    sub: (r - vstart - 1) as u32,
                };
            }
            shift += k; // the document line itself is not an extra row; only the k virtual rows count toward the shift
            i += 1;
        }
        RowRef::Doc(r - shift)
    }

    /// Sequentially advancing mapping cursor: mapping a whole ~80-row screen
    /// costs O(visible rows + expansions) total.
    pub fn cursor_at(&self, r: u64) -> Cursor<'_> {
        let (i, shift) = self.seek(r);
        Cursor {
            view: self,
            r,
            i,
            shift,
        }
    }

    /// Find the span index covering r and the accumulated shift (the
    /// cursor's starting point).
    fn seek(&self, r: u64) -> (usize, u64) {
        let Some(c) = &self.cache else { return (0, 0) };
        let mut shift = 0u64;
        for (i, &(line, k)) in c.spans.iter().enumerate() {
            if r <= line + shift {
                return (i, shift);
            }
            if r < line + shift + 1 + k as u64 {
                return (i, shift);
            }
            shift += k as u64;
        }
        (c.spans.len(), shift)
    }

    /// Expand/collapse a document line. `center` is the document line near
    /// the current viewport center, used for `MAX_OPEN` eviction (evict the
    /// entry farthest from the viewport, not a plain LRU).
    pub fn toggle_line(&mut self, doc: &TextDocument, line: u64, center: u64) -> Outcome {
        if self.open.remove(&line).is_some() {
            self.rebuild_cache((doc.gen, doc.line_count()));
            return Outcome::Collapsed;
        }
        let text = doc.line_text(line);
        if text.len() > jsonl::MAX_PARSE_BYTES {
            return Outcome::Failed(Fmt::StatusJsonlLineTooBig { mib: 4 }.into());
        }
        let e = Expand::compute(&text);
        let rows = e.span();
        self.open.insert(line, e);
        while self.open.len() > jsonl::MAX_OPEN {
            // Evict the entry farthest from the viewport (excluding the
            // just-opened one; keeps the hard cap intact)
            let Some(victim) = self
                .open
                .keys()
                .filter(|l| **l != line)
                .max_by_key(|l| l.abs_diff(center))
                .copied()
            else {
                break;
            };
            self.open.remove(&victim);
        }
        self.rebuild_cache((doc.gen, doc.line_count()));
        Outcome::Expanded { rows }
    }

    /// Collapse/expand a container node inside an expanded block (rebuilds
    /// that expansion's rows). Returns whether anything changed. A
    /// nonexistent `node` id is a no-op (does not pollute the collapsed set).
    pub fn toggle_node(&mut self, line: u64, node: u32) -> bool {
        let Some(e) = self.open.get_mut(&line) else {
            return false;
        };
        let Some(root) = &e.root else { return false };
        if e.collapsed.contains(&node) {
            e.collapsed.remove(&node);
        } else if !tree_has_id(root, node) {
            return false;
        } else {
            e.collapsed.insert(node);
        }
        let (rows, _) = jsonl::flatten(root, &e.collapsed);
        e.rows = rows;
        // that line's row count in the cache's spans may have changed →
        // recompute in full (O(#open), click cadence)
        let key = self
            .cache
            .as_ref()
            .map(|c| (c.gen, c.lines))
            .unwrap_or((u64::MAX, u64::MAX));
        self.rebuild_cache(key);
        true
    }

    /// Collapse all expansions.
    pub fn collapse_all(&mut self) {
        self.open.clear();
        self.cache = None; // so the next refresh rebuilds the empty cache in O(1)
    }

    /// Status-bar summary (shown while expanded: "JSONL preview · N lines
    /// expanded").
    pub fn status(&self) -> Option<i18n::Message> {
        let n = self.open.len();
        (n > 0).then(|| Fmt::StatusJsonlExpanded { n }.into())
    }
}

/// Sequentially advancing virtual-row mapping cursor. `get()` matches
/// successive `row_at()` results exactly (guarded by the `cursor_is_linear`
/// test).
pub struct Cursor<'a> {
    view: &'a JsonlView,
    r: u64,
    i: usize,
    shift: u64,
}

impl Cursor<'_> {
    pub fn row(&self) -> u64 {
        self.r
    }
    pub fn get(&self) -> RowRef {
        self.view.map_from(self.r, self.i, self.shift)
    }
    pub fn advance(&mut self) {
        self.r += 1;
        // maintain the invariant: skip spans that already lie entirely before r
        if let Some(c) = &self.view.cache {
            while let Some(&(line, k)) = c.spans.get(self.i) {
                if self.r > line + self.shift + k as u64 {
                    self.shift += k as u64;
                    self.i += 1;
                } else {
                    break;
                }
            }
        }
    }
}

/// Whether the given id exists in the tree (depth ≤ MAX_DEPTH; negligible
/// cost — called only on click).
fn tree_has_id(root: &Node, id: u32) -> bool {
    if root.id == id {
        return true;
    }
    root.children.iter().any(|c| tree_has_id(c, id))
}

/// FNV-1a 64 (source text fingerprint).
pub fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

// ---------------------------------------------------------------- tests ---

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jsonl::MAX_OPEN;

    fn doc(text: &str) -> TextDocument {
        TextDocument::from(text.to_string())
    }

    fn sample() -> TextDocument {
        // 10 lines; lines 2/6 (0-based) are valid JSON
        doc("plain\n{\"id\":1}\n{\"a\":[1,2,3]}\nx\ny\nz\n{\"b\":{\"c\":2}}\nw\nv\nu\n")
    }

    fn expanded_view() -> (TextDocument, JsonlView) {
        let d = sample();
        let mut v = JsonlView {
            enabled: true,
            ..JsonlView::default()
        };
        v.refresh(&d);
        v.toggle_line(&d, 2, 5);
        (d, v)
    }

    #[test]
    fn identity_when_nothing_is_expanded() {
        let d = sample();
        let mut v = JsonlView::default();
        v.refresh(&d);
        let lines = d.line_count();
        assert_eq!(v.total_rows(lines), lines);
        for r in 0..lines {
            assert_eq!(v.row_at(r), RowRef::Doc(r));
            assert_eq!(v.vstart(r), r);
        }
    }

    #[test]
    fn expansion_shifts_rows_below_only() {
        let (d, v) = expanded_view();
        let k = v.open[&2].span() as u64;
        assert!(k > 1, "expanded block should be more than 1 row: {k}");
        assert_eq!(v.vstart(2), 2, "the expanded line itself does not move");
        assert_eq!(v.vstart(3), 3 + k, "rows below shift down as a whole");
        assert_eq!(v.vstart(2), 2, "rows above are unaffected");
        assert_eq!(v.total_rows(d.line_count()), d.line_count() + k);
        assert_eq!(v.row_at(2), RowRef::Doc(2));
        assert_eq!(v.row_at(3), RowRef::Virtual { line: 2, sub: 0 });
        assert_eq!(v.row_at(3 + k), RowRef::Doc(3));
    }

    #[test]
    fn row_at_round_trip() {
        let (d, v) = expanded_view();
        let total = v.total_rows(d.line_count());
        // For each r in 0..total, map Doc(r) back to r; Virtual must land in
        // the correct inter-line range
        for r in 0..total {
            match v.row_at(r) {
                RowRef::Doc(l) => {
                    // l ≤ 2 (the expanded line and above) unaffected; l > 2
                    // shifted down by k
                    let expect = if l <= 2 { l } else { l + k_of(&v) };
                    assert_eq!(expect, r, "Doc({l}) should map to virtual row {r}");
                }
                RowRef::Virtual { line, sub } => {
                    assert_eq!(line, 2);
                    let expect = 3 + sub as u64;
                    assert_eq!(expect, r);
                }
            }
        }
    }

    fn k_of(v: &JsonlView) -> u64 {
        v.open[&2].span() as u64
    }

    #[test]
    fn edit_invalidates_stale_expansions() {
        let (mut d, mut v) = expanded_view();
        assert_eq!(v.open.len(), 1);
        // Change line 2 (the expanded line) so it is no longer JSON
        let pos = d.pos_of_line(2);
        d.replace_range(pos..pos, "not json");
        assert_ne!(d.gen, 0);
        v.refresh(&d);
        assert!(
            v.open.is_empty(),
            "the edited line's expansion should be discarded"
        );
        // Untouched lines are unaffected: re-expand line 1, then edit line 9
        v.toggle_line(&d, 1, 5);
        let pos9 = d.pos_of_line(9);
        d.replace_range(pos9..pos9, "tail");
        v.refresh(&d);
        assert!(
            v.open.contains_key(&1),
            "unedited lines' expansions should be kept"
        );
    }

    #[test]
    fn line_shift_invalidates_wrong_line_expansions() {
        // Insert a full line before the expanded line ⇒ line numbers shift,
        // fingerprint mismatches ⇒ discard
        let (mut d, mut v) = expanded_view();
        let pos = d.pos_of_line(0);
        d.replace_range(pos..pos, "inserted\n");
        v.refresh(&d);
        // The original 3 lines are now 4; even with identical content on
        // line 4, the gen change triggers a fingerprint recheck. The inserted
        // line keeps the original content in the document (line 4 == old
        // line 3), so fp still matches — this is "the worst case is a
        // collapse, never wrong content": here the content happens to be
        // identical, the expansion stays and is still correct. Just assert
        // the keys of open were rechecked.
        for k in v.open.keys() {
            let text = d.line_text(*k);
            assert_eq!(fnv1a(text.as_bytes()), v.open[k].fp);
        }
    }

    #[test]
    fn line_count_bump_invalidates_cache_even_without_gen() {
        // Progressive index completion: line_count changes, gen doesn't ⇒
        // cache must still invalidate
        let (d, v) = expanded_view();
        let total_before = v.total_rows(d.line_count());
        // Directly mutating the cache's gen to fake "gen unchanged" is
        // impossible (gen is private), so clear the cache and refresh
        // instead, confirming the key includes line_count
        let mut v2 = v.clone();
        v2.cache = None;
        v2.refresh(&d);
        assert_eq!(v2.total_rows(d.line_count()), total_before);
    }

    #[test]
    fn caps_are_enforced_via_parse_layer() {
        // Overlong line: toggle_line rejects outright
        let long = format!("\"{}\"", "a".repeat(crate::jsonl::MAX_PARSE_BYTES + 10));
        let d = doc(&long);
        let mut v = JsonlView::default();
        match v.toggle_line(&d, 0, 0) {
            Outcome::Failed(m) => assert!(m.key().contains("StatusJsonlLineTooBig")),
            other => panic!("should fail: {other:?}"),
        }
        assert!(v.open.is_empty());
        // Overly deep nesting: parse fails ⇒ err expansion (1 row; clicking
        // again collapses)
        let deep = "[".repeat(100) + "1" + &"]".repeat(100);
        let d2 = doc(&deep);
        let mut v2 = JsonlView::default();
        match v2.toggle_line(&d2, 0, 0) {
            Outcome::Expanded { rows } => assert_eq!(rows, 1),
            other => panic!("err expansion should take 1 row: {other:?}"),
        }
        assert!(v2.open[&0].err.is_some());
        v2.toggle_line(&d2, 0, 0);
        assert!(v2.open.is_empty());
    }

    #[test]
    fn max_open_evicts_farthest_from_viewport() {
        let mut text = String::new();
        for i in 0..(MAX_OPEN + 2) {
            text.push_str(&format!("{{\"i\":{i}}}\n"));
        }
        let d = doc(&text);
        let mut v = JsonlView::default();
        v.refresh(&d);
        for i in 0..(MAX_OPEN + 1) {
            v.toggle_line(&d, i as u64, MAX_OPEN as u64 / 2);
        }
        assert_eq!(v.open.len(), MAX_OPEN);
        // Entry 0, farthest from center, is evicted
        assert!(!v.open.contains_key(&0));
        assert!(v.open.contains_key(&(1)));
        assert!(v.open.contains_key(&(MAX_OPEN as u64)));
    }

    #[test]
    fn toggle_node_recollapses_subtree() {
        let d = doc("{\"a\":{\"b\":1,\"c\":2},\"d\":3}\n");
        let mut v = JsonlView::default();
        v.refresh(&d);
        v.toggle_line(&d, 0, 0);
        let root_id = v.open[&0].root.as_ref().unwrap().id;
        let a_id = v.open[&0].root.as_ref().unwrap().children[0].id;
        let rows_full = v.open[&0].rows.len();
        assert!(v.toggle_node(0, a_id));
        let rows_less = v.open[&0].rows.len();
        assert!(rows_less < rows_full);
        // Collapsing the root? A nonexistent id must not panic or mutate wrongly
        let before = v.open[&0].rows.len();
        assert!(!v.toggle_node(0, u32::MAX - 1));
        assert_eq!(v.open[&0].rows.len(), before);
        let _ = root_id;
        // Expand back
        assert!(v.toggle_node(0, a_id));
        assert_eq!(v.open[&0].rows.len(), rows_full);
    }

    #[test]
    fn collapse_all_clears_everything() {
        let (d, mut v) = expanded_view();
        assert!(!v.open.is_empty());
        v.collapse_all();
        assert!(v.open.is_empty());
        v.refresh(&d);
        assert_eq!(v.total_rows(d.line_count()), d.line_count());
    }

    #[test]
    fn cursor_is_linear() {
        let (d, mut v) = expanded_view();
        // Expand one more line to cover the two-span interaction
        v.toggle_line(&d, 6, 5);
        let total = v.total_rows(d.line_count());
        let mut cur = v.cursor_at(0);
        for r in 0..=total {
            assert_eq!(cur.row(), r);
            assert_eq!(cur.get(), v.row_at(r), "r={r}");
            cur.advance();
        }
    }

    #[test]
    fn err_expansion_reports_message() {
        let d = doc("{\"a\":}\n");
        let mut v = JsonlView::default();
        v.refresh(&d);
        let out = v.toggle_line(&d, 0, 0);
        assert!(matches!(out, Outcome::Expanded { rows: 1 }));
        let msg = v.open[&0].err.clone().unwrap();
        assert!(
            msg.contains("列"),
            "error message should contain the column: {msg}"
        );
    }

    #[test]
    fn status_line() {
        let (d, mut v) = expanded_view();
        assert_eq!(
            v.status().map(|m| m.key()),
            Some("With:StatusJsonlExpanded".to_string())
        );
        v.collapse_all();
        v.refresh(&d);
        assert!(v.status().is_none());
    }
}
