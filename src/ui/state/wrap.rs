//! Wrap (soft-wrap) line-break index: the single source of truth for the
//! document line ↔ visual row mapping.
//!
//! # Row space
//!
//! This module is the outermost layer of the row-space chain (see the
//! `state::hidden` module docs):
//!
//! ```text
//! doc line --jsonl.vstart--> JSONL row --hidden--> screen row --this module--> visual row
//! ```
//!
//! When `wrap` is off (or the whole-document JSON tree view is open) the
//! mapping is the identity: visual row == screen row, zero per-frame cost.
//! When on, an over-wide **document line** splits into k ≥ 1 visual rows;
//! JSONL expansion blocks' virtual rows and JSON tree rows are fixed at 1
//! row (they are preview rows with controllable length).
//!
//! # Storage
//!
//! Only **wrapped** document lines are stored (ascending `spans`); most
//! lines don't wrap, so sparsity protects both mapping and memory. Each span
//! carries `(line number, visual row count k)` and the start byte offsets of
//! that line's visual rows 1..k (the first row always starts at 0 and is
//! not stored). `cum[i]` is the cumulative extra rows before span i,
//! Σ(k_j − 1), turning both directions of the "screen row ↔ visual row"
//! queries into binary searches.
//!
//! # Invalidation and progressiveness
//!
//! * Key `(doc_id, gen, metrics, width, lines)`: font size / window width
//!   change ⇒ full rescan.
//! * gen change (edit): `doc.take_edit_from()` gives the earliest line
//!   touched this generation; spans after it are dropped and the scan cursor
//!   rewinds — the rescan start lands exactly on the edited block.
//! * First frames after opening a large file: unscanned lines count as 1
//!   row; scanning advances frame by frame within budget (same convergence
//!   philosophy as `HExtent`); **the visible window is measured immediately
//!   by `ensure_range`**, so rendered rows never rely on unmeasured guesses.
//!
//! # Break strategy
//!
//! Prefer breaking after whitespace (word wrap); when whitespace is all at
//! the line's start, hard-break at the last character that fits; a single
//! character wider than the whole line overflows (no empty rows produced).
//! Tabs advance to **row-local** tab stops (matching rendering: each visual
//! row is an independent div laid out from 0).

use crate::buffer::doc::TextDocument;
use crate::ui::metrics::Metrics;

/// Lines above this byte count are not wrapped (count as 1 row).
///
/// Wrap measurement walks character by character; one compressed 5 MB single
/// line would stall a frame for hundreds of milliseconds; with the cap, such
/// lines render as one over-wide clipped row (the horizontal scrollbar is
/// off under wrap, so the tail is unreachable — versus the everyday cost of
/// a "never-wrapping paragraph", trading for open-without-stall is worth it).
pub const WRAP_LINE_CAP: usize = 4 * 1024 * 1024;

/// Max lines measured per frame (same order of magnitude as `viewport::H_SCAN_BUDGET`).
const SCAN_BUDGET: u64 = 256;

/// Invalidation dimensions for `WrapIndex::sync` (document identity, generation, line count, hidden-set fingerprint).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WrapStamp {
    pub doc_id: u64,
    pub gen: u64,
    pub lines: u64,
    pub hidden_mask: u64,
}

/// Wrap index (see module docs).
#[derive(Clone, Debug, Default)]
pub struct WrapIndex {
    /// Invalidation key: `(doc_id, gen, metrics_key, width_key, lines, hidden_mask)`
    key: (u64, u64, u32, u32, u64, u64),
    /// Scan cursor: the next line to measure (lines before it are either in
    /// `spans` or confirmed non-wrapping)
    next_line: u64,
    /// Wrapped lines: `(document line, visual row count k)`, ascending.
    /// `pub(crate)`: `edit.rs`'s visual-row bidirectional mapping needs the
    /// spans and cumulative sums (including the `screen_vstart` callback;
    /// cannot be kept inside this module).
    pub(crate) spans: Vec<(u64, u32)>,
    /// Parallel to `spans`: start byte offsets of each line's visual rows 1..k (ascending, excluding 0)
    breaks: Vec<Vec<u32>>,
    /// `cum[i]` = cumulative extra rows before span i, Σ(k_j − 1)
    pub(crate) cum: Vec<u64>,
    /// All extra rows (= `cum.last()` or 0), used for the scroll upper bound
    extra: u64,
}

impl WrapIndex {
    fn width_key(w: f32) -> u32 {
        // 0.5px granularity: subpixel jitter while dragging the window won't bust the cache
        (w.max(0.0) * 2.0).round() as u32
    }

    pub fn metrics_key(adv: f32) -> u32 {
        (adv.max(0.0) * 1024.0).round() as u32
    }

    fn reset(&mut self, doc_id: u64, gen: u64, mkey: u32, wkey: u32, lines: u64, mask: u64) {
        self.key = (doc_id, gen, mkey, wkey, lines, mask);
        self.next_line = 0;
        self.spans.clear();
        self.breaks.clear();
        self.cum.clear();
        self.extra = 0;
    }

    /// Sync the invalidation key every frame. Width / font size / document
    /// change ⇒ reset; edit ⇒ truncate and rescan from `edit_from`;
    /// line-count growth (progressive index completion) only updates the key.
    pub fn sync(&mut self, stamp: WrapStamp, mkey: u32, width: f32, edit_from: Option<u64>) {
        let wkey = Self::width_key(width);
        // Hidden-line set changed ⇒ old spans may contain now-invisible lines; reset the whole table.
        if self.key.0 != stamp.doc_id
            || self.key.2 != mkey
            || self.key.3 != wkey
            || self.key.5 != stamp.hidden_mask
        {
            self.reset(
                stamp.doc_id,
                stamp.gen,
                mkey,
                wkey,
                stamp.lines,
                stamp.hidden_mask,
            );
            return;
        }
        if self.key.1 != stamp.gen {
            // Edit: drop spans after the touched point and rewind the cursor.
            // edit_from is the earliest line touched this generation;
            // accumulated across generations (doc side takes min); 0 (full
            // scan) when unavailable.
            let from = edit_from.unwrap_or(0).min(stamp.lines);
            let cut = self.spans.partition_point(|&(l, _)| l < from);
            self.extra = if cut < self.cum.len() {
                self.cum[cut]
            } else {
                self.extra
            };
            self.spans.truncate(cut);
            self.breaks.truncate(cut);
            self.cum.truncate(cut);
            if self.next_line > from {
                self.next_line = from;
            }
            self.key.1 = stamp.gen;
        }
        self.key.4 = stamp.lines;
    }

    fn has_line(&self, line: u64) -> bool {
        self.spans.binary_search_by_key(&line, |&(l, _)| l).is_ok()
    }

    /// Insert a span at `pos` (keeping ascending order; called with non-empty `bs`).
    fn insert_span(&mut self, pos: usize, line: u64, bs: Vec<u32>) {
        let k = bs.len() as u32 + 1;
        let before = if pos < self.cum.len() {
            self.cum[pos]
        } else {
            self.extra
        };
        self.spans.insert(pos, (line, k));
        self.breaks.insert(pos, bs);
        self.cum.insert(pos, before);
        let d = (k - 1) as u64;
        self.extra += d;
        for c in self.cum[pos + 1..].iter_mut() {
            *c += d;
        }
    }

    fn measure(&self, text: &str, width: f32, m: &Metrics) -> Option<Vec<u32>> {
        if text.len() > WRAP_LINE_CAP {
            return None; // over-long lines don't wrap (see WRAP_LINE_CAP)
        }
        let bs = break_points(text, width, crate::ui::state::TAB_SIZE, m);
        if bs.is_empty() {
            None
        } else {
            Some(bs)
        }
    }

    /// Progressive scan: measure lines forward within budget. Lines already
    /// measured ahead of time by `ensure_range` are skipped; lines where
    /// `skip` is true (hidden lines) are not measured and add no extra rows.
    pub fn pump(
        &mut self,
        doc: &TextDocument,
        m: &Metrics,
        width: f32,
        skip: impl Fn(u64) -> bool,
    ) {
        let lines = doc.line_count();
        let mut budget = SCAN_BUDGET;
        while budget > 0 && self.next_line < lines {
            let line = self.next_line;
            if !skip(line) && !self.has_line(line) {
                let text = doc.line_text(line);
                if let Some(bs) = self.measure(&text, width, m) {
                    let pos = self.spans.partition_point(|&(l, _)| l < line);
                    self.insert_span(pos, line, bs);
                }
            }
            self.next_line += 1;
            budget -= 1;
        }
    }

    /// Immediately measure unscanned lines in `[first, last)` (visible-window
    /// only: rendered rows must never rely on the unmeasured "1 row" guess).
    /// Inserts keep ascending order; the cursor doesn't move.
    pub fn ensure_range(
        &mut self,
        doc: &TextDocument,
        m: &Metrics,
        width: f32,
        first: u64,
        last: u64,
        skip: impl Fn(u64) -> bool,
    ) {
        let lines = doc.line_count();
        for line in first..last.min(lines) {
            if skip(line) || line < self.next_line || self.has_line(line) {
                continue; // already scanned (in spans or confirmed 1 row)
            }
            let text = doc.line_text(line);
            if let Some(bs) = self.measure(&text, width, m) {
                let pos = self.spans.partition_point(|&(l, _)| l < line);
                self.insert_span(pos, line, bs);
            }
        }
    }

    /// Whether the invalidation key disagrees with current state, or lines remain unscanned.
    pub fn needs_sync(
        &self,
        doc_id: u64,
        gen: u64,
        mkey: u32,
        width: f32,
        lines: u64,
        hidden_mask: u64,
    ) -> bool {
        let key = (
            doc_id,
            gen,
            mkey,
            Self::width_key(width),
            lines,
            hidden_mask,
        );
        self.key != key || self.next_line < lines
    }

    /// Total extra visual rows (unscanned parts count as 1 row; converges monotonically as scanning proceeds).
    pub fn extra_rows(&self) -> u64 {
        self.extra
    }

    /// Visual row count of document line `line` (unscanned/unwrapped = 1).
    pub fn rows_of(&self, line: u64) -> u64 {
        match self.spans.binary_search_by_key(&line, |&(l, _)| l) {
            Ok(i) => self.spans[i].1 as u64,
            Err(_) => 1,
        }
    }

    /// Byte range `[b, e)` covered by the `j`-th visual row of document line
    /// `line` (within the line text, excluding the terminator). Non-wrapped
    /// lines are always `(0, line_len)`.
    pub fn row_range(&self, line: u64, j: u64, line_len: usize) -> (usize, usize) {
        match self.spans.binary_search_by_key(&line, |&(l, _)| l) {
            Err(_) => (0, line_len),
            Ok(i) => {
                let bs = &self.breaks[i];
                let ju = (j as usize).min(bs.len());
                let b = if ju == 0 { 0 } else { bs[ju - 1] as usize };
                let e = if ju < bs.len() {
                    bs[ju] as usize
                } else {
                    line_len
                };
                (b.min(line_len), e.min(line_len))
            }
        }
    }

    /// Visual row containing in-line byte offset `off`: `(j, [b, e))`.
    pub fn row_of_offset(&self, line: u64, off: usize, line_len: usize) -> (u64, usize, usize) {
        match self.spans.binary_search_by_key(&line, |&(l, _)| l) {
            Err(_) => (0, 0, line_len),
            Ok(i) => {
                let bs = &self.breaks[i];
                // count of break points <= off = the row number (off exactly on a break belongs to the next row)
                let ju = bs.partition_point(|&x| (x as usize) <= off).min(bs.len());
                let b = if ju == 0 { 0 } else { bs[ju - 1] as usize };
                let e = if ju < bs.len() {
                    bs[ju] as usize
                } else {
                    line_len
                };
                (ju as u64, b.min(line_len), e.min(line_len))
            }
        }
    }

    /// Cumulative extra rows before screen row `s` (visual start = s + return value).
    ///
    /// Binary search: spans are ascending by line, and screen row numbers
    /// increase monotonically with lines (hidden rows also decrease
    /// monotonically), so spans with `screen_row(span_i) < s` form a prefix.
    pub fn extra_before(&self, s: u64, screen_row: impl Fn(u64) -> u64) -> u64 {
        let n = self.spans.len();
        // index of the first span with screen_row >= s
        let mut lo = 0usize;
        let mut hi = n;
        while lo < hi {
            let mid = (lo + hi) / 2;
            if screen_row(self.spans[mid].0) < s {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        if lo < self.cum.len() {
            self.cum[lo]
        } else {
            self.extra
        }
    }
}

/// Wrap break points of one text line within width `width` (start byte
/// offsets of visual rows 1..k).
///
/// Same metrics as rendering/caret geometry ([`Metrics`]): every visual row
/// is laid out from x=0, tabs advance to in-row tab stops. Strategy in the
/// module docs.
pub fn break_points(text: &str, width: f32, tab: usize, m: &Metrics) -> Vec<u32> {
    let mut out: Vec<u32> = Vec::new();
    if width <= 0.0 || text.is_empty() {
        return out;
    }
    // width used by the current visual row; tabs advance to in-row tab stops.
    let mut x = 0.0f32;
    // Break candidate: after the most recent whitespace run, `(byte offset,
    // x at that point)`. Breaking at the candidate ⇒ whitespace stays at
    // this row's end (word wrap). The candidate is only set when the row
    // already has a word before the whitespace run — otherwise "leading
    // indent / a single tab" would break into whitespace-only rows.
    let mut cand: Option<(usize, f32)> = None;
    let mut in_ws = false;
    let mut row_word = false;
    for (i, ch) in text.char_indices() {
        let (cw, is_ws) = if ch == '\t' {
            (m.tab_stop_after(x, tab) - x, true)
        } else {
            (m.char_w(ch), ch == ' ')
        };
        if x + cw > width && x > 0.0 {
            // the current row can't fit ch: break.
            let (b, bx) = match cand {
                // candidate must fall inside this row and not produce an empty row
                Some(c) if c.1 > 0.0 && c.0 > 0 => c,
                _ => (i, x), // no candidate → hard-break before ch
            };
            out.push(b as u32);
            x -= bx; // text before ch is re-measured from the new row's start
            cand = None;
            in_ws = false;
            row_word = false;
        }
        if is_ws {
            in_ws = true;
        } else if in_ws {
            // whitespace run ends at i: later breaks prefer this point
            if x > 0.0 && row_word {
                cand = Some((i, x));
            }
            in_ws = false;
        } else {
            row_word = true;
        }
        x += cw;
    }
    out
}

// ---- EditorState integration ---------------------------------------------------
//
// The row-space chain now has one more layer:
//
// ```text
// doc line --jsonl--> screen row --wrap--> visual row
// ```
//
// This only provides mapping and refresh; rendering/hit-testing/caret
// movement in `viewport` and `commands::edit` consume these methods. Font
// metrics, window width, or hidden-line set changes reset the whole table
// in `sync`; edits truncate and rescan from the earliest affected line.

impl super::EditorState {
    /// Whether soft wrap is currently active. The whole-document JSON tree is
    /// a read-only derived view and does not participate in wrapping.
    pub fn wrap_enabled(&self) -> bool {
        self.wrap && !self.tabs[self.active].json.enabled
    }

    /// Text width available for wrapping (minus left padding: every visual row is laid out from x=0).
    pub fn wrap_width(&self) -> f32 {
        (self.text_w() - super::PAD_LEFT).max(1.0)
    }

    /// Fast fingerprint of the hidden-line set (content change ⇒ wrap index reset).
    fn hidden_mask(&self, id: u64) -> u64 {
        let mut m = 0x9E37_79B9_7F4A_7C15u64;
        if let Some(set) = self.hidden.get(&id) {
            for &l in set {
                m ^= l.wrapping_add(0x9E37_79B9_7F4A_7C15);
                m = m.wrapping_mul(0x100_0000_01B3);
            }
        }
        m
    }

    /// Pre-render self-heal: sync the wrap index (visible lines measured
    /// immediately, the rest advances progressively within budget).
    pub fn refresh_wrap(&mut self) {
        if !self.wrap_enabled() {
            return;
        }
        let (id, gen, lines) = {
            let d = self.active_doc();
            (d.id, d.gen, d.line_count())
        };
        let edit_from = self.active_doc().edit_from_line_hint();
        let width = self.wrap_width();
        let mkey = WrapIndex::metrics_key(self.metrics.adv);
        let mask = self.hidden_mask(id);
        let hidden = self.hidden.get(&id).cloned().unwrap_or_default();
        let skip = |l: u64| hidden.contains(&l);
        self.wrap_index.sync(
            WrapStamp {
                doc_id: id,
                gen,
                lines,
                hidden_mask: mask,
            },
            mkey,
            width,
            edit_from,
        );
        let (first, last) = self.visible_doc_range_screen();
        let tab = &self.tabs[self.active];
        self.wrap_index
            .ensure_range(&tab.doc, &self.metrics, width, first, last, skip);
        self.wrap_index.pump(&tab.doc, &self.metrics, width, skip);
    }

    /// Whether the wrap index needs syncing (matching [`Self::refresh_wrap`]'s early-return conditions).
    pub fn wrap_stale(&self) -> bool {
        if !self.wrap_enabled() {
            return false;
        }
        let d = self.active_doc();
        let width = self.wrap_width();
        self.wrap_index.needs_sync(
            d.id,
            d.gen,
            WrapIndex::metrics_key(self.metrics.adv),
            width,
            d.line_count(),
            self.hidden_mask(d.id),
        )
    }

    /// Visual row at a screen row's start.
    pub fn visual_start_of_screen(&self, s: u64) -> u64 {
        if !self.wrap_enabled() {
            return s;
        }
        s + self
            .wrap_index
            .extra_before(s, |line| self.screen_vstart(line))
    }

    /// Total visual rows (scroll upper bound, content height).
    pub fn visual_rows(&self) -> u64 {
        if self.wrap_enabled() {
            self.screen_rows() + self.wrap_index.extra_rows()
        } else {
            self.screen_rows()
        }
    }

    /// Visual row `v` → `(screen row, index of the visual sub-row within that screen row)`.
    pub fn screen_of_visual(&self, v: u64) -> (u64, u64) {
        if !self.wrap_enabled() {
            return (v, 0);
        }
        let total = self.screen_rows();
        if total == 0 {
            return (0, 0);
        }
        // the largest s with visual_start_of_screen(s) <= v; s may point at a
        // virtual row, which has no sub-rows; callers handle the Virtual branch.
        let mut lo = 0u64;
        let mut hi = total - 1;
        while lo < hi {
            let mid = (lo + hi).div_ceil(2);
            if self.visual_start_of_screen(mid) <= v {
                lo = mid;
            } else {
                hi = mid - 1;
            }
        }
        (lo, v.saturating_sub(self.visual_start_of_screen(lo)))
    }

    /// Visual row containing byte position `pos`: `(visual row, document line, sub-row, in-line start, in-line end)`.
    pub fn visual_row_of_pos(&self, pos: u64) -> (u64, u64, u64, usize, usize) {
        let doc = self.active_doc();
        let line = doc.line_of_pos(pos);
        if !self.wrap_enabled() {
            return (
                self.screen_vstart(line),
                line,
                0,
                0,
                doc.line_span(line)
                    .end
                    .saturating_sub(doc.pos_of_line(line)) as usize,
            );
        }
        let text = doc.line_text(line);
        let off = (pos - doc.pos_of_line(line)) as usize;
        let (sub, a, b) = self.wrap_index.row_of_offset(line, off, text.len());
        let v = self.visual_start_of_screen(self.screen_vstart(line)) + sub;
        (v, line, sub, a, b)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m() -> Metrics {
        Metrics::new(None, 14.0) // no font file: goes through the unicode-width classification
    }

    /// Half-width advance at monospace 14px (em * 0.55 = 7.7px)
    fn adv(m: &Metrics) -> f32 {
        m.char_w('x')
    }

    #[test]
    fn short_line_never_breaks() {
        let m = m();
        assert!(break_points("hello", adv(&m) * 100.0, 4, &m).is_empty());
        assert!(break_points("", 100.0, 4, &m).is_empty());
        assert!(break_points("hello", 0.0, 4, &m).is_empty());
    }

    #[test]
    fn word_wrap_prefers_space_boundaries() {
        let m = m();
        let a = adv(&m);
        // "aaa bbb ccc": 7 columns per line. Break after the space; the second
        // row "bbb ccc" is exactly 7 columns too, so it fits → 2 rows, 1 break.
        let bs = break_points("aaa bbb ccc", a * 7.0, 4, &m);
        assert_eq!(bs, vec![4], "break after the first space");
    }

    #[test]
    fn long_token_hard_breaks() {
        let m = m();
        let a = adv(&m);
        // space-free long token: hard-break every 4 columns
        let bs = break_points("abcdefgh", a * 4.0, 4, &m);
        assert_eq!(bs, vec![4]);
        let bs = break_points("abcdef", a * 4.0, 4, &m);
        assert_eq!(bs, vec![4]);
    }

    #[test]
    fn oversized_single_char_overflows_without_empty_rows() {
        let m = m();
        let a = adv(&m);
        // wide character (CJK = 1em = 2a) wider than a row: no empty-row break points
        let bs = break_points("中", a * 1.5, 4, &m);
        assert!(bs.is_empty());
    }

    #[test]
    fn cjk_breaks_every_char() {
        let m = m();
        let a = adv(&m);
        // CJK is 1em (== 2a) wide per character: one row holds 2 → 2 rows, 1 break point
        let bs = break_points("中文中文", a * 4.0, 4, &m);
        assert_eq!(bs, vec![6], "byte offsets (3 bytes per char)");
    }

    #[test]
    fn tabs_advance_to_row_local_stops() {
        let m = m();
        let step = m.adv * 4.0; // Metrics' internal tab step
        let w = step + 4.0 * m.char_w('x');
        // "\taaaa": tab jumps to 4 columns + 4 half-widths → exactly one row
        assert!(break_points("\taaaa", w, 4, &m).is_empty());
        // one more character breaks; no word after the tab → no candidate, hard-break before the 5th 'a'
        let bs = break_points("\taaaaa", w, 4, &m);
        assert_eq!(bs, vec![5]);
    }

    #[test]
    fn leading_whitespace_stays_on_first_row() {
        let m = m();
        let a = adv(&m);
        // 8-column indent + content: a 10-column row can't fit the whole
        // indent+abcd → hard break;
        // no whitespace run before a word after the indent → never break out
        // a whitespace-only row
        let text = "        abcd efgh";
        let bs = break_points(text, a * 10.0, 4, &m);
        assert_eq!(
            bs,
            vec![10],
            "break after 'ab' (no whitespace candidate, hard break)"
        );
    }

    // ---- WrapIndex ----

    fn doc_of(text: &str) -> TextDocument {
        TextDocument::from(text)
    }

    fn stamp(doc_id: u64, gen: u64, lines: u64, hidden_mask: u64) -> WrapStamp {
        WrapStamp {
            doc_id,
            gen,
            lines,
            hidden_mask,
        }
    }

    #[test]
    fn index_identity_when_nothing_wraps() {
        let mut idx = WrapIndex::default();
        let doc = doc_of("short\nlines\n");
        let m = m();
        idx.sync(stamp(1, 0, doc.line_count(), 0), 0, 1000.0, None);
        idx.pump(&doc, &m, 1000.0, |_| false);
        assert_eq!(idx.extra_rows(), 0);
        assert_eq!(idx.rows_of(0), 1);
        assert_eq!(idx.row_range(1, 0, 5), (0, 5));
        assert_eq!(idx.row_of_offset(1, 3, 5), (0, 0, 5));
    }

    #[test]
    fn rows_and_ranges_round_trip() {
        let mut idx = WrapIndex::default();
        let doc = doc_of("aaa bbb ccc\ntail\n");
        let m = m();
        let width = adv(&m) * 7.0;
        idx.sync(stamp(1, 0, doc.line_count(), 0), 0, width, None);
        idx.ensure_range(&doc, &m, width, 0, doc.line_count(), |_| false);
        // "aaa bbb ccc" in 7 columns wraps into 2 rows: "aaa b" / "bb ccc"?
        // No — word wrap wins: break after the space → "aaa " / "bbb ccc" (exactly 7 columns)
        assert_eq!(idx.rows_of(0), 2);
        assert_eq!(idx.rows_of(1), 1);
        assert_eq!(idx.extra_rows(), 1);
        // visual row byte ranges
        assert_eq!(idx.row_range(0, 0, 11), (0, 4));
        assert_eq!(idx.row_range(0, 1, 11), (4, 11));
        // offset → visual row
        assert_eq!(idx.row_of_offset(0, 0, 11).0, 0);
        assert_eq!(idx.row_of_offset(0, 4, 11).0, 1);
        assert_eq!(idx.row_of_offset(0, 7, 11).0, 1);
        assert_eq!(idx.row_of_offset(0, 10, 11).0, 1);
    }

    #[test]
    fn edit_truncates_from_touched_line() {
        let mut idx = WrapIndex::default();
        let mut doc = doc_of("aaa bbb ccc\ntail\n");
        let m = m();
        let width = adv(&m) * 7.0;
        idx.sync(stamp(1, 0, doc.line_count(), 0), 0, width, None);
        idx.pump(&doc, &m, width, |_| false);
        assert_eq!(idx.rows_of(0), 2);

        // edit line 0: gen change + edit_from=0 → full rescan
        doc.insert_text(0, "xxxx ");
        // the test edits from line 0 directly, i.e. "first affected line = 0"
        idx.sync(stamp(1, doc.gen, doc.line_count(), 0), 0, width, Some(0));
        assert_eq!(idx.rows_of(0), 1, "counted as 1 row before rescan");
        idx.pump(&doc, &m, width, |_| false);
        assert!(idx.rows_of(0) >= 2, "line 0 should still wrap after rescan");
    }

    #[test]
    fn width_change_resets_everything() {
        let mut idx = WrapIndex::default();
        let doc = doc_of("aaa bbb ccc\n");
        let m = m();
        let w1 = adv(&m) * 7.0;
        idx.sync(stamp(1, 0, doc.line_count(), 0), 0, w1, None);
        idx.pump(&doc, &m, w1, |_| false);
        assert_eq!(idx.extra_rows(), 1);
        // window got wider: one row fits → extra returns to 0 after reset
        let w2 = adv(&m) * 100.0;
        idx.sync(stamp(1, 0, doc.line_count(), 0), 0, w2, None);
        idx.pump(&doc, &m, w2, |_| false);
        assert_eq!(idx.extra_rows(), 0);
    }

    #[test]
    fn ensure_range_measures_ahead_of_the_cursor() {
        let mut idx = WrapIndex::default();
        // 500 lines, only the first wraps
        let mut text = String::from("aaa bbb ccc\n");
        for i in 0..499 {
            text.push_str(&format!("line{i}\n"));
        }
        let doc = doc_of(&text);
        let m = m();
        let width = adv(&m) * 7.0;
        idx.sync(stamp(1, 0, doc.line_count(), 0), 0, width, None);
        // user jumps straight near line 400 (scan cursor hasn't reached it)
        idx.ensure_range(&doc, &m, width, 400, 402, |_| false);
        assert_eq!(idx.rows_of(400), 1);
        assert_eq!(idx.rows_of(0), 1, "line 0 unmeasured, counted as 1 row");
        idx.pump(&doc, &m, width, |_| false);
        assert_eq!(
            idx.rows_of(0),
            2,
            "line 0 fully measured once the scan catches up"
        );
        // lines inserted by ensure are not re-measured by pump
        assert_eq!(idx.rows_of(400), 1);
    }

    #[test]
    fn extra_before_walks_cumulative_sums() {
        let mut idx = WrapIndex::default();
        let doc = doc_of("aaa bbb ccc\nmid\nzzz yyy www\nend\n");
        let m = m();
        let width = adv(&m) * 7.0;
        idx.sync(stamp(1, 0, doc.line_count(), 0), 0, width, None);
        idx.pump(&doc, &m, width, |_| false);
        // lines 0 and 2 wrap ("mid", "end" don't); extra counts and cumulative sums agree
        let k0 = idx.rows_of(0);
        let k2 = idx.rows_of(2);
        assert!(k0 >= 2, "line 0 should wrap");
        assert!(k2 >= 2, "line 2 should wrap");
        let sr = |l: u64| l; // no hidden lines: screen row == document line
        assert_eq!(idx.extra_before(0, sr), 0);
        assert_eq!(idx.extra_before(1, sr), k0 - 1, "line 0's extra rows");
        assert_eq!(idx.extra_before(2, sr), k0 - 1);
        assert_eq!(
            idx.extra_before(3, sr),
            k0 + k2 - 2,
            "lines 0+2's extra rows"
        );
        assert_eq!(idx.extra_before(4, sr), k0 + k2 - 2);
    }

    /// Hidden lines don't wrap: total visual rows don't count invisible lines.
    #[test]
    fn hidden_lines_are_not_counted() {
        use crate::ui::testutil::st_of_view;
        let mut st = st_of_view("aaa bbb ccc ddd\nplain\n");
        st.view.w = 140.0;
        st.wrap = true;
        st.self_heal();
        let wrapped = st.visual_rows();
        assert!(wrapped > st.screen_rows());
        let id = st.active_doc().id;
        st.hidden.insert(id, std::collections::BTreeSet::from([0]));
        st.self_heal();
        assert_eq!(
            st.visual_rows(),
            st.screen_rows(),
            "hidden lines produce no extra visual rows"
        );
        for v in 0..st.visual_rows() {
            let (s, sub) = st.screen_of_visual(v);
            assert_eq!(
                st.visual_start_of_screen(s) + sub,
                v,
                "visual row mapping should round-trip"
            );
        }
        st.hidden.remove(&id);
        st.self_heal();
        assert_eq!(
            st.visual_rows(),
            wrapped,
            "wrapping restored after unhiding"
        );
    }

    #[test]
    fn oversize_line_is_left_unwrapped() {
        let mut idx = WrapIndex::default();
        let long = "x".repeat(WRAP_LINE_CAP + 1);
        let doc = doc_of(&format!("{long}\n"));
        let m = m();
        let width = adv(&m) * 7.0;
        idx.sync(stamp(1, 0, doc.line_count(), 0), 0, width, None);
        idx.pump(&doc, &m, width, |_| false);
        assert_eq!(idx.rows_of(0), 1, "over-long lines don't wrap");
        assert_eq!(idx.extra_rows(), 0);
    }
}
