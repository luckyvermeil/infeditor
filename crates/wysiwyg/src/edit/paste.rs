//! Paste: the main entry point for clipboard text into the model.
//!
//! The Windows clipboard is routinely CRLF, so the text is normalised before it
//! reaches the inline tree.

use super::inlines::*;
use super::*;

impl Editor {
    pub(crate) fn paste(&mut self, text: &str) -> bool {
        // Paste is the main entry point for text into the model — the Windows clipboard
        // is routinely CRLF, so normalize first
        let text = normalize_text(text);
        let text = text.as_ref();
        if text.is_empty() {
            return false;
        }
        let p = {
            self.delete_selection();
            self.sel.focus
        };
        // Insert verbatim inside a code block (the paste path never touches autoformat).
        // The shell is penetrated: a quoted code block (`> ``` `) is `Quoted { inner: Code }`,
        // and matching the bare block missed it.
        if matches!(
            self.doc.blocks.get(p.block).map(Block::core),
            Some(Block::Code { .. })
        ) {
            return self.insert_text(text, false);
        }
        if !text.contains('\n') {
            return self.insert_text(text, false);
        }
        // Code/Html's plain_text is the source text and Hr is an atomic block:
        // take_inlines is a no-op for them, so taking the "split the focused block's head
        // and tail" path below would replace the focused block with an empty paragraph
        // (the Code content evaporates whole / the Hr is silently swapped). These three
        // kinds of focused blocks are all inserted as source instead (insert_text inserts
        // the source for Code/Html and opens a new paragraph for Hr).
        if matches!(
            self.doc.blocks.get(p.block).map(Block::core),
            Some(Block::Code { .. } | Block::Html { .. } | Block::Hr)
        ) {
            return self.insert_text(text, false);
        }
        // Multi-line: the block containing the caret keeps its first half, the whole text
        // is parsed to insert blocks, and the last half joins the final block
        let b = self.doc.blocks.get(p.block).cloned();
        let Some(b) = b else { return false };
        // The caret may be stale or land mid-character: snap it before splitting the
        // block in half, otherwise a CJK character would be duplicated into both halves.
        let split_off = floor_boundary(&b.plain_text(), p.off);
        let (head_inl, tail_inl) = split_inlines(&take_inlines(&mut b.clone()), split_off);
        // Shell rebuilding penetrates the quote shell: the inner block is rebuilt with
        // its original kind and quote ownership follows the original depth
        let rebuilt_core = if let Block::Head { level, .. } = b.core() {
            Block::Head {
                level: *level,
                inlines: head_inl,
            }
        } else if let Block::Td {
            table_seq,
            row,
            align,
            ..
        } = b.core()
        {
            Block::Td {
                table_seq: *table_seq,
                row: *row,
                align: *align,
                inlines: head_inl,
            }
        } else if matches!(b.core(), Block::Quote(_)) {
            Block::Quote(head_inl)
        } else {
            // Li / Para: a multi-line paste would scatter list items, so treat it as
            // paragraphs
            Block::Para(head_inl)
        };
        // wrap_quote takes a defensive path for a Quote that already has a shell and
        // keeps it as is (the quote_depth=1 shape)
        self.doc.blocks[p.block] = wrap_quote(b.quote_depth(), rebuilt_core);
        // parse the pasted text (trailing newline removed)
        let trimmed = text.trim_matches('\n');
        let parsed = crate::md::parse(trimmed);
        let mut last_pos = Pos::new(p.block, head_plain_len(&self.doc.blocks[p.block]));
        // The focus is inside a table: the pasted content must land **after the whole
        // table** — inserting it into the middle of a contiguous run of Td splits the
        // table in two (rendering and emit chunk by "contiguous same seq", so the second
        // chunk's first row is emitted as the header and the saved Markdown's row order
        // gets scrambled). Same clamping iron rule as insert_block/insert_hr/insert_table.
        let insert_at = self.table_end_after(p.block).unwrap_or(p.block + 1);
        if !parsed.blocks.is_empty() {
            let mut blocks = parsed.blocks;
            // The paste source is always the plain-text clipboard, and md::parse turns a
            // line starting with `<img …>` into an Html block, which the preview passes
            // through and executes. The paste path downgrades Html blocks to ordinary
            // paragraphs (the original markup stays as literal Text) — the meaning of an Html
            // block opened manually is unchanged. An Html block inside a quote shell
            // downgrades too; the shell is kept.
            for b in &mut blocks {
                if matches!(b.core(), Block::Html { .. }) {
                    let t = b.plain_text();
                    *b.core_mut() = Block::Para(vec![Inline::Text(t)]);
                }
            }
            // md::parse reallocates table_seq from 0, so it collides with the tables already
            // in the document and the two are treated as one (rendering and export chunk by
            // "contiguous same seq"). The pasted blocks' table seqs are all renumbered past
            // the document max (the same defence as insert_table).
            let used = self
                .doc
                .blocks
                .iter()
                .filter_map(|b| b.table_seq())
                .max()
                .map_or(0, |m| m + 1)
                .max(self.doc.next_table_seq);
            let mut seq_map: Vec<(u64, u64)> = Vec::new();
            let mut next_seq = used;
            for b in &mut blocks {
                if let Block::Td { table_seq, .. } = b.core_mut() {
                    let old = *table_seq;
                    let new = match seq_map.iter().find(|(o, _)| *o == old) {
                        Some((_, n)) => *n,
                        None => {
                            seq_map.push((old, next_seq));
                            next_seq += 1;
                            next_seq - 1
                        }
                    };
                    *table_seq = new;
                }
            }
            if next_seq > self.doc.next_table_seq {
                self.doc.next_table_seq = next_seq;
            }
            // The last block joins the tail; when that block is an Hr (an atomic block that
            // cannot carry text) the tail must not be lost — append a Para after the Hr to
            // receive it.
            let last = blocks.pop().unwrap_or(Block::Para(vec![]));
            if matches!(last.core(), Block::Hr) {
                // Hr (including inside a quote shell) is an atomic block that cannot carry text:
                // append a block after it to receive the text
                let d = last.quote_depth();
                blocks.push(last);
                if !tail_inl.is_empty() {
                    blocks.push(wrap_quote(d, Block::Para(tail_inl)));
                }
            } else {
                blocks.push(merge_inlines(last, tail_inl));
            }
            let last_idx = insert_at + blocks.len() - 1;
            self.doc.blocks.splice(insert_at..insert_at, blocks);
            let len = self
                .doc
                .blocks
                .get(last_idx)
                .map(|b| b.plain_text().len())
                .unwrap_or(0);
            last_pos = Pos::new(last_idx, len);
        } else {
            // Empty paste: restore the tail
            self.doc.blocks.insert(insert_at, Block::Para(tail_inl));
        }
        self.sel = Sel::caret(last_pos);
        true
    }

    // ------------------------------------------------------------------
    // Undo
    // ------------------------------------------------------------------
}
