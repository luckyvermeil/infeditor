//! [`TextDocument`]: piece table + line index + undo/redo + metadata.
//!
//! All positions are byte offsets into the UTF-8 document text. The engine is
//! UI-agnostic and single-threaded (owned by the UI thread); background tasks
//! (search, index rebuild) work on immutable snapshots taken from the UI.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::ops::Range;

use super::line_index::{count_breaks_by_kind, BreakCounts, LineIndex, LineSpan, Newline};
use super::piece_table::{Original, Piece, PieceTable};

/// A single replace-style edit: the bytes in `pos..pos + deleted.len()` are
/// replaced with `inserted`. `deleted` is retained so the edit can be undone.
#[derive(Clone, Debug)]
pub struct EditOp {
    pub pos: u64,
    pub deleted: String,
    pub inserted: String,
}

impl EditOp {
    pub fn insert(pos: u64, text: impl Into<String>) -> Self {
        EditOp {
            pos,
            deleted: String::new(),
            inserted: text.into(),
        }
    }
    pub fn delete(range: Range<u64>, text: impl Into<String>) -> Self {
        EditOp {
            pos: range.start,
            deleted: text.into(),
            inserted: String::new(),
        }
    }
    pub fn inverse(&self) -> EditOp {
        EditOp {
            pos: self.pos,
            deleted: self.inserted.clone(),
            inserted: self.deleted.clone(),
        }
    }
    pub fn end(&self) -> u64 {
        self.pos + self.deleted.len() as u64
    }
    pub fn len_after(&self) -> u64 {
        self.pos + self.inserted.len() as u64
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CoalesceKey {
    InsertChar,
    DeleteBack,
    DeleteFwd,
    /// CSV table cell live editing: same edit session (whole row replacement
    /// on the same doc row) merges into one undo step. The adjacency check is
    /// trivially true (same key = same row); merge correctness is guaranteed
    /// by undo's reverse-order replay (each op applies to the previous state).
    CellEdit {
        line: u64,
    },
}

/// One undo group. `ops` are applied in order and undone in reverse order.
#[derive(Clone, Debug)]
pub struct UndoEntry {
    pub ops: Vec<EditOp>,
    pub coalesce: Option<CoalesceKey>,
    pub time: std::time::Instant,
}

impl UndoEntry {
    /// Rough heap footprint of the group, including the `String` payloads and
    /// the op vector's capacity. Used to bound the history (see
    /// [`TextDocument::UNDO_MAX_BYTES`]).
    fn heap_bytes(&self) -> usize {
        std::mem::size_of::<UndoEntry>()
            + self.ops.capacity() * std::mem::size_of::<EditOp>()
            + self
                .ops
                .iter()
                .map(|o| o.deleted.capacity() + o.inserted.capacity())
                .sum::<usize>()
    }
}

#[derive(Default, Clone, Debug)]
pub struct DocMeta {
    pub path: Option<std::path::PathBuf>,
    pub display_name: String,
    /// encoding used when saving (None = UTF-8 without BOM)
    pub encoding: Option<&'static encoding_rs::Encoding>,
    pub had_bom: bool,
    pub detected_newline: Newline,
    pub user_newline: Option<Newline>,
    pub file_type: String,
    pub csv_mode: bool,
    /// Opening failed whole-file UTF-8 validation and was decoded leniently
    /// (illegal bytes → U+FFFD).
    ///
    /// The buffer's content **no longer equals** the bytes on disk: saving
    /// will overwrite those illegal sequences with the replacement character.
    /// The save path must state this plainly in the status bar (it cannot
    /// silently claim success).
    pub lossy_utf8: bool,
}

/// One-entry memo for a document line: `(gen, line)` → geometry + text.
///
/// Both halves live in **one** slot because the render path asks the same line
/// for both: the viewport calls `line_text(ln)` and then `line_span(ln)` for
/// every visible row, and each uncached query has to walk up to
/// `CKPT_LINES` (1024) lines from the nearest checkpoint. Caching the span
/// with the text turns those two walks into one.
///
/// `text: None` marks an entry that only `line_span` filled in (nobody has
/// asked for the bytes yet), so a geometry-only caller never pays for a read.
///
/// Invalidated by `gen` (every edit bumps it) and explicitly by
/// [`TextDocument::build_index`] / [`TextDocument::merge_index_patch`] —
/// progressive index merges change what a line number means without bumping
/// `gen`.
#[derive(Clone, Debug)]
struct LineCache {
    gen: u64,
    line: u64,
    span: LineSpan,
    text: Option<String>,
}

#[derive(Clone, Debug)]
pub struct TextDocument {
    pub table: PieceTable,
    pub index: LineIndex,
    pub meta: DocMeta,
    undo: VecDeque<UndoEntry>,
    redo: VecDeque<UndoEntry>,
    /// Approximate bytes held by `undo` + `redo` payloads.
    history_bytes: usize,
    /// bumped on every applied edit; used to invalidate UI caches
    pub gen: u64,
    pub id: u64,
    bulk_edit_threshold: usize,
    line_cache: RefCell<Option<LineCache>>,
    /// **First affected line** of the most recent (unconsumed) edit: used
    /// for line-granular invalidation of highlight checkpoints (reset to
    /// u64::MAX = no info/all invalid after the render consumes it). Together
    /// with gen it forms the "from which line to re-lex" contract — without
    /// it every keystroke would rescan everything from line 0.
    edit_from_line: std::cell::Cell<u64>,
    /// monotonic history position (bumps on every undo-entry push/undo/redo)
    hpos: u64,
    /// history position at the last save (or open)
    saved_hpos: u64,
}

static NEXT_DOC_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

pub fn new_doc_id() -> u64 {
    NEXT_DOC_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

impl TextDocument {
    pub fn empty() -> Self {
        TextDocument {
            table: PieceTable::from_bytes(Vec::new()),
            index: LineIndex::empty(),
            meta: DocMeta {
                // Placeholder name uses the process-level language (the
                // engine layer cannot reach EditorState, see README §3);
                // documents with no path + placeholder name get recomputed by
                // rerender_derived_names on a language switch.
                display_name: i18n::current().tr(i18n::Msg::DocUntitled).into_owned(),
                ..Default::default()
            },
            undo: VecDeque::new(),
            redo: VecDeque::new(),
            history_bytes: 0,
            gen: 0,
            id: new_doc_id(),
            bulk_edit_threshold: 128,
            line_cache: RefCell::new(None),
            edit_from_line: std::cell::Cell::new(u64::MAX),
            hpos: 0,
            saved_hpos: 0,
        }
    }

    /// Serial of applied edits (undo decrements symmetrically).
    ///
    /// For "cancel one edit session": remember the start point, loop undo
    /// back to it for an exact rollback, and the dirty flag returns
    /// symmetrically with `hpos` (no more "modified" shown when content is
    /// back at the save point).
    pub fn edit_serial(&self) -> u64 {
        self.hpos
    }

    /// True when the document has unsaved changes.
    pub fn is_dirty(&self) -> bool {
        self.hpos != self.saved_hpos
    }

    /// Mark the current state as saved (call after a successful save).
    pub fn mark_saved(&mut self) {
        self.saved_hpos = self.hpos;
    }

    /// Mark document as having unsaved changes, without modifying text or undo history.
    ///
    /// Crash recovery: the document is rebuilt from a rescue snapshot and its
    /// content literally is the user's edit — there is no undo entry to
    /// derive the dirty flag from, so it must be set explicitly
    /// ([`Self::is_dirty`] compares `hpos` with `saved_hpos`).
    pub fn mark_dirty(&mut self) {
        if !self.is_dirty() {
            self.hpos += 1;
        }
    }

    /// Create a document over an original backing store (mmap or owned bytes).
    /// The index must be built separately (possibly in a background thread).
    pub fn with_original(original: Original, meta: DocMeta) -> Self {
        let mut d = Self::empty();
        d.table = PieceTable::new(original);
        d.meta = meta;
        d
    }

    pub fn build_index(&mut self, progress: impl FnMut(u64, u64)) {
        self.index = LineIndex::build(&self.table, progress);
        self.line_cache.borrow_mut().take();
    }

    /// Phase 1 of a progressive open: index only the first `upto` bytes so a
    /// huge file becomes visible immediately; the rest is filled in by
    /// background patches (see [`Self::merge_index_patch`]).
    pub fn build_index_prefix(&mut self, upto: u64, progress: impl FnMut(u64, u64)) {
        self.index = LineIndex::build_prefix(&self.table, upto, progress);
        self.line_cache.borrow_mut().take();
    }

    /// True once the whole document has been indexed.
    pub fn index_complete(&self) -> bool {
        self.index.complete
    }

    /// Estimated line count while the index is still building (exact once it
    /// is complete).
    pub fn index_scanned_to(&self) -> u64 {
        self.index.scanned_to
    }

    /// Line breaks already counted before the scan watermark (continuation
    /// scans resume from this line number).
    pub fn index_base_line(&self) -> u64 {
        self.index.scanned_breaks()
    }

    /// Fold a background index patch into this document's line index.
    ///
    /// The **caller** owns the generation check (see `opens.rs`): a patch from
    /// an older generation refers to text that no longer exists, and this
    /// method cannot detect that by itself — it only guards on `complete`.
    pub fn merge_index_patch(&mut self, p: crate::buffer::line_index::IndexPatch) {
        if !self.index.complete {
            self.index.merge_patch(&self.table, p);
            // After the merge the same line number may map to a different
            // byte range (estimates during progressive open); the line-text
            // cache is keyed by (gen, line) and gen is unchanged, so without
            // this clear the pre-merge old text keeps being shown (highlight
            // checkpoints are invalidated by the opens.rs caller).
            self.line_cache.borrow_mut().take();
        }
    }

    pub fn line_count(&self) -> u64 {
        self.index.total_lines
    }

    pub fn total_bytes(&self) -> u64 {
        self.table.len()
    }

    pub fn newline(&self) -> Newline {
        self.meta.user_newline.unwrap_or(self.index.newline)
    }

    /// Content range of a line (excluding terminator).
    pub fn line_range(&self, line: u64) -> Range<u64> {
        let s = self.line_span(line);
        s.start..s.end
    }

    /// Geometry of one line — see `LineCache` for why this is cached at all.
    pub fn line_span(&self, line: u64) -> LineSpan {
        {
            let c = self.line_cache.borrow();
            if let Some(c) = c.as_ref() {
                if c.gen == self.gen && c.line == line {
                    return c.span;
                }
            }
        }
        let span = self.index.line_span(&self.table, line);
        *self.line_cache.borrow_mut() = Some(LineCache {
            gen: self.gen,
            line,
            span,
            text: None,
        });
        span
    }

    pub fn line_text(&self, line: u64) -> String {
        {
            let c = self.line_cache.borrow();
            if let Some(c) = c.as_ref() {
                if c.gen == self.gen && c.line == line {
                    if let Some(t) = c.text.as_ref() {
                        return t.clone();
                    }
                }
            }
        }
        // `line_span` already paid for the checkpoint walk; only the byte read
        // is left, so the viewport's `line_text(ln); line_span(ln)` pair costs
        // one walk per row instead of two.
        let span = self.line_span(line);
        let t = self.table.read_str(span.start..span.end);
        *self.line_cache.borrow_mut() = Some(LineCache {
            gen: self.gen,
            line,
            span,
            text: Some(t.clone()),
        });
        t
    }

    pub fn text_range(&self, range: Range<u64>) -> String {
        self.table.read_str(range)
    }

    pub fn pos_of_line(&self, line: u64) -> u64 {
        self.index.pos_of_line(&self.table, line)
    }

    pub fn line_of_pos(&self, pos: u64) -> u64 {
        self.index.line_of_pos(&self.table, pos)
    }

    /// Round a byte position down to a char boundary.
    pub fn snap(&self, pos: u64) -> u64 {
        self.table.round_down(pos.min(self.table.len()))
    }

    /// The character at `pos`; `None` when out of bounds.
    ///
    /// Used by double-click word selection when classifying by character: it
    /// reads one full code point at a time so callers need not deal with
    /// UTF-8 lead bytes themselves.
    pub fn char_at(&self, pos: u64) -> Option<char> {
        let b0 = self.table.byte_at(pos)?;
        // Lead-byte length goes through textops::utf8_seq_len uniformly;
        // clamp past the document end so read_str takes the first code point.
        let len = crate::textops::utf8_seq_len(b0) as u64;
        let end = (pos + len).min(self.table.len());
        self.table.read_str(pos..end).chars().next()
    }

    // ---- editing -----------------------------------------------------------

    /// Apply edits without creating an undo entry. Offsets in later ops refer
    /// to the document state after earlier ops are applied.
    pub fn apply_ops_raw(&mut self, ops: &[EditOp]) {
        if ops.is_empty() {
            return;
        }
        let bulk = ops.len() > self.bulk_edit_threshold;
        if bulk {
            if let Some(reps) = self.batch_replacements(ops) {
                // Single-pass rebuild of the piece list: bulk scenarios
                // (replace-all / whole-doc rewrite) drop from O(P × ops) to
                // O(P + ops). Ordering, bounds and char boundaries of the
                // replacement ranges were validated above; any failure falls
                // back to the per-op path below.
                let earliest = reps[0].0.start;
                self.table.rebuild_with_replacements(&reps);
                self.index.rebuild_from(&self.table, earliest);
                self.gen += 1;
                self.line_cache.borrow_mut().take();
                self.record_edit_from(earliest);
                return;
            }
        }
        let mut earliest = u64::MAX;
        for op in ops {
            earliest = earliest.min(op.pos);
            let start = self.snap(op.pos);
            // Round the end down like `PieceTable::delete` does. A malformed
            // op (deleted length landing inside a multi-byte char) would
            // otherwise make the table delete fewer bytes than `old_len`
            // claims, and the line index's `total_bytes` would drift from
            // `table.len()` for the rest of the session.
            let end = self.snap(start + op.deleted.len() as u64);
            debug_assert_eq!(
                self.table.read(start..end),
                op.deleted.as_bytes(),
                "edit op deleted-text mismatch at {start}"
            );
            let old_len = end - start;
            // Break bookkeeping by *window comparison*: count terminators in
            // [start-1, end+1) before the edit and in [start-1,
            // start+new_len+1) after. Both windows share every byte outside
            // the edited region (including one byte on each side), so the
            // delta is exactly the terminator change of the edit — including
            // the split/merge of a CRLF pair whose halves the range only
            // partly covers. The naive "removed inside range + inserted
            // count" model got these wrong: deleting just the `\r` of a CRLF
            // (net 0, was booked -1), just the `\n` (net 0, was booked -1)
            // and inserting between the two (net +1, was booked 0).
            let win_start = start.saturating_sub(1);
            let removed = if bulk {
                BreakCounts::default()
            } else {
                let before_end = (end + 1).min(self.table.len());
                count_breaks_by_kind(&self.table, win_start..before_end)
            };
            if old_len > 0 {
                self.table.delete(start..end);
            }
            if !op.inserted.is_empty() {
                self.table.insert(start, op.inserted.as_bytes());
            }
            if !bulk {
                let after_end = (start + op.inserted.len() as u64 + 1).min(self.table.len());
                let added = count_breaks_by_kind(&self.table, win_start..after_end);
                self.index.apply_edit(
                    &self.table,
                    start,
                    old_len,
                    op.inserted.len() as u64,
                    removed,
                    added,
                );
            }
        }
        if bulk {
            self.index.rebuild_from(&self.table, earliest);
        }
        self.gen += 1;
        self.line_cache.borrow_mut().take();
        self.record_edit_from(earliest);
    }

    /// Record the first affected line (min across edits: several edits can
    /// happen between two renders; reset after the render consumes it). The
    /// post-edit earliest is still a valid char boundary and necessarily
    /// falls inside the first affected line; while the index is incomplete
    /// line_of_pos clamps to the scanned region — err toward
    /// over-invalidation (too much just means extra rescans; too little
    /// means wrong colors).
    fn record_edit_from(&mut self, earliest: u64) {
        let line = self.line_of_pos(earliest);
        if line < self.edit_from_line.get() {
            self.edit_from_line.set(line);
        }
    }

    /// Peek (without consuming) at the earliest affected line of this
    /// generation's edits.
    ///
    /// The newline index is synced before the viewport, but lexical
    /// highlighting also needs to consume the same marker; both calling
    /// `take` would leave the second with "no info", so every edit would
    /// trigger a full highlight rescan.
    pub fn edit_from_line_hint(&self) -> Option<u64> {
        let l = self.edit_from_line.get();
        (l != u64::MAX).then_some(l)
    }

    /// Take the "re-lex from which line" marker (consumed by rendering;
    /// u64::MAX = no info → invalidate all).
    pub fn take_edit_from_line(&self) -> u64 {
        self.edit_from_line.replace(u64::MAX)
    }

    /// Reinterpret a bulk op list as disjoint replacements in original-text
    /// coordinates — the shape [`PieceTable::rebuild_with_replacements`]
    /// wants — or `None` when the ops do not form that shape (mixed order,
    /// overlap, out of bounds, mid-character position), in which case the
    /// caller falls back to the per-op path.
    fn batch_replacements<'a>(&self, ops: &'a [EditOp]) -> Option<Vec<(Range<u64>, &'a [u8])>> {
        let total = self.table.len();
        let mut reps: Vec<(Range<u64>, &[u8])> = Vec::with_capacity(ops.len());
        if ops.windows(2).all(|w| w[0].pos <= w[1].pos) {
            // Ascending: each op's position is in "after all previous ops"
            // coordinates, so map it back to original-text coordinates by
            // subtracting the net length change of the earlier ops.
            let mut front: i64 = 0;
            let mut prev_end = 0u64;
            for op in ops {
                let start = i64::try_from(op.pos).ok()?.checked_sub(front)?;
                if start < 0 {
                    return None;
                }
                let start = start as u64;
                let end = start.checked_add(op.deleted.len() as u64)?;
                if end > total || start < prev_end || !self.table.is_boundary(start) {
                    return None;
                }
                debug_assert!(
                    self.table.read(start..end) == op.deleted.as_bytes(),
                    "bulk op deleted-text mismatch at {start}"
                );
                prev_end = end;
                front = front.checked_add(op.inserted.len() as i64 - op.deleted.len() as i64)?;
                reps.push((start..end, op.inserted.as_bytes()));
            }
            return Some(reps);
        }
        if ops.windows(2).all(|w| w[0].pos >= w[1].pos) {
            // Descending: ops are applied back-to-front. Every later op sits
            // to the left of the already-applied ones, so its position is
            // already an original-text coordinate — nothing to adjust.
            let mut prev_start = u64::MAX;
            for op in ops {
                let start = op.pos;
                let end = start.checked_add(op.deleted.len() as u64)?;
                if end > total || end > prev_start || !self.table.is_boundary(start) {
                    return None;
                }
                debug_assert!(
                    self.table.read(start..end) == op.deleted.as_bytes(),
                    "bulk op deleted-text mismatch at {start}"
                );
                prev_start = start;
                reps.push((start..end, op.inserted.as_bytes()));
            }
            reps.reverse();
            return Some(reps);
        }
        None
    }

    /// Apply edits as a single undoable transaction (no coalescing).
    pub fn apply_transaction(&mut self, mut ops: Vec<EditOp>) {
        // All no-op operations (empty insert, same-text replace) are not
        // recorded: they don't change text but would gen += 1 (invalidating
        // the CSV/JSON/Markdown parse caches) and leave an undo step that
        // does nothing when pressed.
        ops.retain(|op| op.deleted != op.inserted);
        if ops.is_empty() {
            return;
        }
        self.apply_ops_raw(&ops);
        self.clear_redo();
        self.push_undo(UndoEntry {
            ops,
            coalesce: None,
            time: std::time::Instant::now(),
        });
        // Book by "number of applied ops", symmetric with undo/redo's per-op
        // accounting (see undo position): replace-all is **one transaction
        // of N ops** — counting only +1 would let hpos drop below the save
        // point after undoing N ops — content back at the save point but
        // shown as modified (false positive); the reverse is worse: one more
        // character lands exactly back on saved_hpos, is_dirty()==false
        // while content changed → close without prompt, crash recovery
        // misjudges as saved.
        self.hpos += self.undo.back().map_or(0, |e| e.ops.len() as u64);
    }

    /// Apply edits that belong to the same logical operation as the previous
    /// transaction (streaming replace-all). They are merged into the last
    /// undo entry, so one Ctrl+Z still restores every batch.
    ///
    /// Returns `false` when there is no open group (empty undo stack, or a
    /// redo entry means the last transaction was undone); the caller must
    /// then start a fresh transaction instead.
    pub fn apply_transaction_continuing(&mut self, mut ops: Vec<EditOp>) -> bool {
        ops.retain(|op| op.deleted != op.inserted);
        if ops.is_empty() {
            return true;
        }
        if self.undo.is_empty() || !self.redo.is_empty() {
            return false;
        }
        self.apply_ops_raw(&ops);
        let added = ops.len() as u64;
        let extra = if let Some(last) = self.undo.back_mut() {
            let before = last.heap_bytes();
            last.ops.extend(ops);
            last.time = std::time::Instant::now();
            last.heap_bytes().saturating_sub(before)
        } else {
            0
        };
        self.history_bytes = self.history_bytes.saturating_add(extra);
        self.hpos += added;
        self.trim_undo_budget();
        true
    }

    /// Apply a typing-style edit that may coalesce with the previous one.
    pub fn apply_typing(&mut self, op: EditOp, key: CoalesceKey) {
        // Empty input is not an edit (same reason as `apply_transaction`)
        if op.deleted == op.inserted {
            return;
        }
        // Only coalesce when the redo stack is empty: after an undo the entry
        // below the popped one may still satisfy the key/time window, and
        // merging into it would leave the redo stack describing edits against
        // a document state that no longer exists (redo would then corrupt the
        // text) and would skip the hpos bookkeeping below.
        let coalesce_with = self.redo.is_empty()
            && self.undo.back().is_some_and(|last| {
                last.coalesce == Some(key)
                    && last.time.elapsed().as_millis() < 800
                    && Self::adjacent(last.ops.last(), &op, key)
            });
        self.apply_ops_raw(std::slice::from_ref(&op));
        // hpos is booked per applied op: the coalesce path must increment
        // too, otherwise typing after save (merged into the old entry) would
        // not set dirty → closing a tab prompts nothing, crash recovery
        // misjudges as saved.
        self.hpos += 1;
        if coalesce_with {
            if let Some(last) = self.undo.back_mut() {
                let before = last.heap_bytes();
                last.ops.push(op);
                last.time = std::time::Instant::now();
                self.history_bytes = self
                    .history_bytes
                    .saturating_add(last.heap_bytes().saturating_sub(before));
                self.trim_undo_budget();
                return;
            }
        }
        self.clear_redo();
        self.push_undo(UndoEntry {
            ops: vec![op],
            coalesce: Some(key),
            time: std::time::Instant::now(),
        });
    }

    /// Drop oldest undo groups until the budget fits (keeping at least one).
    fn trim_undo_budget(&mut self) {
        while self.undo.len() > 1
            && (self.undo.len() > Self::UNDO_MAX_ENTRIES
                || self.history_bytes > Self::UNDO_MAX_BYTES)
        {
            if let Some(old) = self.undo.pop_front() {
                self.history_bytes = self.history_bytes.saturating_sub(old.heap_bytes());
            }
        }
    }

    fn adjacent(prev: Option<&EditOp>, op: &EditOp, key: CoalesceKey) -> bool {
        let Some(prev) = prev else { return false };
        match key {
            CoalesceKey::InsertChar => {
                prev.deleted.is_empty() && op.deleted.is_empty() && prev.len_after() == op.pos
            }
            CoalesceKey::DeleteBack => {
                prev.inserted.is_empty()
                    && op.inserted.is_empty()
                    && prev.pos == op.end()
                    && op.pos < prev.pos
            }
            CoalesceKey::DeleteFwd => {
                prev.inserted.is_empty() && op.inserted.is_empty() && prev.end() == op.pos
            }
            // Same key (same cell-edit session) is enough to merge; each op
            // applies to the previous state, so reverse-order replay is
            // naturally correct — byte positions need not be adjacent.
            CoalesceKey::CellEdit { .. } => true,
        }
    }

    pub fn break_undo_group(&mut self) {
        if let Some(last) = self.undo.back_mut() {
            last.coalesce = None;
        }
    }

    pub fn insert_text(&mut self, pos: u64, text: &str) {
        self.apply_transaction(vec![EditOp::insert(pos, text)]);
    }

    pub fn delete_range(&mut self, range: Range<u64>) -> String {
        let start = self.snap(range.start);
        let end = self.snap(range.end).min(self.table.len());
        if start >= end {
            return String::new();
        }
        let text = self.table.read_str(start..end);
        self.apply_transaction(vec![EditOp::delete(start..end, text.clone())]);
        text
    }

    pub fn replace_range(&mut self, range: Range<u64>, text: &str) {
        let start = self.snap(range.start);
        let end = self.snap(range.end).min(self.table.len());
        let old = self.table.read_str(start..end);
        self.apply_transaction(vec![EditOp {
            pos: start,
            deleted: old,
            inserted: text.to_string(),
        }]);
    }

    // ---- undo/redo ---------------------------------------------------------

    /// Byte budget for undo + redo history. Past this the **oldest** groups are
    /// dropped: history is a convenience, not the document, so losing the tail
    /// is always better than exhausting memory while editing a huge file.
    /// A single group larger than the budget is kept (it is the only way to
    /// undo that edit) and the next push trims around it.
    pub const UNDO_MAX_BYTES: usize = 256 * 1024 * 1024;
    /// Hard entry-count ceiling, so millions of tiny edits cannot pin memory
    /// through per-entry overhead alone.
    pub const UNDO_MAX_ENTRIES: usize = 10_000;

    /// Approximate bytes currently held by the undo/redo stacks.
    pub fn history_bytes(&self) -> usize {
        self.history_bytes
    }

    #[cfg(test)]
    fn undo_len(&self) -> usize {
        self.undo.len()
    }

    fn push_undo(&mut self, entry: UndoEntry) {
        self.history_bytes += entry.heap_bytes();
        self.undo.push_back(entry);
        while self.undo.len() > 1
            && (self.undo.len() > Self::UNDO_MAX_ENTRIES
                || self.history_bytes > Self::UNDO_MAX_BYTES)
        {
            if let Some(old) = self.undo.pop_front() {
                self.history_bytes = self.history_bytes.saturating_sub(old.heap_bytes());
            }
        }
    }

    fn clear_redo(&mut self) {
        for e in self.redo.drain(..) {
            self.history_bytes = self.history_bytes.saturating_sub(e.heap_bytes());
        }
    }

    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }
    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    pub fn undo(&mut self) -> Option<UndoResult> {
        let entry = self.undo.pop_back()?;
        self.history_bytes = self.history_bytes.saturating_sub(entry.heap_bytes());
        let mut first_affected = u64::MAX;
        let mut inv: Vec<EditOp> = Vec::with_capacity(entry.ops.len());
        for op in entry.ops.iter().rev() {
            first_affected = first_affected.min(op.pos);
            inv.push(op.inverse());
        }
        self.apply_ops_raw(&inv);
        // Symmetric with apply_transaction/apply_typing's "one hpos per op":
        // undoing a whole entry steps back by its op count (coalesced
        // entries hold multiple ops).
        self.hpos = self.hpos.saturating_sub(entry.ops.len() as u64);
        let entry = UndoEntry {
            ops: entry.ops,
            coalesce: None,
            time: std::time::Instant::now(),
        };
        self.history_bytes += entry.heap_bytes();
        self.redo.push_back(entry);
        Some(UndoResult { first_affected })
    }

    pub fn redo(&mut self) -> Option<UndoResult> {
        let entry = self.redo.pop_back()?;
        self.history_bytes = self.history_bytes.saturating_sub(entry.heap_bytes());
        let first = entry.ops.iter().map(|o| o.pos).min().unwrap_or(0);
        self.apply_ops_raw(&entry.ops);
        self.hpos += entry.ops.len() as u64;
        self.push_undo(UndoEntry {
            ops: entry.ops,
            coalesce: None,
            time: std::time::Instant::now(),
        });
        Some(UndoResult {
            first_affected: first,
        })
    }

    pub fn read_all(&self) -> String {
        self.table.read_str(0..self.table.len())
    }

    /// Validate the UTF-8 invariant (debug/testing).
    pub fn validate_utf8(&self) -> bool {
        std::str::from_utf8(&self.table.read(0..self.table.len())).is_ok()
    }

    pub fn snapshot(&self) -> DocSnapshot {
        DocSnapshot {
            original: match self.table.original() {
                Original::Mmap(m) => Original::Mmap(m.clone()),
                Original::Owned(v) => Original::Owned(v.clone()),
            },
            pieces: self.table.snapshot_pieces(),
            insert_data: self.table.insert_data(),
            total: self.table.len(),
            doc_id: self.id,
            gen: self.gen,
        }
    }
}

pub struct UndoResult {
    pub first_affected: u64,
}

/// Immutable snapshot of a document for background tasks (search etc.).
///
/// All fields are ordinary `Send` containers (`Arc<Mmap>`, `Arc<Vec<u8>>`,
/// `Vec<Piece>`), so the compiler derives `Send`; no unsafe escape hatch is
/// needed here.
pub struct DocSnapshot {
    pub original: Original,
    pub pieces: Vec<Piece>,
    pub insert_data: Vec<u8>,
    pub total: u64,
    pub doc_id: u64,
    pub gen: u64,
}

impl DocSnapshot {
    /// Reconstruct a standalone read-only table view of the snapshot.
    pub fn table(&self) -> PieceTable {
        PieceTable::from_snapshot(
            self.original.clone(),
            self.pieces.clone(),
            self.insert_data.clone(),
            self.total,
        )
    }
}

/// Count line terminators in a string ("\r\n" counts once).
pub fn count_breaks_in_str(s: &str) -> u64 {
    let b = s.as_bytes();
    let mut n = 0u64;
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'\n' => {
                n += 1;
                i += 1;
            }
            b'\r' => {
                n += 1;
                i += 1;
                if i < b.len() && b[i] == b'\n' {
                    // CRLF is a single terminator: skip the LF
                    i += 1;
                }
            }
            _ => i += 1,
        }
    }
    n
}

impl From<String> for TextDocument {
    fn from(s: String) -> Self {
        let mut d = TextDocument::empty();
        d.table = PieceTable::from_bytes(s.into_bytes());
        d.index = LineIndex::build(&d.table, |_, _| {});
        d
    }
}

impl From<&str> for TextDocument {
    fn from(s: &str) -> Self {
        TextDocument::from(s.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Streaming replacement multi-transactions must merge into one undo step.
    #[test]
    fn continuing_transaction_merges_into_one_undo_step() {
        let mut d = TextDocument::from("a1 b1 c1");
        // The first batch (right side) and second batch (left side) come
        // from different scan passes, both in Original doc coordinates;
        // after merging, one undo must restore everything.
        d.apply_transaction(vec![EditOp {
            pos: 6,
            deleted: "c1".into(),
            inserted: "C".into(),
        }]);
        assert!(d.apply_transaction_continuing(vec![
            EditOp {
                pos: 3,
                deleted: "b1".into(),
                inserted: "B".into(),
            },
            EditOp {
                pos: 0,
                deleted: "a1".into(),
                inserted: "A".into(),
            },
        ]));
        assert_eq!(d.read_all(), "A B C");
        d.undo().unwrap();
        assert_eq!(d.read_all(), "a1 b1 c1");
        assert!(
            !d.can_undo(),
            "both batches should belong to one undo entry"
        );
    }

    /// Dirty-flag accounting for multi-op transactions: after a transaction
    /// books per op, replace-all → undo → content back at the save point
    /// must not be dirty; typing one more character must be dirty again (the
    /// false "saved" where hpos lands exactly back on saved_hpos is a
    /// data-loss class of defect).
    #[test]
    fn transaction_dirty_accounting_matches_per_op_undo() {
        let mut d = TextDocument::from("aaa bbb\n");
        d.mark_saved();
        assert!(!d.is_dirty());
        // One transaction replacing two spots (the replace_all shape)
        d.apply_transaction(vec![
            EditOp {
                pos: 0,
                deleted: "aaa".into(),
                inserted: "xxx".into(),
            },
            EditOp {
                pos: 4,
                deleted: "bbb".into(),
                inserted: "yyy".into(),
            },
        ]);
        assert_eq!(d.read_all(), "xxx yyy\n");
        assert!(d.is_dirty());
        // Undo the whole transaction: content back at the save point, must
        // not be dirty (old code +1/transaction, undo -2/op — hpos dropped
        // below the save point → false dirty)
        d.undo().unwrap();
        assert_eq!(d.read_all(), "aaa bbb\n");
        assert!(
            !d.is_dirty(),
            "is_dirty should be false after returning to the save point"
        );
        // redo→undo→type one more char: hpos may land exactly back on
        // saved_hpos; must still be dirty
        d.redo().unwrap();
        d.undo().unwrap();
        d.insert_text(0, "z");
        assert!(d.is_dirty(), "editing after the save point must set dirty");
    }

    /// Undo history is bounded: past the entry cap, drop the oldest groups
    /// instead of growing forever.
    #[test]
    fn undo_history_is_bounded() {
        let mut d = TextDocument::from("");
        for _ in 0..(TextDocument::UNDO_MAX_ENTRIES + 50) {
            // One standalone transaction per line (inserting a char itself
            // coalesces; we want multiple groups here)
            d.insert_text(0, "x");
        }
        assert_eq!(d.undo_len(), TextDocument::UNDO_MAX_ENTRIES);
        assert!(d.history_bytes() <= TextDocument::UNDO_MAX_BYTES);
        // Still undoable (the oldest undoable groups were dropped, but
        // history is not wholesale invalid)
        assert!(d.undo().is_some());
        assert!(d.history_bytes() < TextDocument::UNDO_MAX_BYTES);
    }

    #[test]
    fn edit_undo_redo() {
        let mut d = TextDocument::from("hello world\nsecond line\n");
        d.insert_text(5, ",");
        assert_eq!(d.read_all(), "hello, world\nsecond line\n");
        d.delete_range(0..6);
        assert_eq!(d.read_all(), " world\nsecond line\n");
        d.replace_range(7..13, "SECOND");
        assert_eq!(d.read_all(), " world\nSECOND line\n");
        d.undo().unwrap();
        assert_eq!(d.read_all(), " world\nsecond line\n");
        d.undo().unwrap();
        assert_eq!(d.read_all(), "hello, world\nsecond line\n");
        d.undo().unwrap();
        assert_eq!(d.read_all(), "hello world\nsecond line\n");
        assert!(
            !d.is_dirty(),
            "undoing back to the opened state clears dirty"
        );
        d.redo().unwrap();
        assert_eq!(d.read_all(), "hello, world\nsecond line\n");
        assert!(d.is_dirty());
    }

    #[test]
    fn multiline_edit_updates_index() {
        let mut d = TextDocument::from("a\nb\nc\nd\n");
        d.insert_text(3, "X\nY\n"); // after "a\nb" (before b's LF)
        assert_eq!(d.read_all(), "a\nbX\nY\n\nc\nd\n");
        assert_eq!(d.line_count(), 6);
        assert_eq!(d.line_text(2), "Y");
        assert_eq!(d.line_of_pos(8), 4); // 'c' starts at 8
        d.undo().unwrap();
        assert_eq!(d.read_all(), "a\nb\nc\nd\n");
        assert_eq!(d.line_count(), 4);
    }

    #[test]
    fn coalesced_typing() {
        let mut d = TextDocument::from("");
        for (i, ch) in "abc".chars().enumerate() {
            d.apply_typing(
                EditOp::insert(i as u64, ch.to_string()),
                CoalesceKey::InsertChar,
            );
        }
        assert_eq!(d.read_all(), "abc");
        d.undo().unwrap();
        assert_eq!(d.read_all(), "");
        assert!(!d.can_undo());
    }

    /// Typing within 800ms after save coalesces with the old entry: the
    /// coalesce path must increment hpos too,
    /// otherwise is_dirty() stays false → closing tabs never prompt, and
    /// crash recovery misjudges the state as saved.
    #[test]
    fn typing_after_save_stays_dirty() {
        let mut d = TextDocument::from("");
        d.apply_typing(EditOp::insert(0, "a"), CoalesceKey::InsertChar);
        d.mark_saved();
        assert!(!d.is_dirty());
        // Subsequent typing that coalesces with the saved entry (adjacent
        // position + within the time window)
        d.apply_typing(EditOp::insert(1, "b"), CoalesceKey::InsertChar);
        assert!(d.is_dirty(), "coalesced typing after save must be dirty");
        // A coalesced entry undoes the whole group at once ("ab" all
        // disappears); the save point was "a", different from the current
        // "", so dirty remains.
        d.undo().unwrap();
        assert_eq!(d.read_all(), "");
        assert!(d.is_dirty(), "saved content was \"a\", now \"\"");
        d.redo().unwrap();
        assert_eq!(d.read_all(), "ab");
        assert!(d.is_dirty());

        // Contrast: undoing a non-coalesced edit back to the save point must
        // clear dirty
        let mut d = TextDocument::from("");
        d.apply_typing(EditOp::insert(0, "a"), CoalesceKey::InsertChar);
        d.mark_saved();
        d.insert_text(1, "b"); // standalone transaction (coalesce: None)
        assert!(d.is_dirty());
        d.undo().unwrap();
        assert_eq!(d.read_all(), "a");
        assert!(!d.is_dirty(), "undo back to saved point clears dirty");
    }

    #[test]
    fn bulk_replace() {
        let mut d = TextDocument::from("x\n".repeat(2000));
        let ops: Vec<EditOp> = (0..2000)
            .map(|i| {
                let pos = i * 2;
                EditOp {
                    pos,
                    deleted: "x".to_string(),
                    inserted: "y".to_string(),
                }
            })
            .collect();
        d.apply_transaction(ops);
        assert_eq!(d.line_count(), 2000);
        assert!(d.line_text(0) == "y" && d.line_text(1999) == "y");
        d.undo().unwrap();
        assert_eq!(d.line_text(0), "x");
    }

    #[test]
    fn breaks_in_str() {
        assert_eq!(count_breaks_in_str("a\nb"), 1);
        assert_eq!(count_breaks_in_str("a\r\nb"), 1);
        assert_eq!(count_breaks_in_str("a\rb"), 1);
        assert_eq!(count_breaks_in_str("a\r\nb\nc\rd"), 3);
        assert_eq!(count_breaks_in_str(""), 0);
        assert_eq!(count_breaks_in_str("\r\n"), 1);
    }

    /// Deleting / inserting *half* of a CRLF pair used to miscount the line
    /// delta (the naive "removed inside range vs inserted count" model):
    /// removing just the `\r` or just the `\n` is a net-0 line change (the
    /// surviving half is still a terminator), inserting between them is +1.
    /// Backspace at a line start in a CRLF file hits exactly this.
    #[test]
    fn half_crlf_edits_keep_line_count() {
        let mk = || TextDocument::from("AB\r\nCD\r\nEF");
        // delete just the \r (pos 2) of the first CRLF
        let mut d = mk();
        d.apply_transaction(vec![EditOp {
            pos: 2,
            deleted: "\r".into(),
            inserted: String::new(),
        }]);
        assert_eq!(d.read_all(), "AB\nCD\r\nEF");
        assert_eq!(d.line_count(), 3, "removing the CR half is net 0");
        assert_eq!(d.line_text(1), "CD");

        // delete just the \n (pos 3)
        let mut d = mk();
        d.apply_transaction(vec![EditOp {
            pos: 3,
            deleted: "\n".into(),
            inserted: String::new(),
        }]);
        assert_eq!(d.read_all(), "AB\rCD\r\nEF");
        assert_eq!(d.line_count(), 3, "removing the LF half is net 0");

        // insert between \r and \n
        let mut d = mk();
        d.apply_transaction(vec![EditOp {
            pos: 3,
            deleted: String::new(),
            inserted: "x".into(),
        }]);
        assert_eq!(d.read_all(), "AB\rx\nCD\r\nEF");
        assert_eq!(d.line_count(), 4, "splitting a CRLF is +1");

        // and backspacing over the \n at a line start merges the lines
        let mut d = mk();
        d.apply_transaction(vec![EditOp {
            pos: 3,
            deleted: "\n".into(),
            inserted: String::new(),
        }]);
        d.apply_transaction(vec![EditOp {
            pos: 2,
            deleted: "\r".into(),
            inserted: String::new(),
        }]);
        assert_eq!(d.read_all(), "ABCD\r\nEF");
        assert_eq!(d.line_count(), 2);
    }

    #[test]
    fn utf8_boundary_ops() {
        let mut d = TextDocument::from("中文abc");
        // position inside a multibyte char snaps down
        let p = d.snap(1);
        assert_eq!(p, 0);
        d.delete_range(0..6);
        assert_eq!(d.read_all(), "abc");
        assert!(d.validate_utf8());
    }

    /// Bulk fast path (descending ops, matching `build_replace_ops` output):
    /// whole-doc replacement goes through the single-pass rebuild, results
    /// correct and restorable with one undo, line index equal to a full
    /// rebuild.
    #[test]
    fn bulk_replace_all_uses_the_single_pass_path() {
        let mut text = String::new();
        for i in 0..600 {
            text.push_str(&format!("row {i} TARGET value\n"));
        }
        let mut d = TextDocument::from(text.clone());
        let hits: Vec<usize> = {
            let mut v = Vec::new();
            let mut at = 0;
            while let Some(p) = text[at..].find("TARGET") {
                v.push(at + p);
                at += p + "TARGET".len();
            }
            v
        };
        assert!(hits.len() > 200, "sample should exceed the bulk threshold");
        let ops: Vec<_> = hits
            .iter()
            .rev()
            .map(|&h| EditOp {
                pos: h as u64,
                deleted: "TARGET".into(),
                inserted: "X".into(),
            })
            .collect();
        d.apply_transaction(ops);
        let expect = text.replace("TARGET", "X");
        assert_eq!(d.read_all(), expect);
        let rebuilt = LineIndex::build(&d.table, |_, _| {});
        assert_eq!(d.index.total_lines, rebuilt.total_lines);
        assert_eq!(d.index.newline_stats(), rebuilt.newline_stats());
        // One undo = back to the original; redo returns to the replaced version
        d.undo().unwrap();
        assert_eq!(d.read_all(), text);
        assert!(!d.is_dirty());
        d.redo().unwrap();
        assert_eq!(d.read_all(), expect);
    }

    /// Ascending form: each op's `pos` is in "before application"
    /// coordinates; the fast path must subtract the net length change of the
    /// earlier edits before mapping back to original-text coordinates.
    #[test]
    fn bulk_ascending_ops_map_back_to_original_coordinates() {
        let mut text = String::new();
        for i in 0..200 {
            text.push_str(&format!("aa{i:03} "));
        }
        let mut d = TextDocument::from(text.clone());
        let hits: Vec<usize> = {
            let mut v = Vec::new();
            let mut at = 0;
            while let Some(p) = text[at..].find("aa") {
                v.push(at + p);
                at += p + 2;
            }
            v
        };
        assert_eq!(hits.len(), 200);
        let grow = "LONGER".len() as i64 - 2; // net byte growth per replacement
        let mut front = 0i64;
        let ops: Vec<_> = hits
            .iter()
            .map(|&h| {
                let op = EditOp {
                    pos: (h as i64 + front) as u64,
                    deleted: "aa".into(),
                    inserted: "LONGER".into(),
                };
                front += grow;
                op
            })
            .collect();
        d.apply_transaction(ops);
        assert_eq!(d.read_all(), text.replace("aa", "LONGER"));
        let rebuilt = LineIndex::build(&d.table, |_, _| {});
        assert_eq!(d.index.total_lines, rebuilt.total_lines);
        d.undo().unwrap();
        assert_eq!(d.read_all(), text);
    }

    /// Non-canonical shapes (unsorted, overlapping, out of bounds) must be
    /// rejected, falling back to the per-op path.
    #[test]
    fn batch_replacements_rejects_non_canonical_shapes() {
        let d = TextDocument::from("abcabcabc");
        // Descending and non-overlapping
        let ok = vec![
            EditOp {
                pos: 6,
                deleted: "abc".into(),
                inserted: "Z".into(),
            },
            EditOp {
                pos: 0,
                deleted: "abc".into(),
                inserted: "Y".into(),
            },
        ];
        assert!(d.batch_replacements(&ok).is_some());
        // Ascending and non-overlapping (equal-length replace: pre-application
        // coords = original coords)
        let ok = vec![
            EditOp {
                pos: 0,
                deleted: "abc".into(),
                inserted: "xyz".into(),
            },
            EditOp {
                pos: 6,
                deleted: "abc".into(),
                inserted: "uvw".into(),
            },
        ];
        assert!(d.batch_replacements(&ok).is_some());
        // Unsorted (3, 6, 0): neither ascending nor descending
        let bad = vec![
            EditOp {
                pos: 3,
                deleted: "abc".into(),
                inserted: "Y".into(),
            },
            EditOp {
                pos: 6,
                deleted: "abc".into(),
                inserted: "Z".into(),
            },
            EditOp {
                pos: 0,
                deleted: "abc".into(),
                inserted: "W".into(),
            },
        ];
        assert!(d.batch_replacements(&bad).is_none());
        // Out of bounds
        let bad = vec![EditOp {
            pos: 0,
            deleted: "abcabcabcX".into(),
            inserted: String::new(),
        }];
        assert!(d.batch_replacements(&bad).is_none());
    }

    /// Hand-rolled benchmark (ignored by default): bulk replace path vs
    /// forced per-op path.
    /// Run: `cargo test --lib bench_bulk_replace -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn bench_bulk_replace() {
        let mut text = String::new();
        for i in 0..8_000 {
            text.push_str(&format!("row {i} TARGET value\n"));
        }
        let hits: Vec<usize> = {
            let mut v = Vec::new();
            let mut at = 0;
            while let Some(p) = text[at..].find("TARGET") {
                v.push(at + p);
                at += p + "TARGET".len();
            }
            v
        };
        let make_ops = || -> Vec<EditOp> {
            hits.iter()
                .rev()
                .map(|&h| EditOp {
                    pos: h as u64,
                    deleted: "TARGET".into(),
                    inserted: "X".into(),
                })
                .collect()
        };
        let mut slow = TextDocument::from(text.clone());
        slow.bulk_edit_threshold = usize::MAX; // force the per-op path
        let t0 = std::time::Instant::now();
        slow.apply_transaction(make_ops());
        let slow_ms = t0.elapsed().as_secs_f64() * 1000.0;
        let mut fast = TextDocument::from(text.clone());
        let t0 = std::time::Instant::now();
        fast.apply_transaction(make_ops());
        let fast_ms = t0.elapsed().as_secs_f64() * 1000.0;
        assert_eq!(slow.read_all(), fast.read_all());
        println!(
            "{} 处替换：逐 op {slow_ms:.1} ms / 批量 {fast_ms:.1} ms（{:.1}x）",
            hits.len(),
            slow_ms / fast_ms.max(0.001)
        );
    }
    /// Line geometry cache (`LineCache`): `line_text` computes the span
    /// alongside, so a following `line_span` hits directly; an edit bumps
    /// gen and both must be recomputed — the cache must not be reused across
    /// gens.
    #[test]
    fn line_cache_shares_span_and_invalidates_on_edit() {
        let mut d = TextDocument::from("ab\ncdef\n");
        assert_eq!(d.line_text(1), "cdef");
        let sp = d.line_span(1);
        assert_eq!(
            (sp.start, sp.end),
            (3, 7),
            "span must match the one from line_text"
        );
        // Same line, same gen: asking again returns the same geometry
        assert_eq!(d.line_span(1), sp);

        d.insert_text(3, "XY");
        assert_eq!(d.line_text(1), "XYcdef");
        let sp2 = d.line_span(1);
        assert_eq!(
            (sp2.start, sp2.end),
            (3, 9),
            "after an edit the cache must invalidate and recompute"
        );
        assert_ne!(sp2, sp);
    }
}
