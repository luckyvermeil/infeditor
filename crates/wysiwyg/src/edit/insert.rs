//! Insertion: typed text, Enter, Shift+Enter.
//!
//! Typing is the only path that runs auto-completion; paste deliberately does
//! not (see `paste.rs`).

use super::autoformat::*;
use super::format::*;
use super::inlines::*;
use super::*;

impl Editor {
    /// `auto`: whether inline/block auto-completion is triggered. Typing
    /// (Cmd::InsertText) passes true; the paste path must pass false (pasting `- `
    /// or `# ` would otherwise become a list/heading).
    pub(crate) fn insert_text(&mut self, text: &str, auto: bool) -> bool {
        // Everything entering the model is normalized at this one entry point — the
        // Windows clipboard is routinely CRLF, the HTML parser normalizes \r\n to \n,
        // and without normalizing, the caret offsets drift gradually.
        let text = normalize_text(text);
        let text = text.as_ref();
        if text.is_empty() {
            return false;
        }
        self.delete_selection();
        let p = self.sel.focus;
        // The atom whose text is on screen right now (see [`crate::reveal`]): typing
        // strictly inside it edits that text (LaTeX source / image alt) instead of
        // splitting the atom. Taken **before** the mutable borrow of the block below.
        let editing = self.reveal().editing_span(p.block);
        // Code and HTML (including inside a quote shell) are plain-text shells:
        // take_inlines/set_inlines are no-ops for them, and falling into the fallback
        // branch silently drops characters (apply reports changed, the caret moves
        // forward, yet the typed text vanishes). Insert the source text instead.
        if let Some(Block::Code { text: code, .. } | Block::Html { text: code }) =
            self.doc.blocks.get_mut(p.block).map(|b| b.core_mut())
        {
            let off = floor_boundary(code, p.off);
            code.insert_str(off, text);
            self.sel = Sel::caret(Pos::new(p.block, off + text.len()));
            return true;
        }
        if self
            .doc
            .blocks
            .get(p.block)
            .is_some_and(|b| matches!(b.core(), Block::Hr))
        {
            // Hr is an atomic block that cannot take text: open a new paragraph after it to
            // receive the input (typing on a rule lands in the new paragraph); inside a
            // quote shell the depth is inherited.
            let d = self.doc.blocks[p.block].quote_depth();
            self.doc
                .blocks
                .insert(p.block + 1, wrap_quote(d, Block::Para(vec![])));
            self.sel = Sel::caret(Pos::new(p.block + 1, 0));
            return self.insert_text(text, auto);
        }
        let Some(b) = self.doc.blocks.get_mut(p.block) else {
            return false;
        };
        let mut inl = take_inlines(b);
        // Snap the caret offset onto a character boundary first: the insert position
        // and the caret that follows it must agree, and a mid-character offset would
        // otherwise be silently floored inside the inline tree.
        let off = floor_boundary(&b_plain(&inl), p.off);
        Editor::inlines_insert(&mut inl, off, text, editing);
        set_inlines(b, inl);
        let mut np = Pos::new(p.block, off + text.len());
        // ---- Block-level auto-completion ----
        // The paste path bypasses these: a pasted `- ` or `# ` is content, not input
        if auto {
            let plain = b.plain_text();
            if let Some((newblock, consumed)) = autoformat(b, &plain) {
                let _ = std::mem::replace(b, newblock);
                np = Pos::new(p.block, consumed);
            } else {
                // Inline math auto-completion: `$…$` → Inline::Math
                let mut inl = take_inlines(b);
                if let Some(new_off) = autoformat_math(&mut inl, np.off) {
                    set_inlines(b, inl);
                    np = Pos::new(p.block, new_off);
                } else {
                    set_inlines(b, inl);
                }
            }
        }
        self.sel = Sel::caret(np);
        true
    }

    pub(crate) fn enter(&mut self) -> bool {
        if !self.sel.collapsed() {
            self.delete_selection();
        }
        let p = self.sel.focus;
        let Some(b) = self.doc.blocks.get(p.block).cloned() else {
            return false;
        };
        // The caret can be stale or land mid-character; snap it before any split so a
        // multibyte character is never cut into both halves (head and tail).
        let p = Pos::new(p.block, floor_boundary(&b.plain_text(), p.off));
        match b {
            Block::Code { .. } => {
                // Enter inside a code block inserts a line break (a break within the block;
                // splitting into two Code blocks would make emit output duplicate fences).
                if let Block::Code { text, .. } = &mut self.doc.blocks[p.block] {
                    let off = floor_boundary(text, p.off);
                    text.insert(off, '\n');
                    self.sel = Sel::caret(Pos::new(p.block, off + 1));
                }
                true
            }
            Block::Html { .. } => {
                // Enter inside an HTML block: same as Code — a break within the block (a
                // multi-line source block, which emit writes back verbatim)
                if let Block::Html { text } = &mut self.doc.blocks[p.block] {
                    let off = floor_boundary(text, p.off);
                    text.insert(off, '\n');
                    self.sel = Sel::caret(Pos::new(p.block, off + 1));
                }
                true
            }
            Block::Hr => {
                self.doc.blocks.insert(p.block + 1, Block::Para(vec![]));
                self.sel = Sel::caret(Pos::new(p.block + 1, 0));
                true
            }
            // Enter inside a table cell inserts `<br>`: a cell is a single-line block, so
            // a line break can only be expressed as an HTML break; emit turns HardBreak
            // into `<br>` inside a table, and reparsing gives HardBreak back. It counts
            // as 1 byte in plain text.
            Block::Td { .. } => {
                if let Block::Td { inlines, .. } = &mut self.doc.blocks[p.block] {
                    let off = p.off.min(inl_total(inlines));
                    inlines_insert_break_node(inlines, off, Inline::HardBreak);
                    self.sel = Sel::caret(Pos::new(p.block, off + 1));
                    true
                } else {
                    false
                }
            }
            Block::Li {
                ordered,
                num,
                level,
                task,
                inlines,
            } => {
                let (head_inl, tail_inl) = split_inlines(&inlines, p.off);
                let head_empty = is_plain_empty(&head_inl);
                if head_empty {
                    // An empty item (including an empty task item) leaves the list
                    self.doc.blocks[p.block] = Block::Para(vec![]);
                    self.sel = Sel::caret(Pos::new(p.block, 0));
                    return true;
                }
                self.doc.blocks[p.block] = Block::Li {
                    ordered,
                    num,
                    level,
                    task,
                    inlines: head_inl,
                };
                let new_num = if ordered { num + 1 } else { 0 };
                // Following same-level ordered items get their number +1 (through the quote
                // shell, so items mixed into a quote renumber too); the scan starts at
                // p.block+1 — the new item is not inserted yet, so the next item is here.
                if ordered {
                    bump_ordered_nums(&mut self.doc.blocks, p.block + 1, level);
                }
                self.doc.blocks.insert(
                    p.block + 1,
                    Block::Li {
                        ordered,
                        num: new_num,
                        level,
                        // Task-list continuation: the new item starts unchecked
                        task: task.map(|_| false),
                        inlines: tail_inl,
                    },
                );
                self.sel = Sel::caret(Pos::new(p.block + 1, 0));
                true
            }
            Block::Quote(_) => {
                let mut cur = self.doc.blocks[p.block].clone();
                let (head_inl, tail_inl) = split_inlines(&take_inlines(&mut cur), p.off);
                let head_empty = is_plain_empty(&head_inl);
                if head_empty {
                    // Empty head: leave the quote shell first, then split as a paragraph — this
                    // used to produce "para\n> tail" or an orphan `> ` shell block.
                    self.doc.blocks[p.block] = Block::Para(head_inl);
                    if is_plain_empty(&tail_inl) {
                        self.sel = Sel::caret(Pos::new(p.block, 0));
                    } else {
                        self.doc.blocks.insert(p.block + 1, Block::Para(tail_inl));
                        self.sel = Sel::caret(Pos::new(p.block + 1, 0));
                    }
                    return true;
                }
                self.doc.blocks[p.block] = Block::Quote(head_inl);
                self.doc.blocks.insert(p.block + 1, Block::Quote(tail_inl));
                self.sel = Sel::caret(Pos::new(p.block + 1, 0));
                true
            }
            // Block-level Enter inside a quote shell — an empty block peels one level off
            // (the family rule), otherwise the quote continues at the same depth (both
            // halves stay inside the shell, and the new block inherits the neighbour's
            // depth).
            Block::Quoted { depth, inner } => {
                match &*inner {
                    // Code/HTML block: a break within the block (same Enter semantics as Code)
                    Block::Code { .. } | Block::Html { .. } => {
                        if let Block::Code { text, .. } | Block::Html { text } =
                            self.doc.blocks[p.block].core_mut()
                        {
                            let off = p.off.min(text.len());
                            text.insert(off, '\n');
                            self.sel = Sel::caret(Pos::new(p.block, off + 1));
                        }
                        return true;
                    }
                    // A table cell inside a quote shell behaves exactly like a
                    // top-level cell: Enter inserts a `<br>` (a cell is a
                    // single-line block). This used to be a silent no-op, so
                    // the same table behaved differently after being quoted.
                    Block::Td { .. } => {
                        if let Block::Td { inlines, .. } = self.doc.blocks[p.block].core_mut() {
                            let off = p.off.min(inl_total(inlines));
                            inlines_insert_break_node(inlines, off, Inline::HardBreak);
                            self.sel = Sel::caret(Pos::new(p.block, off + 1));
                            return true;
                        }
                        return false;
                    }
                    Block::Hr => {
                        // Open a new paragraph after the atomic block to receive the input,
                        // inheriting the quote depth
                        self.doc
                            .blocks
                            .insert(p.block + 1, wrap_quote(depth, Block::Para(vec![])));
                        self.sel = Sel::caret(Pos::new(p.block + 1, 0));
                        return true;
                    }
                    _ => {}
                }
                let mut core = (*inner).clone();
                let (head_inl, tail_inl) = split_inlines(&take_inlines(&mut core), p.off);
                if is_plain_empty(&head_inl) {
                    if matches!(core, Block::Li { .. }) {
                        // An empty list item drops the list shell but stays in the quote
                        self.doc.blocks[p.block] = wrap_quote(depth, Block::Para(vec![]));
                        self.sel = Sel::caret(Pos::new(p.block, 0));
                        return true;
                    }
                    // Empty head: peel one quote level, then split as a paragraph (depth 1 means
                    // leaving the shell entirely)
                    let d = depth.saturating_sub(1);
                    self.doc.blocks[p.block] = wrap_quote(d, Block::Para(head_inl));
                    if is_plain_empty(&tail_inl) {
                        self.sel = Sel::caret(Pos::new(p.block, 0));
                    } else {
                        self.doc
                            .blocks
                            .insert(p.block + 1, wrap_quote(d, Block::Para(tail_inl)));
                        self.sel = Sel::caret(Pos::new(p.block + 1, 0));
                    }
                    return true;
                }
                // Tail half: a list item continues with the same parameters (ordered +1, task
                // reset), everything else becomes a paragraph
                let (tail, cont_level) = match &core {
                    Block::Li {
                        ordered,
                        num,
                        level,
                        task,
                        ..
                    } => (
                        Block::Li {
                            ordered: *ordered,
                            num: if *ordered { num + 1 } else { 0 },
                            level: *level,
                            task: task.map(|_| false),
                            inlines: tail_inl,
                        },
                        // Ordered continuation: following same-level items renumber (through the shell)
                        if *ordered { Some(*level) } else { None },
                    ),
                    _ => (Block::Para(tail_inl), None),
                };
                if let Some(level) = cont_level {
                    // The new item p.block+1 is not inserted yet, so the following items are scanned
                    // from p.block+1
                    bump_ordered_nums(&mut self.doc.blocks, p.block + 1, level);
                }
                set_inlines(&mut core, head_inl);
                self.doc.blocks[p.block] = wrap_quote(depth, core);
                self.doc.blocks.insert(p.block + 1, wrap_quote(depth, tail));
                self.sel = Sel::caret(Pos::new(p.block + 1, 0));
                true
            }
            Block::Para(_) | Block::Head { .. } => {
                let is_head = matches!(b, Block::Head { .. });
                // Auto-completion: a whole line `| a | b |` + Enter becomes a table directly
                // (that line is the header, the body is one empty row, and the caret lands in
                // the body's first cell)
                if !is_head && self.table_autoformat(p.block) {
                    return true;
                }
                let (head_inl, tail_inl) = split_inlines(&take_inlines(&mut b.clone()), p.off);
                self.doc.blocks[p.block] = if is_head {
                    Block::Head {
                        level: head_level(&b),
                        inlines: head_inl,
                    }
                } else {
                    Block::Para(head_inl)
                };
                // Enter after a heading opens a new paragraph (Enter after H6 yields a paragraph)
                self.doc.blocks.insert(p.block + 1, Block::Para(tail_inl));
                self.sel = Sel::caret(Pos::new(p.block + 1, 0));
                true
            }
        }
    }

    /// Shift+Enter: soft break within a block — insert `Inline::SoftBreak`, **without
    /// splitting the block**. plain counts `"\n"` as one byte and the caret lands
    /// after the break (off+1; plain contains `\n`). Head/Td are single-line blocks
    /// (ATX headings and GFM cells have no in-block line-break semantics) and Hr is
    /// atomic, so those are no-ops (Enter's block-splitting semantics are unchanged).
    /// Code/HTML blocks behave like Enter: insert `\n` into the source.
    pub(crate) fn soft_break(&mut self) -> bool {
        if !self.sel.collapsed() {
            self.delete_selection();
        }
        let p = self.sel.focus;
        let Some(b) = self.doc.blocks.get_mut(p.block) else {
            return false;
        };
        // Same as Enter: snap the caret before splitting (a mid-character offset would
        // otherwise be cut into both halves). The match runs on the *core* block: a
        // quoted Code/Head/Hr (`Quoted { inner: … }`) used to fall into the inline
        // fallback, where take_inlines/set_inlines are no-ops — nothing was inserted
        // but the caret still jumped and the document was marked dirty.
        let p_off = floor_boundary(&b.plain_text(), p.off);
        match b.core_mut() {
            Block::Code { text, .. } | Block::Html { text } => {
                let off = floor_boundary(text, p.off);
                text.insert(off, '\n');
                self.sel = Sel::caret(Pos::new(p.block, off + 1));
                true
            }
            Block::Td { inlines, .. } => {
                let off = p_off.min(inl_total(inlines));
                inlines_insert_break_node(inlines, off, Inline::HardBreak);
                self.sel = Sel::caret(Pos::new(p.block, off + 1));
                true
            }
            Block::Head { .. } | Block::Hr => false,
            Block::Para(inl) | Block::Quote(inl) | Block::Li { inlines: inl, .. } => {
                let off = p_off.min(inl_total(inl));
                inlines_insert_break(inl, off);
                self.sel = Sel::caret(Pos::new(p.block, off + 1));
                true
            }
            _ => false,
        }
    }
}
