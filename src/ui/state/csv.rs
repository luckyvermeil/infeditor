//! Per-tab state of the CSV toolbar + table view.
//!
//! The table view **directly replaces** the editor text box (see
//! `components::csvtable`): csv/tsv documents open straight into table mode,
//! and the toolbar can switch between table/text at any time.
//!
//! Parse results are cached in [`CsvView::cache`] keyed by
//! `(document id, gen, delimiter)`: the doc's `gen` changes with every
//! keystroke, and re-parsing the whole CSV per frame is unacceptable; the
//! cache makes "don't re-parse what didn't change" the default path. Parsing
//! itself has row and byte caps (see [`CSV_MAX_ROWS`] / [`CSV_MAX_BYTES`]);
//! past the cap it truncates and says so on the toolbar — the table view is
//! a browsing tool, not a lossless converter.
//!
//! Table metrics (row height, column bounds, the auto-fit width rule
//! [`auto_col_widths`]) live here too, and every module that needs them imports
//! them: the command layer measures scrolling without depending on the render
//! component, and the two can never disagree about a column's width.

use std::rc::Rc;
use unicode_width::UnicodeWidthChar;

/// Row cap for the parse cache (header row included).
pub const CSV_MAX_ROWS: usize = 200_000;
/// Byte cap participating in parsing: past it, truncate directly (huge CSVs
/// preview only the start).
pub const CSV_MAX_BYTES: u64 = 64 * 1024 * 1024;

/// Table row height: matches the text view's default line height so
/// switching is visually seamless. Defined in the state layer,
/// components::csvtable re-exports it — the command layer computes row
/// height/scrolling without a reverse dependency on the render component.
pub const ROW_H: f32 = 20.0;
/// Column width lower/upper bounds (px).
pub const COL_MIN: f32 = 40.0;
pub const COL_MAX: f32 = 480.0;
/// Width of the table's own scrollbar (a bit narrower than the text view's,
/// Excel style).
pub const SB_W: f32 = 12.0;
/// Number of sampled rows feeding column-width estimation.
pub const WIDTH_SAMPLE: usize = 300;
/// Default width of blank filler columns (adaptive widths make no sense for
/// empty columns).
pub const EMPTY_COL_W: f32 = 72.0;
/// Maximum number of blank filler columns (guards extreme viewports).
pub const MAX_EMPTY_COLS: usize = 60;
/// Left/right padding inside a cell (px). The renderer draws the cell with this
/// padding and [`auto_col_widths`] adds `2 * CELL_PAD` per cell, so the two can
/// never disagree about how much room the text gets.
pub const CELL_PAD: f32 = 6.0;

/// Display width of a cell string in "cell units" (CJK/fullwidth ≈ 2 Latin
/// glyphs, matching the monospace advance the renderer measures text with).
pub fn visual_len(s: &str) -> f32 {
    s.chars().map(|c| c.width().unwrap_or(1) as f32).sum()
}

/// Auto-fit column widths: the widest of the header row and the first
/// [`WIDTH_SAMPLE`] data rows, floored at [`COL_MIN`] and capped at [`COL_MAX`].
///
/// This is the **single** place where cell text turns into a column width: the
/// rendered table (`components::csvtable`) and the scrollbar geometry
/// (`commands::csv`) both call it, so column widths and the thumb they are
/// mapped against cannot disagree. The data-column count comes back with the
/// widths because it falls out of the same scan (rows have ragged lengths; the
/// widest row decides).
pub fn auto_col_widths(rows: &[Vec<String>], header_on: bool, adv: f32) -> (usize, Vec<f32>) {
    let n_cols = rows.iter().map(Vec::len).max().unwrap_or(0);
    let mut auto = vec![COL_MIN; n_cols];
    let mut grow = |row: &Vec<String>| {
        for (i, cell) in row.iter().enumerate() {
            let w = (visual_len(cell) * adv + CELL_PAD * 2.0 + 2.0).min(COL_MAX);
            if w > auto[i] {
                auto[i] = w;
            }
        }
    };
    if header_on {
        if let Some(h) = rows.first() {
            grow(h);
        }
    }
    for row in rows.iter().skip(header_on as usize).take(WIDTH_SAMPLE) {
        grow(row);
    }
    (n_cols, auto)
}

/// What is selected (Excel semantics): a single cell, a whole row, or a
/// whole column. Row/column selection is the target of insert/delete
/// row/column — click the row number to select the row, the column header
/// to select the column.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum SelKind {
    #[default]
    Cell,
    Row,
    Col,
}

/// The table view's current selection (set by clicking a cell/row
/// number/column header; sorting acts on the selected column).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct CellSel {
    pub row: usize,
    pub col: usize,
    pub kind: SelKind,
}

impl CellSel {
    /// Cell selection.
    pub fn cell(row: usize, col: usize) -> Self {
        CellSel {
            row,
            col,
            kind: SelKind::Cell,
        }
    }
    /// Whole-row selection.
    pub fn row(row: usize) -> Self {
        CellSel {
            row,
            col: 0,
            kind: SelKind::Row,
        }
    }
    /// Whole-column selection.
    pub fn col(col: usize) -> Self {
        CellSel {
            row: 0,
            col,
            kind: SelKind::Col,
        }
    }
}

/// The cell being edited in place (Excel semantics: typing writes back to
/// the document live, Enter/Tab commits and moves, Esc restores the original
/// value). The text buffer lives in state, not a DOM node — the whole app
/// has exactly one real keyboard focus (the composer's transparent
/// textarea), which routes keys by state.
#[derive(Clone, Debug, PartialEq)]
pub struct CellEdit {
    pub row: usize,
    pub col: usize,
    pub value: String,
    /// Original value at edit start (for Esc cancel/restore: live editing
    /// has already written intermediate values to the document, so cancel
    /// must write the original value back, not just drop the buffer).
    pub orig: String,
    /// IME preedit text being composed (pinyin string, candidate window not
    /// yet committed). The edit box renders it after `value` (accent color);
    /// cleared on commit/cancel.
    pub preedit: String,
    /// The document's edit serial at edit start (`TextDocument::edit_serial`).
    ///
    /// Live write-back applies every key as an op; the correct Esc semantics
    /// is to **roll back all ops of this session** (`undo` decrements the
    /// serial symmetrically), not to write the original value again — the
    /// latter keeps the serial moving forward, so the content is restored
    /// but the title still shows "modified" and closing still prompts to
    /// save (a real defect caught by the GUI test test_escape_cancels_edit).
    pub start_serial: u64,
}

/// An in-progress pointer gesture in the table view (scrollbar drag /
/// row column resize).
///
/// A scrollbar drag caches the total content size (`total`) at **press
/// time**: if every drag frame recomputed column widths (300 sampled rows ×
/// per-column char width) or total height, each pointer move would be an
/// O(sampled rows × columns) scan — that is exactly why the scrollbar would
/// not drag and lagged half a step behind. Content size cannot change during
/// a drag (scrolling does not change data), so the cache is exact.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum CsvDrag {
    /// Vertical scrollbar thumb drag; `grab` is the press point's offset
    /// within the thumb, `total` the total content height at press (px).
    SbV { grab: f32, total: f32 },
    /// Horizontal scrollbar thumb drag; `total` is the total content width
    /// at press (px).
    SbH { grab: f32, total: f32 },
    /// Column resize: dragging a header's right edge
    ColResize {
        col: usize,
        start_x: f32,
        start_w: f32,
    },
    /// Row resize: dragging a row-number slot's bottom edge
    RowResize {
        row: usize,
        start_y: f32,
        start_h: f32,
    },
}

/// Return value of `csv_rows_ensure()`: `(rows, start-line map, end-line
/// map, truncated)`.
///
/// Start/end lines come in pairs; `end` is the row's **exclusive** last
/// line. Ordinary rows always have `end == start + 1`; multi-line quoted
/// fields have a larger `end`. The command layer no longer assumes "the
/// whole table is one line per row" and instead rewrites a single record
/// precisely by `[start, end)`.
pub type CsvRows = (Rc<Vec<Vec<String>>>, Rc<Vec<u64>>, Rc<Vec<u64>>, bool);

/// `(rows, start-line map, end-line map)` after the guard passes (without
/// the truncation flag).
pub type CsvRows2 = (Rc<Vec<Vec<String>>>, Rc<Vec<u64>>, Rc<Vec<u64>>);

/// Parse snapshot: document generation, delimiter and result rows. `Rc`
/// makes per-frame clones cheap for components.
///
/// The arrays are in **view order**: document order unless a sort lens is
/// applied ([`CsvView::sort_col`]). Sorting never rewrites the document, so
/// the snapshot carries the mapping back to document order itself
/// ([`Self::doc_row`]) — all row→text addresses (`line_of_row`,
/// `end_line_of_row`) travel with their row, so editing a cell or deleting a
/// record in a sorted view still writes the record the user is looking at.
#[derive(Clone)]
pub struct CsvCache {
    pub doc_id: u64,
    pub gen: u64,
    pub delim: u8,
    pub rows: Rc<Vec<Vec<String>>>,
    /// Document line number where each parsed row starts (maps back to text
    /// mode even after skipping blank lines)
    pub line_of_row: Rc<Vec<u64>>,
    /// Exclusive end line of each row (`line_of_row[i] + 1`; larger for
    /// multi-line quoted fields)
    pub end_line_of_row: Rc<Vec<u64>>,
    /// Document row index shown at each view position (`doc_row[i] == i` in
    /// document order). The only record of the original order once a sort lens
    /// is applied: clearing the lens permutes by this instead of re-parsing.
    pub doc_row: Rc<Vec<usize>>,
    /// Truncated by the cap (data remains unshown)
    pub truncated: bool,
}

#[derive(Clone)]
pub struct CsvView {
    /// Toolbar visible (on by default for csv/tsv; the "CSV Table View"
    /// menu or a language switch can also enable it)
    pub enabled: bool,
    /// true = table view replaces the text box; false = plain text editing
    pub table: bool,
    /// Field delimiter (`,` `\t` `;` `|`)
    pub delim: u8,
    /// First row is a header row (pinned at the top, excluded from scrolling)
    pub header: bool,
    /// Sort lens: column the **table view** is ordered by, `None` = document
    /// order. Display only — the document text is never reordered, so the
    /// arrow this drives is the only trace a sort leaves.
    pub sort_col: Option<usize>,
    /// Lens direction (`true` = descending)
    pub sort_desc: bool,
    /// Cell selection (by click)
    pub sel: Option<CellSel>,
    /// Table's vertical/horizontal scrolling (maintained by the component,
    /// separate from the text view's scroll_*)
    pub scroll_top: f64,
    pub scroll_left: f32,
    /// Parse cache (see module comment)
    pub cache: Option<CsvCache>,
    /// Column widths adjusted by the user (aligned to data columns; `None` =
    /// adaptive to content). Session memory only — nothing to persist;
    /// reopening a document returns to adaptive.
    pub col_widths: Vec<Option<f32>>,
    /// Row heights adjusted by the user (view-row index → px; uncovered rows
    /// use the default). Cleared when the cache is rebuilt (editing moves
    /// rows) or re-ordered (sorting moves rows) — a height belongs to a
    /// position, not to a record.
    pub row_heights: std::collections::BTreeMap<usize, f32>,
    /// Cell being edited in place (see [`CellEdit`]).
    pub editing: Option<CellEdit>,
    /// In-progress pointer gesture (see [`CsvDrag`]).
    pub drag: Option<CsvDrag>,
}

impl CsvCache {
    /// Whether the snapshot is in document order (no sort lens applied).
    pub fn in_doc_order(&self) -> bool {
        self.doc_row.iter().enumerate().all(|(i, &d)| i == d)
    }

    /// Same snapshot re-ordered: `order[i]` = index **in this snapshot** that
    /// belongs at view position `i`.
    ///
    /// The row→text addresses travel with their rows, which is why a sorted
    /// view can still write the record the user is looking at. Returns `None`
    /// for the identity order (nothing to change) so callers can skip the
    /// cache swap entirely — a sort that would not move anything must not
    /// invalidate the user's row heights.
    pub fn permuted(&self, order: &[usize]) -> Option<CsvCache> {
        if order.iter().enumerate().all(|(i, &j)| i == j) {
            return None;
        }
        Some(CsvCache {
            doc_id: self.doc_id,
            gen: self.gen,
            delim: self.delim,
            rows: Rc::new(order.iter().map(|&i| self.rows[i].clone()).collect()),
            line_of_row: Rc::new(order.iter().map(|&i| self.line_of_row[i]).collect()),
            end_line_of_row: Rc::new(order.iter().map(|&i| self.end_line_of_row[i]).collect()),
            doc_row: Rc::new(order.iter().map(|&i| self.doc_row[i]).collect()),
            truncated: self.truncated,
        })
    }
}

impl Default for CsvView {
    fn default() -> Self {
        CsvView {
            enabled: false,
            table: false,
            delim: b',',
            header: false,
            sort_col: None,
            sort_desc: false,
            sel: None,
            scroll_top: 0.0,
            scroll_left: 0.0,
            cache: None,
            col_widths: Vec::new(),
            row_heights: std::collections::BTreeMap::new(),
            editing: None,
            drag: None,
        }
    }
}

impl CsvView {
    /// Initialize from document info: csv/tsv opens straight into table
    /// mode; other documents stay in text. `.tsv` defaults to tab delimiter
    /// even when language detection said CSV.
    pub fn for_doc(spec_name: &str, csv_mode: bool, path: Option<&std::path::Path>) -> Self {
        let mut v = CsvView::default();
        v.enabled = csv_mode || spec_name == "CSV";
        v.table = v.enabled;
        if spec_name == "CSV" {
            v.delim = b',';
        }
        if let Some(p) = path {
            let ext = p
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| e.to_ascii_lowercase());
            if ext.as_deref() == Some("tsv") {
                v.delim = b'\t';
            }
        }
        v
    }

    /// Whether the cache is still valid for `(doc_id, gen, delim)`.
    pub fn fresh(&self, doc_id: u64, gen: u64, delim: u8) -> bool {
        self.cache
            .as_ref()
            .is_some_and(|c| c.doc_id == doc_id && c.gen == gen && c.delim == delim)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The rule the table's renderer and the scrollbar geometry share: the widest
    /// sampled cell (header included when it is on), `CELL_PAD` on both sides.
    #[test]
    fn auto_col_widths_takes_the_widest_sampled_cell() {
        let adv = 8.0;
        let rows = vec![
            vec!["aaaaa".to_string(), "b".to_string()],
            vec!["c".to_string(), "longer cell".to_string()],
        ];
        let (n_cols, widths) = auto_col_widths(&rows, false, adv);
        assert_eq!(n_cols, 2);
        assert_eq!(widths[0], 5.0 * adv + CELL_PAD * 2.0 + 2.0);
        assert_eq!(widths[1], 11.0 * adv + CELL_PAD * 2.0 + 2.0);
    }

    /// Ragged rows decide the column count, a narrow column stays at `COL_MIN`
    /// (a floor, never a shrink), and a long one stops at `COL_MAX`.
    #[test]
    fn auto_col_widths_clamps_and_counts_ragged_rows() {
        let rows = vec![
            vec!["i".to_string()],
            vec!["j".to_string(), "x".repeat(1000)],
        ];
        let (n_cols, widths) = auto_col_widths(&rows, false, 8.0);
        assert_eq!(n_cols, 2);
        assert_eq!(widths[0], COL_MIN);
        assert_eq!(widths[1], COL_MAX);
    }

    /// The first row counts either way — grown as the header, or sampled as the
    /// first data row: `header_on` decides where the sampled window starts, it
    /// must never drop a row from the estimate.
    #[test]
    fn auto_col_widths_always_samples_the_first_row() {
        let rows = vec![vec!["column heading".to_string()], vec!["x".to_string()]];
        let (_, with_header) = auto_col_widths(&rows, true, 8.0);
        let (_, without_header) = auto_col_widths(&rows, false, 8.0);
        assert_eq!(
            with_header[0],
            14.0 * 8.0 + CELL_PAD * 2.0 + 2.0,
            "the header text sets the width"
        );
        assert_eq!(without_header[0], with_header[0]);
    }

    // ---- View-order permutation (sort lens) ---------------------------------

    fn cache_of(rows: &[&[&str]]) -> CsvCache {
        let rows: Vec<Vec<String>> = rows
            .iter()
            .map(|r| r.iter().map(|c| c.to_string()).collect())
            .collect();
        let n = rows.len();
        CsvCache {
            doc_id: 1,
            gen: 0,
            delim: b',',
            rows: Rc::new(rows),
            line_of_row: Rc::new((0..n as u64).collect()),
            end_line_of_row: Rc::new((1..=n as u64).collect()),
            doc_row: Rc::new((0..n).collect()),
            truncated: false,
        }
    }

    /// Re-ordering keeps every row's text address with its row: that is what
    /// lets a sorted view edit the record it displays.
    #[test]
    fn permuted_carries_the_row_addresses_along() {
        let c = cache_of(&[&["h"], &["b"], &["a"]]);
        let p = c.permuted(&[0, 2, 1]).expect("reordered");
        assert_eq!(
            p.rows.iter().map(|r| r[0].as_str()).collect::<Vec<_>>(),
            ["h", "a", "b"]
        );
        assert_eq!(
            p.line_of_row.as_slice(),
            &[0, 2, 1],
            "line 1 stays with 'b'"
        );
        assert_eq!(p.end_line_of_row.as_slice(), &[1, 3, 2]);
        assert_eq!(
            p.doc_row.as_slice(),
            &[0, 2, 1],
            "the original order survives the permutation"
        );
        assert!(!p.in_doc_order());
    }

    /// The identity order is a no-op, not a new snapshot: sorting must not
    /// drop the user's row heights by swapping an identical cache.
    #[test]
    fn permuting_with_the_identity_order_is_a_no_op() {
        let c = cache_of(&[&["a"], &["b"]]);
        assert!(c.in_doc_order());
        assert!(c.permuted(&[0, 1]).is_none());
        // …and permuting by document order restores it exactly
        let sorted = c.permuted(&[1, 0]).expect("reordered");
        assert_eq!(sorted.doc_row.as_slice(), &[1, 0]);
        let mut back = vec![0, 1];
        back.sort_by_key(|&pos| sorted.doc_row[pos]);
        let restored = sorted.permuted(&back).expect("restored");
        assert!(restored.in_doc_order());
        assert_eq!(
            restored
                .rows
                .iter()
                .map(|r| r[0].as_str())
                .collect::<Vec<_>>(),
            ["a", "b"]
        );
    }
}
