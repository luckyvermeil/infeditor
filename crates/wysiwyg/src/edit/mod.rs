//! Edit command layer: `Cmd` → `Editor::apply()`. Pure logic, zero GUI
//! dependencies, fully unit-testable.
//!
//! Key algorithm: **byte-range slicing of the inline tree**
//! (`inlines_insert` / `inlines_delete` / `inlines_wrap`) — `Pos.off` is a
//! byte offset within a block's plain text; slicing a nested inline tree must
//! agree exactly with `plain_text`'s ordering (`Inline::push_plain` is the
//! single source of truth for measurement).
//!
//! Block-level key behaviors (Enter continues/empties lists, Backspace exits
//! lists/quotes/downgrades headings/merges blocks), but **nothing relies on
//! browser default behavior** — everything is implemented explicitly
//! (Blitz has no contenteditable).

pub(crate) use std::ops::Range;
pub(crate) use std::sync::{Arc, Mutex};
pub(crate) use std::time::{Duration, Instant};

pub(crate) use crate::model::{
    ceil_boundary, clamp_span, floor_boundary, wrap_quote, Block, Doc, Inline, Pos, Sel,
};

/// Inline wrap type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WrapKind {
    Bold,
    Italic,
    Del,
    Code,
}

/// Move commands. Visual line/word movement needs layout info and is handled
/// upstream by the geom layer (see ui.rs); these are the pure-logic fallbacks:
/// Left/Right (chars), Home/End within a block, Up/Down across blocks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Move {
    Left,
    Right,
    WordLeft,
    WordRight,
    BlockStart,
    BlockEnd,
    /// Document start/end (Ctrl+Home / Ctrl+End).
    DocStart,
    DocEnd,
    PrevBlockEnd,
    NextBlockStart,
}

/// Edit commands.
#[derive(Debug, Clone, PartialEq)]
pub enum Cmd {
    /// Insert text at the caret (replaces the selection). Triggers inline and
    /// block-level autocomplete.
    InsertText(String),
    DeleteBack,
    DeleteForward,
    /// Ctrl+Backspace: delete the previous word (cross-block merge semantics
    /// same as [`Cmd::DeleteBack`]).
    DeleteBackWord,
    /// Ctrl+Delete: delete the next word.
    DeleteForwardWord,
    /// Delete the selection (clipboard content is extracted upstream; model
    /// only handles the document).
    DeleteSelection,
    Enter,
    /// Soft line break within a block (Shift+Enter): inserts
    /// `Inline::SoftBreak` without splitting the block.
    /// Head/Td are single-line blocks (ATX headings / GFM cells cannot
    /// express intra-block line breaks) → no-op.
    SoftBreak,
    Move(Move),
    /// Whether the move extends the selection (composed by the ui layer:
    /// `Move(m)` vs `MoveExtend(m)`).
    MoveExtend(Move),
    SelectAll,
    SelectWord,
    /// Triple-click block selection: select the whole block (block start to
    /// block end; Li selects option text, Td selects the cell).
    SelectBlock,
    Wrap(WrapKind),
    /// Inline link: turn the selection into a link. When `url` is empty, use
    /// the selection text.
    Link(String),
    /// Inline image: with an empty selection insert
    /// `Inline::Image{url, alt:""}`; with a selection, the selection text
    /// becomes the alt. **Never insert literal `![](url)`** — emit escapes
    /// the brackets into `!\[\](url)`, which no longer parses as an image
    /// (hit this on a real machine).
    Image(String),
    /// Heading level (1..=6). Applies to the focused block.
    SetHead(u8),
    ToggleList {
        ordered: bool,
    },
    ToggleQuote,
    ToggleTask,
    /// Paragraph ↔ fenced code block.
    ToggleCodeBlock,
    InsertHr,
    /// List indent / outdent (Tab).
    Indent,
    Outdent,
    /// Insert an empty paragraph **before/after** the focused block and move
    /// the caret into it.
    InsertBefore,
    InsertAfter,
    /// Insert a `rows × cols` table after the focused block (first row is the
    /// header), caret lands in the first cell (toolbar table grid picker).
    InsertTable {
        rows: u8,
        cols: u8,
    },
    /// Table commands (apply to the table containing the focused block).
    Table(TableCmd),
    /// Toggle a task list checkbox (clicking the checkbox).
    ToggleTaskChecked(usize),
    /// Paste plain text/Markdown (cross-block text is split into md blocks
    /// and inserted).
    Paste(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TableCmd {
    InsertRowAfter,
    DeleteRow,
    InsertColAfter,
    DeleteCol,
    /// Tab: caret moves to the next cell (selecting the whole cell content);
    /// after the last cell, append a row at the table end and land in its
    /// first cell. Pure movement does not enter undo (row appending pushes
    /// undo inside `table_cmd` itself).
    CellNext,
    /// Shift+Tab: caret moves to the previous cell (selecting the whole cell
    /// content); no-op on the first cell.
    CellPrev,
    /// Column alignment: set `align` on all Tds of the **column** containing
    /// the focused cell (same encoding as `Td.align`: 0=default 1=left
    /// 2=center 3=right). GFM alignment is a column property of the delimiter
    /// row and cannot express per-cell alignment — hence it applies to the
    /// whole column.
    /// Enters the undo stack (`Cmd::Table(_)` is already inside `may_edit`).
    Align(u8),
}

/// Undo snapshot. **Deep copy of the whole Doc** (block tree + all Strings):
/// buys simplicity of `Editor::clone` and undo/redo. Memory ≈ stack depth ×
/// document bytes, capped by two gates (see [`push_undo`]): keystroke-wave
/// coalescing (typing / consecutive deletes push only one snapshot) +
/// reduced depth for large docs (>2MB drops to [`UNDO_CAP_BIGDOC`]).
/// Sharing block-level structure (snapshots store `Rc<Block>`, edits are
/// copy-on-write) would reduce memory to O(N + number of edited blocks), but
/// `Doc.blocks` is a public `Vec<Block>` and the edit layer mutates it with
/// raw indices throughout — the refactor would cut across every command in
/// edit.rs and downstream. The payoff doesn't justify it; not done for now.
#[derive(Debug, Clone)]
struct Snapshot {
    doc: Doc,
    sel: Sel,
}

/// Undo log (Arc-shared: `Editor` is cloned by Signals, the stack does not
/// duplicate on clone).
#[derive(Default)]
struct UndoLog {
    undo: Vec<Snapshot>,
    redo: Vec<Snapshot>,
    /// Keystroke-wave coalescing: consecutive InsertText/DeleteBack/
    /// DeleteForward within one wave push only one snapshot.
    last_type: Option<Instant>,
    /// Caret position right after the last applied edit. The time window alone
    /// merged waves across a caret move — clicking into another paragraph and
    /// typing within 500 ms made one Ctrl+Z undo text from both places. Caret
    /// moves (arrows, mouse, Home/End) never reach `push_undo`, so comparing
    /// the pre-edit caret with where the previous edit left off is the only
    /// check that covers every way the caret can move.
    last_caret: Option<Pos>,
}

const UNDO_CAP: usize = 100;
/// Undo depth for large docs (plain > [`BIG_DOC_BYTES`]): a snapshot is a
/// full deep copy, memory ≈ depth × doc size; 100 layers on a multi-MB doc
/// is hundreds of MB.
const UNDO_CAP_BIGDOC: usize = 30;
/// "Large document" threshold (plain-text bytes).
const BIG_DOC_BYTES: usize = 2 * 1024 * 1024;
const TYPE_COALESCE: Duration = Duration::from_millis(500);

/// Total plain-text bytes of the document (no allocation; used by the undo
/// stack's memory gate).
fn doc_plain_bytes(doc: &Doc) -> usize {
    doc.blocks.iter().map(block_plain_bytes).sum()
}

/// Plain-text bytes of a single block (penetrates quote shells).
fn block_plain_bytes(b: &Block) -> usize {
    match b {
        Block::Code { text, .. } | Block::Html { text } => text.len(),
        Block::Hr => 0,
        Block::Para(i) | Block::Quote(i) => i.iter().map(Inline::plain_len).sum(),
        Block::Head { inlines, .. } | Block::Li { inlines, .. } | Block::Td { inlines, .. } => {
            inlines.iter().map(Inline::plain_len).sum()
        }
        Block::Quoted { inner, .. } => block_plain_bytes(inner),
    }
}

/// Editor state: doc + sel + undo. The upper layer drives re-rendering through
/// a `Signal<Editor>`.
pub struct Editor {
    pub doc: Doc,
    pub sel: Sel,
    /// Target column for vertical movement (block content-box coordinate x, in
    /// physical pixels). Maintained by `move_visual_cmd` while ↑/↓ are chained: the
    /// column is kept when crossing rows or empty paragraphs shorter than it; any
    /// other selection change (horizontal movement, typing, clicking) clears it.
    /// None = no chain.
    pub h_pos: Option<f32>,
    undo: Arc<Mutex<UndoLog>>,
}

impl std::fmt::Debug for Editor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Editor")
            .field("sel", &self.sel)
            .finish_non_exhaustive()
    }
}

impl Clone for Editor {
    fn clone(&self) -> Self {
        Self {
            doc: self.doc.clone(),
            sel: self.sel,
            h_pos: self.h_pos,
            undo: Arc::clone(&self.undo),
        }
    }
}

impl Default for Editor {
    fn default() -> Self {
        Self::new(Doc::default())
    }
}

/// Everything a table command needs to know about the focused table:
/// identity (`seq`), focus geometry (row / column count) and the table's
/// block range. Computed once by `table_cmd`, consumed by the arm methods.
pub(crate) struct TableCtx {
    seq: u64,
    /// focus row (Td.row)
    row: usize,
    /// quote depth inherited by new cells
    depth: u8,
    /// first block index of the table
    start: usize,
    /// one past the last block index of the table
    end: usize,
    /// cells in the focus row
    cols: usize,
}

impl Editor {
    pub fn new(doc: Doc) -> Self {
        let sel = Sel::caret(Pos::new(0, 0));
        Self {
            doc,
            sel,
            h_pos: None,
            undo: Arc::default(),
        }
    }

    /// The URL of the link inline covering `pos` (the Ctrl+click open target).
    ///
    /// Model-level on purpose: the DOM link hit-test (`geom::link_at`) and the
    /// caret positioning disagree on coordinates, while the model is the same
    /// ledger the caret rides on — the caret lands in the link's label and the
    /// lookup reads exactly that position. `None` when the position is not
    /// inside a `Link` span. A Link's plain contribution is its label inlines
    /// (the url itself is virtual, measured by [`Inline::plain_len`]).
    pub fn link_at_pos(&self, pos: Pos) -> Option<String> {
        use crate::model::{Block, Inline};
        let b = self.doc.blocks.get(pos.block)?;
        let inlines: &[Inline] = match b.core() {
            Block::Para(i) | Block::Quote(i) => i,
            Block::Head { inlines: i, .. }
            | Block::Li { inlines: i, .. }
            | Block::Td { inlines: i, .. } => i,
            _ => return None,
        };
        let mut off = 0usize;
        for inline in inlines {
            let len = inline.plain_len();
            if let Inline::Link { url, .. } = inline {
                if pos.off >= off && pos.off <= off + len {
                    return Some(url.clone());
                }
            }
            off += len;
        }
        None
    }

    /// Replace the whole document (host `setValue`). Clears the undo stack.
    pub fn set_doc(&mut self, doc: Doc) {
        self.doc = doc;
        self.sel = Sel::caret(Pos::new(0, 0));
        self.h_pos = None;
        let mut log = self.undo.lock().unwrap_or_else(|e| e.into_inner());
        log.undo.clear();
        log.redo.clear();
        log.last_type = None;
    }

    /// Apply a command. Returns whether the document changed (the upper layer
    /// recomputes the caret geometry from it).
    pub fn apply(&mut self, cmd: Cmd) -> bool {
        // Any command that is not vertical movement clears the remembered target
        // column: the ↑/↓ target column only survives across a run of ↑/↓ presses
        // (short rows and empty paragraphs keep the column), while typing, left/right,
        // Home/End and mouse clicks all re-establish it. Vertical movement does not go
        // through apply (see `ui::move_visual_cmd`).
        self.h_pos = None;
        // Commands that may edit the document: push a snapshot first, then pop it when
        // the command turned out to leave the document unchanged — otherwise one Ctrl+Z
        // is a no-op "nothing happened" (a cross-block Wrap, an invalid
        // ToggleTaskChecked, an empty InsertText all take this path).
        let may_edit = match &cmd {
            // Table-cell navigation (the Tab chain) is pure caret movement: it does not
            // enter undo and must not clear redo (push_undo clears redo, so clearing it for
            // a pure move is collateral damage). Tab from the last cell inserting a row is
            // the only branch that edits the document, and table_cmd pushes its own
            // snapshot internally.
            Cmd::Table(TableCmd::CellNext) | Cmd::Table(TableCmd::CellPrev) => false,
            _ => matches!(
                cmd,
                Cmd::InsertText(_)
                    | Cmd::DeleteBack
                    | Cmd::DeleteForward
                    | Cmd::DeleteBackWord
                    | Cmd::DeleteForwardWord
                    | Cmd::DeleteSelection
                    | Cmd::Enter
                    | Cmd::SoftBreak
                    | Cmd::Wrap(_)
                    | Cmd::Link(_)
                    | Cmd::Image(_)
                    | Cmd::SetHead(_)
                    | Cmd::ToggleList { .. }
                    | Cmd::ToggleQuote
                    | Cmd::ToggleTask
                    | Cmd::ToggleCodeBlock
                    | Cmd::InsertHr
                    | Cmd::Indent
                    | Cmd::Outdent
                    | Cmd::InsertBefore
                    | Cmd::InsertAfter
                    | Cmd::InsertTable { .. }
                    | Cmd::Table(_)
                    | Cmd::ToggleTaskChecked(_)
                    | Cmd::Paste(_)
            ),
        };
        // Typing and a run of backspaces/deletes are one "keystroke wave" — a snapshot
        // is pushed only at the start of the wave and coalesced within it (the
        // TYPE_COALESCE window), so a single undo steps back over the whole wave (the
        // existing typing-coalescing semantics, extended to delete sequences).
        let coalesce = matches!(
            cmd,
            Cmd::InsertText(_)
                | Cmd::DeleteBack
                | Cmd::DeleteForward
                | Cmd::DeleteBackWord
                | Cmd::DeleteForwardWord
        );
        // A selection that arrives from outside (a click hit-test, or a document that
        // shrank under a pending signal) can carry an out-of-range block index. Clamp
        // the blocks here — cheap, no allocation — so no command can index past the
        // document; the byte offsets are clamped where they are used.
        if !self.doc.blocks.is_empty() {
            let last = self.doc.blocks.len() - 1;
            self.sel.anchor.block = self.sel.anchor.block.min(last);
            self.sel.focus.block = self.sel.focus.block.min(last);
            // Offsets too, without allocating: a stale or out-of-range caret must not
            // survive a command that turns out to be a no-op (otherwise the caret is
            // drawn past the text until the next real edit). Character-boundary
            // snapping stays at the point of use (`clamp_span`); this only bounds the
            // block.
            // A collapsed caret inside an atom is normalized to the atom's front
            // edge (the same place geom draws it): one Backspace from a caret the
            // user saw in front of a formula used to delete the whole formula, and
            // typing turned one image into two. A **selection** endpoint is left
            // alone — a range that partially covers an atom is handled by the range
            // consumers (delete takes the atom whole), so widening or collapsing it
            // here would change what the user dragged over.
            let collapsed = self.sel.collapsed();
            for pos in [&mut self.sel.anchor, &mut self.sel.focus] {
                let len = self.doc.blocks[pos.block].plain_len();
                pos.off = pos.off.min(len);
                if collapsed {
                    pos.off = atom_floor_block(&self.doc.blocks[pos.block], pos.off);
                }
            }
        }
        // The table Tab chain sits outside `may_edit`, but its **last-cell**
        // branch appends a row and therefore does edit the document. Detect
        // that by the block count delta instead of assuming every CellNext is
        // an edit: a plain cell-to-cell Tab used to clear the redo chain.
        let cell_next = matches!(cmd, Cmd::Table(TableCmd::CellNext));
        let blocks_before = self.doc.blocks.len();
        let pushed = if may_edit {
            self.push_undo(coalesce)
        } else {
            false
        };
        let changed = self.apply_inner(cmd);
        if changed {
            self.clamp_sel();
            let mut log = self.undo.lock().unwrap_or_else(|e| e.into_inner());
            // Where the caret landed: the next keystroke only joins this wave
            // if it starts exactly here (see `UndoLog::last_caret`).
            log.last_caret = Some(self.sel.focus);
            // Only a command that really changed the **document** invalidates the
            // redo chain. `changed` also covers pure caret/selection commands
            // (Move, SelectAll, SelectWord, …), which return "something moved" —
            // clearing on those wiped the whole redo stack after Ctrl+Z as soon as
            // the user pressed an arrow key, so Ctrl+Y / the Redo button did
            // nothing. Clearing it in `push_undo` instead wiped it for a no-op
            // (a Wrap across blocks, a SetHead on an Html block).
            let doc_changed = may_edit || (cell_next && self.doc.blocks.len() != blocks_before);
            if doc_changed {
                log.redo.clear();
            }
        } else if pushed {
            let mut log = self.undo.lock().unwrap_or_else(|e| e.into_inner());
            log.undo.pop();
            log.last_type = None;
        }
        changed
    }

    /// Push a snapshot. Returns whether one was really pushed (nothing is pushed
    /// inside the keystroke-coalescing window, so that returns false).
    /// `typing` = whether this edit belongs to a "keystroke wave" (typing, or a run
    /// of backspaces/deletes) — several edits inside a wave push one snapshot at the
    /// wave's start (within the TYPE_COALESCE window).
    pub(crate) fn push_undo(&self, typing: bool) -> bool {
        let mut log = self.undo.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        if typing {
            if let Some(t) = log.last_type {
                let same_spot = log.last_caret == Some(self.sel.focus) && self.sel.collapsed();
                if now.duration_since(t) < TYPE_COALESCE && same_spot {
                    log.last_type = Some(now);
                    return false;
                }
            }
            log.last_type = Some(now);
        } else {
            log.last_type = None;
        }
        log.undo.push(Snapshot {
            doc: self.doc.clone(),
            sel: self.sel,
        });
        // A snapshot is a deep copy of the whole document, so the stack depth is the
        // memory multiplier — the cap is lowered for large documents (a >2MB document
        // at 100 levels ≈ 200MB+, at 30 levels ≈ 60MB).
        let cap = if doc_plain_bytes(&self.doc) > BIG_DOC_BYTES {
            UNDO_CAP_BIGDOC
        } else {
            UNDO_CAP
        };
        while log.undo.len() > cap {
            log.undo.remove(0);
        }
        true
    }

    pub(crate) fn apply_inner(&mut self, cmd: Cmd) -> bool {
        match cmd {
            Cmd::InsertText(t) => self.insert_text(&t, true),
            Cmd::DeleteBack => self.delete_back(),
            Cmd::DeleteForward => self.delete_forward(false),
            Cmd::DeleteBackWord => self.delete_word(true),
            Cmd::DeleteForwardWord => self.delete_word(false),
            Cmd::DeleteSelection => self.delete_selection().is_some(),
            Cmd::Enter => self.enter(),
            Cmd::SoftBreak => self.soft_break(),
            Cmd::Move(m) => self.move_caret(m, false),
            Cmd::MoveExtend(m) => self.move_caret(m, true),
            Cmd::SelectAll => self.select_all(),
            Cmd::SelectWord => self.select_word(),
            Cmd::SelectBlock => self.select_block(),
            Cmd::Wrap(k) => self.wrap_selection(k),
            Cmd::Link(url) => self.wrap_link(&url),
            Cmd::Image(url) => self.wrap_image(&url),
            Cmd::SetHead(l) => self.set_head(l),
            Cmd::ToggleList { ordered } => self.toggle_list(ordered),
            Cmd::ToggleQuote => self.toggle_quote(),
            Cmd::ToggleTask => self.toggle_task(),
            Cmd::ToggleCodeBlock => self.toggle_code_block(),
            Cmd::InsertHr => self.insert_hr(),
            Cmd::InsertBefore => self.insert_block(true),
            Cmd::InsertAfter => self.insert_block(false),
            Cmd::InsertTable { rows, cols } => self.insert_table(rows, cols),
            Cmd::Indent => self.indent(true),
            Cmd::Outdent => self.indent(false),
            Cmd::Table(c) => self.table_cmd(c),
            Cmd::ToggleTaskChecked(i) => self.toggle_task_checked(i),
            Cmd::Paste(t) => self.paste(&t),
        }
    }

    // ------------------------------------------------------------------
    // Selection helpers
    // ------------------------------------------------------------------

    pub(crate) fn clamp_sel(&mut self) {
        self.sel.anchor = self.sel.anchor.clamped(&self.doc);
        self.sel.focus = self.sel.focus.clamped(&self.doc);
    }

    pub fn undo(&mut self) -> bool {
        let snap = {
            let mut log = self.undo.lock().unwrap_or_else(|e| e.into_inner());
            log.last_type = None;
            log.undo.pop()
        };
        let Some(snap) = snap else { return false };
        let mut log = self.undo.lock().unwrap_or_else(|e| e.into_inner());
        log.redo.push(Snapshot {
            doc: self.doc.clone(),
            sel: self.sel,
        });
        drop(log);
        self.doc = snap.doc;
        self.sel = snap.sel;
        true
    }

    pub fn redo(&mut self) -> bool {
        let snap = {
            let mut log = self.undo.lock().unwrap_or_else(|e| e.into_inner());
            log.redo.pop()
        };
        let Some(snap) = snap else { return false };
        let mut log = self.undo.lock().unwrap_or_else(|e| e.into_inner());
        log.undo.push(Snapshot {
            doc: self.doc.clone(),
            sel: self.sel,
        });
        drop(log);
        self.doc = snap.doc;
        self.sel = snap.sel;
        true
    }

    pub fn can_undo(&self) -> bool {
        !self
            .undo
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .undo
            .is_empty()
    }
}

mod autoformat;
mod delete;
mod format;
mod inlines;
mod insert;
mod motion;
mod paste;
mod select;
mod table;

#[cfg(test)]
mod tests;

// Minimal API for geom/ui (the visual-row part of movement lives in the geom layer)
pub use crate::model::Block as EditBlock;
pub(crate) use inlines::atom_floor_block;
pub use table::table_cell_vertical;
