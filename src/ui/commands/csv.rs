//! Command layer for the CSV toolbar / table view: mode and delimiter
//! switching, header row, column sorting, and table↔text round-trips
//! (double-clicking a cell jumps back to text mode).
//!
//! **Sorting is a view lens, not an edit.** The table view can be ordered by
//! any column (`sort_col` / `sort_desc`), but the document text, its dirty
//! flag and the undo stack are never touched: sorting reorders the parse cache
//! ([`crate::ui::state::csv::CsvCache`]) only, so a sorted file saves in its
//! original row order and a read-only document can still be sorted. Every
//! row→text address (`line_of_row`, `end_line_of_row`) travels with its row,
//! which is what makes editing / deleting / inserting in a sorted view hit the
//! record on screen. Clearing the lens restores file order by permuting back
//! via `CsvCache::doc_row`, not by re-parsing.
//!
//! Structural row edits (insert / delete row) are defined in **file order**,
//! so they drop the lens first rather than guess what "the row above" means in
//! a reordered view.
//!
//! Writing a cell back uses the minimal-quoting rule
//! ([`crate::format::serialize_csv_row`]), quoting only fields that need it.

use crate::ui::state::csv::{
    auto_col_widths, CellSel, CsvCache, CsvRows, CsvRows2, SelKind, CSV_MAX_BYTES, CSV_MAX_ROWS,
    EMPTY_COL_W, MAX_EMPTY_COLS,
};
use crate::ui::state::EditorState;
use crate::ui::{Fmt, Msg};

/// Thumb geometry: `(offset, len)`, length proportional to content/track, min 24px.
/// Shared with the csvtable component so drag mapping and drawing agree.
pub(crate) fn thumb_geom(scroll: f32, total: f32, track_len: f32) -> (f32, f32) {
    if total <= track_len || total <= 0.0 {
        return (0.0, track_len);
    }
    let len = (track_len * track_len / total).clamp(24.0, track_len);
    let max_scroll = total - track_len;
    let max_off = track_len - len;
    let off = if max_scroll > 0.0 {
        scroll / max_scroll * max_off
    } else {
        0.0
    };
    (off, len)
}

impl EditorState {
    /// Records the byte range for `[start_line, end_line)`. When the last
    /// record has no terminator, `end_line == line_count` and the range ends
    /// at the document's end.
    fn row_byte_span(&self, start_line: u64, end_line: u64) -> std::ops::Range<u64> {
        let doc = self.active_doc();
        let start = doc.pos_of_line(start_line);
        let end = if end_line < doc.line_count() {
            doc.pos_of_line(end_line)
        } else {
            doc.total_bytes()
        };
        start..end
    }

    // ---- Cached parse results -----------------------------------------------

    /// Parse result (including the header row), re-parsing at the current
    /// `(doc_id, gen, delim)` when needed. Returns `(cache, truncated)`; an
    /// empty document yields an empty row set.
    pub fn csv_rows_ensure(&mut self) -> CsvRows {
        let doc_id = self.tab().doc.id;
        let gen = self.tab().doc.gen;
        let delim = self.tab().csv.delim;
        if !self.tab().csv.fresh(doc_id, gen, delim) {
            // The order the rows are displayed in *before* this rebuild: a live
            // cell edit freezes it (see below), clearing a lens restores it
            let prev_order = self.tab().csv.cache.as_ref().map(|c| c.doc_row.clone());
            let editing = self.tab().csv.editing.is_some();
            let end = self.active_doc().total_bytes().min(CSV_MAX_BYTES);
            let src = self.active_doc().text_range(0..end);
            let indexed = crate::format::parse_csv_spans(&src, delim);
            let truncated = indexed.len() > CSV_MAX_ROWS || end < self.active_doc().total_bytes();
            let mut lines: Vec<u64> = Vec::with_capacity(indexed.len().min(CSV_MAX_ROWS));
            let mut end_lines: Vec<u64> = Vec::with_capacity(lines.capacity());
            let mut rows: Vec<Vec<String>> = Vec::with_capacity(lines.capacity());
            for (start, stop, row) in indexed.into_iter().take(CSV_MAX_ROWS) {
                lines.push(start);
                end_lines.push(stop);
                rows.push(row);
            }
            let n_rows = rows.len();
            {
                let csv = &mut self.tab_mut().csv;
                // Document changed ⇒ the row→document-line mapping is entirely
                // invalid: row-height overrides (keyed by row) must be cleared,
                // otherwise heights get mismatched after sorting/editing.
                // Column widths are keyed by index against columns and usually
                // still valid, so they are kept (column-count changes are
                // clamped by the table component).
                // `editing` is **kept**: live cell editing rewrites the whole
                // row once per keystroke and bumps gen; the data-row index the
                // edit state references stays valid under single-row rewrite,
                // and clearing it would interrupt typing. Structural operations
                // (sort/insert-delete rows & cols/view switch) each clear it
                // explicitly.
                csv.row_heights.clear();
                csv.col_widths
                    .resize(rows.first().map(Vec::len).unwrap_or(0), None);
            }
            let doc_row: Vec<usize> = (0..n_rows).collect();
            self.tab_mut().csv.cache = Some(CsvCache {
                doc_id,
                gen,
                delim,
                rows: std::rc::Rc::new(rows),
                line_of_row: std::rc::Rc::new(lines),
                end_line_of_row: std::rc::Rc::new(end_lines),
                doc_row: std::rc::Rc::new(doc_row),
                truncated,
            });
            // Re-apply the view lens to the new row set. A live edit freezes the
            // order instead: the rows are the same records (a cell value cannot
            // contain a newline), so reusing the previous permutation keeps the
            // edited row under the caret and keeps a sort out of the per-keystroke
            // path. A changed row count falls back to a real re-sort.
            let frozen = editing
                .then_some(prev_order)
                .flatten()
                .filter(|prev| prev.len() == n_rows);
            match frozen {
                // `permuted` returns `None` when the frozen order is the identity,
                // i.e. the fresh cache already shows it — nothing to swap
                Some(prev) => {
                    let next = self
                        .tab()
                        .csv
                        .cache
                        .as_ref()
                        .and_then(|c| c.permuted(&prev));
                    if let Some(next) = next {
                        self.tab_mut().csv.cache = Some(next);
                    }
                }
                None => self.csv_apply_lens(),
            }
        }
        let c = self.tab().csv.cache.clone().unwrap();
        (c.rows, c.line_of_row, c.end_line_of_row, c.truncated)
    }

    /// Read-only version of [`Self::csv_rows_ensure`]: returns `(rows,
    /// line_of_row, end_line_of_row, truncated)` when the cache is fresh, or
    /// `None` when stale (no side effects).
    ///
    /// The table view's build path uses this for write-need probing: during
    /// rendering, a `state.write()` guard drop unconditionally marks dirty, so
    /// writing every frame would create a self-sustaining render loop — the
    /// build must be pure-read in steady state (same pattern as the
    /// viewport/toolbar conditional writes).
    pub fn csv_rows_cached(&self) -> Option<CsvRows> {
        let doc_id = self.tab().doc.id;
        let gen = self.tab().doc.gen;
        let delim = self.tab().csv.delim;
        if !self.tab().csv.fresh(doc_id, gen, delim) {
            return None;
        }
        let c = self.tab().csv.cache.clone()?;
        Some((c.rows, c.line_of_row, c.end_line_of_row, c.truncated))
    }

    // ---- Sort lens (view order only) -----------------------------------------

    /// Document row shown at `view_row` (`None` when the cache is missing or
    /// the index is out of range).
    fn csv_doc_row(&self, view_row: usize) -> Option<usize> {
        self.tab()
            .csv
            .cache
            .as_ref()?
            .doc_row
            .get(view_row)
            .copied()
    }

    /// View position currently showing document row `doc_row`.
    fn csv_view_row_of(&self, doc_row: usize) -> Option<usize> {
        self.tab()
            .csv
            .cache
            .as_ref()?
            .doc_row
            .iter()
            .position(|&d| d == doc_row)
    }

    /// Keep the selection on the same **record** across a re-order.
    ///
    /// `sel.row` is a view position, so permuting the cache would leave the
    /// highlight on whichever record happens to land there. The document row is
    /// the stable identity, so remember it before the swap and look it up after.
    fn csv_keep_selection_on_record(&mut self, doc_row: Option<usize>) {
        let Some(doc) = doc_row else {
            return;
        };
        let hdr = self.tab().csv.header as usize;
        let Some(pos) = self.csv_view_row_of(doc) else {
            return;
        };
        if let Some(mut sel) = self.tab().csv.sel {
            sel.row = pos.saturating_sub(hdr);
            self.tab_mut().csv.sel = Some(sel);
        }
    }

    /// Selection translated into a document row (header offset included).
    fn csv_selection_doc_row(&self) -> Option<usize> {
        let sel = self.tab().csv.sel?;
        let hdr = self.tab().csv.header as usize;
        self.csv_doc_row(sel.row + hdr)
    }

    /// Re-apply the active sort lens to the parse cache (view order only).
    ///
    /// The comparison is `(sort key, document row)`, so the arrangement never
    /// depends on the order the rows happen to be in: sorting by another
    /// column, sorting twice, or re-sorting after a rebuild all converge on the
    /// same table, and equal keys keep document order. Nothing here touches the
    /// document, the undo stack or the dirty flag.
    fn csv_apply_lens(&mut self) {
        let Some(col) = self.tab().csv.sort_col else {
            self.csv_restore_doc_order();
            return;
        };
        let Some(cache) = self.tab().csv.cache.clone() else {
            return;
        };
        let desc = self.tab().csv.sort_desc;
        let header = self.tab().csv.header as usize;
        let n = cache.rows.len();
        // Zero or one data row: every order is the same table
        if n <= header + 1 {
            self.csv_restore_doc_order();
            return;
        }
        let n_cols = cache.rows.iter().map(Vec::len).max().unwrap_or(0);
        let col = col.min(n_cols.saturating_sub(1));
        let mut keyed: Vec<(f64, String, usize)> = (header..n)
            .map(|i| {
                let cell = cache.rows[i].get(col).map(String::as_str).unwrap_or("");
                let (num, text) = crate::format::csv_sort_key(cell);
                (num, text, cache.doc_row[i])
            })
            .collect();
        keyed.sort_by(|a, b| {
            let ord =
                a.0.partial_cmp(&b.0)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| a.1.cmp(&b.1))
                    .then_with(|| a.2.cmp(&b.2));
            if desc {
                ord.reverse()
            } else {
                ord
            }
        });
        // Document row → position in the order currently on screen
        let mut pos_of_doc = vec![0usize; n];
        for (pos, &doc) in cache.doc_row.iter().enumerate() {
            if let Some(slot) = pos_of_doc.get_mut(doc) {
                *slot = pos;
            }
        }
        let mut order: Vec<usize> = (0..header.min(n)).collect();
        order.extend(keyed.iter().map(|&(_, _, doc)| pos_of_doc[doc]));
        let sel_doc = self.csv_selection_doc_row();
        if let Some(next) = cache.permuted(&order) {
            self.tab_mut().csv.cache = Some(next);
            self.tab_mut().csv.row_heights.clear();
            self.csv_keep_selection_on_record(sel_doc);
        }
    }

    /// Put the table back into document order (identity `doc_row`).
    ///
    /// Exact and cheap: the cache carries the original order in `doc_row`, so
    /// clearing a lens is a permutation, not a re-parse.
    fn csv_restore_doc_order(&mut self) {
        let Some(cache) = self.tab().csv.cache.clone() else {
            return;
        };
        if cache.in_doc_order() {
            return;
        }
        let mut order: Vec<usize> = (0..cache.rows.len()).collect();
        order.sort_by_key(|&pos| cache.doc_row.get(pos).copied().unwrap_or(pos));
        let sel_doc = self.csv_selection_doc_row();
        if let Some(next) = cache.permuted(&order) {
            self.tab_mut().csv.cache = Some(next);
            self.tab_mut().csv.row_heights.clear();
            self.csv_keep_selection_on_record(sel_doc);
        }
    }

    /// Drop the sort lens (back to file order); the status message is the
    /// caller's.
    fn csv_drop_lens(&mut self) {
        self.tab_mut().csv.sort_col = None;
        self.tab_mut().csv.sort_desc = false;
        self.csv_restore_doc_order();
    }

    /// Drop the sort lens and say so (header-click third state).
    pub fn csv_clear_sort(&mut self) {
        self.csv_drop_lens();
        self.status = Some(Msg::StatusCsvSortCleared.into());
    }

    /// Translate the selection into document coordinates, then drop the lens.
    ///
    /// Structural row edits are stated in file order ("above the first record",
    /// "append after the last one") while `sel.row` is a view-row index, so the
    /// two conversions belong together. Returns the translated selection.
    ///
    /// The translation happens **before** the drop, the write-back **after** it:
    /// `csv_drop_lens` restores document order through
    /// `csv_keep_selection_on_record`, which reads `sel.row` as a view position.
    /// Storing the translated index first would make it read a document index as
    /// a view position and park the highlight on another record.
    fn csv_file_order_selection(&mut self) -> Option<CellSel> {
        let hdr = self.tab().csv.header as usize;
        let cur = self.tab().csv.sel;
        let sel = cur.and_then(|s| {
            let doc = self.csv_doc_row(s.row + hdr)?;
            Some(CellSel {
                row: doc.saturating_sub(hdr),
                ..s
            })
        });
        self.csv_drop_lens();
        self.tab_mut().csv.sel = sel;
        sel
    }

    // ---- Toolbar actions ------------------------------------------------------

    /// Toolbar/menu: table view ↔ text view mode toggle.
    pub fn csv_toggle_mode(&mut self) {
        let t = self.tab_mut();
        t.csv.table = !t.csv.table;
        t.csv.scroll_top = 0.0;
        t.csv.scroll_left = 0.0;
        // Leaving table view: the in-place edit state is dropped with it (live edits already committed)
        t.csv.editing = None;
        let to_table = t.csv.table;
        self.view_switched(if to_table {
            Msg::StatusCsvTableView
        } else {
            Msg::StatusViewText
        });
    }

    /// View menu → CSV table view: documents without the CSV toolbar enable
    /// it first, then switch to table view (when already on, clicking returns
    /// to text view, matching standard toggle-item behavior).
    pub fn csv_toggle_table(&mut self) {
        if !self.tab().csv.enabled {
            self.tab_mut().csv.enabled = true;
            self.tab_mut().csv.table = true;
            self.view_switched(Msg::StatusCsvTableView);
        } else {
            self.csv_toggle_mode();
        }
    }

    /// Toolbar: select delimiter (`,` `\t` `;` `|`). Forces a re-parse after switching.
    pub fn csv_set_delim(&mut self, delim: u8) {
        if self.tab().csv.delim == delim {
            return;
        }
        let t = self.tab_mut();
        t.csv.delim = delim;
        t.csv.cache = None;
        // A different delimiter is a different table: the lens means nothing
        // for the new one, and the rebuild starts from file order anyway
        t.csv.sort_col = None;
        t.csv.sort_desc = false;
        t.csv.sel = None;
        t.csv.col_widths.clear();
        t.csv.editing = None;
        let name = match delim {
            b'\t' => "Tab".to_string(),
            _ => (delim as char).to_string(),
        };
        self.status = Some(Fmt::StatusCsvDelim { name }.into());
    }

    /// Toolbar: whether the first row acts as the header.
    pub fn csv_toggle_header(&mut self) {
        // The header decides which rows the lens may reorder, and it also shifts
        // every row index by one: end the in-place edit (live write-back has
        // already committed the cell), drop the lens, and drop a view-relative
        // row selection (a column selection keeps its meaning — column indexes
        // do not move), so no edit/selection state points at the wrong record.
        self.tab_mut().csv.editing = None;
        self.csv_drop_lens();
        if matches!(self.tab().csv.sel, Some(s) if s.kind != SelKind::Col) {
            self.tab_mut().csv.sel = None;
        }
        let t = self.tab_mut();
        t.csv.header = !t.csv.header;
        self.status = Some(
            if t.csv.header {
                Msg::StatusCsvHeaderOn
            } else {
                Msg::StatusCsvHeaderOff
            }
            .into(),
        );
    }

    /// Record the lens column (no reordering — [`Self::csv_sort`] applies it).
    pub fn csv_pick_column(&mut self, col: usize) {
        self.tab_mut().csv.sort_col = Some(col);
    }

    /// Header click: the sort indicator lives on the titles (the toolbar no
    /// longer has sort ↑/↓). Excel-style three-state cycle: unsorted →
    /// ascending → descending → cleared.
    pub fn csv_header_click(&mut self, col: usize) {
        let (prev_col, prev_desc) = (self.tab().csv.sort_col, self.tab().csv.sort_desc);
        match prev_col {
            Some(c) if c == col && prev_desc => {
                // Third click: drop the lens (the table returns to file order;
                // there is nothing to undo, the document was never reordered)
                self.csv_clear_sort();
            }
            Some(c) if c == col => {
                // Already ascending → descending
                self.csv_sort(true);
            }
            _ => {
                let prev = (self.tab().csv.sort_col, self.tab().csv.sort_desc);
                self.csv_pick_column(col);
                if !self.csv_sort(false) {
                    // Refused (no data / too few rows / truncated table): undo the
                    // column pick so no arrow points at an unapplied lens
                    self.tab_mut().csv.sort_col = prev.0;
                    self.tab_mut().csv.sort_desc = prev.1;
                }
            }
        }
    }

    /// Cell click: selection highlight. Clicking another cell implicitly ends
    /// in-place editing (live editing already committed the current value, so
    /// just drop the edit state — no data loss).
    pub fn csv_pick_cell(&mut self, row: usize, col: usize) {
        if self
            .tab()
            .csv
            .editing
            .as_ref()
            .is_some_and(|e| e.row != row || e.col != col)
        {
            self.tab_mut().csv.editing = None;
        }
        self.tab_mut().csv.sel = Some(CellSel::cell(row, col));
    }

    /// Row-number click: select the whole row (target for insert/delete row). Also implicitly ends editing.
    pub fn csv_pick_row(&mut self, row: usize) {
        self.tab_mut().csv.editing = None;
        self.tab_mut().csv.sel = Some(CellSel::row(row));
    }

    /// Column-letter click: select the whole column (target for insert/delete
    /// column). Implicitly ends editing.
    ///
    /// Sorting belongs to the header **text** click ([`Self::csv_header_click`]);
    /// the letter strip used to record the sort column as well, which drew the
    /// ↑/↓ arrow on a column the table was not ordered by.
    pub fn csv_pick_col(&mut self, col: usize) {
        self.tab_mut().csv.editing = None;
        self.tab_mut().csv.sel = Some(CellSel::col(col));
    }

    /// Sort the table **view** by the picked column (column 0 when none is
    /// picked), ascending or descending. Returns whether a lens was applied.
    ///
    /// Display only: the document text, its dirty flag and the undo stack are
    /// untouched, so a sorted file still saves in its original row order and a
    /// read-only document can be sorted. Rows are addressed through
    /// `line_of_row`, which travels with each row, so editing / deleting /
    /// inserting in a sorted view still hits the record on screen.
    pub fn csv_sort(&mut self, desc: bool) -> bool {
        // Rows move under the cursor: the in-place edit's row index would point
        // at another cell
        self.tab_mut().csv.editing = None;
        let (rows, _, _, truncated) = self.csv_rows_ensure();
        if rows.is_empty() {
            self.status = Some(Msg::StatusCsvSortNoData.into());
            return false;
        }
        let header = self.tab().csv.header as usize;
        if rows.len() <= header + 1 {
            self.status = Some(Msg::StatusCsvSortTooFewRows.into());
            return false;
        }
        // The table shows only the front of an oversized file: ordering that
        // prefix would look as if the whole file had been sorted.
        if truncated {
            self.status = Some(Msg::StatusCsvSortTruncated.into());
            return false;
        }
        let n_cols = rows.iter().map(Vec::len).max().unwrap_or(0);
        let col = self
            .tab()
            .csv
            .sort_col
            .unwrap_or(0)
            .min(n_cols.saturating_sub(1));
        // Committed only now: a refused sort must not leave an arrow pointing at
        // a column the table is not ordered by
        self.tab_mut().csv.sort_col = Some(col);
        self.tab_mut().csv.sort_desc = desc;
        self.csv_apply_lens();
        self.status = Some(
            if desc {
                Fmt::StatusCsvSortedDesc { col: col + 1 }
            } else {
                Fmt::StatusCsvSortedAsc { col: col + 1 }
            }
            .into(),
        );
        true
    }

    /// Table double-click cell: jump back to text mode with the caret at that
    /// line's start (the row's fields may be quoted in the source, so byte
    /// offsets cannot be reliably reverse-derived; line start is a stable
    /// anchor).
    pub fn csv_cell_to_text(&mut self, row_idx: usize) {
        let (_, line_of_row, _, _) = self.csv_rows_ensure();
        let Some(&line) = line_of_row.get(row_idx) else {
            return;
        };
        self.tab_mut().csv.table = false;
        self.tab_mut().csv.editing = None;
        self.relayout();
        let pos = self.active_doc().pos_of_line(line);
        self.set_caret(pos, false);
        self.scroll_caret_into_view();
        self.status = Some(Msg::StatusCsvToText.into());
    }

    /// Menu is_checked: whether table mode is on.
    pub fn csv_table_active(&self) -> bool {
        self.tab().csv.enabled && self.tab().csv.table
    }

    /// Table view scroll clamping (the wheel path calls this after each
    /// scroll; the build path clamps itself too).
    pub fn clamp_csv_scroll(&mut self) {
        let lh = crate::ui::state::csv::ROW_H.max(self.metrics.lh.max(1.0));
        let text_h = self.text_h().max(40.0);
        let (rows, _, _, _) = self.csv_rows_ensure();
        let data = rows.len().saturating_sub(self.tab().csv.header as usize);
        let max_v = (data as f64 * lh as f64 - text_h as f64).max(0.0);
        let max_h = 100_000.0; // exact horizontal clamping happens in the build path (needs column widths)
        let csv = &mut self.tab_mut().csv;
        csv.scroll_top = csv.scroll_top.clamp(0.0, max_v);
        csv.scroll_left = csv.scroll_left.clamp(-max_h, max_h).max(0.0);
    }

    // ---- Column geometry (shared by the component and scrollbar dragging) -----

    /// Column width vector (user overrides first, auto-fit fallback, padded
    /// with blank columns) plus `(data column count, total column count,
    /// total width)`.
    ///
    /// The auto-fit half comes from [`auto_col_widths`] instead of a private
    /// copy: the rendered table and horizontal scrollbar dragging map the thumb
    /// against the same widths, and two implementations of "how wide is this
    /// column" drift apart silently.
    pub fn csv_col_geometry(&mut self, avail_w: f32) -> (Vec<f32>, usize, usize, f32) {
        let (rows_all, _, _, _) = self.csv_rows_ensure();
        let header_on = self.tab().csv.header;
        let over = self.tab().csv.col_widths.clone();
        let adv = self.metrics.adv;

        let (n_cols, auto) = auto_col_widths(&rows_all, header_on, adv);
        let mut widths: Vec<f32> = auto
            .iter()
            .enumerate()
            .map(|(i, w)| over.get(i).copied().flatten().unwrap_or(*w))
            .collect();
        let mut total: f32 = widths.iter().sum();
        let mut n_total = n_cols;
        while total < avail_w && n_total - n_cols < MAX_EMPTY_COLS {
            widths.push(EMPTY_COL_W);
            total += EMPTY_COL_W;
            n_total += 1;
        }
        (widths, n_cols, n_total, total)
    }

    /// Total table content height (px): row count × default row height + row-height override deltas.
    fn csv_total_height(&mut self) -> f32 {
        let lh = crate::ui::state::csv::ROW_H.max(self.metrics.lh.max(1.0));
        let (rows, _, _, _) = self.csv_rows_ensure();
        let n = rows.len().saturating_sub(self.tab().csv.header as usize);
        let extra: f32 = self
            .tab()
            .csv
            .row_heights
            .range(..n)
            .map(|(_, h)| h - lh)
            .sum();
        n as f32 * lh + extra
    }

    // ---- Pointer gestures (scrollbars / row & column resizing) ----------------

    /// Row-number slot click: select that row (whole-row semantics, see [`CellSel::row`]).
    pub fn csv_pick_cell_row(&mut self, row: usize) {
        self.csv_pick_row(row);
    }

    /// Thumb press: record the grab offset and start dragging. The content's
    /// total size is measured **once, at this moment** and cached in the
    /// gesture (recomputing it every frame while dragging is what makes the
    /// scrollbar undraggable; see [`CsvDrag`](crate::ui::state::csv::CsvDrag)).
    pub fn csv_sb_drag(&mut self, vertical: bool, pos: f32) {
        let grab = self.csv_thumb_grab(vertical, pos);
        let total = if vertical {
            self.csv_total_height()
        } else {
            let (_, _, _, total) = self.csv_col_geometry(self.csv_sb_viewport(false));
            total
        };
        self.tab_mut().csv.drag = Some(if vertical {
            crate::ui::state::csv::CsvDrag::SbV { grab, total }
        } else {
            crate::ui::state::csv::CsvDrag::SbH { grab, total }
        });
    }

    /// Track click: jump the thumb center to the click point and enter dragging.
    pub fn csv_sb_track(&mut self, vertical: bool, pos: f32) {
        self.csv_scroll_thumb_center(vertical, pos);
        self.csv_sb_drag(vertical, pos);
    }

    fn csv_sb_viewport(&self, vertical: bool) -> f32 {
        if vertical {
            (self.view.h - crate::ui::state::csv::SB_W).max(1.0)
        } else {
            (self.view.w - self.gutter_w() - crate::ui::state::csv::SB_W).max(1.0)
        }
    }

    fn csv_thumb_grab(&mut self, vertical: bool, pos: f32) -> f32 {
        let track = self.csv_sb_viewport(vertical).max(20.0);
        let total = if vertical {
            self.csv_total_height()
        } else {
            let (_, _, _, total) = self.csv_col_geometry(self.csv_sb_viewport(false));
            total
        };
        let (_, len) = thumb_geom(
            if vertical {
                self.tab().csv.scroll_top as f32
            } else {
                self.tab().csv.scroll_left
            },
            total,
            track,
        );
        // pos is relative to the track start (the component subtracts vtop / measures from the x origin)
        (pos - len / 2.0).clamp(0.0, len)
    }

    fn csv_scroll_thumb_center(&mut self, vertical: bool, pos: f32) {
        let track = self.csv_sb_viewport(vertical).max(20.0);
        let total = if vertical {
            self.csv_total_height()
        } else {
            let (_, _, _, total) = self.csv_col_geometry(self.csv_sb_viewport(false));
            total
        };
        let viewport = self.csv_sb_viewport(vertical);
        let (_, len) = thumb_geom(
            if vertical {
                self.tab().csv.scroll_top as f32
            } else {
                self.tab().csv.scroll_left
            },
            total,
            track,
        );
        // Aim the thumb center at the click position
        let ratio = ((pos - len / 2.0) / (track - len).max(1.0)).clamp(0.0, 1.0);
        let scroll = ratio * (total - viewport).max(0.0);
        if vertical {
            self.tab_mut().csv.scroll_top = scroll as f64;
        } else {
            self.tab_mut().csv.scroll_left = scroll;
        }
    }

    /// Drag move (the table root element's pointermove routes here).
    ///
    /// Scrollbar drags only use the `total` cached at press time (see
    /// [`CsvDrag`](crate::ui::state::csv::CsvDrag)): this function runs every
    /// frame with the pointer; any column-geometry/total-height recompute
    /// would turn the drag into a slideshow.
    pub fn csv_drag_move(&mut self, x: f32, y: f32) {
        use crate::ui::state::csv::CsvDrag;
        let Some(drag) = self.tab().csv.drag else {
            return;
        };
        match drag {
            CsvDrag::SbV { grab, total } => {
                let vtop = self.view.y;
                let track = self.csv_sb_viewport(true).max(20.0);
                let viewport = self.csv_sb_viewport(true);
                let scroll =
                    ((y - vtop - grab) / track * total).clamp(0.0, (total - viewport).max(0.0));
                self.tab_mut().csv.scroll_top = scroll as f64;
            }
            CsvDrag::SbH { grab, total } => {
                let track = self.csv_sb_viewport(false).max(20.0);
                let viewport = self.csv_sb_viewport(false);
                let scroll = ((x - grab) / track * total).clamp(0.0, (total - viewport).max(0.0));
                self.tab_mut().csv.scroll_left = scroll;
            }
            CsvDrag::ColResize {
                col,
                start_x,
                start_w,
            } => {
                use crate::ui::state::csv::{COL_MAX, COL_MIN};
                let w = (start_w + (x - start_x)).clamp(COL_MIN, COL_MAX);
                let csv = &mut self.tab_mut().csv;
                if csv.col_widths.len() <= col {
                    csv.col_widths.resize(col + 1, None);
                }
                csv.col_widths[col] = Some(w);
            }
            CsvDrag::RowResize {
                row,
                start_y,
                start_h,
            } => {
                let h = (start_h + (y - start_y)).clamp(16.0, 120.0);
                self.tab_mut().csv.row_heights.insert(row, h);
            }
        }
    }

    /// Gesture end (pointerup).
    pub fn csv_end_drag(&mut self) {
        self.tab_mut().csv.drag = None;
    }

    /// Header right edge press: enter column resize (records the start point and initial width).
    pub fn csv_begin_col_resize(&mut self, col: usize, x: f32, cur_w: f32) {
        self.tab_mut().csv.drag = Some(crate::ui::state::csv::CsvDrag::ColResize {
            col,
            start_x: x,
            start_w: cur_w,
        });
    }

    /// Row-number slot bottom edge press: enter row resize.
    pub fn csv_begin_row_resize(&mut self, row: usize, y: f32) {
        let lh = crate::ui::state::csv::ROW_H.max(self.metrics.lh.max(1.0));
        let cur_h = self.tab().csv.row_heights.get(&row).copied().unwrap_or(lh);
        self.tab_mut().csv.drag = Some(crate::ui::state::csv::CsvDrag::RowResize {
            row,
            start_y: y,
            start_h: cur_h,
        });
    }

    // ---- In-place cell editing ------------------------------------------------

    /// Cell double-click: enter in-place editing (Excel semantics). Rows
    /// containing multi-line quoted fields cannot be written back per row;
    /// refuse with a notice.
    pub fn csv_begin_edit(&mut self, row: usize, col: usize) {
        if self.read_only {
            self.readonly_notice();
            return;
        }
        let (rows, _, _, truncated) = self.csv_rows_ensure();
        if truncated {
            self.status = Some(Msg::StatusCsvEditTruncated.into());
            return;
        }
        // The `row` from the component is a **data row** index (header excluded); the parse cache includes the header row
        let ridx = row + self.tab().csv.header as usize;
        let Some(r) = rows.get(ridx) else {
            return;
        };
        let value = r.get(col).cloned().unwrap_or_default();
        self.tab_mut().csv.sel = Some(CellSel::cell(row, col));
        let start_serial = self.tab().doc.edit_serial();
        self.tab_mut().csv.editing = Some(crate::ui::state::csv::CellEdit {
            row,
            col,
            orig: value.clone(),
            value,
            preedit: String::new(),
            start_serial,
        });
    }

    /// Write the edit buffer's current value back to the document line holding
    /// the cell (shared by live editing and commit). Guards match commit:
    /// truncation / contiguous row mapping / per-record quote balance /
    /// whole-record re-parse cross-validation. No change (same value or same
    /// serialization) is not a failure.
    /// Returns whether it was safe to commit (or there was no change at all).
    fn csv_write_field(&mut self, ed: &crate::ui::state::csv::CellEdit) -> bool {
        let (rows, line_of_row, end_line_of_row, truncated) = self.csv_rows_ensure();
        if truncated {
            return false;
        }
        // The component's `ed.row` is a data row index; the parse cache (rows / line_of_row) includes the header
        let hdr = self.tab().csv.header as usize;
        let ridx = ed.row + hdr;
        let (Some(row), Some(&start_line), Some(&end_line)) = (
            rows.get(ridx).cloned(),
            line_of_row.get(ridx),
            end_line_of_row.get(ridx),
        ) else {
            return false;
        };
        // The record may span lines (newline inside a quoted field): rebuild
        // the whole record from the parsed fields and write it back over
        // `[start_line, end_line)` as one piece. Other fields' quoting style
        // gets normalized by the minimal-quote serialization, matching the
        // old behavior.
        let mut new_row = row;
        while new_row.len() <= ed.col {
            new_row.push(String::new());
        }
        if new_row[ed.col] == ed.value {
            return true; // no change
        }
        new_row[ed.col] = ed.value.clone();
        let out = crate::format::serialize_csv_row(&new_row, self.tab().csv.delim);
        let old = self
            .active_doc()
            .text_range(self.row_byte_span(start_line, end_line));
        let term = self.active_doc().newline().terminator();
        let mut expected = out.clone();
        if old.ends_with(term) {
            expected.push_str(term);
        }
        if expected == old {
            return true;
        }
        // Live editing replaces the whole record once per keystroke: edits in
        // the same session (same record start line) coalesce into one undo
        // step, so Ctrl+Z restores the whole cell at once (like Excel).
        let key = crate::buffer::CoalesceKey::CellEdit { line: start_line };
        self.write_lines_coalesced(start_line, end_line - 1, &[out], Some(key));
        true
    }

    /// Typing while editing (character / IME commit). Returns false when no
    /// cell is being edited.
    /// **Live write-back**: the value is committed as typed, no Enter needed
    /// (Enter/Tab only move).
    pub fn csv_edit_insert(&mut self, s: &str) -> bool {
        let Some(mut ed) = self.tab().csv.editing.clone() else {
            return false;
        };
        // Cells do not accept newlines: multi-line fields invalidate the row mapping (same restriction as sorting)
        ed.value.push_str(&s.replace(['\r', '\n'], " "));
        // The IME commit is already on screen; the composing preedit string goes away with it
        ed.preedit.clear();
        self.csv_write_field(&ed);
        self.tab_mut().csv.editing = Some(ed);
        true
    }

    /// Receive an IME composing string (pinyin preedit) while editing. Returns
    /// false when no cell is being edited (callers should ignore the event).
    /// The composing string is not yet on screen, so no write-back fires.
    pub fn csv_edit_preedit(&mut self, text: &str) -> bool {
        match self.tab_mut().csv.editing.as_mut() {
            Some(e) => {
                e.preedit.clear();
                e.preedit.push_str(text);
                true
            }
            None => false,
        }
    }

    /// Backspace while editing. Returns false when no cell is being edited. Live write-back.
    pub fn csv_edit_backspace(&mut self) -> bool {
        let Some(mut ed) = self.tab().csv.editing.clone() else {
            return false;
        };
        ed.value.pop();
        self.csv_write_field(&ed);
        self.tab_mut().csv.editing = Some(ed);
        true
    }

    /// Cancel editing (Esc): restore the original value captured at edit
    /// start (live editing already committed intermediate values; merely
    /// dropping the buffer would leave half-typed input). No write when the
    /// original value already matches.
    pub fn csv_cancel_edit(&mut self) {
        let Some(ed) = self.tab_mut().csv.editing.take() else {
            return;
        };
        // Roll back every op applied by this session (live write-back is once
        // per keystroke).
        //
        // The old implementation "rewrote the original value" had two
        // problems: ① text briefly flashed back through intermediate values,
        // and ② the edit serial kept advancing — content was restored but the
        // title still showed "modified", and closing the tab still prompted
        // to save. Undo is the only symmetric operation: the serial
        // decrements back to the start and the dirty flag settles (see
        // `TextDocument::edit_serial`).
        let mut guard = 0u32;
        while self.tab().doc.edit_serial() > ed.start_serial && self.tab().doc.can_undo() {
            if self.tab_mut().doc.undo().is_none() {
                break;
            }
            guard += 1;
            if guard > 100_000 {
                break; // defensive: an anomalous state must not let one keystroke hang the UI
            }
        }
        // Fallback check: undo-stack clamping/coalescing may prevent returning
        // to the start; rewrite from the original value in that case.
        let (rows, _, _, _) = self.csv_rows_ensure();
        let hdr = self.tab().csv.header as usize;
        let cur = rows
            .get(ed.row + hdr)
            .and_then(|r| r.get(ed.col))
            .cloned()
            .unwrap_or_default();
        if cur != ed.orig {
            let restore = crate::ui::state::csv::CellEdit {
                row: ed.row,
                col: ed.col,
                value: ed.orig.clone(),
                orig: ed.orig,
                preedit: String::new(),
                start_serial: ed.start_serial,
            };
            self.csv_write_field(&restore);
        }
    }

    /// Commit editing: with live editing the value was already committed per
    /// keystroke; this only wraps up (rewrites any lagging buffer,
    /// idempotent) and moves per Excel semantics. `move_dir`: 0 = stay,
    /// 1 = down (Enter), 2 = right (Tab).
    pub fn csv_commit_edit(&mut self, move_dir: u8) {
        let Some(ed) = self.tab().csv.editing.clone() else {
            return;
        };
        let ok = self.csv_write_field(&ed);
        // The live edit froze the view order (see [`Self::csv_rows_ensure`]):
        // now that the edit is over, the edited record settles into its sorted
        // place and the selection follows the record, not the frozen position.
        let doc_row = self.csv_doc_row(ed.row + self.tab().csv.header as usize);
        self.tab_mut().csv.editing = None;
        self.csv_apply_lens();
        if !ok {
            self.status = Some(Msg::StatusCsvWriteUnsafe.into());
            return;
        }
        // Enter moves down / Tab moves right (Excel semantics); out of bounds stays put
        let hdr = self.tab().csv.header as usize;
        let (rows, _, _, _) = self.csv_rows_ensure();
        let n_rows = rows.len().saturating_sub(hdr);
        let n_cols = rows.first().map(Vec::len).unwrap_or(0);
        let base = doc_row
            .and_then(|d| self.csv_view_row_of(d))
            .map(|pos| pos.saturating_sub(hdr))
            .unwrap_or(ed.row);
        let (r2, c2) = match move_dir {
            1 => ((base + 1).min(n_rows.saturating_sub(1)), ed.col),
            2 => (base, (ed.col + 1).min(n_cols.saturating_sub(1))),
            _ => (base, ed.col),
        };
        self.tab_mut().csv.sel = Some(CellSel::cell(r2, c2));
        if move_dir == 1 {
            // The document may have gained/lost a line after commit; scrolling
            // is clamped by the next frame; here we only bring the newly
            // selected cell into view
            self.scroll_caret_into_view();
        }
        self.status = Some(
            Fmt::StatusCsvCellUpdated {
                row: base + 1,
                col: ed.col + 1,
            }
            .into(),
        );
    }

    // ---- Cell copy / paste ---------------------------------------------------

    /// Ctrl+C (not in edit mode): copy selected text to clipboard. Cell = field text;
    /// whole row = fields joined by tabs; whole column = fields joined by newlines.
    /// Returns whether anything was copied.
    pub fn csv_copy_sel(&mut self) -> bool {
        let Some(text) = self.csv_sel_text() else {
            return false;
        };
        let _clip = platform::clipboard::lock();
        if let Ok(mut cb) = arboard::Clipboard::new() {
            let _ = cb.set_text(text.as_str());
        }
        self.clip = text;
        true
    }

    /// Selected text (pure computation part of `Self::csv_copy_sel`, can headless test).
    pub fn csv_sel_text(&self) -> Option<String> {
        let sel = self.tab().csv.sel?;
        let (rows, _, _, _) = self.csv_rows_cached()?;
        let hdr = self.tab().csv.header as usize;
        match sel.kind {
            SelKind::Cell => rows
                .get(sel.row + hdr)
                .and_then(|r| r.get(sel.col))
                .cloned(),
            SelKind::Row => rows.get(sel.row + hdr).map(|r| r.join("\t")),
            SelKind::Col => {
                let v: Vec<&str> = rows
                    .iter()
                    .skip(hdr)
                    .filter_map(|r| r.get(sel.col).map(String::as_str))
                    .collect();
                (!v.is_empty()).then(|| v.join("\n"))
            }
        }
    }

    /// The pure commit part of paste (testable core of [`Self::csv_paste_route`]).
    pub fn csv_paste_text_into_sel(&mut self, text: &str) -> bool {
        let Some(sel) = self.tab().csv.sel else {
            return false;
        };
        if sel.kind != SelKind::Cell {
            return false;
        }
        self.csv_begin_edit(sel.row, sel.col);
        if self.tab().csv.editing.is_none() {
            return false;
        }
        let mut ed = self.tab().csv.editing.clone().unwrap();
        ed.value = text.replace(['\r', '\n'], " ");
        ed.preedit.clear();
        self.csv_write_field(&ed);
        self.tab_mut().csv.editing = Some(ed);
        true
    }

    // ---- CSV dispatch for Cmd::Copy / Cmd::Paste ------------------------------
    //
    // The global shortcut table (shortcuts.rs) sends Ctrl+C/V straight to
    // [`EditorState::copy`] / [`EditorState::paste`] — the composer key
    // layer's Ctrl branch never receives events (dead code); routing must
    // live at the command dispatch layer. When the table view is active,
    // these two entry points go here first.

    /// CSV dispatch for [`EditorState::copy`]. Returns true = consumed.
    pub fn csv_copy_route(&mut self) -> bool {
        // Editing: copy the edit buffer (not the cell's old value)
        if let Some(ed) = self.tab().csv.editing.clone() {
            if !ed.value.is_empty() {
                let _clip = platform::clipboard::lock();
                if let Ok(mut cb) = arboard::Clipboard::new() {
                    let _ = cb.set_text(ed.value.as_str());
                }
                self.clip = ed.value;
            }
            return true;
        }
        if self.tab().csv.sel.is_some() {
            return self.csv_copy_sel();
        }
        // No selection: defer to document semantics (copy is a no-op without a selection anyway)
        false
    }

    /// CSV dispatch for [`EditorState::paste`]. Returns true = consumed.
    pub fn csv_paste_route(&mut self) -> bool {
        let mut text = String::new();
        let _clip = platform::clipboard::lock();
        if let Ok(mut cb) = arboard::Clipboard::new() {
            if let Ok(t) = cb.get_text() {
                text = t;
            }
        }
        if text.is_empty() {
            text = self.clip.clone();
        }
        // Editing: paste into the cell buffer
        if self.tab().csv.editing.is_some() {
            if !text.is_empty() {
                self.csv_edit_insert(&text);
            }
            return true;
        }
        if self.tab().csv.sel.is_some() {
            if text.is_empty() {
                return true;
            }
            // `false` = "not a single-cell target" (row/column selection,
            // read-only, truncated file, unsafe row). Returning it made
            // `paste()` fall through to the *document* paste, which inserted
            // the clipboard at the table view's invisible caret — the CSV
            // source text was silently rewritten behind the user's back.
            if self.csv_paste_text_into_sel(&text) {
                return true;
            }
            self.status = Some(crate::ui::Msg::StatusCsvWriteUnsafe.into());
            return true;
        }
        // No selection: swallow it — table mode has no visible document caret
        true
    }

    // ---- Row/column structural editing (Excel semantics: select a row/column, then insert/delete) ----

    /// Shared guard for structural row/column operations: writable and not
    /// truncated. Multi-line quoted fields are no longer a rejection reason —
    /// callers rewrite by each record's `[start_line, end_line)` range.
    fn csv_struct_guard(&mut self) -> Option<CsvRows2> {
        if self.read_only {
            self.readonly_notice();
            return None;
        }
        let (rows, line_of_row, end_line_of_row, truncated) = self.csv_rows_ensure();
        if rows.is_empty() {
            self.status = Some(Msg::StatusCsvNoData.into());
            return None;
        }
        if truncated {
            self.status = Some(Msg::StatusCsvEditTruncated.into());
            return None;
        }
        Some((rows, line_of_row, end_line_of_row))
    }

    /// Split a record's byte range into `(body, terminator)`: the terminator
    /// is preserved verbatim so a quoted field's embedded CRLF or a file with
    /// mixed newlines is never rewritten by a structural edit.
    fn csv_record_body(&self, span: std::ops::Range<u64>) -> (String, String) {
        let old = self.active_doc().text_range(span);
        let n = if old.ends_with("\r\n") {
            2
        } else if old.ends_with('\n') || old.ends_with('\r') {
            1
        } else {
            0
        };
        let split = old.len() - n;
        (old[..split].to_string(), old[split..].to_string())
    }

    /// Feeds **every row of the whole table** to `f` for rewriting (column
    /// structural ops: insert/delete/clear column) and writes back with a
    /// single-step undo. Each record is rewritten by its own
    /// `[start_line, end_line)` range, so multi-line quoted fields are kept
    /// as a whole; blank document lines are outside record ranges and stay
    /// as-is. Unclosed quotes are still bad data: rejected wholesale
    /// (rewriting would "fix" the quotes and swallow subsequent lines).
    fn csv_map_cols(&mut self, quote_msg: i18n::Message, f: impl Fn(&mut Vec<String>)) -> bool {
        // Column structure changed, edit state invalidated
        self.tab_mut().csv.editing = None;
        let Some((rows, line_of_row, end_line_of_row)) = self.csv_struct_guard() else {
            return false;
        };
        let delim = self.tab().csv.delim;
        let mut ops = Vec::with_capacity(rows.len());
        for i in 0..rows.len() {
            let span = self.row_byte_span(line_of_row[i], end_line_of_row[i]);
            let (body, term) = self.csv_record_body(span.clone());
            if !crate::format::csv_quotes_balanced(&body, delim) {
                self.status = Some(quote_msg);
                return false;
            }
            let mut fields = rows[i].clone();
            f(&mut fields);
            let mut out = crate::format::serialize_csv_row(&fields, delim);
            out.push_str(&term);
            if out != self.active_doc().text_range(span.clone()) {
                ops.push(crate::buffer::doc::EditOp {
                    pos: span.start,
                    deleted: self.active_doc().text_range(span),
                    inserted: out,
                });
            }
        }
        // Descending positions keep every op in original coordinates, and let
        // the piece table take its single-pass bulk path. The rows may be in
        // **view order** (sorted table), so their spans are not collected in
        // document order — sort explicitly instead of trusting (and reversing)
        // the collection order.
        ops.sort_by_key(|op| std::cmp::Reverse(op.pos));
        if !ops.is_empty() {
            self.tab_mut().doc.apply_transaction(ops);
        }
        true
    }

    /// Insert an empty row above/below the selected row (no selection: above
    /// = before the first row after the header, below = append at document
    /// end). The new row's field count matches the current widest row.
    /// Insertion points are record boundaries: multi-line quoted fields are
    /// not split and blank lines between records are not swallowed.
    ///
    /// A sorted table drops its sort lens first: "above the first record" and
    /// "append at the end" are statements about the file, not about the view.
    pub fn csv_insert_row(&mut self, above: bool) {
        // Guard first so a refused insert does not drop the user's sort
        if self.csv_struct_guard().is_none() {
            return;
        }
        let sel = self.csv_file_order_selection();
        let Some(cache) = self.tab().csv.cache.clone() else {
            return;
        };
        let (rows, line_of_row, end_line_of_row) =
            (cache.rows, cache.line_of_row, cache.end_line_of_row);
        // Row structure changed, edit state invalidated
        self.tab_mut().csv.editing = None;
        let hdr = self.tab().csv.header as usize;
        let n_data = rows.len().saturating_sub(hdr);
        let n_cols = rows.iter().map(Vec::len).max().unwrap_or(1).max(1);
        let delim = self.tab().csv.delim;
        let empty = crate::format::serialize_csv_row(&vec![String::new(); n_cols], delim);
        let nl = self.active_doc().newline().terminator().to_string();
        let append_after = |st: &Self, ridx: usize| {
            let span = st.row_byte_span(line_of_row[ridx], end_line_of_row[ridx]);
            let old = st.active_doc().text_range(span.clone());
            let text = if old.ends_with('\n') || old.ends_with('\r') {
                format!("{empty}{nl}")
            } else {
                format!("{nl}{empty}")
            };
            (span.end, text)
        };
        let (pos, text) = match sel {
            // Above: insert at the record start (blank lines above stay above).
            Some(s) if above => {
                let ridx = (s.row + hdr).min(rows.len() - 1);
                let span = self.row_byte_span(line_of_row[ridx], end_line_of_row[ridx]);
                (span.start, format!("{empty}{nl}"))
            }
            // Below: insert immediately after the record's terminator, before
            // any blank line that separates it from the next record.
            Some(s) => append_after(self, (s.row + hdr).min(rows.len() - 1)),
            // No selection: above = before the first data record, below = append.
            None if above && hdr < rows.len() => {
                let span = self.row_byte_span(line_of_row[hdr], end_line_of_row[hdr]);
                (span.start, format!("{empty}{nl}"))
            }
            None => append_after(self, rows.len() - 1),
        };
        let deleted = self.active_doc().text_range(pos..pos);
        self.tab_mut()
            .doc
            .apply_transaction(vec![crate::buffer::doc::EditOp {
                pos,
                deleted,
                inserted: text,
            }]);
        let new_data_row = match sel {
            Some(s) if above => s.row,
            Some(s) => s.row + 1,
            None if above => 0,
            None => n_data,
        }
        .min(n_data);
        self.tab_mut().csv.sel = Some(CellSel::row(new_data_row));
        self.tab_mut().csv.row_heights.clear();
        self.status = Some(
            if above {
                Fmt::StatusCsvInsRowAbove {
                    row: new_data_row + 1,
                }
            } else if sel.is_some() {
                Fmt::StatusCsvInsRowBelow {
                    row: new_data_row + 1,
                }
            } else {
                Fmt::StatusCsvInsRowEnd {
                    row: new_data_row + 1,
                }
            }
            .into(),
        );
    }

    /// Delete the selected row (whole-row selection or a cell on it both
    /// work). Deletes the whole record (including its internal multi-line
    /// quoted fields), not the text line.
    ///
    /// A sorted table drops its sort lens first (file-order selection), so the
    /// record under the selection is the one removed.
    pub fn csv_delete_row(&mut self) {
        if self.tab().csv.sel.is_none() {
            self.status = Some(Msg::StatusCsvDelRowNoSel.into());
            return;
        }
        if self.csv_struct_guard().is_none() {
            return;
        }
        let Some(sel) = self.csv_file_order_selection() else {
            return;
        };
        let Some(cache) = self.tab().csv.cache.clone() else {
            return;
        };
        let (rows, line_of_row, end_line_of_row) =
            (cache.rows, cache.line_of_row, cache.end_line_of_row);
        // Row structure changed, edit state invalidated
        self.tab_mut().csv.editing = None;
        let hdr = self.tab().csv.header as usize;
        let ridx = sel.row + hdr;
        if ridx >= rows.len() {
            self.status = Some(Msg::StatusCsvDelRowEmpty.into());
            return;
        }
        let span = self.row_byte_span(line_of_row[ridx], end_line_of_row[ridx]);
        let mut start = span.start;
        let end = span.end;
        let old = self.active_doc().text_range(start..end);
        if end == self.active_doc().total_bytes() && !(old.ends_with('\n') || old.ends_with('\r')) {
            // Last record without a trailing newline: also drop the terminator
            // that separated it from the previous record, otherwise deleting
            // it would leave a spurious empty final line.
            let nl = self.active_doc().newline().terminator().to_string();
            if start >= nl.len() as u64
                && self.active_doc().text_range(start - nl.len() as u64..start) == nl
            {
                start -= nl.len() as u64;
            }
        }
        let deleted = self.active_doc().text_range(start..end);
        self.tab_mut()
            .doc
            .apply_transaction(vec![crate::buffer::doc::EditOp {
                pos: start,
                deleted,
                inserted: String::new(),
            }]);
        self.tab_mut().csv.row_heights.clear();
        self.tab_mut().csv.sel = None;
        self.status = Some(Fmt::StatusCsvRowDeleted { row: sel.row + 1 }.into());
    }

    /// Insert an empty column left/right of the selected column.
    pub fn csv_insert_col(&mut self, left: bool) {
        let Some(sel) = self.tab().csv.sel else {
            self.status = Some(Msg::StatusCsvColNoSel.into());
            return;
        };
        let at = if left { sel.col } else { sel.col + 1 };
        let quote_msg = if left {
            Msg::StatusCsvInsColLeftQuote
        } else {
            Msg::StatusCsvInsColRightQuote
        }
        .into();
        let ok = self.csv_map_cols(quote_msg, move |fields| {
            let at = at.min(fields.len());
            fields.insert(at, String::new());
        });
        if ok {
            self.tab_mut().csv.sel = Some(CellSel::col(at));
            let cw = &mut self.tab_mut().csv.col_widths;
            let at_w = at.min(cw.len());
            cw.insert(at_w, None);
            self.status = Some(
                if left {
                    Fmt::StatusCsvInsColLeft { col: at + 1 }
                } else {
                    Fmt::StatusCsvInsColRight { col: at + 1 }
                }
                .into(),
            );
        }
    }

    /// Delete the selected column. Refused when the data has only one column
    /// (deleting the last column makes the structure meaningless).
    pub fn csv_delete_col(&mut self) {
        let Some(sel) = self.tab().csv.sel else {
            self.status = Some(Msg::StatusCsvDelColNoSel.into());
            return;
        };
        let (rows, _, _, _) = self.csv_rows_ensure();
        let n_cols = rows.iter().map(Vec::len).max().unwrap_or(0);
        if n_cols <= 1 {
            self.status = Some(Msg::StatusCsvDelColLast.into());
            return;
        }
        let col = sel.col.min(n_cols - 1);
        let ok = self.csv_map_cols(Msg::StatusCsvDelColQuote.into(), move |fields| {
            if col < fields.len() {
                fields.remove(col);
            }
        });
        if ok {
            self.tab_mut().csv.sel = None;
            // The deleted column may be the lens column; the document changed
            // anyway, so the next parse starts from file order
            self.tab_mut().csv.sort_col = None;
            self.tab_mut().csv.sort_desc = false;
            self.status = Some(Fmt::StatusCsvColDeleted { col: col + 1 }.into());
        }
    }

    /// Clear the selected cell's content (Delete key, Excel semantics: clear
    /// content, keep the cell).
    pub fn csv_clear_cell(&mut self) {
        let Some(sel) = self.tab().csv.sel else {
            return;
        };
        if sel.kind != SelKind::Cell {
            return;
        }
        if self.read_only {
            self.readonly_notice();
            return;
        }
        let (rows, line_of_row, end_line_of_row, truncated) = self.csv_rows_ensure();
        if truncated {
            self.status = Some(Msg::StatusCsvEditTruncated.into());
            return;
        }
        let hdr = self.tab().csv.header as usize;
        let ridx = sel.row + hdr;
        let (Some(fields), Some(&doc_line), Some(&end_line)) = (
            rows.get(ridx).cloned(),
            line_of_row.get(ridx),
            end_line_of_row.get(ridx),
        ) else {
            return;
        };
        if fields.len() <= sel.col || fields[sel.col].is_empty() {
            return; // the row never had this column, or it was already empty: no document change
        }
        let mut fields = fields;
        fields[sel.col] = String::new();
        let out = crate::format::serialize_csv_row(&fields, self.tab().csv.delim);
        self.write_lines(doc_line, end_line - 1, &[out]);
        self.status = Some(
            Fmt::StatusCsvCellCleared {
                row: sel.row + 1,
                col: sel.col + 1,
            }
            .into(),
        );
    }

    /// Move the selection with arrow keys (table mode, Excel semantics):
    /// `dr`/`dc` are row/column deltas. Returns whether it moved (the
    /// component layer swallows the key accordingly).
    pub fn csv_move_sel(&mut self, dr: i64, dc: i64) -> bool {
        let (rows, _, _, _) = self.csv_rows_ensure();
        let hdr = self.tab().csv.header as usize;
        let n_data = rows.len().saturating_sub(hdr);
        let n_cols = rows.iter().map(Vec::len).max().unwrap_or(0);
        if n_data == 0 || n_cols == 0 {
            return false;
        }
        let sel = self.tab().csv.sel.unwrap_or_else(|| CellSel::cell(0, 0));
        let row = (sel.row as i64 + dr).clamp(0, n_data as i64 - 1) as usize;
        let col = (sel.col as i64 + dc).clamp(0, n_cols as i64 - 1) as usize;
        if row == sel.row && col == sel.col && self.tab().csv.sel.is_some() {
            return false;
        }
        self.tab_mut().csv.sel = Some(CellSel::cell(row, col));
        true
    }
}

#[cfg(test)]
mod tests {
    use super::super::testutil::st_of;
    use crate::ui::state::csv::SelKind;
    use crate::ui::EditorState;

    /// Column values in **view order** — what the table shows.
    fn view_col(st: &mut EditorState, col: usize) -> Vec<String> {
        let (rows, _, _, _) = st.csv_rows_ensure();
        rows.iter()
            .map(|r| r.get(col).cloned().unwrap_or_default())
            .collect()
    }

    fn text_of(st: &EditorState) -> String {
        st.active_doc().text_range(0..st.active_doc().total_bytes())
    }

    /// Header-click three-state cycle (unsorted → ascending → descending →
    /// cleared) drives the **view**; the document is the same bytes before and
    /// after, with no undo step and no dirty flag.
    #[test]
    fn csv_header_click_cycles_the_view_only() {
        let src = "n\n10\n2\n33\n";
        let mut st = st_of(src);
        st.tab_mut().csv.enabled = true;
        st.tab_mut().csv.header = true;
        let serial = st.tabs[0].doc.edit_serial();
        // First click: ascending
        st.csv_header_click(0);
        assert_eq!(st.tab().csv.sort_col, Some(0));
        assert!(!st.tab().csv.sort_desc);
        assert_eq!(view_col(&mut st, 0), ["n", "2", "10", "33"]);
        // Second click: descending
        st.csv_header_click(0);
        assert!(st.tab().csv.sort_desc);
        assert_eq!(view_col(&mut st, 0), ["n", "33", "10", "2"]);
        // Third click: lens dropped, table back in file order
        st.csv_header_click(0);
        assert_eq!(st.tab().csv.sort_col, None);
        assert_eq!(view_col(&mut st, 0), ["n", "10", "2", "33"]);
        // Nothing above touched the document
        assert_eq!(text_of(&st), src, "sorting must not rewrite the document");
        assert_eq!(st.tabs[0].doc.edit_serial(), serial);
        assert!(
            !st.tabs[0].doc.can_undo(),
            "sorting must not push an undo step"
        );
    }

    /// The core contract: a sorted table is a **lens**. Text, edit serial,
    /// undo stack and dirty flag are untouched, so saving writes file order.
    #[test]
    fn csv_sort_is_display_only() {
        let src = "n,name\n10,ten\n2,two\n33,thirty\n";
        let mut st = st_of(src);
        st.tab_mut().csv.enabled = true;
        st.tab_mut().csv.header = true;
        let serial = st.tabs[0].doc.edit_serial();
        st.csv_pick_column(0);
        st.csv_sort(false);
        assert_eq!(view_col(&mut st, 0), ["n", "2", "10", "33"]);
        assert_eq!(view_col(&mut st, 1), ["name", "two", "ten", "thirty"]);
        assert_eq!(text_of(&st), src, "the document text must not change");
        assert_eq!(
            st.tabs[0].doc.edit_serial(),
            serial,
            "sorting must not bump the edit serial"
        );
        assert!(!st.tabs[0].doc.can_undo());
        assert!(
            !st.tabs[0].doc.is_dirty(),
            "a sorted view is not a modification"
        );
    }

    /// A refused sort must not leave the ↑/↓ arrow on a column the table is not
    /// ordered by (the header text click records the column before it knows
    /// whether there is anything to sort).
    #[test]
    fn csv_refused_sort_leaves_no_indicator() {
        let mut st = st_of("only\nvalue\n"); // one data row: nothing to order
        st.tab_mut().csv.enabled = true;
        st.tab_mut().csv.header = true;
        st.csv_header_click(2);
        assert_eq!(st.tab().csv.sort_col, None, "no lens was applied");
        assert!(!st.tab().csv.sort_desc);
    }

    /// A view lens is not an edit, so read-only documents sort like a read-only
    /// text can be searched.
    #[test]
    fn csv_sort_works_in_read_only_mode() {
        let mut st = st_of("n\n10\n2\n");
        st.tab_mut().csv.enabled = true;
        st.tab_mut().csv.header = true;
        st.read_only = true;
        st.csv_pick_column(0);
        st.csv_sort(false);
        assert_eq!(view_col(&mut st, 0), ["n", "2", "10"]);
        assert_eq!(text_of(&st), "n\n10\n2\n");
    }

    #[test]
    fn csv_sort_descending_keeps_the_header_pinned() {
        let mut st = st_of("n\n10\n2\n33\n");
        st.tab_mut().csv.enabled = true;
        st.tab_mut().csv.header = true;
        st.csv_pick_column(0);
        st.csv_sort(true);
        assert_eq!(view_col(&mut st, 0), ["n", "33", "10", "2"]);
    }

    /// Toggling the header shifts every data row by one: a cell/row selection is
    /// view-relative and must not survive pointing at another record, while a
    /// column selection keeps its meaning (column indexes do not move).
    #[test]
    fn csv_toggle_header_drops_a_stale_row_selection() {
        let mut st = st_of("a,b\n1,2\n");
        st.tab_mut().csv.enabled = true;
        st.csv_pick_cell(0, 0);
        st.csv_toggle_header();
        assert!(st.tab().csv.sel.is_none(), "the row index shifted by one");

        st.tab_mut().csv.header = false;
        st.csv_pick_col(1);
        st.csv_toggle_header();
        assert_eq!(
            st.tab().csv.sel.map(|s| s.kind),
            Some(SelKind::Col),
            "a column selection is still about the same column"
        );
    }

    /// The row→line map travels with each row, so a sorted view edits the
    /// record it displays — not the file's first record.
    #[test]
    fn csv_sorted_view_edits_the_record_it_shows() {
        let mut st = st_of("id,name\n10,ten\n2,two\n");
        st.tab_mut().csv.enabled = true;
        st.tab_mut().csv.header = true;
        st.csv_pick_column(0);
        st.csv_sort(false); // 2 (line 2), 10 (line 1)
        st.csv_begin_edit(0, 1);
        let mut ed = st.tab().csv.editing.clone().expect("editable cell");
        assert_eq!(ed.value, "two", "display row 0 is the record with key 2");
        ed.value = "TWO".into();
        st.tab_mut().csv.editing = Some(ed);
        st.csv_commit_edit(0);
        assert_eq!(text_of(&st), "id,name\n10,ten\n2,TWO\n");
    }

    /// …and deletes the record it displays (structural edits first resolve the
    /// selection into file order, then drop the lens).
    #[test]
    fn csv_sorted_view_deletes_the_record_it_shows() {
        let mut st = st_of("id,name\n10,ten\n2,two\n33,tt\n");
        st.tab_mut().csv.enabled = true;
        st.tab_mut().csv.header = true;
        st.csv_pick_column(0);
        st.csv_sort(false);
        assert_eq!(view_col(&mut st, 0), ["id", "2", "10", "33"]);
        st.csv_pick_row(0); // the "2" record
        st.csv_delete_row();
        assert_eq!(text_of(&st), "id,name\n10,ten\n33,tt\n");
        assert_eq!(
            st.tab().csv.sort_col,
            None,
            "structural edit drops the lens"
        );
    }

    /// Re-applying the lens on every rebuild keeps a sorted table sorted after
    /// an unrelated edit — and the edit still lands on the real record.
    #[test]
    fn csv_sort_survives_a_document_change() {
        let mut st = st_of("n,x\n10,a\n2,b\n");
        st.tab_mut().csv.enabled = true;
        st.tab_mut().csv.header = true;
        st.csv_pick_column(0);
        st.csv_sort(false);
        assert_eq!(view_col(&mut st, 0), ["n", "2", "10"]);
        // Edit the second column of display row 1 (the "10" record)
        st.csv_begin_edit(1, 1);
        let mut ed = st.tab().csv.editing.clone().expect("editable cell");
        ed.value = "A".into();
        st.tab_mut().csv.editing = Some(ed);
        st.csv_commit_edit(0);
        assert_eq!(st.tab().csv.sort_col, Some(0));
        assert_eq!(view_col(&mut st, 0), ["n", "2", "10"], "still sorted");
        assert_eq!(text_of(&st), "n,x\n10,A\n2,b\n", "written to the real row");
    }

    /// A re-order moves the highlight with the **record**, not with the
    /// position: `sel.row` is positional, so it has to be remapped by hand.
    #[test]
    fn csv_sort_keeps_the_selection_on_its_record() {
        let mut st = st_of("id,name\n10,ten\n2,two\n33,tt\n");
        st.tab_mut().csv.enabled = true;
        st.tab_mut().csv.header = true;
        st.csv_pick_cell(1, 1); // display row 1 is the "2" record
        assert_eq!(st.tab().csv.sel.expect("selected").row, 1);
        st.csv_pick_column(0);
        st.csv_sort(false); // 2, 10, 33 → that record is now row 0
        let sel = st.tab().csv.sel.expect("selection survives the sort");
        assert_eq!(sel.row, 0);
        let (rows, _, _, _) = st.csv_rows_ensure();
        assert_eq!(
            rows[sel.row + 1][0],
            "2",
            "the highlight still sits on the same record"
        );
    }

    /// Dropping the lens through the structural-edit path must leave the
    /// **stored** selection on the record the user picked.
    ///
    /// `csv_file_order_selection` translates `sel.row` from a view position to a
    /// document row; writing that index back *before* the restore made
    /// `csv_keep_selection_on_record` read a document index as a view position
    /// and park the highlight on whatever record sat at that position in the
    /// still-sorted cache.
    #[test]
    fn csv_file_order_selection_keeps_the_selected_record() {
        let mut st = st_of("id,name\n10,ten\n2,two\n33,tt\n");
        st.tab_mut().csv.enabled = true;
        st.tab_mut().csv.header = true;
        st.csv_pick_column(0);
        st.csv_sort(false); // view: 2 (doc 2), 10 (doc 1), 33 (doc 3)
        st.csv_pick_row(0); // the "2" record
        let sel = st.csv_file_order_selection().expect("selection survives");
        assert_eq!(sel.row, 1, "file-order data row of the '2' record");
        assert_eq!(
            st.tab().csv.sel.map(|s| s.row),
            Some(1),
            "the stored selection is the translated one, not a remap of it"
        );
        let (rows, _, _, _) = st.csv_rows_ensure();
        assert_eq!(rows[1 + 1][0], "2", "still the same record");
        assert_eq!(st.tab().csv.sort_col, None, "the lens is gone");
    }

    /// Editing the key column must not move the row out from under the caret
    /// mid-edit: the order is frozen while `editing` is set and settles on
    /// commit.
    #[test]
    fn csv_editing_the_sort_column_jumps_only_on_commit() {
        let mut st = st_of("n\n2\n10\n");
        st.tab_mut().csv.enabled = true;
        st.tab_mut().csv.header = true;
        st.csv_pick_column(0);
        st.csv_sort(false);
        assert_eq!(view_col(&mut st, 0), ["n", "2", "10"]);
        st.csv_begin_edit(0, 0);
        let mut ed = st.tab().csv.editing.clone().expect("editable cell");
        ed.value = "99".into();
        st.tab_mut().csv.editing = Some(ed);
        assert!(st.csv_edit_insert(""), "live write-back");
        assert_eq!(text_of(&st), "n\n99\n10\n");
        assert_eq!(
            view_col(&mut st, 0),
            ["n", "99", "10"],
            "frozen while the cell is being edited"
        );
        st.csv_commit_edit(0);
        assert_eq!(
            view_col(&mut st, 0),
            ["n", "10", "99"],
            "settles into its sorted place on commit"
        );
        assert_eq!(text_of(&st), "n\n99\n10\n", "the file order is untouched");
    }

    /// Column ops rewrite **every** record; in a sorted view they are visited
    /// out of document order, so the bulk transaction must be ordered by byte
    /// position (regression: `ops.reverse()` relied on collection order).
    #[test]
    fn csv_insert_col_in_a_sorted_view_rewrites_every_record() {
        let mut st = st_of("id,name\n10,ten\n2,two\n33,tt\n");
        st.tab_mut().csv.enabled = true;
        st.tab_mut().csv.header = true;
        st.csv_pick_column(0);
        st.csv_sort(true); // 33, 10, 2 — reverse of file order
        st.csv_pick_col(0);
        st.csv_insert_col(false);
        assert_eq!(text_of(&st), "id,,name\n10,,ten\n2,,two\n33,,tt\n");
    }

    /// In-place cell edit: the change is written back to the original row
    /// (minimal-quote rule); a single undo restores it.
    #[test]
    fn csv_cell_edit_commits_to_document() {
        let mut st = st_of("a,b\n1,2\n3,4\n");
        st.tab_mut().csv.enabled = true;
        st.tab_mut().csv.header = true;
        st.csv_begin_edit(1, 1); // data row 1 (header excluded) = document line 2 = "3,4"
        assert!(st.tab().csv.editing.is_some());
        assert_eq!(st.tab().csv.editing.as_ref().unwrap().value, "4");
        // Simulate keyboard input (value is pre-filled; rewrite it here)
        st.csv_edit_backspace();
        st.csv_edit_insert("9");
        st.csv_commit_edit(1);
        let text = st.active_doc().text_range(0..st.active_doc().total_bytes());
        assert_eq!(text, "a,b\n1,2\n3,9\n");
        assert!(st.tab().csv.editing.is_none());
        // undo restores
        st.undo();
        let text = st.active_doc().text_range(0..st.active_doc().total_bytes());
        assert_eq!(text, "a,b\n1,2\n3,4\n");
    }

    #[test]
    fn csv_state_defaults_off_for_plain_text() {
        let st = st_of("a,b\n1,2\n");
        assert!(!st.tabs[0].csv.enabled);
        assert!(!st.tabs[0].csv.table);
    }

    /// IME composing strings only enter the cell being edited; after commit
    /// (insert) the composing string is cleared.
    #[test]
    fn csv_edit_preedit_lifecycle() {
        let mut st = st_of("a,b\n1,2\n");
        st.tab_mut().csv.enabled = true;
        st.tab_mut().csv.header = true;
        // No cell being edited: preedit events are ignored
        assert!(!st.csv_edit_preedit("ni"));
        st.csv_begin_edit(0, 1);
        assert!(st.tab().csv.editing.is_some());
        // Composing: the pinyin string hangs on the edit state, not affecting the already-committed value
        assert!(st.csv_edit_preedit("nihao"));
        let e = st.tab().csv.editing.as_ref().unwrap();
        assert_eq!(e.preedit, "nihao");
        assert_eq!(e.value, "2");
        // Candidate updates overwrite the old string
        assert!(st.csv_edit_preedit("niha"));
        assert_eq!(st.tab().csv.editing.as_ref().unwrap().preedit, "niha");
        // IME commit: characters go into value, the composing string disappears
        assert!(st.csv_edit_insert("你好"));
        let e = st.tab().csv.editing.as_ref().unwrap();
        assert_eq!(e.value, "2你好");
        assert!(e.preedit.is_empty());
        st.csv_commit_edit(0);
        let text = st.active_doc().text_range(0..st.active_doc().total_bytes());
        assert_eq!(text, "a,b\n1,2你好\n");
    }

    /// Live editing: typed values commit immediately, no Enter needed; the
    /// edit state survives across gens (the parse cache is invalidated and
    /// rebuilt on every write-back; the edit state must not be cleared
    /// mid-typing).
    #[test]
    fn csv_cell_edit_is_live_no_enter_required() {
        let mut st = st_of("a,b\n1,2\n3,4\n");
        st.tab_mut().csv.enabled = true;
        st.tab_mut().csv.header = true;
        st.csv_begin_edit(0, 0); // original value "1"
        st.csv_edit_insert("9"); // append → "19"
                                 // no Enter pressed, the document already has the new value
        assert_eq!(text_of(&st), "a,b\n19,2\n3,4\n");
        assert!(
            st.tab().csv.editing.is_some(),
            "edit state survives across gens"
        );
        st.csv_edit_insert("9");
        assert_eq!(text_of(&st), "a,b\n199,2\n3,4\n");
        st.csv_edit_backspace();
        assert_eq!(text_of(&st), "a,b\n19,2\n3,4\n");
        // Enter only wraps up + moves
        st.csv_commit_edit(1);
        assert!(st.tab().csv.editing.is_none());
        assert_eq!(text_of(&st), "a,b\n19,2\n3,4\n");
        // one undo restores the whole cell (consecutive typing in the same cell coalesces into one undo step)
        st.undo();
        assert_eq!(text_of(&st), "a,b\n1,2\n3,4\n");
    }

    /// Esc cancel = roll back the whole edit session: content restored, edit
    /// serial reverted, dirty flag settled.
    ///
    /// Regression: the old implementation "rewrote the original value" —
    /// content was right but the serial kept advancing: edit one cell in an
    /// empty document, press Esc, and the title still showed "modified" while
    /// closing the tab still prompted to save (a real defect caught by the
    /// on-machine GUI test `test_escape_cancels_edit_without_touching_disk`).
    #[test]
    fn csv_cell_edit_escape_restores_original() {
        let mut st = st_of("a,b\n1,2\n");
        st.tab_mut().csv.enabled = true;
        st.tab_mut().csv.header = true;
        st.active_doc_mut().mark_saved();
        let serial = st.active_doc().edit_serial();
        st.csv_begin_edit(0, 0); // original value "1"
        st.csv_edit_insert("XY"); // append → "1XY"
        assert_eq!(text_of(&st), "a,b\n1XY,2\n");
        assert!(
            st.active_doc().is_dirty(),
            "content being edited should count as modified"
        );
        st.csv_cancel_edit();
        assert_eq!(text_of(&st), "a,b\n1,2\n");
        assert_eq!(
            st.active_doc().edit_serial(),
            serial,
            "cancel must return the edit serial to its starting point"
        );
        assert!(
            !st.active_doc().is_dirty(),
            "after cancel, content is back to the save point and must not show as modified"
        );
        assert!(st.tab().csv.editing.is_none());
        // Esc without changes: no write
        st.csv_begin_edit(0, 0);
        st.csv_cancel_edit();
        assert_eq!(text_of(&st), "a,b\n1,2\n");
    }

    /// Clicking another cell implicitly ends editing (the value was already
    /// committed live); paste replaces the whole cell content.
    #[test]
    fn csv_pick_elsewhere_ends_edit_and_paste_replaces() {
        let mut st = st_of("a,b\n1,2\n3,4\n");
        st.tab_mut().csv.enabled = true;
        st.tab_mut().csv.header = true;
        st.csv_begin_edit(0, 0); // original value "1"
        st.csv_edit_insert("Z"); // append → "1Z"
                                 // click another cell: editing ends, the document keeps the committed value
        st.csv_pick_cell(1, 1);
        assert!(st.tab().csv.editing.is_none());
        assert_eq!(text_of(&st), "a,b\n1Z,2\n3,4\n");
        // paste: replaces the whole cell (not append). Selection stays at (1,1) ("4")
        st.csv_paste_text_into_sel("PQ");
        assert_eq!(text_of(&st), "a,b\n1Z,2\n3,PQ\n");
        assert!(
            st.tab().csv.editing.is_some(),
            "stays in edit state after paste"
        );
        // newlines replaced with spaces: cells do not accept newlines
        st.csv_paste_text_into_sel("x\ny");
        assert_eq!(text_of(&st), "a,b\n1Z,2\n3,x y\n");
    }

    /// Selection copy (pure computation): cell = field text, row = tab-joined, column = newline-joined.
    #[test]
    fn csv_sel_text_cell_row_col() {
        let mut st = st_of("a,b\n1,2\n3,4\n");
        st.tab_mut().csv.enabled = true;
        st.tab_mut().csv.header = true;
        let _ = st.csv_rows_ensure(); // warm up the read-only cache
        st.csv_pick_cell(1, 0);
        assert_eq!(st.csv_sel_text().as_deref(), Some("3"));
        st.csv_pick_row(0);
        assert_eq!(st.csv_sel_text().as_deref(), Some("1\t2"));
        st.csv_pick_col(1);
        assert_eq!(st.csv_sel_text().as_deref(), Some("2\n4"));
        // no selection
        st.tab_mut().csv.sel = None;
        assert_eq!(st.csv_sel_text(), None);
    }

    #[test]
    fn csv_table_parse_and_cache_is_gen_keyed() {
        let mut st = st_of("a,b\n1,2\n3,4\n");
        st.tab_mut().csv.enabled = true;
        st.tab_mut().csv.table = true;
        let (rows, lines, end_lines, trunc) = st.csv_rows_ensure();
        assert_eq!(rows.len(), 3);
        assert_eq!(lines.as_slice(), &[0, 1, 2]);
        assert_eq!(end_lines.as_slice(), &[1, 2, 3]);
        assert!(!trunc);
        let gen = st.tabs[0].doc.gen;
        // the same gen does not re-parse: changing the delimiter invalidates the cache
        st.csv_set_delim(b'\t');
        assert!(st.tabs[0].csv.cache.is_none());
        let _ = gen;
        // editing the document invalidates the cache by gen
        st.tab_mut().doc.insert_text(0, "x");
        assert!(!st
            .tab()
            .csv
            .fresh(st.tabs[0].doc.id, st.tabs[0].doc.gen, b'\t'));
    }

    /// Numeric-first comparison still governs the view order.
    #[test]
    fn csv_sort_orders_numerically_in_the_view() {
        let mut st = st_of("n\n10\n2\n33\n");
        st.tab_mut().csv.enabled = true;
        st.tab_mut().csv.header = true;
        st.csv_pick_column(0);
        st.csv_sort(false);
        assert_eq!(view_col(&mut st, 0), ["n", "2", "10", "33"]);
    }

    /// Multi-line quoted fields: record ranges are `[start, end)` and the
    /// start/end maps travel with their row through a sort, so the table shows
    /// whole records and editing still writes the record on screen.
    #[test]
    fn csv_multiline_records_sort_as_whole_records() {
        let mut st = st_of("id,note\n1,\"a\nb\"\n2,\"c\nd\"\n");
        st.tab_mut().csv.enabled = true;
        st.tab_mut().csv.header = true;
        let (rows, starts, ends, _) = st.csv_rows_ensure();
        assert_eq!(rows[1][1], "a\nb");
        assert_eq!(starts.as_slice(), &[0, 1, 3]);
        assert_eq!(ends.as_slice(), &[1, 3, 5]);
        // edit a normal field inside a multi-line record: the whole record is written back over [1, 3)
        st.csv_begin_edit(0, 1);
        let mut ed = st
            .tab()
            .csv
            .editing
            .clone()
            .expect("multi-line record should be editable");
        ed.value = "z".into();
        st.tab_mut().csv.editing = Some(ed);
        st.csv_commit_edit(0);
        assert_eq!(text_of(&st), "id,note\n1,z\n2,\"c\nd\"\n");
        // descending by id: the records swap places in the view, addresses and all
        st.csv_pick_column(0);
        st.csv_sort(true);
        let (rows, starts, ends, _) = st.csv_rows_ensure();
        assert_eq!(rows[1][0], "2");
        assert_eq!(
            rows[1][1], "c\nd",
            "the quoted newline is still inside one record"
        );
        assert_eq!(starts.as_slice(), &[0, 2, 1]);
        assert_eq!(ends.as_slice(), &[1, 4, 2]);
        assert_eq!(text_of(&st), "id,note\n1,z\n2,\"c\nd\"\n", "file untouched");
    }

    /// A last record without a trailing newline is none of a view sort's
    /// business — and it must stay that way (no terminator invented or lost).
    #[test]
    fn csv_sort_never_touches_a_terminator() {
        let src = "id,note\n2,\"c\nd\"\n1,\"a\nb\"";
        let mut st = st_of(src);
        st.tab_mut().csv.enabled = true;
        st.tab_mut().csv.header = true;
        st.csv_pick_column(0);
        st.csv_sort(false);
        assert_eq!(view_col(&mut st, 0), ["id", "1", "2"]);
        assert_eq!(text_of(&st), src);
    }

    #[test]
    fn csv_delim_switch_reparses() {
        let mut st = st_of("a;b\n1;2\n");
        st.tab_mut().csv.enabled = true;
        st.csv_set_delim(b';');
        let (rows, _, _, _) = st.csv_rows_ensure();
        assert_eq!(rows[0], vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn csv_cell_to_text_moves_caret() {
        let mut st = st_of("a,b\n1,2\n3,4\n");
        st.tab_mut().csv.enabled = true;
        st.csv_cell_to_text(2);
        assert!(!st.tab().csv.table);
        let doc = st.active_doc();
        let line = doc.line_of_pos(st.caret().pos);
        assert_eq!(line, 2);
    }

    // ---- Row/column structural editing ----------------------------------------

    #[test]
    fn csv_insert_row_above_below_and_append() {
        let mut st = st_of("a,b\n1,2\n3,4\n");
        st.tab_mut().csv.enabled = true;
        st.tab_mut().csv.header = true;
        // insert above data row 0 ("1,2")
        st.csv_pick_row(0);
        st.csv_insert_row(true);
        assert_eq!(text_of(&st), "a,b\n,\n1,2\n3,4\n");
        st.undo();
        // insert below data row 1 ("3,4")
        st.csv_pick_row(1);
        st.csv_insert_row(false);
        assert_eq!(text_of(&st), "a,b\n1,2\n3,4\n,\n");
        st.undo();
        // no selection: below = append at the end
        st.tab_mut().csv.sel = None;
        st.csv_insert_row(false);
        assert_eq!(text_of(&st), "a,b\n1,2\n3,4\n,\n");
        // undo restores
        st.undo();
        assert_eq!(text_of(&st), "a,b\n1,2\n3,4\n");
    }

    #[test]
    fn csv_delete_row_removes_selected_line() {
        let mut st = st_of("a,b\n1,2\n3,4\n5,6\n");
        st.tab_mut().csv.enabled = true;
        st.tab_mut().csv.header = true;
        st.csv_pick_row(1);
        st.csv_delete_row();
        assert_eq!(text_of(&st), "a,b\n1,2\n5,6\n");
        st.undo();
        assert_eq!(text_of(&st), "a,b\n1,2\n3,4\n5,6\n");
    }

    #[test]
    fn csv_insert_col_left_right() {
        let mut st = st_of("a,b\n1,2\n3,4\n");
        st.tab_mut().csv.enabled = true;
        st.tab_mut().csv.header = true;
        // select column 1 (b), insert left → the empty column becomes column 1
        st.csv_pick_col(1);
        st.csv_insert_col(true);
        assert_eq!(text_of(&st), "a,,b\n1,,2\n3,,4\n");
        assert!(st
            .tab()
            .csv
            .sel
            .is_some_and(|s| s.kind == SelKind::Col && s.col == 1));
        st.undo();
        // insert right → the empty column is column 2
        st.csv_pick_col(1);
        st.csv_insert_col(false);
        assert_eq!(text_of(&st), "a,b,\n1,2,\n3,4,\n");
    }

    #[test]
    fn csv_delete_col_keeps_at_least_one() {
        let mut st = st_of("a,b\n1,2\n3,4\n");
        st.tab_mut().csv.enabled = true;
        st.tab_mut().csv.header = true;
        st.csv_pick_col(0);
        st.csv_delete_col();
        assert_eq!(text_of(&st), "b\n2\n4\n");
        st.undo();
        assert_eq!(text_of(&st), "a,b\n1,2\n3,4\n");
        // refuse to delete when only one column remains
        let mut st1 = st_of("only\n1\n");
        st1.tab_mut().csv.enabled = true;
        st1.csv_pick_col(0);
        st1.csv_delete_col();
        assert_eq!(
            text_of(&st1),
            "only\n1\n",
            "single-column documents must not allow column deletion"
        );
    }

    #[test]
    fn csv_delete_key_semantics_clear_cell() {
        let mut st = st_of("a,b\n1,2\n");
        st.tab_mut().csv.enabled = true;
        st.tab_mut().csv.header = true;
        st.csv_pick_cell(0, 1);
        st.csv_clear_cell();
        assert_eq!(text_of(&st), "a,b\n1,\n");
        // already-empty cell: no document change (gen unchanged)
        let gen = st.tabs[0].doc.gen;
        st.csv_clear_cell();
        assert_eq!(st.tabs[0].doc.gen, gen);
    }

    #[test]
    fn csv_move_sel_clamps_to_data() {
        let mut st = st_of("a,b\n1,2\n3,4\n");
        st.tab_mut().csv.enabled = true;
        st.tab_mut().csv.header = true;
        assert!(st.csv_move_sel(0, 0)); // no selection → lands at (0,0)
        assert!(st.tab().csv.sel.is_some_and(|s| s.row == 0 && s.col == 0));
        // already at origin: out-of-bounds move stays put and returns false (the component layer lets the key through)
        assert!(!st.csv_move_sel(-5, -5));
        assert!(st.tab().csv.sel.is_some_and(|s| s.row == 0 && s.col == 0));
        assert!(st.csv_move_sel(1, 1));
        let sel = st.tab().csv.sel.unwrap();
        assert_eq!((sel.row, sel.col), (1, 1));
        // already at the last cell: out-of-bounds move stays put and returns false
        assert!(!st.csv_move_sel(9, 9));
        let sel = st.tab().csv.sel.unwrap();
        assert_eq!(
            (sel.row, sel.col),
            (1, 1),
            "out-of-bounds clamped to the last cell"
        );
    }

    #[test]
    fn csv_struct_ops_rewrite_multiline_quoted_fields() {
        // first row's quoted field spans lines: column insert must rewrite the whole record as one block, not split by text lines
        let mut st = st_of("\"x\ny\",b\n1,2\n");
        st.tab_mut().csv.enabled = true;
        st.csv_pick_col(0);
        st.csv_insert_col(false);
        assert_eq!(text_of(&st), "\"x\ny\",,b\n1,,2\n");
        st.undo();
        assert_eq!(text_of(&st), "\"x\ny\",b\n1,2\n");
    }

    #[test]
    fn csv_row_struct_ops_keep_multiline_records_intact() {
        let mut st = st_of("id,note\n1,\"a\nb\"\n2,\"c\nd\"\n");
        st.tab_mut().csv.enabled = true;
        st.tab_mut().csv.header = true;
        // insert below the first multi-line record: the insertion point must be after the whole record
        st.csv_pick_row(0);
        st.csv_insert_row(false);
        assert_eq!(text_of(&st), "id,note\n1,\"a\nb\"\n,\n2,\"c\nd\"\n");
        st.undo();
        // delete the first multi-line record: the whole [1,3) block disappears
        st.csv_pick_row(0);
        st.csv_delete_row();
        assert_eq!(text_of(&st), "id,note\n2,\"c\nd\"\n");
        st.undo();
        assert_eq!(text_of(&st), "id,note\n1,\"a\nb\"\n2,\"c\nd\"\n");
    }

    #[test]
    fn csv_row_struct_ops_preserve_crlf_and_blank_lines() {
        let mut st = st_of("id,note\r\n1,\"a\r\nb\"\r\n\r\n2,c\r\n");
        st.tab_mut().csv.enabled = true;
        st.tab_mut().csv.header = true;
        st.csv_pick_row(0);
        st.csv_insert_row(false);
        assert_eq!(
            text_of(&st),
            "id,note\r\n1,\"a\r\nb\"\r\n,\r\n\r\n2,c\r\n",
            "blank lines between records must be preserved"
        );
        st.undo();
        st.csv_pick_row(0);
        st.csv_delete_row();
        assert_eq!(text_of(&st), "id,note\r\n\r\n2,c\r\n");
    }

    /// Unclosed multi-line quoted fields are still bad data: rewriting would
    /// "fix" the quotes and swallow subsequent lines, so column structural
    /// ops keep rejecting wholesale.
    #[test]
    fn csv_struct_ops_refuse_unclosed_quote_multiline() {
        let src = "a,b\n\"x\ny\n";
        let mut st = st_of(src);
        st.tab_mut().csv.enabled = true;
        st.csv_pick_col(0);
        st.csv_insert_col(false);
        assert_eq!(
            text_of(&st),
            src,
            "multi-line fields with unclosed quotes must be rejected wholesale"
        );
        st.csv_sort(false);
        assert_eq!(
            text_of(&st),
            src,
            "a view sort never rewrites the field, so the quotes stay as they are"
        );
    }
}
