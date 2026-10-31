//! Caret motion and text extraction (plain and Markdown).

use super::inlines::*;
use super::motion::*;
use super::table::*;
use super::*;

impl Editor {
    /// Markdown source for the current selection (copy path).
    ///
    /// Whole blocks keep their structure — tables, lists, formulas and code
    /// fences survive a copy instead of collapsing into plain text. A partial
    /// block is re-emitted from its inline tree, so `**bold**`, `` `code` `` and
    /// `$x$` keep their markup.
    pub fn selected_markdown(&self) -> String {
        let (start, end) = self.sel.range();
        if self.sel.collapsed() {
            return String::new();
        }
        let last = self.doc.blocks.len().saturating_sub(1);
        let sb = start.block.min(last);
        let eb = end.block.min(last);
        let mut out: Vec<Block> = Vec::new();
        if sb == eb {
            let b = &self.doc.blocks[sb];
            // A range that stays **inside** one atom whose source is on screen copies
            // those bytes. Re-emitting the truncated node would hand the clipboard a
            // broken formula (`$+y$`) or a broken image (`![MG](u)`) — the user
            // selected text, and the text is what they get.
            if let Some((s, e)) = self.reveal().editing_span(sb) {
                if s <= start.off && end.off <= e {
                    let plain = b.plain_text();
                    let (a, z) = clamp_span(&plain, start.off, end.off);
                    return plain[a..z].to_string();
                }
            }
            let inl = block_src_inlines(b);
            let len = b_plain(&inl).len();
            if start.off == 0 && end.off >= len {
                out.push(b.clone());
            } else {
                out.push(Block::Para(sub_inlines(&inl, &(start.off..end.off))));
            }
        } else {
            let first = &self.doc.blocks[sb];
            let fi = block_src_inlines(first);
            let flen = b_plain(&fi).len();
            if start.off == 0 {
                out.push(first.clone());
            } else {
                out.push(Block::Para(sub_inlines(&fi, &(start.off.min(flen)..flen))));
            }
            out.extend(self.doc.blocks[sb + 1..eb].iter().cloned());
            let tail = &self.doc.blocks[eb];
            let ti = block_src_inlines(tail);
            let tlen = b_plain(&ti).len();
            if end.off >= tlen {
                out.push(tail.clone());
            } else {
                out.push(Block::Para(sub_inlines(&ti, &(0..end.off.min(tlen)))));
            }
        }
        if out.is_empty() || out.iter().all(|b| b.plain_text().is_empty()) {
            // Nothing structural to emit (e.g. the selection covered only a block
            // separator): fall back to the plain text so the clipboard is not empty.
            return self.selected_text();
        }
        crate::md::emit(&Doc::new(out))
            .trim_end_matches('\n')
            .to_string()
    }

    /// Plain text covered by the selection (used for copy; blocks are joined with
    /// \n\n — Markdown semantics).
    pub fn selected_text(&self) -> String {
        let (start, end) = self.sel.range();
        // Copy must survive a stale selection: a drag anchor can sit mid-character
        // (CJK/emoji) or on a block index the document no longer has, and slicing
        // there used to panic while copying.
        let doc_len = self.doc.blocks.len();
        let sb = start.block.min(doc_len.saturating_sub(1));
        let eb = end.block.min(doc_len.saturating_sub(1));
        let Some(first) = self.doc.blocks.get(sb) else {
            return String::new();
        };
        if sb == eb {
            let p = first.plain_text();
            let (a, b) = clamp_span(&p, start.off, end.off);
            return p[a..b].to_string();
        }
        let mut out = {
            let p = first.plain_text();
            let (a, _) = clamp_span(&p, start.off, p.len());
            p[a..].to_string()
        };
        for b in self.doc.blocks.get(sb + 1..eb).unwrap_or(&[]) {
            out.push_str("\n\n");
            out.push_str(&b.plain_text());
        }
        out.push_str("\n\n");
        if let Some(last) = self.doc.blocks.get(eb) {
            let p = last.plain_text();
            let (_, b) = clamp_span(&p, 0, end.off);
            out.push_str(&p[..b]);
        }
        out
    }

    // ------------------------------------------------------------------
    // Slicing the inline tree (the core algorithm, in the same order as plain_text)
    // ------------------------------------------------------------------

    pub(crate) fn move_caret(&mut self, m: Move, extend: bool) -> bool {
        // A horizontal move with a live selection collapses it to the matching edge
        // instead of stepping on from the focus (browser semantics). Without this, ←
        // on a backwards drag kept the anchor and jumped one character left of the
        // focus, throwing the selection away from the end the user was looking at;
        // → from the same state left the caret one past the selection.
        // The vertical fallbacks are left alone: inside a table they are the
        // Excel-like cell/column navigation, and a Tab-selected cell must survive
        // the first ↓.
        if !extend && !self.sel.collapsed() {
            let backward = matches!(m, Move::Left | Move::WordLeft);
            let forward = matches!(m, Move::Right | Move::WordRight);
            if backward || forward {
                let (start, end) = self.sel.range();
                self.sel = Sel::caret(if backward { start } else { end });
                return true;
            }
        }
        let focus = self.sel.focus;
        // The formula whose source is on screen is text, not an atom (its interior
        // offsets are real caret positions).
        let edit = self.reveal().editing_span(focus.block);
        let doc = &self.doc;
        let mut new = match m {
            Move::Left => move_left(doc, focus, edit),
            Move::Right => move_right(doc, focus, edit),
            Move::WordLeft => move_word_left(doc, focus, edit),
            Move::WordRight => move_word_right(doc, focus, edit),
            Move::BlockStart => Pos::new(focus.block, 0),
            Move::BlockEnd => Pos::new(
                focus.block,
                doc.blocks
                    .get(focus.block)
                    .map_or(0, |b| b.plain_text().len()),
            ),
            Move::DocStart => Pos::new(0, 0),
            Move::DocEnd => {
                let last = doc.blocks.len().saturating_sub(1);
                Pos::new(
                    last,
                    doc.blocks.get(last).map_or(0, |b| b.plain_text().len()),
                )
            }
            // The pure-logic fallback for cross-block Up/Down: Td gets its own case — move
            // by column (ArrowUp/Down semantics) rather than "end of the previous block /
            // start of the next one" (adjacent blocks in the same table are the previous
            // cell in the same row, so column semantics would be lost).
            Move::PrevBlockEnd if doc.blocks.get(focus.block).is_some_and(|b| b.is_td()) => {
                table_cell_vertical(doc, focus, true).unwrap_or(focus)
            }
            Move::NextBlockStart if doc.blocks.get(focus.block).is_some_and(|b| b.is_td()) => {
                table_cell_vertical(doc, focus, false).unwrap_or(focus)
            }
            Move::PrevBlockEnd => {
                if focus.block == 0 {
                    // No previous block: moving "to the previous block's end"
                    // inside block 0 would be a *forward* jump to its own end.
                    return false;
                }
                Pos::new(
                    focus.block - 1,
                    doc.blocks
                        .get(focus.block - 1)
                        .map_or(0, |b| b.plain_text().len()),
                )
            }
            Move::NextBlockStart => {
                if focus.block + 1 >= doc.blocks.len() {
                    // Last block: there is no next block; the old `min(len-1)`
                    // wrapped the caret back to the last block's start.
                    return false;
                }
                Pos::new(focus.block + 1, 0)
            }
        };
        // A table that **starts or ends** the document has no neighbouring block,
        // so ←/→ at its first/last cell could never leave it — the caret stayed
        // inside the table. A word processor always keeps a paragraph beside a
        // table, and the arrows must be able to cross the border (→ at the last
        // cell lands after the table, ← at the first cell before it). Add that
        // paragraph on demand: this is the one `Move` branch that edits the
        // document (like Tab's last-cell row append, it takes its own undo
        // snapshot). Shift+←/→ only extends a selection and never edits.
        let mut inserted = false;
        if new == focus && !extend {
            if let Some(p) = self.table_edge_paragraph(m, focus) {
                new = p;
                inserted = true;
            }
        }
        // `inserted` matters for the **leading** table: the caret stays at
        // (0, 0) (the new paragraph takes that address), so the "nothing moved"
        // test would report a no-op even though the document changed.
        if new == focus && !inserted && (extend || self.sel.collapsed()) {
            return false;
        }
        self.sel.focus = new;
        if !extend {
            self.sel.anchor = new;
        }
        true
    }

    /// The paragraph a table needs so the caret can leave it at the document
    /// edge (see the call site in [`Self::move_caret`]).
    ///
    /// `→` at the end of the document's **last** cell appends it; `←` at the
    /// start of the **first** cell inserts it before the table. Returns the
    /// caret position on the other side of the border, or `None` when the caret
    /// is not on a table edge or a block is already there (plain motion handles
    /// that case and the caller keeps its result).
    fn table_edge_paragraph(&mut self, m: Move, p: Pos) -> Option<Pos> {
        let block = self.doc.blocks.get(p.block)?;
        match m {
            Move::Right
                if block.is_td()
                    && p.off == block.plain_len()
                    && p.block + 1 == self.doc.blocks.len() =>
            {
                self.push_undo(false);
                self.doc.blocks.push(Block::Para(Vec::new()));
                Some(Pos::new(p.block + 1, 0))
            }
            Move::Left if p.block == 0 && p.off == 0 && block.is_td() => {
                self.push_undo(false);
                self.doc.blocks.insert(0, Block::Para(Vec::new()));
                Some(Pos::new(0, 0))
            }
            _ => None,
        }
    }

    pub(crate) fn select_all(&mut self) -> bool {
        let last = self.doc.blocks.len().saturating_sub(1);
        let end_off = self
            .doc
            .blocks
            .get(last)
            .map_or(0, |b| b.plain_text().len());
        self.sel = Sel {
            anchor: Pos::new(0, 0),
            focus: Pos::new(last, end_off),
        };
        true
    }

    /// Double-click selects a word: extend from the focus to word boundaries on both
    /// sides (a run of alphanumeric / CJK characters).
    /// Triple-click selects the block: the whole focused block (block start → block
    /// end). An empty block is a no-op.
    pub(crate) fn select_block(&mut self) -> bool {
        let b = self.sel.focus.block;
        let Some(block) = self.doc.blocks.get(b) else {
            return false;
        };
        let len = block.plain_text().len();
        if len == 0 {
            return false;
        }
        self.sel = Sel {
            anchor: Pos::new(b, 0),
            focus: Pos::new(b, len),
        };
        false
    }

    pub(crate) fn select_word(&mut self) -> bool {
        let p = self.sel.focus;
        let Some(b) = self.doc.blocks.get(p.block) else {
            return false;
        };
        let plain = b.plain_text();
        if plain.is_empty() {
            return false;
        }
        let off = p.off.min(plain.len());
        let bytes = plain.as_bytes();
        // Extend on both sides of the click point: a run classified by character kind
        // (alphanumeric / CJK / other) counts as a word
        let cls = |c: char| {
            if c.is_alphanumeric() {
                if c.is_ascii() {
                    1
                } else {
                    2
                }
            } else {
                3
            }
        };
        let char_at = |i: usize| plain[i..].chars().next();
        // Find the character containing or adjacent to off
        let mut start = off;
        while start > 0 && !plain.is_char_boundary(start) {
            start -= 1;
        }
        // Double-click at the end of a block (off == len) or on whitespace: by
        // convention select the **next word**; when there is no next word at the block
        // end, fall back to the last word (a double-click at the block end used to do
        // nothing, and on whitespace it selected a "punctuation + whitespace" run).
        let mut cur_char = char_at(start);
        if off >= plain.len() || cur_char.is_some_and(|c| c.is_whitespace()) {
            let mut i = start;
            while i < plain.len() && char_at(i).is_some_and(|c| c.is_whitespace()) {
                i += char_at(i).map_or(0, |c| c.len_utf8());
            }
            if i < plain.len() {
                start = i;
            } else {
                // No next word: fall back to the last word (skipping trailing whitespace first)
                let mut j = plain.len();
                while j > 0 && char_at(prev_boundary(&plain, j)).is_some_and(|c| c.is_whitespace())
                {
                    j = prev_boundary(&plain, j);
                }
                if j == 0 {
                    return false; // The whole block is whitespace: no word to select
                }
                start = prev_boundary(&plain, j);
            }
            cur_char = char_at(start);
        }
        let Some(cur_char) = cur_char else {
            return false;
        };
        let cur_cls = cls(cur_char);
        let mut lo = start;
        while lo > 0 {
            let prev = prev_boundary(&plain, lo);
            match char_at(prev) {
                Some(c) if cls(c) == cur_cls => lo = prev,
                _ => break,
            }
        }
        let mut hi = start + cur_char.len_utf8();
        while hi < bytes.len() {
            match char_at(hi) {
                Some(c) if cls(c) == cur_cls => hi += c.len_utf8(),
                _ => break,
            }
        }
        self.sel = Sel {
            anchor: Pos::new(p.block, lo),
            focus: Pos::new(p.block, hi),
        };
        // Pure selection commands do not edit the document (apply's return contract is
        // "did it change")
        false
    }
}
