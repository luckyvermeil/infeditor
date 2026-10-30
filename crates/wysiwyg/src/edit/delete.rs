//! Deletion: selection, one character, one word.
//!
//! Backspace at a block start carries block semantics (exit the list, downgrade
//! the heading, merge into the previous block) — nothing is delegated to the
//! renderer.

use super::inlines::*;
use super::motion::*;
use super::*;

impl Editor {
    /// Delete the selection. Returns the new caret (the collapse point).
    pub(crate) fn delete_selection(&mut self) -> Option<Pos> {
        if self.sel.collapsed() {
            return None;
        }
        let (start, end) = self.sel.range();
        // A stale or UI-provided selection can point past the document; clamp the block
        // indices only. The offsets are clamped where they are used — the **end** goes
        // up (`clamp_span`/`ceil_boundary`), so a selection ending inside a character
        // takes the whole character instead of leaving half of it behind.
        let last = self.doc.blocks.len().saturating_sub(1);
        let start = Pos::new(start.block.min(last), start.off);
        let end = Pos::new(end.block.min(last), end.off);
        // A selection **inside** an atom's text (it is on screen, so it is text the
        // user is editing) deletes those characters, not the whole atom. For a
        // cross-block range the reveal never marks one.
        let editing = self.reveal().editing_span(start.block);
        if start.block == end.block {
            let b = &mut self.doc.blocks[start.block];
            // Code blocks are plain text (including those inside a quote shell); edit the
            // text directly
            if let Block::Code { text, .. } | Block::Html { text } = b.core_mut() {
                let (a, z) = clamp_span(text, start.off, end.off);
                text.replace_range(a..z, "");
            } else {
                // Go through inlines_delete to preserve the inline structure — with_plain
                // flattens the whole block into a single Text, and every Strong/Em/Link/Math
                // the selection never touched evaporates (character deletion
                // delete_back/delete_forward already takes this path; selection deletion used
                // to bypass it).
                let mut inl = take_inlines(b);
                Self::inlines_delete(&mut inl, &(start.off..end.off), editing);
                set_inlines(b, inl);
            }
            self.sel = Sel::caret(start);
            return Some(start);
        }
        // Cross-block: the first block keeps its prefix and the last keeps its
        // suffix, and **both ends go through inlines_delete to preserve inline
        // structure** (with_plain flattens the whole block into a single Text —
        // every Strong/Em/Link/Math/Image the selection never touched evaporates,
        // and that Markdown syntax is gone for good after saving). Merge into the
        // first block and delete everything in between.
        let mut head = self.doc.blocks[start.block].clone();
        let mut end_b = self.doc.blocks[end.block].clone();
        // First block: delete [start.off, block end), keeping the inline structure of
        // the prefix
        match head.core_mut() {
            Block::Code { text, .. } | Block::Html { text } => {
                text.truncate(floor_boundary(text, start.off));
            }
            b => {
                let mut inl = take_inlines(b);
                let block_len = b_plain(&inl).len();
                Self::inlines_delete(&mut inl, &(start.off.min(block_len)..block_len), editing);
                set_inlines(b, inl);
            }
        }
        // Last block: delete [0, end.off), keeping the inline structure of the suffix
        let tail_inl = match end_b.core_mut() {
            Block::Code { text, .. } | Block::Html { text } => {
                // Keep the suffix from the **next** character boundary: dropping a
                // partially covered multi-byte character would lose it for good.
                let rest = text.split_off(ceil_boundary(text, end.off));
                vec![Inline::Text(rest)]
            }
            b => {
                let mut inl = take_inlines(b);
                let block_len = b_plain(&inl).len();
                Self::inlines_delete(&mut inl, &(0..end.off.min(block_len)), editing);
                inl
            }
        };
        // Merge the last block's suffix into the first block: **structured merge** —
        // tail_inl goes straight into the first block's inline tree; push_plain /
        // concatenating plain text flattens Strong/Link/Image away.
        let tail_structured = tail_inl.iter().any(|i| !matches!(i, Inline::Text(_)));
        let src_text = match head.core() {
            Block::Code { text, .. } | Block::Html { text } => Some(text.clone()),
            _ => None,
        };
        let mut merged = head;
        if let Some(src_text) = src_text {
            if tail_structured {
                // A plain-text shell (Code/Html) cannot carry structured inlines: the shell
                // degrades to a Para with the prefix's source text + the structured tail —
                // the shell may be demoted, the structure must not be lost.
                let mut inl = Vec::new();
                if !src_text.is_empty() {
                    inl.push(Inline::Text(src_text));
                }
                inl.extend(tail_inl);
                merged = wrap_quote(merged.quote_depth(), Block::Para(inl));
            } else if let Block::Code { text, .. } | Block::Html { text } = merged.core_mut() {
                for i in &tail_inl {
                    i.push_plain(text);
                }
            }
        } else if matches!(merged.core(), Block::Hr) {
            // `Hr` carries no inlines and `take_inlines`/`set_inlines` are
            // no-ops on it, so the merge below would keep the rule and drop the
            // tail's text on the floor (selecting backwards over `---` ate the
            // whole following paragraph). Selecting across a rule deletes the
            // rule, so the first block degrades to the tail — the same
            // degradation as `Td`, quote depth kept.
            merged = wrap_quote(merged.quote_depth(), Block::Para(tail_inl));
        } else {
            let mut inl = take_inlines(&mut merged);
            inl.extend(tail_inl);
            set_inlines(&mut merged, inl);
        }
        // After Td is merged it is no longer a table (it collapses into a paragraph)
        // — but the inline structure must survive: it used to become
        // `Para(head_plain + tail_plain)` with Image/Strong/Link all evaporated.
        // The quote shell is kept: what collapses is the table shell, not the quote.
        let merged = if merged.is_td() {
            wrap_quote(merged.quote_depth(), Block::Para(take_inlines(&mut merged)))
        } else {
            merged
        };
        // Select-all delete: once the document empties it returns to an empty
        // paragraph — the first block's kind (Head/Li/quote shell) must not linger
        // (found on a real machine: after Ctrl+A+Delete the emit produced "# \n").
        let doc_len = self.doc.blocks.len();
        let end_len = self.doc.blocks[end.block].plain_text().len();
        let merged =
            if start.block == 0 && start.off == 0 && end.block == doc_len - 1 && end.off == end_len
            {
                Block::Para(vec![])
            } else {
                merged
            };
        self.doc.blocks.splice(start.block..=end.block, [merged]);
        self.sel = Sel::caret(start);
        Some(start)
    }

    pub(crate) fn delete_back(&mut self) -> bool {
        if !self.sel.collapsed() {
            return self.delete_selection().is_some();
        }
        let p = self.sel.focus;
        // The atom whose text is on screen (see [`crate::reveal`]): a delete inside
        // it takes single characters, not the whole atom. Read before the mutable
        // block borrow below.
        let editing = self.reveal().editing_span(p.block);
        // Byte boundary of the previous character in the plain text
        if p.off > 0 {
            let Some(b) = self.doc.blocks.get_mut(p.block) else {
                return false;
            };
            // Code and Html are isomorphic (plain_text = source text): delete the source
            // text (including inside a quote shell). Neither should take the take_inlines
            // path — it is a no-op for them, and after an empty delete the caret also jumps
            // to the block start.
            if let Block::Code { text, .. } | Block::Html { text } = b.core_mut() {
                let off = floor_boundary(text, p.off);
                let start = char_boundary_before(text, off);
                text.replace_range(start..off, "");
                self.sel = Sel::caret(Pos::new(p.block, start));
                return true;
            }
            let mut inl = take_inlines(b);
            let plain = b_plain(&inl);
            let off = floor_boundary(&plain, p.off);
            let start = char_boundary_before(&plain, off);
            Self::inlines_delete(&mut inl, &(start..off), editing);
            set_inlines(b, inl);
            self.sel = Sel::caret(Pos::new(p.block, start));
            return true;
        }
        // off == 0: block-level backspace. The first block has no "previous block" to
        // merge with, but the shell still peels off (list/quote/heading/code shells all
        // downgrade at off 0).
        if p.block == 0 {
            let cur = self.doc.blocks[0].clone();
            return match cur {
                Block::Li { .. } | Block::Quote(_) if cur.plain_text().is_empty() => {
                    self.doc.blocks[0] = Block::Para(vec![]);
                    true
                }
                Block::Li { inlines, .. } | Block::Quote(inlines) => {
                    self.doc.blocks[0] = Block::Para(inlines);
                    true
                }
                Block::Head { inlines, .. } => {
                    self.doc.blocks[0] = Block::Para(inlines);
                    true
                }
                Block::Code { text, .. } => {
                    self.doc.blocks[0] = Block::Para(vec![Inline::Text(text)]);
                    true
                }
                Block::Html { text } => {
                    self.doc.blocks[0] = Block::Para(vec![Inline::Text(text)]);
                    true
                }
                // Removing the only block would leave `blocks` empty; every caret
                // command indexes `blocks[focus.block]` and `Doc::new` guarantees
                // at least one, so a lone rule becomes an empty paragraph instead.
                Block::Hr if self.doc.blocks.len() == 1 => {
                    self.doc.blocks[0] = Block::Para(vec![]);
                    true
                }
                Block::Hr => {
                    self.doc.blocks.remove(0);
                    true
                }
                // Backspace at off 0 inside a quote shell peels one shell level off (the inner
                // block's kind is kept)
                Block::Quoted { depth, inner } => {
                    self.doc.blocks[0] = wrap_quote(depth.saturating_sub(1), *inner);
                    true
                }
                _ => false, // Para at off 0 in the first block: nothing to do
            };
        }
        let cur = self.doc.blocks[p.block].clone();
        match cur {
            Block::Hr => {
                self.doc.blocks.remove(p.block);
                self.sel = Sel::caret(Pos::new(p.block - 1, 0));
                true
            }
            Block::Code { .. } => {
                // Downgrade to a paragraph (Backspace at the start of a code block removes the
                // code shell)
                let text = cur.plain_text();
                self.doc.blocks[p.block] = Block::Para(vec![Inline::Text(text)]);
                true
            }
            Block::Html { text } => {
                // Same as Code: Backspace at the start of an HTML block removes the shell and
                // downgrades to a paragraph
                self.doc.blocks[p.block] = Block::Para(vec![Inline::Text(text)]);
                true
            }
            Block::Li { task, level, .. } if cur.plain_text().is_empty() => {
                // Empty list item → leave the list
                self.doc.blocks[p.block] = Block::Para(vec![]);
                let _ = (task, level);
                true
            }
            Block::Quote(_) if cur.plain_text().is_empty() => {
                self.doc.blocks[p.block] = Block::Para(vec![]);
                true
            }
            Block::Head { level, inlines } => {
                // An empty heading loses its shell and becomes a paragraph; Backspace at off 0
                // of a non-empty heading downgrades too
                let _ = level;
                self.doc.blocks[p.block] = Block::Para(inlines);
                true
            }
            Block::Li {
                ordered,
                num,
                level,
                task,
                inlines,
            } => {
                // Non-empty item at off 0: drop the list shell and become a paragraph
                let _ = (ordered, num, level, task);
                self.doc.blocks[p.block] = Block::Para(inlines);
                true
            }
            Block::Quote(inlines) => {
                self.doc.blocks[p.block] = Block::Para(inlines);
                true
            }
            Block::Td { .. } => {
                // Backspace at off 0 inside a table only moves the caret (to the previous cell
                // or the end of the previous block) and leaves the document alone — demoting
                // the cell to a Para would tear the table shell and collapse the table into a
                // ragged shape.
                self.sel = Sel::caret(move_left(&self.doc, p, editing));
                false
            }
            // Backspace at the block start inside a quote shell peels one shell level off
            // (the inner block is kept; the emitted `>` prefix follows the depth, so no
            // data is lost)
            Block::Quoted { depth, inner } => {
                self.doc.blocks[p.block] = wrap_quote(depth.saturating_sub(1), *inner);
                true
            }
            Block::Para(_) => {
                // Merge with the previous block
                let prev_idx = p.block - 1;
                match self.doc.blocks[prev_idx].clone() {
                    Block::Code { lang, text } => {
                        let cur_text = cur.plain_text();
                        let join = text.len();
                        self.doc.blocks[prev_idx] = Block::Code {
                            lang,
                            text: format!("{text}\n{cur_text}"),
                        };
                        self.doc.blocks.remove(p.block);
                        self.sel = Sel::caret(Pos::new(prev_idx, join));
                        true
                    }
                    Block::Html { text } => {
                        // Paragraph merged into an HTML block: the text is appended as a new source
                        // line (same merge semantics as Code)
                        let cur_text = cur.plain_text();
                        let join = text.len();
                        self.doc.blocks[prev_idx] = Block::Html {
                            text: format!("{text}\n{cur_text}"),
                        };
                        self.doc.blocks.remove(p.block);
                        self.sel = Sel::caret(Pos::new(prev_idx, join));
                        true
                    }
                    prev @ (Block::Li { .. } | Block::Quote(_)) => {
                        let join = prev.plain_text().len();
                        let mut prev = prev;
                        // Structured merge: does not go through plain_text — flattening would
                        // evaporate Strong/Link/Image into bare text
                        extend_block_inlines(&mut prev, &cur);
                        self.doc.blocks[prev_idx] = prev;
                        self.doc.blocks.remove(p.block);
                        self.sel = Sel::caret(Pos::new(prev_idx, join));
                        true
                    }
                    // A table cell keeps its shell, mirroring `delete_forward`'s table rule:
                    // merging the paragraph into the cell moved its text into the table body
                    // (and silently deleted the paragraph after the table). Park the caret at
                    // the cell's end instead; the next Backspace then edits inside the cell.
                    Block::Td { .. } => {
                        let end = self.doc.blocks[prev_idx].plain_len();
                        self.sel = Sel::caret(Pos::new(prev_idx, end));
                        false
                    }
                    Block::Hr => {
                        self.doc.blocks.remove(prev_idx);
                        self.sel = Sel::caret(Pos::new(prev_idx, 0));
                        true
                    }
                    prev_para => {
                        let join = prev_para.plain_text().len();
                        let mut prev = prev_para;
                        // Structured merge: does not go through plain_text
                        extend_block_inlines(&mut prev, &cur);
                        self.doc.blocks[prev_idx] = prev;
                        self.doc.blocks.remove(p.block);
                        self.sel = Sel::caret(Pos::new(prev_idx, join));
                        true
                    }
                }
            }
        }
    }

    pub(crate) fn delete_forward(&mut self, _word: bool) -> bool {
        if !self.sel.collapsed() {
            return self.delete_selection().is_some();
        }
        let p = self.sel.focus;
        // Same rule as delete_back: inside an atom whose text is on screen the
        // delete takes single characters.
        let editing = self.reveal().editing_span(p.block);
        let plain = self
            .doc
            .blocks
            .get(p.block)
            .map(|b| b.plain_text())
            .unwrap_or_default();
        let off = floor_boundary(&plain, p.off);
        if off < plain.len() {
            let end = char_boundary_after(&plain, off);
            let b = &mut self.doc.blocks[p.block];
            if let Block::Code { text, .. } | Block::Html { text } = b.core_mut() {
                let (a, z) = clamp_span(text, off, end);
                text.replace_range(a..z, "");
            } else {
                let mut inl = take_inlines(b);
                Self::inlines_delete(&mut inl, &(off..end), editing);
                set_inlines(b, inl);
            }
            return true;
        }
        // Block end: merge with the next block (mirrors delete_back's merge branch)
        if p.block + 1 >= self.doc.blocks.len() {
            return false;
        }
        let cur = self.doc.blocks[p.block].clone();
        let next = self.doc.blocks[p.block + 1].clone();
        // A rule is atomic: Delete on it removes the rule itself (the mirror of
        // delete_back's Hr arm). It used to reach the generic merge below, where
        // `extend_block_inlines` is a no-op for Hr — the following block was removed
        // together with its text (one keypress ate a whole paragraph).
        if matches!(cur.core(), Block::Hr) {
            self.doc.blocks.remove(p.block);
            self.sel = Sel::caret(Pos::new(p.block, 0));
            return true;
        }
        // Table cells keep their shell (the family rule): merging the next cell into
        // the current one deletes a cell and leaves the row ragged, and merging a
        // paragraph into a cell moves text into the table. Park the caret at the next
        // block's start instead — delete_back's Td arm does the same in the other
        // direction.
        if cur.is_td() || next.is_td() {
            self.sel = Sel::caret(Pos::new(p.block + 1, 0));
            return false;
        }
        // The decision looks at the next block's **core**: a shell-wrapped next block
        // (`> ```…``` ` = `Quoted { inner: Code }`) used to fall into the generic merge,
        // which dissolved the code fence into paragraph text.
        match next.core() {
            Block::Hr => {
                self.doc.blocks.remove(p.block + 1);
                true
            }
            Block::Code { lang, text } => {
                // The mirror of delete_back's "paragraph merged into a code block": the
                // current block's content joins the code block (appended below), keeping the
                // Code shell and the language tag. It used to flatten into
                // "paragraph + multi-line code span" — the language was lost and the code
                // block's meaning changed entirely.
                let cur = self.doc.blocks[p.block].clone();
                let join = cur.plain_text().len();
                let cur_text = cur.plain_text();
                let src = if cur_text.is_empty() {
                    text.clone()
                } else {
                    format!("{cur_text}\n{text}")
                };
                // The quote shell is kept: what merges is the code shell, and quote ownership
                // follows the original block's depth
                let depth = cur.quote_depth();
                self.doc.blocks[p.block] = wrap_quote(
                    depth,
                    match cur.core() {
                        Block::Code { lang: l, .. } => Block::Code {
                            lang: l.clone(),
                            text: src,
                        },
                        Block::Html { .. } => Block::Html { text: src },
                        _ => Block::Code {
                            lang: lang.clone(),
                            text: src,
                        },
                    },
                );
                self.doc.blocks.remove(p.block + 1);
                self.sel = Sel::caret(Pos::new(p.block, join));
                true
            }
            // Current block is a plain-text shell (Code/Html) and the next one is not:
            // the text joins as a **new source line**. The generic merge below used to
            // append it straight onto the last line ("fn a() {}next"), silently
            // deleting the line boundary — delete_back already joins with a newline in
            // the other direction.
            n if matches!(cur.core(), Block::Code { .. } | Block::Html { .. }) => {
                let join = cur.plain_text().len();
                let depth = cur.quote_depth();
                let next_text = n.plain_text();
                let joined = match cur.core() {
                    Block::Code { lang, text } => Block::Code {
                        lang: lang.clone(),
                        text: format!("{text}\n{next_text}"),
                    },
                    Block::Html { text } => Block::Html {
                        text: format!("{text}\n{next_text}"),
                    },
                    _ => unreachable!(),
                };
                self.doc.blocks[p.block] = wrap_quote(depth, joined);
                self.doc.blocks.remove(p.block + 1);
                self.sel = Sel::caret(Pos::new(p.block, join));
                true
            }
            _ => {
                let join = self.doc.blocks[p.block].plain_text().len();
                let mut cur = self.doc.blocks[p.block].clone();
                // Structured merge: does not go through plain_text — flattening would
                // evaporate Strong/Link/Image into bare text. The **whole** next block
                // is passed: block_src_inlines penetrates a quote shell.
                extend_block_inlines(&mut cur, &next);
                self.doc.blocks[p.block] = cur;
                self.doc.blocks.remove(p.block + 1);
                self.sel = Sel::caret(Pos::new(p.block, join));
                true
            }
        }
    }

    /// Ctrl+Backspace / Ctrl+Delete: turn the word-boundary move result into a
    /// selection and then delete it. Reuses [`move_word_left`]/[`move_word_right`]
    /// (including atomic skipping and crossing blocks to the end), so the
    /// delete/merge semantics are inherited from [`Self::delete_selection`].
    pub(crate) fn delete_word(&mut self, back: bool) -> bool {
        if !self.sel.collapsed() {
            return self.delete_selection().is_some();
        }
        let focus = self.sel.focus;
        // An atom being edited in place is ordinary text for word movement (its
        // text is on screen, so the atom-skipping would jump over what the user is
        // looking at).
        let edit = self.reveal().editing_span(focus.block);
        let target = if back {
            move_word_left(&self.doc, focus, edit)
        } else {
            move_word_right(&self.doc, focus, edit)
        };
        if target == focus {
            return false;
        }
        self.sel = Sel {
            anchor: target,
            focus,
        };
        self.delete_selection().is_some()
    }
}
