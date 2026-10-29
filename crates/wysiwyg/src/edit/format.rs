//! Formatting commands: inline wrap/link/image, heading, list, quote, task,
//! code block, and indentation.

use super::inlines::*;
use super::*;

impl Editor {
    pub(crate) fn wrap_selection(&mut self, kind: WrapKind) -> bool {
        let (start, end) = self.sel.range();
        if start.block != end.block {
            return false; // Cross-block wrapping is not supported yet (the toolbar mainly handles single-block selections)
        }
        let Some(b) = self.doc.blocks.get_mut(start.block) else {
            return false;
        };
        if matches!(
            b.core(),
            Block::Code { .. } | Block::Html { .. } | Block::Hr
        ) {
            return false;
        }
        if start.off == end.off {
            // Empty selection (caret only): pressing B yields a `**` pair with the caret
            // between them, and the next typed character goes inside the container
            // (`**X**`). The model side inserts an **empty container placeholder** and
            // leaves the caret at its original offset — `inlines_insert`'s "empty-container
            // descent" then sends the next character into the container.
            // **Do not compact**: compact strips the empty container and the placeholder
            // disappears.
            let inl = take_inlines(b);
            let off = floor_boundary(&b_plain(&inl), start.off);
            let (head, tail) = split_inlines(&inl, off);
            let mut new_inl = head;
            new_inl.push(empty_wrapper(kind));
            new_inl.extend(tail);
            set_inlines(b, new_inl);
            self.sel = Sel::caret(Pos::new(start.block, off));
            return true;
        }
        let mut inl = take_inlines(b);
        // Snap the non-empty range the same way: the wrapped span must not cut a
        // multibyte character in half.
        let (a, z) = clamp_span(&b_plain(&inl), start.off, end.off);
        let start = Pos::new(start.block, a);
        let end = Pos::new(end.block, z);
        let ok = match kind {
            WrapKind::Bold => {
                Self::inlines_wrap(&mut inl, &(start.off..end.off), &|c| Inline::Strong(c))
            }
            WrapKind::Italic => {
                Self::inlines_wrap(&mut inl, &(start.off..end.off), &|c| Inline::Em(c))
            }
            WrapKind::Del => {
                Self::inlines_wrap(&mut inl, &(start.off..end.off), &|c| Inline::Del(c))
            }
            WrapKind::Code => Self::inlines_wrap(&mut inl, &(start.off..end.off), &|c| {
                // A code wrap takes the plain text
                let mut t = String::new();
                for i in &c {
                    i.push_plain(&mut t);
                }
                Inline::Code(t)
            }),
        };
        set_inlines(b, inl);
        if ok {
            self.sel = Sel {
                anchor: start,
                focus: Pos::new(start.block, end.off),
            };
        }
        ok
    }

    pub(crate) fn wrap_link(&mut self, url: &str) -> bool {
        let (start, end) = self.sel.range();
        if start.block != end.block {
            return false;
        }
        let Some(b) = self.doc.blocks.get_mut(start.block) else {
            return false;
        };
        // Look through the quote shell (`wrap_selection`/`wrap_image` already do):
        // on `Quoted { inner: Code }` the bare match was false, so take_inlines
        // returned nothing and the caret still jumped as if a link had been made.
        if matches!(
            b.core(),
            Block::Code { .. } | Block::Hr | Block::Html { .. }
        ) {
            // An Html block is like Code: take_inlines/set_inlines are no-ops for it — an
            // empty selection used to silently drop "[url](url)" while the caret moved on
            return false;
        }
        let mut inl = take_inlines(b);
        // Snap the selection onto character boundaries before splitting (a collapsed
        // caret stays collapsed; a real selection takes the partially covered
        // characters whole).
        let plain = b_plain(&inl);
        let start = Pos::new(start.block, floor_boundary(&plain, start.off));
        let end = if start.off == end.off {
            start
        } else {
            Pos::new(end.block, ceil_boundary(&plain, end.off))
        };
        // Empty selection: construct an Inline::Link node directly and insert it (it
        // used to insert the literal text "[x](u)", which is not a link until reparsed)
        if start.off == end.off {
            let u = if url.is_empty() { "https://" } else { url };
            let (head, tail) = split_inlines(&inl, start.off);
            let mut new_inl = head;
            new_inl.push(Inline::Link {
                url: u.to_string(),
                inlines: vec![Inline::Text(u.to_string())],
            });
            new_inl.extend(tail);
            set_inlines(b, new_inl);
            self.sel = Sel::caret(Pos::new(start.block, start.off + u.len()));
            return true;
        }
        let u = if url.is_empty() {
            let plain = {
                let mut t = String::new();
                let sub = sub_inlines(&inl, &(start.off..end.off));
                for i in &sub {
                    i.push_plain(&mut t);
                }
                t
            };
            if plain.is_empty() {
                "https://".to_string()
            } else {
                plain
            }
        } else {
            url.to_string()
        };
        let ok = Self::inlines_wrap(&mut inl, &(start.off..end.off), &|c| Inline::Link {
            url: u.clone(),
            inlines: c,
        });
        set_inlines(b, inl);
        if ok {
            self.sel = Sel {
                anchor: start,
                focus: Pos::new(start.block, end.off),
            };
        }
        ok
    }

    /// Inline image insertion — a **structured** insertion, isomorphic to
    /// [`Self::wrap_link`].
    ///
    /// Empty selection: insert `Inline::Image { url, alt: "" }` at the caret. With a
    /// selection: the selection text becomes the alt and the selection is replaced by
    /// the image node.
    ///
    /// **Why not insert the literal `![](url)`** (the old implementation): that is a
    /// plain-text node, and emit runs it through `push_escaped_text`, which escapes
    /// the brackets into `!\[\](url)` — reparsing then gives literal text rather than
    /// an image (verified on a real machine: the toolbar produced something that was
    /// not an image).
    pub(crate) fn wrap_image(&mut self, url: &str) -> bool {
        let (start, end) = self.sel.range();
        if start.block != end.block {
            return false;
        }
        let Some(b) = self.doc.blocks.get_mut(start.block) else {
            return false;
        };
        if matches!(
            b.core(),
            Block::Code { .. } | Block::Html { .. } | Block::Hr
        ) {
            return false;
        }
        let u = if url.is_empty() { "https://" } else { url };
        let inl = take_inlines(b);
        // Same boundary normalization as wrap_link: a collapsed caret only needs
        // flooring, a real selection takes the partially covered characters whole.
        let plain = b_plain(&inl);
        let start = Pos::new(start.block, floor_boundary(&plain, start.off));
        let end = if start.off == end.off {
            start
        } else {
            Pos::new(end.block, ceil_boundary(&plain, end.off))
        };
        let (alt, head, tail) = if start.off == end.off {
            (
                String::new(),
                split_inlines(&inl, start.off).0,
                split_inlines(&inl, start.off).1,
            )
        } else {
            let sub = sub_inlines(&inl, &(start.off..end.off));
            let mut t = String::new();
            for i in &sub {
                i.push_plain(&mut t);
            }
            let head = split_inlines(&inl, start.off).0;
            let tail = split_inlines(&inl, end.off).1;
            (t, head, tail)
        };
        let mut new_inl = head;
        new_inl.push(Inline::Image {
            url: u.to_string(),
            alt: alt.clone(),
        });
        new_inl.extend(tail);
        set_inlines(b, new_inl);
        self.sel = Sel::caret(Pos::new(start.block, start.off + alt.len()));
        true
    }

    pub(crate) fn set_head(&mut self, level: u8) -> bool {
        let p = self.sel.focus;
        let Some(b) = self.doc.blocks.get_mut(p.block) else {
            return false;
        };
        let level = level.clamp(1, 6);
        // Decide on the *core* block and rebuild inside the quote shell.
        // Assigning to the outer block replaced `Quoted` wholesale, so turning
        // a paragraph inside a quote into a heading dropped the `>` prefix —
        // one open+save destroyed the quote structure.
        if matches!(
            b.core(),
            Block::Code { .. } | Block::Hr | Block::Td { .. } | Block::Html { .. }
        ) {
            // Html is like Code: take_inlines is a no-op for it, so raising the
            // heading would replace the whole HTML source with an empty heading
            return false;
        }
        let depth = b.quote_depth();
        let core = std::mem::replace(b.core_mut(), Block::Para(Vec::new()));
        let new_core = match core {
            // Same level again: drop the heading, keep the text
            Block::Head { level: l, inlines } if l == level => Block::Para(inlines),
            Block::Head { inlines, .. } => Block::Head { level, inlines },
            other => {
                let mut other = other;
                let inl = take_inlines(&mut other);
                Block::Head {
                    level,
                    inlines: inl,
                }
            }
        };
        *b = wrap_quote(depth, new_core);
        true
    }

    pub(crate) fn toggle_list(&mut self, ordered: bool) -> bool {
        let p = self.sel.focus;
        // Ordered: look at the neighbouring items to decide the starting number (read
        // before write, to avoid a borrow conflict; looks through the quote shell)
        let num = if ordered {
            match self
                .doc
                .blocks
                .get(p.block.wrapping_sub(1))
                .map(|b| b.core())
            {
                Some(Block::Li {
                    ordered: true, num, ..
                }) if p.block > 0 => num + 1,
                _ => 1,
            }
        } else {
            0
        };
        let Some(b) = self.doc.blocks.get_mut(p.block) else {
            return false;
        };
        // Looks through the quote shell: the decision reads the core and the shell is
        // kept — turning a paragraph inside a quote into a list works, and the new Li
        // inherits the same quote_depth (wrap_quote normalizes Quote/Quoted).
        let is_li = matches!(b.core(), Block::Li { .. });
        if !is_li
            && !matches!(
                b.core(),
                Block::Para(_) | Block::Head { .. } | Block::Quote(_)
            )
        {
            return false;
        }
        let depth = b.quote_depth();
        let inl = take_inlines(b);
        *b = wrap_quote(
            depth,
            if is_li {
                Block::Para(inl)
            } else {
                Block::Li {
                    ordered,
                    num,
                    level: 0,
                    task: None,
                    inlines: inl,
                }
            },
        );
        true
    }

    pub(crate) fn toggle_quote(&mut self) -> bool {
        let p = self.sel.focus;
        let Some(b) = self.doc.blocks.get_mut(p.block) else {
            return false;
        };
        match b {
            Block::Quote(_) => {
                let inl = take_inlines(b);
                *b = Block::Para(inl);
            }
            // Inside a quote shell = peel one level off (depth 1 means leaving the shell
            // entirely; the inner block is kept)
            Block::Quoted { .. } => {
                let depth = b.quote_depth();
                let old = std::mem::replace(b, Block::Para(vec![]));
                if let Block::Quoted { inner, .. } = old {
                    *b = wrap_quote(depth.saturating_sub(1), *inner);
                }
            }
            b if matches!(b, Block::Para(_) | Block::Head { .. }) => {
                let inl = take_inlines(b);
                *b = Block::Quote(inl);
            }
            _ => return false,
        }
        true
    }

    pub(crate) fn toggle_task(&mut self) -> bool {
        let p = self.sel.focus;
        let Some(b) = self.doc.blocks.get_mut(p.block) else {
            return false;
        };
        // Penetrate the quote shell, like toggle_list: `Quoted { inner: Li }`
        // renders a checkbox, so the command must reach it, and the new Li
        // inherits the original depth.
        if matches!(b.core(), Block::Li { .. }) {
            if let Block::Li { task, .. } = b.core_mut() {
                *task = match *task {
                    Some(c) => Some(!c),
                    None => Some(false),
                };
            }
            return true;
        }
        if !matches!(b.core(), Block::Para(_) | Block::Quote(_)) {
            return false;
        }
        let depth = b.quote_depth();
        let inl = take_inlines(b);
        *b = wrap_quote(
            depth,
            Block::Li {
                ordered: false,
                num: 0,
                level: 0,
                task: Some(false),
                inlines: inl,
            },
        );
        true
    }

    pub(crate) fn toggle_task_checked(&mut self, idx: usize) -> bool {
        let Some(b) = self.doc.blocks.get_mut(idx) else {
            return false;
        };
        // The render draws the checkbox for a quoted item too, so the click
        // target goes through the shell. A plain list item has no checkbox:
        // report "no change" instead of leaving a dirty flag and an undo step.
        match b.core_mut() {
            Block::Li { task: Some(c), .. } => {
                *c = !*c;
                true
            }
            _ => false,
        }
    }

    pub(crate) fn toggle_code_block(&mut self) -> bool {
        let p = self.sel.focus;
        let Some(b) = self.doc.blocks.get_mut(p.block) else {
            return false;
        };
        // Same shell handling as toggle_list: rebuild *inside* the quote
        // wrapper, otherwise `> ```…``` ` collapses to a top-level code block.
        let depth = b.quote_depth();
        let plain = b.core().plain_text();
        if matches!(b.core(), Block::Code { .. }) {
            *b = wrap_quote(depth, Block::Para(vec![Inline::Text(plain)]));
            return true;
        }
        if !matches!(
            b.core(),
            Block::Para(_) | Block::Quote(_) | Block::Head { .. }
        ) {
            return false;
        }
        *b = wrap_quote(
            depth,
            Block::Code {
                lang: None,
                text: plain,
            },
        );
        true
    }

    pub(crate) fn insert_hr(&mut self) -> bool {
        let p = self.sel.focus;
        let at = self.table_end_after(p.block).unwrap_or(p.block + 1);
        self.doc.blocks.insert(at, Block::Hr);
        self.doc.blocks.insert(at + 1, Block::Para(vec![]));
        self.sel = Sel::caret(Pos::new(at + 1, 0));
        true
    }

    /// List indent / outdent (Tab / Shift+Tab, toolbar indent/outdent).
    ///
    /// Feasibility is checked up front (an indent needs a preceding sibling at the
    /// same level, an outdent must not already be at the top), otherwise you get
    /// "the button does nothing, the text does not change, yet a dirty marker and an
    /// extra undo entry are left behind":
    /// * **Indent** requires a **preceding list item at the same level** (only a
    ///   preceding sibling can hang the item under it). The first item never indents
    ///   — emit takes indentation from the stack of preceding markers' widths, and an
    ///   orphan level cannot be expressed in markdown (it is lost on round trip).
    /// * **Outdent** does nothing at the top level (level 0).
    ///
    /// Quote-shell penetration: a list item inside a quote only changes `Li.level`;
    /// the `Quoted` shell is untouched.
    pub(crate) fn indent(&mut self, deeper: bool) -> bool {
        let p = self.sel.focus;
        // Code/HTML blocks: Tab inserts 4 spaces of indentation and does not take part
        // in list-indent semantics; Shift+Tab is a no-op.
        if matches!(
            self.doc.blocks.get(p.block).map(|b| b.core()),
            Some(Block::Code { .. } | Block::Html { .. })
        ) {
            return deeper && self.insert_text("    ", false);
        }
        let cur_level = match self.doc.blocks.get(p.block).map(|b| b.core()) {
            Some(Block::Li { level, .. }) => *level,
            _ => return false,
        };
        if deeper {
            if cur_level >= 6 {
                return false;
            }
            // The preceding sibling (a sibling in the same parent list): scanning the
            // contiguous Li run for the first item at the same level is enough; after
            // indenting, it becomes this item's parent (the preceding sibling).
            let has_prev_sibling = (0..p.block)
                .rev()
                .take_while(|&i| matches!(self.doc.blocks[i].core(), Block::Li { .. }))
                .any(|i| matches!(self.doc.blocks[i].core(), Block::Li { level, .. } if *level == cur_level));
            if !has_prev_sibling {
                return false;
            }
        } else if cur_level == 0 {
            return false;
        }
        let Some(Block::Li { level, .. }) = self.doc.blocks.get_mut(p.block).map(|b| b.core_mut())
        else {
            return false;
        };
        if deeper {
            *level = (*level + 1).min(6);
        } else {
            *level -= 1;
        }
        true
    }
}

// ----------------------------------------------------------------------
// Free functions
// ----------------------------------------------------------------------

// ----------------------------------------------------------------------
// Free functions and helpers
// ----------------------------------------------------------------------

/// After an ordered-list Enter continuation, bump the numbers of **same-level**
/// ordered items from `from` onwards by +1 (through the quote shell, so items
/// inside a quote renumber too). Stops at the first non-list block (the end of the
/// list); different levels and unordered items are left alone (a nested sublist
/// does not take part in the outer numbering).
pub(crate) fn bump_ordered_nums(blocks: &mut [Block], from: usize, level: u8) {
    // Take the length first: `blocks.len()` inside the slice expression would
    // contend with the later `iter_mut()` for the borrow
    let start = from.min(blocks.len());
    for b in blocks[start..].iter_mut() {
        match b.core_mut() {
            Block::Li {
                ordered: true,
                num: n,
                level: l,
                ..
            } if *l == level => *n += 1,
            Block::Li { .. } => {}
            _ => break,
        }
    }
}

pub(crate) fn head_level(b: &Block) -> u8 {
    match b {
        Block::Head { level, .. } => *level,
        _ => 1,
    }
}
