//! Table commands: insert, cell navigation, row/column edits, alignment.
//!
//! A table is a flat run of `Td` blocks; `TableCtx` is the geometry snapshot
//! shared by the per-command arms.

use super::*;

impl Editor {
    /// Focus block inside a table (Td) → the index just after the table's last Td
    /// (after the whole table); otherwise `None`. A table is a contiguous run of `Td`
    /// blocks with **one `table_seq`**, and inserting a paragraph / new table / rule
    /// in the middle would split it into fragments (rendering chunks contiguous runs
    /// by `table_seq`). The scan must therefore stop at a Td of a *different* table:
    /// two tables separated by one blank line parse into one contiguous run of Td
    /// blocks, and an `is_td`-only scan treated them as a single table — `InsertHr`
    /// (and paste / insert-block / insert-table) then landed between the second
    /// table's cells. `table_cmd` scans in the same direction (backwards to find the
    /// header, then forwards to find the end).
    pub(crate) fn table_end_after(&self, block: usize) -> Option<usize> {
        let seq = self.doc.blocks.get(block)?.table_seq()?;
        Some(
            (block..self.doc.blocks.len())
                .find(|&i| self.doc.blocks[i].table_seq() != Some(seq))
                .unwrap_or(self.doc.blocks.len()),
        )
    }

    /// Insert an empty paragraph before/after the focused block and move the caret
    /// into it. When the focus is inside a table, "before/after" is clamped to
    /// before/after the whole table — no paragraph ever lands between cells.
    pub(crate) fn insert_block(&mut self, before: bool) -> bool {
        let p = self.sel.focus;
        let at = if before {
            match self.doc.blocks.get(p.block).and_then(|b| b.table_seq()) {
                // The table's first block index: walk back from the focus to the last
                // block that is not a cell of *this* table (the block before the header).
                // A neighbouring table's cells are not part of it.
                Some(seq) => (0..=p.block)
                    .rev()
                    .find(|&i| self.doc.blocks[i].table_seq() != Some(seq))
                    .map_or(0, |i| i + 1),
                None => p.block,
            }
        } else {
            self.table_end_after(p.block).unwrap_or(p.block + 1)
        };
        self.doc.blocks.insert(at, Block::Para(vec![]));
        self.sel = Sel::caret(Pos::new(at, 0));
        true
    }

    /// Insert a `rows × cols` table after the focused block (the first row renders as
    /// header `th`), then add an empty paragraph to receive further input; the caret
    /// lands in the first cell. `table_seq` comes from `doc.next_table_seq` (the same
    /// allocator the parser uses), and a defensive max is taken over the
    /// `table_seq`s already in the document — a hand-built `Doc` (`Doc::new` always
    /// yields 0) then cannot collide with an existing table (a collision would make
    /// the renderer treat the two as one).
    pub(crate) fn insert_table(&mut self, rows: u8, cols: u8) -> bool {
        let rows = rows.clamp(1, 9) as usize;
        let cols = cols.clamp(1, 9) as usize;
        let p = self.sel.focus;
        let used = self
            .doc
            .blocks
            .iter()
            .filter_map(|b| b.table_seq())
            .max()
            .map_or(0, |m| m + 1);
        let seq = self.doc.next_table_seq.max(used);
        self.doc.next_table_seq = seq + 1;
        let mut tds = Vec::with_capacity(rows * cols);
        for r in 0..rows {
            for _c in 0..cols {
                tds.push(Block::Td {
                    table_seq: seq,
                    row: r,
                    align: 0,
                    inlines: vec![],
                });
            }
        }
        let at = self.table_end_after(p.block).unwrap_or(p.block + 1);
        self.doc.blocks.splice(at..at, tds);
        self.doc
            .blocks
            .insert(at + rows * cols, Block::Para(vec![]));
        self.sel = Sel::caret(Pos::new(at, 0));
        true
    }

    /// Table auto-completion: when the whole paragraph is `| a | b |` (at least two
    /// cells), Enter turns that paragraph into a table — the first row becomes the
    /// header, an all-empty row is added below, and the caret lands in the body's
    /// first cell (typing continues into it).
    ///
    /// The header is cut from the **original inline tree**
    /// (`table_cell_ranges` + `Editor::collect_range`), never re-parsed from plain
    /// text — which is why the bold in `| **a** |` survives (re-parsing plain would
    /// flatten it).
    pub(crate) fn table_autoformat(&mut self, block: usize) -> bool {
        let Some(Block::Para(inlines)) = self.doc.blocks.get(block) else {
            return false;
        };
        let text = Block::Para(inlines.clone()).plain_text();
        let Some(ranges) = table_cell_ranges(&text) else {
            return false;
        };
        let n = ranges.len();
        if n < 2 {
            return false;
        }
        let cells: Vec<Vec<Inline>> = ranges
            .iter()
            .map(|&(s, e)| {
                let mut out = Vec::new();
                Editor::collect_range(inlines, &(s..e), 0, &mut out);
                out
            })
            .collect();
        // The seq is max+1: other tables in the document have their own seqs, and a
        // collision makes the renderer merge contiguous tables with the same seq.
        let used = self
            .doc
            .blocks
            .iter()
            .filter_map(|b| b.table_seq())
            .max()
            .map_or(0, |m| m + 1);
        let seq = self.doc.next_table_seq.max(used);
        self.doc.next_table_seq = seq + 1;
        let mut tds = Vec::with_capacity(n * 2);
        for cell in cells {
            tds.push(Block::Td {
                table_seq: seq,
                row: 0,
                align: 0,
                inlines: cell,
            });
        }
        for _ in 0..n {
            tds.push(Block::Td {
                table_seq: seq,
                row: 1,
                align: 0,
                inlines: vec![],
            });
        }
        self.doc.blocks.splice(block..=block, tds);
        self.sel = Sel::caret(Pos::new(block + n, 0));
        true
    }

    pub(crate) fn table_cmd(&mut self, cmd: TableCmd) -> bool {
        let p = self.sel.focus;
        // Locating the focus cell looks through the quote shell: a Td inside a Quoted
        // shell counts too
        let (seq, row) = match self.doc.blocks.get(p.block).map(|b| b.core()) {
            Some(Block::Td { table_seq, row, .. }) => (*table_seq, *row),
            _ => return false,
        };
        // A table inside a quote shell: new cells inherit the focused block's quote
        // depth (emit's `> ` prefix is not lost)
        let depth = self.doc.blocks[p.block].quote_depth();
        // Find the table's block range. The scan is by **`table_seq`**, not by
        // `is_td`: two tables separated by a single blank line are one contiguous run
        // of Td blocks, and mixing them into one range made the column count include
        // the other table's cells (InsertColAfter silently did nothing, Tab appended
        // its row to the wrong table).
        let start = (0..=p.block)
            .rev()
            .find(|&i| self.doc.blocks[i].table_seq() != Some(seq))
            .map_or(0, |i| i + 1);
        let end = (start..self.doc.blocks.len())
            .find(|&i| self.doc.blocks[i].table_seq() != Some(seq))
            .unwrap_or(self.doc.blocks.len());
        let cols = self.doc.blocks[start..end]
            .iter()
            .filter(|b| matches!(b.core(), Block::Td { row: r, .. } if *r == row))
            .count();
        let t = TableCtx {
            seq,
            row,
            depth,
            start,
            end,
            cols,
        };
        match cmd {
            TableCmd::InsertRowAfter => self.table_insert_row_after(&t),
            TableCmd::DeleteRow => self.table_delete_row(&t),
            TableCmd::InsertColAfter => self.table_insert_col_after(&t, p.block),
            TableCmd::DeleteCol => self.table_delete_col(&t, p.block),
            TableCmd::Align(a) => self.table_align(&t, p.block, a),
            // Tab chain: move to the next cell and select its whole content (an empty cell
            // collapses to the cell start). After the last cell, **append a row** at the
            // table end and land on the new row's first cell — that edits the document,
            // this command sits outside may_edit, so it pushes its own snapshot (one undo
            // removes the whole row insertion).
            TableCmd::CellNext => {
                if p.block + 1 < end {
                    self.select_cell_contents(p.block + 1);
                    true
                } else {
                    self.push_undo(false);
                    let ins: Vec<Block> = (0..cols)
                        .map(|_| Self::new_cell(seq, row + 1, 0, depth))
                        .collect();
                    self.doc.blocks.splice(end..end, ins);
                    self.select_cell_contents(end);
                    true
                }
            }
            // Shift+Tab: move to the previous cell (select the whole cell's content like
            // Tab); the first cell is a no-op.
            TableCmd::CellPrev => {
                if p.block > start {
                    self.select_cell_contents(p.block - 1);
                    true
                } else {
                    false
                }
            }
        }
    }

    /// InsertRowAfter: splice an empty row after the focus row's last cell.
    pub(crate) fn table_insert_row_after(&mut self, t: &TableCtx) -> bool {
        let ins: Vec<Block> = (0..t.cols)
            .map(|_| Self::new_cell(t.seq, t.row + 1, 0, t.depth))
            .collect();
        // All rows after this one get +1 (within this table only: other tables have
        // their own seqs, and lifting their row numbers misaligns their row grouping and
        // emit entirely. Rows below the focus row inside this table must be lifted too
        // — otherwise they collide with the new row's number and emit groups by row
        // into a ragged shape)
        for b in self.doc.blocks.iter_mut() {
            if let Block::Td {
                table_seq: s,
                row: r,
                ..
            } = b.core_mut()
            {
                if *s == t.seq && *r > t.row {
                    *r += 1;
                }
            }
        }
        // The new row is spliced **after** the focus row's last cell: it used to be
        // simplified to appending at the table end, and once the row numbers were
        // renumbered the physical order no longer matched them — the wys view renders
        // physical order while export groups by row number, so inserting a row that was
        // not the last one made the view and the saved result diverge.
        let ins_at = t.start
            + self.doc.blocks[t.start..t.end]
                .iter()
                .rposition(|b| matches!(b.core(), Block::Td { row: r, .. } if *r == t.row))
                .map_or(t.end - t.start, |i| i + 1);
        self.doc.blocks.splice(ins_at..ins_at, ins);
        true
    }

    /// DeleteRow: remove the focus row and close the numbering gap. The last
    /// remaining row is kept (a table needs its header/separator).
    pub(crate) fn table_delete_row(&mut self, t: &TableCtx) -> bool {
        if !self.doc.blocks[t.start..t.end]
            .iter()
            .any(|b| matches!(b.core(), Block::Td { row: r, .. } if *r != t.row))
        {
            return false; // The last row is not deleted (simplified)
        }
        self.doc.blocks.retain(|b| {
            !matches!(b.core(),
                Block::Td { table_seq: s, row: r, .. } if *s == t.seq && *r == t.row)
        });
        for b in self.doc.blocks.iter_mut() {
            if let Block::Td {
                table_seq: s,
                row: r,
                ..
            } = b.core_mut()
            {
                if *s == t.seq && *r > t.row {
                    *r -= 1;
                }
            }
        }
        self.sel = Sel::caret(Pos::new(
            t.start.min(self.doc.blocks.len().saturating_sub(1)),
            0,
        ));
        true
    }

    /// InsertColAfter: add a cell after the focus column in every row. It used to
    /// add one only to the focus row, which tore the table into a ragged shape
    /// (row0 = 4 cells, row1 = 2 cells).
    pub(crate) fn table_insert_col_after(&mut self, t: &TableCtx, focus_block: usize) -> bool {
        // The focus cell's column ordinal (the same pass as DeleteCol/Align, see focus_col_ord)
        let col = self.focus_col_ord(t.start, focus_block, t.row, t.seq);
        // Collect every row's insertion point (after that row's col-th cell) and insert
        // from the table end towards the header so the indices do not drift. The new
        // cell inherits the alignment and quote depth of the cell at the same column in
        // its row.
        let mut ins_at: Vec<(usize, Block)> = Vec::new();
        let mut cur_row = usize::MAX;
        let mut cur_col = 0;
        for i in t.start..t.end {
            if let Block::Td {
                row: r, align: a, ..
            } = self.doc.blocks[i].core()
            {
                if *r != cur_row {
                    cur_row = *r;
                    cur_col = 0;
                }
                if cur_col == col {
                    ins_at.push((
                        i + 1,
                        Self::new_cell(t.seq, *r, *a, self.doc.blocks[i].quote_depth()),
                    ));
                }
                cur_col += 1;
            }
        }
        ins_at.sort_by_key(|x| std::cmp::Reverse(x.0));
        for (at, cell) in ins_at {
            self.doc.blocks.insert(at, cell);
        }
        true
    }

    /// DeleteCol: remove the focus column's cell from every row. Positions are
    /// collected in one pass (per-row cell ordinal) and removed back-to-front so
    /// the indices stay valid — equivalent to the old rescan-after-each-removal
    /// double loop, which the 3×3 quote-shell test pins.
    pub(crate) fn table_delete_col(&mut self, t: &TableCtx, focus_block: usize) -> bool {
        // A table needs at least one column. Removing the last column of a
        // one-column table takes every Td with it, and such a table has no
        // trailing paragraph — `blocks` would end up empty, breaking the
        // "at least one block" invariant that every caret command indexes into
        // (`Doc::new`). `table_delete_row` already refuses to delete the last
        // row for the same reason.
        let end = t.end.min(self.doc.blocks.len());
        if self.doc.blocks[t.start.min(end)..end]
            .iter()
            .filter(|b| matches!(b.core(), Block::Td { row: r, .. } if *r == t.row))
            .nth(1)
            .is_none()
        {
            return false;
        }
        // This row and cell's column ordinal (like InsertColAfter, excluding the focus)
        let col = self.focus_col_ord(t.start, focus_block, t.row, t.seq);
        let mut hits = Vec::new();
        let mut cur_row = usize::MAX;
        let mut cur_col = 0;
        for i in t.start..self.doc.blocks.len() {
            if let Block::Td {
                table_seq: s,
                row: r,
                ..
            } = self.doc.blocks[i].core()
            {
                if *s != t.seq {
                    continue;
                }
                if *r != cur_row {
                    cur_row = *r;
                    cur_col = 0;
                }
                if cur_col == col {
                    hits.push(i);
                }
                cur_col += 1;
            }
        }
        let mut removed_any = false;
        for &i in hits.iter().rev() {
            self.doc.blocks.remove(i);
            removed_any = true;
        }
        if removed_any {
            // The caret was probably inside a cell that no longer exists (the
            // indices shift left after each removal). Park it on the table's
            // first block, same as DeleteRow.
            self.sel = Sel::caret(Pos::new(
                t.start.min(self.doc.blocks.len().saturating_sub(1)),
                0,
            ));
        }
        removed_any
    }

    /// Align: in GFM alignment is a **column** property of the separator
    /// row (per-cell alignment cannot be expressed in the syntax) — so it applies to
    /// every Td in the focus cell's column (a selection crossing cells does not widen
    /// it).
    /// `a` uses the same encoding as Td.align: 0=default 1=left 2=center 3=right
    /// (clamped to 3 when out of range).
    pub(crate) fn table_align(&mut self, t: &TableCtx, focus_block: usize, a: u8) -> bool {
        let a = a.min(3);
        // The focus cell's column ordinal (0-based: the cells before the focus in this
        // row, same as focus_col_ord)
        let col = self.focus_col_ord(t.start, focus_block, t.row, t.seq);
        let mut changed = false;
        let mut cur_row = usize::MAX;
        let mut cur_col = 0;
        for b in self.doc.blocks[t.start..t.end].iter_mut() {
            if let Block::Td {
                table_seq: s,
                row: r,
                align,
                ..
            } = b.core_mut()
            {
                if *s != t.seq {
                    continue;
                }
                if *r != cur_row {
                    cur_row = *r;
                    cur_col = 0;
                }
                if cur_col == col && *align != a {
                    *align = a;
                    changed = true;
                }
                cur_col += 1;
            }
        }
        changed
    }

    /// The focus cell's column ordinal (0-based: the cells before the focus in this
    /// row; looks through the quote shell and **excludes** the focus cell — including
    /// it would count as "the next column", and the last column would come out empty).
    /// Shared by InsertColAfter / DeleteCol / Align.
    pub(crate) fn focus_col_ord(
        &self,
        start: usize,
        focus_block: usize,
        row: usize,
        seq: u64,
    ) -> usize {
        let mut col = 0;
        for b in &self.doc.blocks[start..focus_block] {
            // Only this table's cells count: a neighbouring table (same physical run)
            // shares the row numbers and would inflate the ordinal.
            if let Block::Td {
                table_seq, row: r, ..
            } = b.core()
            {
                if *table_seq == seq && *r == row {
                    col += 1;
                }
            }
        }
        col
    }

    /// The new empty cell (inherits the quote depth). Shared by InsertRowAfter /
    /// InsertColAfter / CellNext.
    pub(crate) fn new_cell(seq: u64, row: usize, align: u8, depth: u8) -> Block {
        wrap_quote(
            depth,
            Block::Td {
                table_seq: seq,
                row,
                align,
                inlines: vec![],
            },
        )
    }

    /// Select the whole cell's content: an empty cell collapses to the cell start.
    /// The next keystroke replaces the whole cell — matching the table Tab experience
    /// of Excel/Word.
    pub(crate) fn select_cell_contents(&mut self, idx: usize) {
        let len = self.doc.blocks.get(idx).map_or(0, |b| b.plain_text().len());
        self.sel = Sel {
            anchor: Pos::new(idx, 0),
            focus: Pos::new(idx, len),
        };
    }
}

// ----------------------------------------------------------------------
// Free functions
// ----------------------------------------------------------------------

/// Row parsing for table auto-completion: returns the **plain-text byte ranges**
/// of each cell's content (leading and trailing whitespace trimmed). Requires the
/// whole line to be a table row wrapped in a matched pair of pipes (`| a | b |`,
/// at least two cells; `\|` counts as an ordinary character). Ranges are returned
/// rather than strings because the header cells must keep their structure —
/// `**bold**`, inline code and so on — so the original inline tree has to be cut
/// by range.
pub(crate) fn table_cell_ranges(text: &str) -> Option<Vec<(usize, usize)>> {
    let mut pipes = Vec::new();
    let mut esc = false;
    for (i, ch) in text.char_indices() {
        if esc {
            esc = false;
        } else if ch == '\\' {
            esc = true;
        } else if ch == '|' {
            pipes.push(i);
        }
    }
    let (&first, &last) = (pipes.first()?, pipes.last()?);
    if !text[..first].trim().is_empty() || !text[last + 1..].trim().is_empty() {
        return None; // there is still content around the pipes: not a whole-line table
    }
    if pipes.len() < 3 {
        return None; // `||` and friends: fewer than two cells
    }
    let mut cells = Vec::with_capacity(pipes.len() - 1);
    for w in pipes.windows(2) {
        let (s, e) = (w[0] + 1, w[1]);
        let raw = &text[s..e];
        let lead = raw.len() - raw.trim_start().len();
        let trail = raw.len() - raw.trim_end().len();
        cells.push((s + lead, e - trail));
    }
    Some(cells)
}

/// Vertical movement between table cells (↑/↓ use ArrowUp/Down semantics, moving
/// by column across rows): the target is the cell in the row above or below in the
/// same column, and the caret lands clamped by **byte column** (the visual column
/// is the ui layer's `h_pos` geometry path; this is the pure-logic fallback).
/// Crossing the table boundary leaves it: the first row moves up to the end of the
/// block before the table, the last row moves down to the start of the next block
/// (staying put if there is no block outside). Returns `None` when the focus is
/// not in a Td (the caller uses the generic cross-block fallback). Rows are
/// grouped by **physical adjacency** (both the parser and the editing commands
/// keep `row` physically ordered).
pub fn table_cell_vertical(doc: &Doc, p: Pos, up: bool) -> Option<Pos> {
    // `core()` penetrates the quote shell: a quoted table stores
    // `Quoted { inner: Td }`, and matching the bare block sent ↑/↓ in a quoted
    // table down the generic cross-block fallback (which hops to the next cell
    // horizontally, since cells are physically adjacent).
    let (seq, row) = match doc.blocks.get(p.block).map(Block::core)? {
        Block::Td { table_seq, row, .. } => (*table_seq, *row),
        _ => return None,
    };
    // The table's block range: the contiguous run of Td with the same seq
    let start = (0..=p.block)
        .rev()
        .find(|&i| doc.blocks[i].table_seq() != Some(seq))
        .map_or(0, |i| i + 1);
    let end = (start..doc.blocks.len())
        .find(|&i| doc.blocks[i].table_seq() != Some(seq))
        .unwrap_or(doc.blocks.len());
    let row_of = |i: usize| match doc.blocks[i].core() {
        Block::Td { row, .. } => *row,
        _ => usize::MAX,
    };
    // The focus's column ordinal (which cell in this row, 0-based)
    let col = (start..p.block).filter(|&i| row_of(i) == row).count();
    // The current row's run [rs..re)
    let rs = (start..=p.block)
        .rev()
        .find(|&i| row_of(i) != row)
        .map_or(start, |i| i + 1);
    let re = (p.block..end).find(|&i| row_of(i) != row).unwrap_or(end);
    // The target row's run [ts..te)
    let (ts, te) = if up {
        if rs == start {
            // Past the first row: leave the table and land at the end of the preceding block
            return Some(match start.checked_sub(1) {
                Some(prev) => Pos::new(prev, doc.blocks[prev].plain_text().len()),
                None => p,
            });
        }
        let tv = row_of(rs - 1);
        let ts = (start..rs)
            .rev()
            .find(|&i| row_of(i) != tv)
            .map_or(start, |i| i + 1);
        (ts, rs)
    } else {
        if re >= end {
            // Past the last row: leave the table and land at the start of the following block
            return Some(if end < doc.blocks.len() {
                Pos::new(end, 0)
            } else {
                p
            });
        }
        let tv = row_of(re);
        let te = (re..end).find(|&i| row_of(i) != tv).unwrap_or(end);
        (re, te)
    };
    // The target cell: the column ordinal clamps to the target row's cell count
    // (defence against ragged tables)
    let idx = ts + col.min(te.saturating_sub(ts).saturating_sub(1));
    // `p.off` is a byte boundary of the **source** cell, not of this one: clamp
    // it onto a character boundary of the target's text (moving down from
    // "abcd" at off 1 into "中中" landed the caret inside the first character),
    // then out of any atom — landing "inside" a formula's source made the next
    // keystroke split it in two.
    let plain = doc.blocks[idx].plain_text();
    let off = crate::model::floor_boundary(&plain, p.off.min(plain.len()));
    Some(Pos::new(idx, atom_floor_block(&doc.blocks[idx], off)))
}
