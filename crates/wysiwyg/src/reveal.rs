//! "Show the Markdown source where the user is working."
//!
//! Two triggers, one mechanism:
//!
//! 1. **A selection that spans two or more blocks expands them whole.** Every
//!    block the sweep touches renders as its Markdown source: the inline markup
//!    (`**bold**`, `` `code` ``, `[text](url)`, `$x$`) *plus* the block prefix
//!    (`#`, `-`/`1.`, `> `, the code fence, the table's `|` and delimiter row,
//!    `---`). The unit is the **block**, not the visual line: a selection that
//!    stays inside one block expands nothing, however many lines that block
//!    wraps to (a paragraph written as several lines is still one block), and a
//!    table is one block for the reader. Picking a few words of a paragraph is a
//!    copy gesture, not a request to read that paragraph's source.
//! 2. **A formula at the caret expands on its own** — the LaTeX source replaces
//!    the typeset SVG on both sides of the caret and inside it, so the formula
//!    can be edited character by character without turning the whole block into
//!    source. A selection that stays **inside** one atom (shift+→ in a formula,
//!    a drag across an image's alt) keeps it expanded the same way: the
//!    highlight then covers readable source instead of an object it cannot
//!    show, and the edit layer deletes the covered characters rather than the
//!    whole atom.
//!
//! The expansion is a **view** concern only: no command writes the markers into
//! the document, and every offset the kernel uses is still a plain-text byte
//! offset. The render layer emits the markers as *virtual* spans
//! (`data-vmark`), which occupy layout space but zero plain bytes — see `geom`'s
//! virtual-byte ledger.
//!
//! ## It follows the pointer
//!
//! The value is derived from `sel` on every call and never stored, because the
//! expansion has to change **while** the selection grows: dragging across a
//! paragraph turns it into source the moment the pointer reaches it. That is only
//! safe because the drag is ours (blitz's native drag selection is switched off
//! with `user-select: none`, see `render::SHELL_CSS`): every pointer move is
//! re-mapped against the current layout, so a block rebuilt under the pointer
//! leaves no dangling anchor behind.
//!
//! [`Editor::selected_markdown`]: crate::edit::Editor::selected_markdown

use std::ops::Range;

use crate::edit::Editor;
use crate::model::{Block, Doc};

/// Which blocks — and which single atom — are shown as Markdown source.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Reveal {
    /// Blocks rendered as source because a selection touches them, ascending.
    /// The **whole** block is expanded: a partial selection is enough, because
    /// the unit is the block (a paragraph however many lines it wraps to, a
    /// code fence, a table).
    blocks: Vec<usize>,
    /// The atomic inline expanded on its own: `(block, start, end)`. It is the
    /// formula at the caret and the atom a selection stays inside (the two cases
    /// where the source is the text being worked on); the edit layer reads it to
    /// decide whose text may be edited in place (see `edit::inlines`).
    atom: Option<(usize, usize, usize)>,
}

impl Reveal {
    /// Nothing expanded (the size guard the render layer passes around).
    pub fn none() -> Self {
        Self::default()
    }

    pub fn is_empty(&self) -> bool {
        self.blocks.is_empty() && self.atom.is_none()
    }

    /// Is this whole block shown as source?
    pub fn contains(&self, block: usize) -> bool {
        self.blocks.binary_search(&block).is_ok()
    }

    /// The blocks shown as source (ascending).
    pub fn blocks(&self) -> &[usize] {
        &self.blocks
    }

    /// Is the atom `[range)` of `block` expanded on its own (the formula at the
    /// caret, or the atom a selection sits inside)? Zero-length nodes never
    /// expand.
    pub fn shows_atom(&self, block: usize, range: Range<usize>) -> bool {
        self.atom.is_some_and(|(b, s, e)| {
            b == block && range.start < range.end && range.start < e && s < range.end
        })
    }

    /// The block that holds the atom expanded **on its own** (the formula at the
    /// caret / the atom a selection stays inside), if any. At most one block can
    /// carry it, so this is the cheap test the render layer uses to keep that
    /// block's typeset box (the source and the typeset form are not the same
    /// height).
    pub fn atom_block(&self) -> Option<usize> {
        self.atom.map(|(b, _, _)| b)
    }

    /// The atomic inline whose text may be edited **in place** in `block`, in
    /// plain-byte offsets (`None` = none). A formula qualifies while its source
    /// is on screen — a caret at either edge (where the caret is drawn *inside*
    /// the delimiters, `$|x$` / `$x|$`) or a selection inside it. An image
    /// qualifies only once the range is strictly inside its alt (never at its
    /// edges — that would turn every caret next to an image into a source view).
    pub fn editing_span(&self, block: usize) -> Option<(usize, usize)> {
        self.atom.filter(|a| a.0 == block).map(|a| (a.1, a.2))
    }

    /// The reveal implied by `ed`'s selection, ignoring any frozen value.
    pub(crate) fn from_editor(ed: &Editor) -> Self {
        let doc = &ed.doc;
        let sel = ed.sel;
        let mut out = Reveal::default();
        if doc.blocks.is_empty() {
            return out;
        }
        let last = doc.blocks.len().saturating_sub(1);
        let (a, b) = sel.range();
        if ed.dragging {
            // The pointer, not the selected bytes, owns the unit. Blank space
            // at a block's start/end (or its whole right margin) still means
            // "this block"; otherwise the source expands only after the pointer
            // reaches a glyph and jumps back when it leaves the text run. The
            // **block** rule still holds: a drag that stays inside one block
            // expands nothing, however far down the pointer travels inside it.
            for blk in a.block.min(last)..=b.block.min(last) {
                out.blocks.push(blk);
            }
            if out.blocks.len() < 2 {
                out.blocks.clear();
            } else {
                expand_tables(doc, &mut out.blocks);
            }
            return out;
        }
        if !sel.collapsed() {
            // The unit is the block: any overlap expands the whole line. A block
            // with no plain text (an empty paragraph, a rule) has nothing to show,
            // and a selection ending exactly at a block's start does not include
            // that block.
            for blk in a.block..=b.block.min(last) {
                let (first, tail) = (blk == a.block, blk == b.block);
                // An interior block is inside the selection whatever its length (a
                // rule carries no bytes but its `---` does belong to the selected
                // source), and it needs no length at all — which keeps this loop
                // linear over the block count instead of walking every inline tree
                // (Ctrl+A on a long document lands here on every frame).
                let include = if !first && !tail {
                    true
                } else {
                    let Some(block) = doc.blocks.get(blk) else {
                        continue;
                    };
                    let (lo, hi) = if first && tail {
                        (a.off, b.off)
                    } else if first {
                        (a.off, block.plain_len())
                    } else {
                        (0, b.off)
                    };
                    // An end block only counts when it contributes bytes, so a
                    // selection ending exactly at a block's start does not include it.
                    hi > lo
                };
                if include {
                    out.blocks.push(blk);
                }
            }
            // Two blocks or more, otherwise nothing: the unit is the **block**,
            // not the visual line. Selecting inside one block — a word, a whole
            // wrapped paragraph, a line of it — never turns it into source; cells
            // count as blocks, so a sweep across a table does expand it.
            if out.blocks.len() < 2 {
                out.blocks.clear();
            }
            if !out.blocks.is_empty() {
                expand_tables(doc, &mut out.blocks);
                return out;
            }
            // A selection that stays **inside one atom** keeps that atom's source on
            // screen: the highlight then covers readable characters and the edit layer
            // may take single characters instead of the whole node (see
            // `editing_span`). A range that reaches the atom's edge belongs to the
            // surrounding text and still takes the atom whole.
            //
            // Same-block only: `b.off` is measured in **its own block's** plain text,
            // and feeding it to block `a`'s spans lets a cross-block range
            // ("selection ends at the next block's start") pass the containment test
            // with a meaningless endpoint — the formula was then marked as text being
            // edited and a delete truncated it to `$x+$` instead of taking it whole.
            if a.block == b.block {
                if let Some(block) = doc.blocks.get(a.block.min(last)) {
                    out.atom = atom_inside(block, a.block.min(last), a.off, b.off);
                }
            }
            return out;
        }
        // Collapsed caret: the formula it sits in or right next to, else an atom
        // whose text the caret is inside (see `editing_span`).
        let blk = sel.focus.block;
        let Some(block) = doc.blocks.get(blk) else {
            return out;
        };
        // Two allocation-free walks (a click is a hot path): formulas expand at
        // their edges, images only while the caret is inside the alt.
        let off = sel.focus.off;
        if let Some((s, e)) = block
            .math_spans()
            .into_iter()
            .find(|(s, e)| *s <= off && off <= *e)
        {
            out.atom = Some((blk, s, e));
            return out;
        }
        if let Some((s, e)) = block
            .image_spans()
            .into_iter()
            .find(|(s, e)| *s < off && off < *e)
        {
            out.atom = Some((blk, s, e));
        }
        out
    }
}

/// A **table is one block for the reader**: the pipes and the delimiter row
/// only make sense as a whole, so touching one cell expands every cell of that
/// table (the DOM keeps one `data-bid` per cell, so the caret still addresses
/// cells).
///
/// One pass over the block list: the touched tables' `seq`s are collected first,
/// then every cell of those is marked. Scanning the whole document once per
/// touched cell (the previous shape) was O(blocks × cells) and ran on every frame
/// of a drag and on Ctrl+A.
fn expand_tables(doc: &Doc, blocks: &mut Vec<usize>) {
    let mut seqs: Vec<u64> = blocks
        .iter()
        .filter_map(|&blk| doc.blocks.get(blk).and_then(Block::table_seq))
        .collect();
    if seqs.is_empty() {
        return;
    }
    seqs.sort_unstable();
    seqs.dedup();
    let mut extra: Vec<usize> = Vec::new();
    for (k, candidate) in doc.blocks.iter().enumerate() {
        if candidate
            .table_seq()
            .is_some_and(|s| seqs.binary_search(&s).is_ok())
        {
            extra.push(k);
        }
    }
    blocks.append(&mut extra);
    blocks.sort_unstable();
    blocks.dedup();
}

/// The atom of `block` that **contains** `[start, end)` without being covered by
/// it — the "a selection sits inside the source" case. A range that covers the
/// whole atom (or starts before / ends after it) is a range over an object, not
/// over its text.
fn atom_inside(
    block: &Block,
    blk: usize,
    start: usize,
    end: usize,
) -> Option<(usize, usize, usize)> {
    let fits = |&(s, e): &(usize, usize)| s <= start && end <= e && (start > s || end < e);
    block
        .math_spans()
        .into_iter()
        .find(fits)
        .or_else(|| block.image_spans().into_iter().find(fits))
        .map(|(s, e)| (blk, s, e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::md;
    use crate::model::{Pos, Sel};

    fn ed(src: &str) -> Editor {
        Editor::new(md::parse(src))
    }

    fn select(e: &mut Editor, a: (usize, usize), b: (usize, usize)) {
        e.sel = Sel {
            anchor: Pos::new(a.0, a.1),
            focus: Pos::new(b.0, b.1),
        };
    }

    /// Two blocks or more expand, and each one **whole**; a selection inside a
    /// single line expands nothing.
    #[test]
    fn a_sweep_expands_from_the_second_block_on() {
        let mut e = ed("**a** b `c`\n\nsecond\n\n**third**");
        // Inside one block: stays typeset, whatever part is selected.
        select(&mut e, (0, 0), (0, 3));
        assert!(e.reveal().is_empty(), "one line stays typeset");
        select(&mut e, (0, 1), (0, 8));
        assert!(e.reveal().is_empty());
        // A selection ending at the next block's start does not reach it.
        select(&mut e, (0, 1), (1, 0));
        assert!(e.reveal().is_empty(), "still one block");
        // Two blocks: both expand whole, including the parts the sweep misses.
        select(&mut e, (0, 3), (1, 2));
        assert_eq!(e.reveal().blocks(), &[0, 1]);
        // Three: every block covered.
        select(&mut e, (0, 3), (2, 2));
        assert_eq!(e.reveal().blocks(), &[0, 1, 2]);
    }

    /// A live drag owns its blocks even when the moving end is on a blank
    /// boundary (offset 0) or the block has only one row. Releasing restores
    /// the ordinary single-block rule.
    #[test]
    fn a_drag_expands_the_block_under_the_pointer_even_on_blank_space() {
        let mut e = ed("**a** b\n\nsecond");
        e.dragging = true;
        // Blank start of the second block: a byte-only reveal drops it.
        select(&mut e, (0, 0), (1, 0));
        assert_eq!(e.reveal().blocks(), &[0, 1]);
        // A drag that stays inside one block expands nothing — the unit is the
        // block, even for the pointer (the rule "cross a block to see source"
        // holds while dragging too).
        select(&mut e, (0, 1), (0, 2));
        assert!(e.reveal().is_empty(), "one block stays typeset");
        // Release: unchanged.
        e.dragging = false;
        assert!(e.reveal().is_empty());
    }

    /// A collapsed caret expands **only** the formula it is at — the line stays
    /// typeset around it (that is the difference between the two triggers).
    #[test]
    fn a_caret_expands_only_the_formula() {
        let mut e = ed("abc $x+y$ def");
        for off in [4, 7] {
            e.sel = Sel::caret(Pos::new(0, off));
            let rv = e.reveal();
            assert!(rv.blocks().is_empty(), "no whole line at a caret");
            assert_eq!(rv.editing_span(0), Some((4, 7)), "caret at {off}");
            assert!(rv.shows_atom(0, 4..7));
            assert!(!rv.shows_atom(0, 0..3), "plain text is not source");
        }
        // Away from the formula: nothing at all.
        e.sel = Sel::caret(Pos::new(0, 1));
        assert!(e.reveal().is_empty());
    }

    /// A table is one block for the reader: once the sweep covers two cells (its
    /// blocks), every cell expands.
    #[test]
    fn sweeping_across_cells_expands_the_whole_table() {
        let src = "| a | b |\n|---|---|\n| c | d |";
        let mut e = ed(src);
        assert_eq!(e.doc.blocks.len(), 4);
        // One cell is one block: nothing expands.
        select(&mut e, (2, 0), (2, 1));
        assert!(e.reveal().is_empty());
        // Two cells: the whole table shows its pipes and delimiter row.
        select(&mut e, (0, 0), (1, 1));
        assert_eq!(e.reveal().blocks(), &[0, 1, 2, 3], "every cell");
    }

    /// A **caret** at a formula's edge, or a range that stays inside it, marks the
    /// atom as text being worked on; a range that reaches past its edge is a range
    /// over the object, and the whole atom goes (the old, unchanged rule).
    #[test]
    fn atom_editing_follows_the_caret_and_an_inner_range() {
        let mut e = ed("a $x+y$ b");
        e.sel = Sel::caret(Pos::new(0, 3));
        assert_eq!(e.reveal().editing_span(0), Some((2, 5)));

        // A range inside the formula keeps its source on screen — the highlight then
        // covers readable source, and a delete takes characters (see `edit::delete`).
        select(&mut e, (0, 3), (0, 4));
        assert_eq!(
            e.reveal().editing_span(0),
            Some((2, 5)),
            "a range inside the atom"
        );
        assert!(e.reveal().shows_atom(0, 2..5));

        // A range touching an edge (or covering the atom) belongs to the text around
        // it: the atom is one object again.
        select(&mut e, (0, 1), (0, 3));
        assert!(e.reveal().is_empty(), "a range over the atom's edge");
        select(&mut e, (0, 2), (0, 5));
        assert!(e.reveal().is_empty(), "the whole atom");

        // Image alt: editable while the caret is *inside* it, never at its edges.
        let mut e = ed("a ![示例图](u.png) b");
        e.sel = Sel::caret(Pos::new(0, 5));
        assert_eq!(e.reveal().editing_span(0), Some((2, 11)));
        e.sel = Sel::caret(Pos::new(0, 2));
        assert_eq!(e.reveal().editing_span(0), None);
    }

    /// A **cross-block** range never marks an editing atom: `b.off` is measured in
    /// the *second* block's plain text, and feeding it to the first block's spans
    /// let a range like "from inside the formula to the next block's start" pass
    /// the containment test with a meaningless endpoint — the delete then truncated
    /// the formula to `$x+$` instead of taking it whole.
    #[test]
    fn a_cross_block_range_never_marks_an_editing_atom() {
        let mut e = ed("a $x+y$ b\n\nsecond");
        // From inside the formula to the next block's start: the atom's edge is
        // reached, so it belongs to the surrounding text.
        select(&mut e, (0, 4), (1, 0));
        assert!(e.reveal().editing_span(0).is_none());
        assert!(e.reveal().is_empty());
        // From the first block's end into the middle of the next one.
        select(&mut e, (0, 7), (1, 2));
        assert!(e.reveal().editing_span(0).is_none());
        // A same-block range that stays inside still does (unchanged rule).
        select(&mut e, (0, 3), (0, 4));
        assert_eq!(e.reveal().editing_span(0), Some((2, 5)));
    }

    /// The unit is the **block**, not the visual line: a sweep across a soft/hard
    /// break *inside* one paragraph stays typeset — only crossing into another
    /// block shows source.
    #[test]
    fn a_sweep_across_an_in_block_break_stays_typeset() {
        let mut e = ed("**a** b\n第二行 c\n\n尾");
        // plain = "a b\n第二行 c": the soft break sits at offset 3.
        select(&mut e, (0, 1), (0, 3));
        assert!(e.reveal().is_empty(), "inside one block");
        select(&mut e, (0, 1), (0, 5));
        assert!(
            e.reveal().is_empty(),
            "a break inside a block is not a block boundary"
        );
        // Reaching the next block is what expands — and both blocks whole.
        select(&mut e, (0, 1), (1, 1));
        assert_eq!(e.reveal().blocks(), &[0, 1]);
        // A heading is a single line by construction (its breaks degrade to spaces),
        // so nothing there either.
        let mut e = ed("# 标题\n\n尾");
        select(&mut e, (0, 1), (0, 3));
        assert!(e.reveal().is_empty());
    }
}
