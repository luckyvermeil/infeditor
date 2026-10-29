//! Core editing on [`EditorState`]: selection, caret movement, typing,
//! clipboard, undo/redo, mouse hit testing and the line-targeting
//! primitives (which lines a line operation acts on) the line operations
//! build on.
//!
//! All byte offsets are UTF-8 boundaries; helpers snap where needed.

use std::ops::Range;

use crate::buffer::doc::{CoalesceKey, EditOp};
use crate::textops::is_word_char;
use crate::ui::minimap::{self, MmDrag, MmGeom, MmModel};
use crate::ui::recorder::MacroStep;
use crate::ui::scrollbar::{self, SbDrag, ThumbGeom};
use crate::ui::state::{
    EditorState, HExtent, MARKER_W, MINIMAP_W, PAD_LEFT, PAD_TOP, SCROLLBAR_W, TAB_SIZE,
};
use crate::ui::{Fmt, Msg};

use super::READONLY_MSG;

/// What a marker-column click hits: a document line (whole-line
/// expand/fold) or a container node inside a virtual row.
#[derive(Clone, Copy)]
enum JsonlTarget {
    Line(u64),
    Node { line: u64, node: u32 },
}

/// Characters of a single line we are willing to measure for the horizontal
/// extent. A minified 5 MB one-liner is a real thing, and measuring all of it
/// every time the scan walks past would cost more than the scan itself; the
/// cost of truncating is only that the extent stops growing for that line.
const H_LINE_CAP: usize = 8192;

/// Lines measured in the opening sample of a freshly seen document.
///
/// The scan only advances while frames are being produced, so on a window that
/// is merely sitting there it would never reach line 50 000 of a file that was
/// just opened and the horizontal bar would stay hidden until the user scrolled
/// that far. One strided pass over up to this many lines costs about a
/// millisecond and yields a usable estimate whatever the file size.
const H_SAMPLE_LINES: u64 = 2000;

/// Cap on lines sampled when a document is opened. The density scales with
/// document size (1 in every 32 lines, see [`Self::sample_h_extent`]) and is
/// capped at this tier: 8192 lines ≈ a one-off 4 ms cost, shrinking the
/// stride on a 300k-line document from 150 to ~36 — "runs of long lines"
/// (minified JSON lines, very long log lines) land in the sample an order of
/// magnitude more often.
const H_SAMPLE_MAX: u64 = 8192;

/// Cap on lines for the full line-by-line scan. Past this, only strided
/// sampling + folding in the visible lines happens; no more full scan.
///
/// The scan advances [`crate::ui::state::heal::H_SCAN_BUDGET`] lines per
/// frame, so 300k lines would take 1172 frames. It used to hang off
/// `needs_heal`, i.e. opening a large file spun idle at 60fps for nearly 20
/// seconds (measured 76% idle CPU). The horizontal bar only needs a "good
/// enough" width estimate: strided sampling already covers the whole
/// document, on-screen accuracy is always guaranteed by folding in the
/// visible lines, and the widest line far away folds in naturally once the
/// user scrolls near it.
const H_SCAN_LINES: u64 = 50_000;

/// Bytes of a line the minimap reads before giving up on the rest.
///
/// The strip is 84 columns wide at 1px per column, so nothing past the first
/// ~340 bytes can ever be drawn; 1024 is a safe ceiling that keeps
/// [`mm_line`] from materialising a 5 MB minified line on every frame.
const MM_LINE_CAP: usize = 1024;

/// Narrowest text column the minimap is willing to leave behind (px).
///
/// A minimap that squeezes the document into a sliver is worse than no
/// minimap, so on a very narrow window it turns itself off. Measured against
/// [`SCROLLBAR_W`] rather than [`Self::vbar_w`] so the answer cannot depend on
/// the horizontal bar, which in turn depends on the width this produces.
const MM_MIN_TEXT_W: f32 = 160.0;

/// Document size past which the minimap turns itself off (bytes).
///
/// The ink model is virtualized, but its per-frame cost still grows with the
/// number of *newly entered* lines a scroll brings in (cache misses need a
/// `pos_of_line` walk + a text read + a scan each), and on a multi-megabyte
/// file that tax lands on every scroll frame for a strip nobody can read at
/// that scale anyway. Same limit the outline parser draws the line at
/// (`state::outline::OUTLINE_MAX_BYTES`).
const MM_MAX_BYTES: u64 = 4 * 1024 * 1024;

/// Most rows one auto-scroll frame moves while a selection is dragged past the
/// text band's edge (`EditorState::drag_autoscroll`).
///
/// At 60 fps this is ~240 rows/s — fast enough to cross a screenful in a
/// couple of hundred milliseconds, slow enough that the dragged end does not
/// shoot past the row the user is aiming at.
const MAX_DRAG_SCROLL_LINES: f32 = 4.0;

/// One edit of a column-selection rectangle (see `EditorState::col_apply_map`).
#[derive(Clone, Copy, PartialEq, Eq)]
enum ColOp {
    /// Typing / block paste
    Insert,
    /// Backspace (extends one char left when zero-width)
    DeleteBack,
    /// Delete (extends one char right when zero-width)
    DeleteFwd,
}

/// Byte offset of character column `col` in `text` (clamped to the end).
fn col_to_byte(text: &str, col: usize) -> usize {
    text.char_indices()
        .nth(col)
        .map(|(i, _)| i)
        .unwrap_or(text.len())
}

/// Character column of byte offset `off` in `text` (non-boundaries snap down).
fn byte_to_col(text: &str, off: usize) -> usize {
    let mut off = off.min(text.len());
    while off > 0 && !text.is_char_boundary(off) {
        off -= 1;
    }
    text[..off].chars().count()
}

/// First `cap` bytes of line `ln`, cut at a UTF-8 boundary.
///
/// Deliberately not [`crate::buffer::TextDocument::line_text`]: that allocates
/// the whole line (and flushes the one-line cache) even when the caller only
/// wants the head. A minified one-liner would otherwise cost megabytes per
/// frame — per row of the minimap, or per line of the width scan.
fn line_prefix(doc: &crate::buffer::TextDocument, ln: u64, cap: usize) -> String {
    let r = doc.line_range(ln);
    let end = (r.start + cap as u64).min(r.end);
    let end = if end < r.end { doc.snap(end) } else { r.end };
    doc.text_range(r.start..end.max(r.start))
}

/// Head of line `ln` for the horizontal-extent scan (see [`H_LINE_CAP`]).
fn line_head(doc: &crate::buffer::TextDocument, ln: u64) -> String {
    line_prefix(doc, ln, H_LINE_CAP)
}

/// First [`MM_LINE_CAP`] bytes of line `ln` (minimap row).
fn mm_line(doc: &crate::buffer::TextDocument, ln: u64) -> String {
    line_prefix(doc, ln, MM_LINE_CAP)
}

impl EditorState {
    // ---- selection helpers ------------------------------------------------

    /// Selected range of the active document, if any.
    pub fn sel_range(&self) -> Option<Range<u64>> {
        self.caret().selection()
    }

    /// Whether an edit is being attempted in read-only mode (for a one-shot
    /// notice, so it does not spam).
    ///
    /// All "modify the document" command entries share this one notice: the
    /// copy lives in a single place ([`READONLY_MSG`]) and repeated triggers
    /// do not rewrite `status` (every write dirties the whole window into a
    /// re-render, see the comment on `state::update_if`).
    pub(crate) fn readonly_notice(&mut self) {
        if self.status.as_ref().map(|m| m.key()) != Some(READONLY_MSG.key().to_string()) {
            self.status = Some(READONLY_MSG.into());
        }
    }

    /// Typing path: auto-close brackets/quotes, then hand off to
    /// [`Self::insert_text`].
    ///
    /// Hooked only on the **typing** entry point; pasting (`Cmd::Paste` goes
    /// through `insert_text`) is never rewritten by auto-close — what the
    /// user pasted is what should land.
    ///
    /// Rules match mainstream editors:
    /// * typing an open bracket inserts the pair with the caret in between
    ///   (one undo step);
    /// * typing a close bracket when the caret sits on the very same close
    ///   bracket **moves over** it instead of inserting another;
    /// * paired quotes are completed only when not followed by an
    ///   alphanumeric (otherwise `it's` would become `it's'`).
    pub fn type_text(&mut self, s: &str) {
        if self.read_only {
            self.readonly_notice();
            return;
        }
        const PAIRS: &[(char, char)] =
            &[('(', ')'), ('[', ']'), ('{', '}'), ('"', '"'), ('\'', '\'')];
        if !self.auto_close {
            self.insert_text(s);
            return;
        }
        let mut it = s.chars();
        let (Some(c), None) = (it.next(), it.next()) else {
            // Multi-char (IME commit) or empty string: insert as-is.
            self.insert_text(s);
            return;
        };
        // With a selection: typing an open bracket should wrap the selection
        // — a simple version for now: no auto-close when a selection exists,
        // so a "replace selection" is not rewritten unexpectedly.
        if self.sel_range().is_some() {
            self.insert_text(s);
            return;
        }
        // Move over an existing close bracket
        if PAIRS.iter().any(|(_, close)| *close == c)
            && self.active_doc().char_at(self.caret().pos) == Some(c)
        {
            self.caret_mut().pos += c.len_utf8() as u64;
            self.scroll_caret_into_view();
            return;
        }
        // Insert the pair
        if let Some((open, close)) = PAIRS.iter().copied().find(|(o, _)| *o == c) {
            let next_is_word = self
                .active_doc()
                .char_at(self.caret().pos)
                .map(|n| n.is_alphanumeric())
                .unwrap_or(false);
            // Quote pairing only when followed by a non-word char; ordinary
            // brackets always pair.
            let paired = open != close || !next_is_word;
            if paired {
                let pair_str = format!("{open}{close}");
                self.insert_text(&pair_str);
                self.caret_mut().pos -= close.len_utf8() as u64;
                self.scroll_caret_into_view();
                return;
            }
        }
        self.insert_text(s);
    }

    /// Insert text at the caret (replacing the selection).
    pub fn insert_text(&mut self, s: &str) {
        if s.is_empty() {
            return;
        }
        if self.read_only {
            self.readonly_notice();
            return;
        }
        // Column mode: insert one copy per row of the rectangle (typing and
        // paste share this path)
        if self.col_apply(ColOp::Insert, s) {
            self.record_step(MacroStep::Text(s.to_string()));
            return;
        }
        let (range, single_char) = {
            let c = self.caret();
            let r = c.selection().unwrap_or(c.pos..c.pos);
            let single =
                r.is_empty() && s.chars().count() == 1 && !s.contains('\n') && !s.contains('\r');
            (r, single)
        };
        let start = range.start;
        let doc = self.active_doc_mut();
        if single_char {
            doc.apply_typing(EditOp::insert(start, s), CoalesceKey::InsertChar);
        } else if range.is_empty() {
            doc.insert_text(start, s);
        } else {
            doc.replace_range(range, s);
        }
        let c = self.caret_mut();
        c.pos = start + s.len() as u64;
        c.anchor = None;
        self.scroll_caret_into_view();
        // Macro recording hook (see the recorder module): typing is the body
        // of a macro.
        self.record_step(MacroStep::Text(s.to_string()));
    }

    /// Delete the selection, or the character before the caret.
    pub fn delete_back(&mut self) {
        if self.read_only {
            self.readonly_notice();
            return;
        }
        // Column mode: delete the rectangle's content on every line
        if self.col_apply(ColOp::DeleteBack, "") {
            self.record_step(MacroStep::Backspace);
            return;
        }
        let pos = self.caret().pos;
        let sel = self.caret().selection();
        let doc = self.active_doc_mut();
        if let Some(sel) = sel {
            let deleted = doc.delete_range(sel.clone());
            let _ = deleted;
            let c = self.caret_mut();
            c.pos = sel.start;
            c.anchor = None;
        } else if pos > 0 {
            let start = prev_boundary(doc, pos);
            // The second argument of `EditOp::delete` is **the deleted text**
            // (undo needs it to replay), while `apply_ops_raw` uses
            // `deleted.len()` to compute the removed range — passing `""`
            // turns this step into "caret moves back one, not a single byte
            // deleted". This used to pass `""`, i.e. Backspace never actually
            // deleted anything anywhere in the project; the original text
            // must be read out and passed in.
            let deleted = doc.table.read_str(start..pos);
            doc.apply_typing(EditOp::delete(start..pos, deleted), CoalesceKey::DeleteBack);
            self.caret_mut().pos = start;
        }
        self.scroll_caret_into_view();
        // Recording hook: Backspace does not go through `Cmd`, so it can only
        // be intercepted here.
        self.record_step(MacroStep::Backspace);
    }

    /// Delete the selection, or the character after the caret.
    pub fn delete_fwd(&mut self) {
        if self.read_only {
            self.readonly_notice();
            return;
        }
        // Column mode: delete the rectangle's content on every line
        if self.col_apply(ColOp::DeleteFwd, "") {
            self.record_step(MacroStep::Delete);
            return;
        }
        let pos = self.caret().pos;
        let sel = self.caret().selection();
        let doc = self.active_doc_mut();
        if let Some(sel) = sel {
            doc.delete_range(sel.clone());
            let c = self.caret_mut();
            c.pos = sel.start;
            c.anchor = None;
        } else if pos < doc.table.len() {
            let end = next_boundary(doc, pos);
            // Same as `delete_back`: the deleted text must be passed in for
            // real, or Delete is a no-op too.
            let deleted = doc.table.read_str(pos..end);
            doc.apply_typing(EditOp::delete(pos..end, deleted), CoalesceKey::DeleteFwd);
        }
        self.scroll_caret_into_view();
        self.record_step(MacroStep::Delete);
    }

    pub fn select_all(&mut self) {
        // Select-all is linear semantics; column mode ends here (same
        // convention as click, Esc)
        self.col_anchor = None;
        let len = self.tab().doc.table.len();
        let c = self.caret_mut();
        c.anchor = Some(0);
        c.pos = len;
    }

    pub fn clear_selection(&mut self) {
        let c = self.caret_mut();
        c.anchor = None;
    }

    // ---- caret movement ---------------------------------------------------

    /// Move/set the caret to `pos`, honouring `extend` (shift) selection.
    pub fn set_caret(&mut self, pos: u64, extend: bool) {
        let len = self.tab().doc.table.len();
        let pos = snap_boundary(self.active_doc(), pos.min(len));
        let c = self.caret_mut();
        if extend {
            if c.anchor.is_none() {
                c.anchor = Some(c.pos);
            }
        } else {
            c.anchor = None;
        }
        c.pos = pos;
        self.scroll_caret_into_view();
    }

    pub fn move_left(&mut self, extend: bool) {
        if !extend {
            if let Some(sel) = self.sel_range() {
                let start = sel.start;
                self.set_caret(start, false);
                return;
            }
        }
        let pos = self.caret().pos;
        if pos == 0 {
            return;
        }
        let new = prev_boundary(self.active_doc(), pos);
        self.set_caret(new, extend);
    }

    pub fn move_right(&mut self, extend: bool) {
        if !extend {
            if let Some(sel) = self.sel_range() {
                let end = sel.end;
                self.set_caret(end, false);
                return;
            }
        }
        let pos = self.caret().pos;
        let len = self.tab().doc.table.len();
        if pos >= len {
            return;
        }
        let new = next_boundary(self.active_doc(), pos);
        self.set_caret(new, extend);
    }

    pub fn move_up(&mut self, extend: bool) {
        self.move_vert(-1, extend);
    }

    pub fn move_down(&mut self, extend: bool) {
        self.move_vert(1, extend);
    }

    fn move_vert(&mut self, dir: i64, extend: bool) {
        if self.wrap_enabled() {
            self.move_vert_visual(dir, extend);
            return;
        }
        let doc = self.active_doc();
        let pos = self.caret().pos;
        let line = doc.line_of_pos(pos);
        let target = if dir < 0 {
            line.saturating_sub(1)
        } else {
            (line + 1).min(doc.line_count().saturating_sub(1))
        };
        if target == line {
            // already at the first/last line: keep the caret where it is
            // (clamped to the line's real extent) instead of jumping
            let start = doc.pos_of_line(line);
            let span = doc.line_span(line);
            let clamped = pos.clamp(start, span.end);
            self.set_caret(clamped, extend);
            return;
        }
        let start = doc.pos_of_line(line);
        let text = doc.line_text(line);
        let x = self
            .metrics
            .x_of_offset(&text, (pos - start) as usize, TAB_SIZE);
        let tstart = doc.pos_of_line(target);
        let ttext = doc.line_text(target);
        let off = self.metrics.offset_of_x(&ttext, x, TAB_SIZE);
        self.set_caret(tstart + off as u64, extend);
    }

    /// Move up/down by **visual** rows under soft wrap: keep the in-line x
    /// first, then land on the matching byte of the target sub-row. When the
    /// target falls on a JSONL virtual row, do nothing (virtual rows are not
    /// editable).
    fn move_vert_visual(&mut self, dir: i64, extend: bool) {
        let total = self.visual_rows();
        if total == 0 {
            return;
        }
        let (v, line, _sub, a, b) = self.visual_row_of_pos(self.caret().pos);
        let target_v = (v as i64 + dir).clamp(0, total as i64 - 1) as u64;
        if target_v == v {
            return;
        }
        let text = self.active_doc().line_text(line);
        let line_start = self.active_doc().pos_of_line(line) as usize;
        let off = (self.caret().pos as usize).saturating_sub(line_start);
        let x = self.metrics.x_of_offset(
            &text[a.min(text.len())..b.min(text.len())],
            off.saturating_sub(a),
            TAB_SIZE,
        );
        let (screen, sub) = self.screen_of_visual(target_v);
        let jrow = self.jsonl_row_of(screen);
        let target = match self.tab().jsonl.row_at(jrow) {
            crate::ui::state::jsonl::RowRef::Doc(l) => l,
            crate::ui::state::jsonl::RowRef::Virtual { .. } => return,
        };
        let ttext = self.active_doc().line_text(target);
        let (ta, tb) = self.wrap_index.row_range(target, sub, ttext.len());
        let off = self.metrics.offset_of_x(
            &ttext[ta.min(ttext.len())..tb.min(ttext.len())],
            x,
            TAB_SIZE,
        );
        let pos = self.active_doc().pos_of_line(target) + (ta + off) as u64;
        self.set_caret(pos, extend);
    }

    pub fn move_home(&mut self, extend: bool, doc_start: bool) {
        if doc_start {
            self.set_caret(0, extend);
            return;
        }
        let pos = self.caret().pos;
        let doc = self.active_doc();
        let line = doc.line_of_pos(pos);
        let start = doc.pos_of_line(line);
        let text = doc.line_text(line);
        let first_nonspace = text.len() - text.trim_start().len();
        let cur = (pos - start) as usize;
        if self.wrap_enabled() {
            let (_v, _line, sub, a, _b) = self.visual_row_of_pos(pos);
            let target = if sub > 0 {
                if cur == a {
                    start // at visual-row start, pressing again → line start
                } else {
                    start + a as u64
                }
            } else if cur == first_nonspace {
                start
            } else {
                start + first_nonspace as u64
            };
            self.set_caret(target, extend);
            return;
        }
        // toggling: when already at first non-blank / line start, go to first non-blank
        let target = if cur == first_nonspace {
            // toggling: already at first non-blank → go to the true line start
            start
        } else {
            start + first_nonspace as u64
        };
        self.set_caret(target, extend);
    }

    pub fn move_end(&mut self, extend: bool, doc_end: bool) {
        let doc = self.active_doc();
        if doc_end {
            let len = doc.table.len();
            self.set_caret(len, extend);
            return;
        }
        let pos = self.caret().pos;
        let line = doc.line_of_pos(pos);
        if self.wrap_enabled() {
            let text_len = doc.line_text(line).len();
            let (_v, _line, sub, _a, b) = self.visual_row_of_pos(pos);
            let rows = self.wrap_index.rows_of(line);
            let start = doc.pos_of_line(line);
            let target = if sub + 1 < rows {
                if (pos - start) as usize == b {
                    // already at visual-row end: pressing again moves to the
                    // next visual row's end (eventually the line end)
                    let (_, nb) = self.wrap_index.row_range(line, sub + 1, text_len);
                    start + nb as u64
                } else {
                    start + b as u64
                }
            } else {
                start + b as u64
            };
            self.set_caret(target, extend);
            return;
        }
        let span = doc.line_span(line);
        self.set_caret(span.end, extend);
    }

    pub fn page(&mut self, dir: i64, extend: bool) {
        let lh = self.metrics.lh.max(1.0);
        let lines = ((self.view.h / lh) as i64 - 1).max(1);
        if self.wrap_enabled() {
            // Under soft wrap a "page" is visual rows; moving row by row
            // keeps it correct across document lines/sub-rows.
            for _ in 0..lines {
                self.move_vert_visual(dir, extend);
            }
            let dy = dir as f64 * lines as f64 * lh as f64;
            self.scroll_top = (self.scroll_top + dy).max(0.0);
            self.clamp_scroll();
            return;
        }
        let doc = self.active_doc();
        let line = doc.line_of_pos(self.caret().pos);
        let total = doc.line_count();
        let target = (line as i64 + dir * lines).clamp(0, total as i64 - 1) as u64;
        let start = doc.pos_of_line(target);
        let text = doc.line_text(target);
        let cur_line = doc.line_of_pos(self.caret().pos);
        let cur_text = doc.line_text(cur_line);
        let cur_start = doc.pos_of_line(cur_line);
        let x =
            self.metrics
                .x_of_offset(&cur_text, (self.caret().pos - cur_start) as usize, TAB_SIZE);
        let off = self.metrics.offset_of_x(&text, x, TAB_SIZE);
        self.set_caret(start + off as u64, extend);
        // scroll by the same amount so the caret keeps its screen row
        let dy = dir as f64 * lines as f64 * lh as f64;
        self.scroll_top = (self.scroll_top + dy).max(0.0);
        self.clamp_scroll();
    }

    /// Ctrl+←: one word left. Only a bounded window (64 KB) before the
    /// caret is read — it once materialised **all** text before the caret
    /// into a String and collected it into a Vec; with the caret at the end
    /// of a 2 GB document, one keypress meant a 2 GB allocation + a
    /// 10⁸-element Vec.
    pub fn move_word_left(&mut self, extend: bool) {
        let pos = self.caret().pos;
        if pos == 0 {
            return;
        }
        let (wstart, text) = {
            let doc = self.active_doc();
            let wstart = doc.snap(pos.saturating_sub(WORD_SCAN_WINDOW));
            (wstart, doc.text_range(wstart..pos))
        };
        let new_off = prev_word(&text);
        self.set_caret(wstart + new_off as u64, extend);
    }

    /// Ctrl+→: one word right (windowed scan, see [`Self::move_word_left`]).
    pub fn move_word_right(&mut self, extend: bool) {
        let pos = self.caret().pos;
        let len = self.active_doc().table.len();
        if pos >= len {
            return;
        }
        let text = {
            let doc = self.active_doc();
            let wend = doc.snap((pos + WORD_SCAN_WINDOW).min(len));
            doc.text_range(pos..wend)
        };
        let new_off = next_word(&text);
        self.set_caret(pos + new_off as u64, extend);
    }

    // ---- newline / tab ----------------------------------------------------

    /// Insert a line break, carrying the previous line's indentation.
    pub fn insert_newline(&mut self) {
        let nl = {
            let doc = self.active_doc();
            let m = doc.meta.user_newline.unwrap_or(doc.meta.detected_newline);
            let _ = doc.meta;
            m.terminator()
        };
        let indent = {
            let doc = self.active_doc();
            let pos = self.caret().pos;
            let line = doc.line_of_pos(pos);
            let text = doc.line_text(line);
            text.chars()
                .take_while(|c| *c == ' ' || *c == '\t')
                .collect::<String>()
        };
        let mut s = String::with_capacity(nl.len() + indent.len());
        s.push_str(nl);
        s.push_str(&indent);
        self.insert_text(&s);
        self.break_undo_group();
    }

    pub fn insert_tab(&mut self) {
        self.insert_text("    ");
        self.break_undo_group();
    }

    pub fn break_undo_group(&mut self) {
        self.tab_mut().doc.break_undo_group();
    }

    // ---- undo / redo ------------------------------------------------------

    pub fn undo(&mut self) {
        if let Some(r) = self.tab_mut().doc.undo() {
            let len = self.tab().doc.table.len();
            let pos = self.tab().doc.snap(r.first_affected.min(len));
            self.col_anchor = None;
            let c = self.caret_mut();
            c.pos = pos;
            c.anchor = None;
            self.scroll_caret_into_view();
        }
    }

    pub fn redo(&mut self) {
        if let Some(r) = self.tab_mut().doc.redo() {
            let len = self.tab().doc.table.len();
            let pos = self.tab().doc.snap(r.first_affected.min(len));
            self.col_anchor = None;
            let c = self.caret_mut();
            c.pos = pos;
            c.anchor = None;
            self.scroll_caret_into_view();
        }
    }

    // ---- clipboard --------------------------------------------------------

    pub fn copy(&mut self) {
        // CSV table view first: copy the text of the selected cells/rows/
        // columns (the editing buffer when a cell is being edited).
        // The global shortcut table routes Ctrl+C straight here; the
        // composer-level key branch never sees it.
        if self.tab().csv.enabled && self.tab().csv.table && self.csv_copy_route() {
            return;
        }
        // Column mode: one segment per line, joined with newlines
        if let Some(text) = self.col_clip_text() {
            if !text.is_empty() {
                self.set_clipboard(text, |n| Fmt::StatusCopiedColumn { bytes: n }.into());
            }
            return;
        }
        if let Some(sel) = self.sel_range() {
            let text = self.tab().doc.text_range(sel);
            self.clip = text.clone();
            let _clip = platform::clipboard::lock();
            if let Ok(mut cb) = arboard::Clipboard::new() {
                let _ = cb.set_text(text);
            }
            let n = self.clip.len();
            self.status = Some(Fmt::StatusCopied { bytes: n }.into());
        }
    }

    pub fn cut(&mut self) {
        // Table view first, like `paste`: Ctrl+X used to fall through to the
        // document, deleting whatever hidden selection it still had while the
        // table was on screen. Copy the cell, then clear it when a single cell
        // is selected (the only shape whose content can be replaced safely);
        // a row/column selection is only copied.
        if self.tab().csv.enabled && self.tab().csv.table {
            if !self.csv_copy_route() {
                return;
            }
            if self.tab().csv.sel.map(|s| s.kind) == Some(crate::ui::state::csv::SelKind::Cell) {
                self.csv_paste_text_into_sel("");
            } else {
                self.status = Some(Msg::StatusCsvWriteUnsafe.into());
            }
            return;
        }
        if self.col_anchor.is_some() && self.col_rect().is_some() {
            let text = self.col_clip_text().unwrap_or_default();
            if text.is_empty() {
                // A zero-width rectangle has nothing to cut — and must not
                // fall back to deleting like Backspace
                return;
            }
            self.set_clipboard(text, |n| Fmt::StatusCutColumn { bytes: n }.into());
            self.col_apply(ColOp::DeleteBack, "");
            return;
        }
        if self.sel_range().is_some() {
            self.copy();
            self.delete_back();
        }
    }

    pub fn paste(&mut self) {
        // CSV table view first: paste into the selected cell / the buffer of
        // the cell being edited. Table mode has no visible document caret;
        // landing in the hidden document only confuses, so swallow it when
        // there is no selection either.
        if self.tab().csv.enabled && self.tab().csv.table && self.csv_paste_route() {
            return;
        }
        let mut text = String::new();
        let _clip = platform::clipboard::lock();
        if let Ok(mut cb) = arboard::Clipboard::new() {
            if let Ok(t) = cb.get_text() {
                text = t;
            }
        }
        if text.is_empty() {
            text = self.clip.clone();
        }
        if !text.is_empty() {
            // normalize newlines into the document's style
            let doc = self.active_doc();
            let nl = doc.meta.user_newline.unwrap_or(doc.meta.detected_newline);
            let text = match nl {
                crate::buffer::Newline::Lf => text.replace("\r\n", "\n"),
                crate::buffer::Newline::Cr => text.replace("\r\n", "\r").replace('\n', "\r"),
                crate::buffer::Newline::CrLf => {
                    text.replace('\n', "\r\n").replace("\r\r\n", "\r\n")
                }
            };
            // Column mode + multi-line block + rectangle no smaller than the
            // block: paste line-by-line in place (the point of column paste).
            // When the block is larger than the rectangle (or the rectangle
            // is a single line), fall back to normal paste or lines are lost.
            if self.col_anchor.is_some() {
                let lines: Vec<String> = if matches!(nl, crate::buffer::Newline::CrLf) {
                    text.split("\r\n").map(String::from).collect()
                } else {
                    let ch = if matches!(nl, crate::buffer::Newline::Cr) {
                        '\r'
                    } else {
                        '\n'
                    };
                    text.split(ch).map(String::from).collect()
                };
                let rect_len = self.col_rect().map(|r| r.len()).unwrap_or(0);
                if lines.len() >= 2
                    && rect_len >= lines.len()
                    && self.col_apply_map(ColOp::Insert, |i| lines.get(i).cloned())
                {
                    self.break_undo_group();
                    return;
                }
            }
            self.insert_text(&text);
            self.break_undo_group();
        }
    }

    /// Edit → Copy All.
    pub fn copy_all(&mut self) {
        let text = self.active_doc().read_all();
        self.set_clipboard(text, |n| Fmt::StatusCopiedAll { bytes: n }.into());
    }

    /// Edit → Copy File Name.
    pub fn copy_file_name(&mut self) {
        let name = self.active_doc().meta.display_name.clone();
        self.set_clipboard(name, |n| Fmt::StatusCopiedFileName { bytes: n }.into());
    }

    /// Edit → Copy Full Path.
    pub fn copy_file_path(&mut self) {
        match self.active_doc().meta.path.clone() {
            Some(p) => {
                let s = p.to_string_lossy().into_owned();
                self.set_clipboard(s, |n| Fmt::StatusCopiedFullPath { bytes: n }.into());
            }
            None => self.status = Some(Msg::StatusNoFilePath.into()),
        }
    }

    fn set_clipboard(&mut self, text: String, msg: impl FnOnce(usize) -> i18n::Message) {
        let n = text.len();
        self.clip = text.clone();
        let _clip = platform::clipboard::lock();
        if let Ok(mut cb) = arboard::Clipboard::new() {
            let _ = cb.set_text(text);
        }
        self.status = Some(msg(n));
    }

    /// Edit → Begin/End Selection.
    ///
    /// The first invocation drops an anchor at the caret, the second turns it
    /// into a selection — selecting a long range without dragging the mouse.
    /// The anchor is remembered per document and dropped when the tab changes.
    pub fn begin_end_select(&mut self) {
        // Linear and column selections are mutually exclusive
        self.col_anchor = None;
        let doc_id = self.tab().doc.id;
        let pos = self.caret().pos;
        match self.sel_anchor {
            None => {
                self.sel_anchor = Some((doc_id, pos));
                self.status = Some(Msg::StatusSelAnchorSet.into());
            }
            Some((id, a)) if id == doc_id => {
                self.sel_anchor = None;
                let c = self.caret_mut();
                if a == pos {
                    c.anchor = None;
                    self.status = Some(Msg::StatusSelAnchorSame.into());
                } else {
                    c.anchor = Some(a);
                    c.pos = pos;
                    let n = c.selection().map(|r| r.end - r.start).unwrap_or(0);
                    self.status = Some(Fmt::StatusSelected { bytes: n }.into());
                }
                self.scroll_caret_into_view();
            }
            Some(_) => {
                // the anchor belonged to a document that is no longer active
                self.sel_anchor = Some((doc_id, pos));
                self.status = Some(Msg::StatusSelAnchorSet.into());
            }
        }
    }

    /// Edit → Begin/End Selection (column mode).
    ///
    /// The first press drops the rectangle anchor; from then on the rectangle
    /// grows wherever the caret goes; pressing again (or clicking the text,
    /// <kbd>Esc</kbd>, select-all) ends it. Copy/cut/typing inside the
    /// rectangle apply line by line — mutually exclusive with the linear
    /// selection: with both alive, "copy" would not know which to honour.
    pub fn begin_end_select_column(&mut self) {
        if self.col_anchor.take().is_some() {
            self.clear_selection();
            self.status = Some(Msg::StatusColumnSelectEnded.into());
            return;
        }
        self.sel_anchor = None;
        self.clear_selection();
        self.col_anchor = Some(self.caret().pos);
        self.status = Some(Msg::StatusColumnSelectStarted.into());
    }

    /// Per-line ranges of the rectangle: `(line, start-in-line, end-in-line)`,
    /// ascending by line.
    ///
    /// Columns are counted in **characters** (consistent with column
    /// selection): CJK wide characters take two display columns but are
    /// still 1 character — using display columns would skew the rectangle
    /// for mixed CJK/Latin text. Short lines are clamped to the line end, so
    /// the rectangle can be stair-shaped — exactly what column selection
    /// should look like.
    pub fn col_rect(&self) -> Option<Vec<(u64, u64, u64)>> {
        self.col_rect_range(0, u64::MAX)
    }

    /// [`Self::col_rect`] clipped to document lines `[first, last)`.
    ///
    /// The render path only paints visible lines: a column selection can
    /// span hundreds of thousands of lines, and reading every line of the
    /// whole span into a String + building a HashMap each frame would
    /// needlessly allocate tens of MB while scrolling. Editing commands
    /// still use [`Self::col_rect`] (they need the full rectangle).
    pub fn col_rect_range(&self, clip_first: u64, clip_last: u64) -> Option<Vec<(u64, u64, u64)>> {
        let anchor = self.col_anchor?;
        let doc = self.active_doc();
        let len = doc.table.len();
        let (a, c) = (anchor.min(len), self.caret().pos.min(len));
        let (a_line, c_line) = (doc.line_of_pos(a), doc.line_of_pos(c));
        let a_off = a - doc.pos_of_line(a_line);
        let c_off = c - doc.pos_of_line(c_line);
        let a_col = byte_to_col(&doc.line_text(a_line), a_off as usize);
        let c_col = byte_to_col(&doc.line_text(c_line), c_off as usize);
        let (lo, hi) = (a_col.min(c_col), a_col.max(c_col));
        let (sel_first, sel_last) = if a_line <= c_line {
            (a_line, c_line)
        } else {
            (c_line, a_line)
        };
        let first = sel_first.max(clip_first);
        let last = sel_last.min(clip_last.saturating_sub(1));
        if first > last {
            return Some(Vec::new());
        }
        let mut out = Vec::with_capacity((last - first + 1) as usize);
        for line in first..=last {
            let text = doc.line_text(line);
            out.push((
                line,
                col_to_byte(&text, lo) as u64,
                col_to_byte(&text, hi) as u64,
            ));
        }
        Some(out)
    }

    /// Apply one edit to every line of the rectangle: `repl` yields the text
    /// to put into each line (`None` = leave this line alone, for block
    /// pastes shorter than the rectangle), then moves the anchor/caret back
    /// to the edited rectangle's corners.
    ///
    /// Where the corners go is decided by the operation type — the whole
    /// secret of how column editing feels:
    ///
    /// * after an **insert**, both corners land on the **right edge** of the
    ///   inserted text — the rectangle collapses to zero width, so the next
    ///   keystroke appends after what was just typed (instead of replacing
    ///   it), and Backspace extends one cell left and deletes it, exactly
    ///   like normal typing;
    /// * after a **delete**, the corners follow the deleted interval's
    ///   boundary and the selection shrinks back.
    fn col_apply_map(&mut self, op: ColOp, mut repl: impl FnMut(usize) -> Option<String>) -> bool {
        let Some(mut rect) = self.col_rect() else {
            return false;
        };
        // A zero-width rectangle (just the caret column) deletes by extending
        // one character to one side first, otherwise "type then Backspace"
        // would be a no-op. If it cannot extend (line start/end), skip that
        // line.
        if op != ColOp::Insert {
            let doc = self.active_doc();
            for e in rect.iter_mut() {
                if e.1 != e.2 {
                    continue;
                }
                let text = doc.line_text(e.0);
                let (mut c0, mut c1) = (e.1 as usize, e.2 as usize);
                while c1 < text.len() && !text.is_char_boundary(c1) {
                    c1 += 1;
                }
                while c0 > 0 && !text.is_char_boundary(c0) {
                    c0 -= 1;
                }
                if op == ColOp::DeleteBack {
                    if c0 == 0 {
                        continue;
                    }
                    let mut p = c0 - 1;
                    while p > 0 && !text.is_char_boundary(p) {
                        p -= 1;
                    }
                    e.1 = p as u64;
                } else {
                    if c1 >= text.len() {
                        continue;
                    }
                    let mut n = c1 + 1;
                    while n < text.len() && !text.is_char_boundary(n) {
                        n += 1;
                    }
                    e.2 = n as u64;
                }
            }
        }
        // **In-line** offsets of the endpoints: in-line edits do not move
        // line starts, so they are the same number before and after the edit
        // — exactly the precondition the shift correction below relies on.
        let len = self.active_doc().table.len();
        let a_pos = self.col_anchor.unwrap_or(0).min(len);
        let c_pos = self.caret().pos.min(len);
        let (a_line, a_off, c_line, c_off) = {
            let doc = self.active_doc();
            let (al, cl) = (doc.line_of_pos(a_pos), doc.line_of_pos(c_pos));
            (
                al,
                a_pos - doc.pos_of_line(al),
                cl,
                c_pos - doc.pos_of_line(cl),
            )
        };
        // Bottom-up: the absolute offsets of upper lines are unaffected by
        // edits to lower lines. Collect all ops first, then write back in one
        // apply_transaction — N lines = 1 undo step (previously one
        // transaction per line: a 50-line column paste took 50 Ctrl+Z
        // presses). Applying bottom-up keeps each op's absolute position
        // independent of preceding ops — the same invariant as the original
        // implementation.
        {
            let doc = self.active_doc_mut();
            let mut ops: Vec<crate::buffer::doc::EditOp> = Vec::new();
            for (idx, &(line, c0, c1)) in rect.iter().enumerate().rev() {
                let Some(r) = repl(idx) else { continue };
                if r.is_empty() && c0 == c1 {
                    continue;
                }
                let start = doc.pos_of_line(line) + c0;
                let end = doc.pos_of_line(line) + c1;
                if c0 == c1 {
                    ops.push(crate::buffer::doc::EditOp::insert(start, &r));
                } else {
                    let deleted = doc.table.read_str(start..end);
                    ops.push(crate::buffer::doc::EditOp {
                        pos: start,
                        deleted,
                        inserted: r,
                    });
                }
            }
            if !ops.is_empty() {
                doc.apply_transaction(ops);
            }
        }
        let index_of = |line: u64| rect.iter().position(|&(l, ..)| l == line);
        let doc = self.active_doc();
        let mut endpoint = |line: u64, off: u64| -> u64 {
            let Some(idx) = index_of(line) else {
                return off;
            };
            let &(_, c0, c1) = &rect[idx];
            match repl(idx) {
                None => off, // this line untouched
                Some(r) if op == ColOp::Insert => c0 + r.len() as u64,
                Some(r) => {
                    // Delete: off <= c0 stays; off >= c1 shifts by the net
                    // delta
                    let s_len = r.len() as u64;
                    if off <= c0 {
                        off
                    } else if off >= c1 {
                        off + s_len - (c1 - c0)
                    } else {
                        c0 + s_len
                    }
                }
            }
        };
        let new_len = doc.table.len();
        let a_new = (doc.pos_of_line(a_line) + endpoint(a_line, a_off)).min(new_len);
        let c_new = (doc.pos_of_line(c_line) + endpoint(c_line, c_off)).min(new_len);
        self.col_anchor = Some(a_new);
        let c = self.caret_mut();
        c.pos = c_new;
        c.anchor = None;
        self.scroll_caret_into_view();
        true
    }

    /// Put the same text on every line (typing / deleting).
    fn col_apply(&mut self, op: ColOp, text: &str) -> bool {
        let r = text.to_string();
        self.col_apply_map(op, |_| Some(r.clone()))
    }

    /// Clipboard text of a column selection: one segment per line, joined
    /// with newlines (pasting it back expands line by line).
    fn col_clip_text(&self) -> Option<String> {
        let rect = self.col_rect()?;
        let doc = self.active_doc();
        let text: Vec<String> = rect
            .iter()
            .map(|&(line, c0, c1)| {
                let t = doc.line_text(line);
                let len = t.len() as u64;
                let (c0, c1) = (c0.min(len), c1.min(len));
                t.get(c0 as usize..c1 as usize).unwrap_or("").to_string()
            })
            .collect();
        Some(text.join("\n"))
    }

    // ---- scrolling --------------------------------------------------------

    /// Full height of the document in px, including the padding above the first
    /// and below the last line.
    pub fn content_h(&self) -> f64 {
        // Visual rows: JSONL expansion/hidden rows/soft wrap all folded in
        // (see state::wrap).
        self.visual_rows() as f64 * self.metrics.lh as f64 + PAD_TOP as f64 * 2.0
    }

    /// Full width of the document in px: the widest line measured so far plus
    /// the padding on both sides. See [`crate::ui::state::HExtent`] for why
    /// this is an estimate that converges.
    pub fn content_w(&self) -> f32 {
        self.h_extent.width + PAD_LEFT * 2.0
    }

    /// Height of the text area: the viewport minus the horizontal scrollbar
    /// when it has to be shown.
    pub fn text_h(&self) -> f32 {
        (self.view.h - self.hbar_h()).max(1.0)
    }

    /// Width of the text column: the viewport minus the gutter, the vertical
    /// scrollbar and the minimap — each of which takes space only when it is
    /// actually being drawn.
    pub fn text_w(&self) -> f32 {
        (self.view.w - self.gutter_w() - self.vbar_w() - self.mm_w()).max(1.0)
    }

    /// Height taken by the horizontal scrollbar (0 when it is not shown).
    pub fn hbar_h(&self) -> f32 {
        if self.hbar_on() {
            SCROLLBAR_W
        } else {
            0.0
        }
    }

    /// Width taken by the vertical scrollbar (0 when it is not shown).
    pub fn vbar_w(&self) -> f32 {
        if self.vbar_on() {
            SCROLLBAR_W
        } else {
            0.0
        }
    }

    /// Whether the vertical scrollbar has to be drawn.
    ///
    /// The two bars compete for the same corner, so each one's need is judged
    /// *after* the other has taken its strip: a bar appearing shrinks the
    /// viewport on the other axis and can turn a "fits" into a "doesn't".
    /// Resolving both against the un-shrunk viewport would leave the last line
    /// or the tail of the longest line permanently unreachable.
    pub fn vbar_on(&self) -> bool {
        let h = self.view.h - if self.hbar_on() { SCROLLBAR_W } else { 0.0 };
        self.content_h() > h as f64
    }

    /// Whether the horizontal scrollbar has to be drawn.
    pub fn hbar_on(&self) -> bool {
        // No horizontal overflow under soft wrap: every line is folded into
        // the text column width.
        if self.wrap_enabled() {
            return false;
        }
        // `vbar_on` reads `hbar_on`, so this one is answered from the
        // *un-shrunk* width plus the vertical bar's own need — see `vbar_on`.
        let need_v = self.content_h() > self.view.h as f64;
        let mut w = self.view.w - self.gutter_w() - self.mm_w();
        if need_v {
            w -= SCROLLBAR_W;
        }
        self.content_w() > w
    }

    /// Maximum vertical scroll, in f64 px (see [`EditorState::scroll_top`]
    /// for why the precision matters on huge documents).
    pub fn max_scroll_top(&self) -> f64 {
        (self.content_h() - self.text_h() as f64).max(0.0)
    }

    /// Maximum horizontal scroll, in px.
    pub fn max_scroll_left(&self) -> f32 {
        if self.wrap_enabled() {
            return 0.0;
        }
        (self.content_w() - self.text_w()).max(0.0)
    }

    pub fn clamp_scroll(&mut self) {
        let max = self.max_scroll_top();
        self.scroll_top = self.scroll_top.clamp(0.0, max);
        if self.wrap_enabled() {
            self.scroll_left = 0.0;
        }
        let max_x = self.max_scroll_left();
        self.scroll_left = self.scroll_left.clamp(0.0, max_x);
    }

    /// Content-following scroll: `dx`/`dy` is the distance the **content**
    /// should move in px (positive = right/down), i.e. the common semantics
    /// of wheel `WheelDelta` and touch panning (finger displacement negated).
    ///
    /// Returns whether state actually changed: scrolling further at a
    /// boundary must not dirty (`Signal::write()`'s guard drop marks dirty
    /// unconditionally, re-rendering the whole tree one more round).
    pub fn scroll_by(&mut self, dx: f64, dy: f64) -> bool {
        let (t0, l0) = (self.scroll_top, self.scroll_left);
        self.scroll_top -= dy;
        if !self.wrap_enabled() {
            self.scroll_left -= dx as f32;
        }
        self.clamp_scroll();
        self.scroll_top != t0 || self.scroll_left != l0
    }

    /// Whether scrolling by `(dx, dy)` now would change anything
    /// (content-following semantics, same clamping as [`Self::scroll_by`]).
    ///
    /// Wheel/touch use it to drop "already at the boundary" events **before
    /// enqueueing**: otherwise every scroll reaching the top would still
    /// queue an empty render frame.
    pub fn can_scroll(&self, dx: f64, dy: f64) -> bool {
        let nt = (self.scroll_top - dy).clamp(0.0, self.max_scroll_top());
        let nl = if self.wrap_enabled() {
            0.0
        } else {
            (self.scroll_left - dx as f32).clamp(0.0, self.max_scroll_left())
        };
        nt != self.scroll_top || nl != self.scroll_left
    }

    // ---- drag-selection auto-scroll ---------------------------------------

    /// Top edge of the text band (the first row's top) in client px.
    fn drag_band_top(&self) -> f32 {
        self.view.y + PAD_TOP
    }

    /// Bottom edge of the text band: the viewport bottom minus the horizontal
    /// scrollbar when it is shown (`text_h` already accounts for it).
    fn drag_band_bottom(&self) -> f32 {
        self.view.y + self.text_h()
    }

    /// Is a pressed drag selection's pointer parked outside the text band?
    ///
    /// Read-only probe for the frame hook (`update_if`): a drag *inside* the
    /// band must not dirty the state — the DOM `pointermove` already extends
    /// the selection there, and a write per pixel would rebuild the viewport.
    pub fn drag_autoscroll_due(&self) -> bool {
        if !self.drag {
            return false;
        }
        crate::ui::scroll::drag_ptr_peek()
            .is_some_and(|(_, y)| y < self.drag_band_top() || y > self.drag_band_bottom())
    }

    /// The document-view twin of [`Self::drag_autoscroll_due`]: the wys
    /// content fills the whole viewport (no gutter/scrollbar band), and its
    /// scroll step lives in the kernel (`components::mdws::drag_tick`).
    pub fn wys_drag_due(&self) -> bool {
        self.md_drag
            && crate::ui::scroll::drag_ptr_peek()
                .is_some_and(|(_, y)| y < self.view.y || y > self.view.y + self.view.h)
    }

    /// One auto-scroll step of a drag selection.
    ///
    /// The pointer is held outside the text band (past the last line, above the
    /// first, or out over the status bar/toolbar), so the view scrolls toward
    /// it and the dragged end follows the row that is now at the edge. Returns
    /// whether a step was queued; the caller re-arms the frame, and *that* is
    /// what makes a stationary pointer keep scrolling — a purely
    /// pointer-driven implementation would move once per event and stall the
    /// moment the user stops wiggling the mouse.
    ///
    /// The far end is pinned to the edge row **before** the scroll lands (the
    /// next frame applies it, see the frame hook), so the selection grows one
    /// row per frame instead of jumping to the document end the way
    /// [`Self::hit_test`] would clamp an out-of-band y.
    pub fn drag_autoscroll(&mut self) -> bool {
        let Some((px, py)) = crate::ui::scroll::drag_ptr_peek() else {
            return false;
        };
        let lh = self.metrics.lh.max(1.0);
        let (top, bottom) = (self.drag_band_top(), self.drag_band_bottom());
        // Overshoot in rows, capped: far away scrolls fast, a nudge past the
        // edge scrolls slow (VS Code / browser feel).
        let lines = |over: f32| (over / lh).ceil().clamp(1.0, MAX_DRAG_SCROLL_LINES);
        // Sign is content displacement: dragging below the band must reveal
        // **later** rows (`scroll_top` grows ⇒ `dy` negative).
        let dy = if py < top {
            f64::from(lh * lines(top - py))
        } else if py > bottom {
            -f64::from(lh * lines(py - bottom))
        } else {
            return false;
        };
        if !self.can_scroll(0.0, dy) {
            // At the document edge: no step, no frame — a selection dragged
            // past the end of the file must not spin the render loop.
            return false;
        }
        let edge_y = (py.clamp(top + lh * 0.5, bottom - lh * 0.5)) as f64;
        if let Some(p) = self.hit_test(px as f64, edge_y) {
            self.set_caret(p, true);
        }
        crate::ui::scroll::queue(crate::ui::scroll::Target::Doc, 0.0, dy);
        // Always "moved": the queue's own return value means "the accumulator
        // was empty", which is not the same question (the caller uses this to
        // re-arm the frame).
        true
    }

    /// Frame hook: flushes the scroll deltas accumulated within one frame
    /// ([`crate::ui::scroll`]) into state in one go. High-resolution wheels/
    /// trackpads/touchscreens can deliver 3–10 events per frame; writing the
    /// signal per event means a full re-render per event; coalesced, at most
    /// one per frame.
    ///
    /// Returns whether anything changed (the caller decides whether to
    /// `write()` — this function requires `&mut self`, already inside a
    /// write guard).
    pub fn apply_pending_scroll(&mut self) -> bool {
        let d = crate::ui::scroll::drain();
        let mut changed = false;
        if d[0] != (0.0, 0.0) {
            changed |= self.scroll_by(d[0].0, d[0].1);
        }
        if d[1] != (0.0, 0.0) {
            let before = self.tab().csv.scroll_top;
            let tab = self.tab_mut();
            tab.csv.scroll_top -= d[1].1;
            self.clamp_csv_scroll();
            changed |= self.tab().csv.scroll_top != before;
        }
        if d[2] != (0.0, 0.0) {
            // The find results panel scrolls by **rows** (see
            // `components::findbar::RESULT_ROW_H`); pixel deltas round to
            // rows, clamped to the last page of the list. Sign matches the
            // body: `dy` is content displacement (content moves up → later
            // rows come in → panel_top grows).
            const ROW_H: f64 = crate::ui::components::findbar::RESULT_ROW_H as f64;
            let rows = (d[2].1 / ROW_H).round() as i64;
            if rows != 0 {
                let max = self
                    .find
                    .results
                    .len()
                    .saturating_sub(self.find.rows_per_page());
                let next = (self.find.panel_top as i64 - rows).clamp(0, max as i64) as usize;
                if next != self.find.panel_top {
                    self.find.panel_top = next;
                    changed = true;
                }
            }
        }
        changed
    }

    /// Schedule a frame (called by the event side once scroll deltas are
    /// enqueued; see [`crate::ui::scroll`]).
    pub fn request_frame(&self) {
        if let Some(w) = &self.wake {
            w();
        }
    }

    // ---- scrollbars -------------------------------------------------------

    /// Draggable length of the vertical bar (its full length minus both arrows).
    pub fn v_track_len(&self) -> f64 {
        (self.text_h() as f64 - 2.0 * SCROLLBAR_W as f64).max(0.0)
    }

    /// Draggable length of the horizontal bar.
    pub fn h_track_len(&self) -> f64 {
        (self.text_w() as f64 - 2.0 * SCROLLBAR_W as f64).max(0.0)
    }

    pub fn v_geom(&self) -> ThumbGeom {
        scrollbar::thumb_geom(
            self.v_track_len(),
            self.content_h(),
            self.text_h() as f64,
            self.scroll_top,
        )
    }

    pub fn h_geom(&self) -> ThumbGeom {
        scrollbar::thumb_geom(
            self.h_track_len(),
            self.content_w() as f64,
            self.text_w() as f64,
            self.scroll_left as f64,
        )
    }

    /// Start dragging a thumb. `at` is the pointer position along the bar's own
    /// axis (0 = the bar's first pixel, i.e. the top of the vertical bar), so
    /// the grab offset can be recovered without the caller knowing the layout.
    pub fn sb_begin(&mut self, vertical: bool, at: f64) {
        let g = if vertical {
            self.v_geom()
        } else {
            self.h_geom()
        };
        let start = SCROLLBAR_W as f64 + g.off;
        // Grabbing the bare track jumps the thumb to the pointer (centred) and
        // starts a drag from there — the Windows behaviour, and the only way a
        // track click can turn into a drag without a second press.
        let grab = if at >= start && at <= start + g.len {
            at - start
        } else {
            g.len / 2.0
        };
        self.sb_drag = Some(SbDrag { vertical, grab });
        self.sb_press = true;
    }

    /// Track the pointer while a thumb is being dragged. `p.cx`/`p.cy` are
    /// client (window content) coordinates in CSS px.
    ///
    /// **Content side, called once per frame** from the `RedrawRequested` hook
    /// with the latest sample from [`crate::ui::scroll::sb_take`] — never per
    /// pointer event (that path only updates the input-side thumb, see
    /// [`Self::sb_live_off`]). A drag routinely leaves the 14px strip, so the
    /// sample is fed from the window-level `CursorMoved` event rather than the
    /// bar's own `onmousemove`: a DOM handler there would simply stop firing
    /// mid-drag.
    pub fn sb_apply(&mut self, p: crate::ui::scroll::SbPointer) {
        let Some(d) = self.sb_drag else {
            return;
        };
        if p.vertical {
            let g = self.v_geom();
            let off = p.cy as f64 - self.view.y as f64 - SCROLLBAR_W as f64 - d.grab;
            self.scroll_top = scrollbar::scroll_of_thumb(&g, off);
        } else {
            let g = self.h_geom();
            let off = p.cx as f64 - self.view.x as f64 - SCROLLBAR_W as f64 - d.grab;
            self.scroll_left = scrollbar::scroll_of_thumb(&g, off) as f32;
        }
        self.clamp_scroll();
    }

    /// The **input-side** thumb offset for the newest pointer position, while
    /// a drag is live. Clamped to the track; rendered by the thumb's cheap
    /// scope so the thumb tracks the pointer even before the content frame
    /// lands ([`Self::sb_apply`] is what moves the content, once per frame).
    ///
    /// Returns `None` when no drag is live — callers must not invent a thumb
    /// position without one.
    pub fn sb_live_off(&self, vertical: bool, cx: f32, cy: f32) -> Option<scrollbar::SbLive> {
        let d = self.sb_drag.as_ref()?;
        if d.vertical != vertical {
            return None;
        }
        let g = if vertical {
            self.v_geom()
        } else {
            self.h_geom()
        };
        let raw = if vertical {
            cy as f64 - self.view.y as f64 - SCROLLBAR_W as f64 - d.grab
        } else {
            cx as f64 - self.view.x as f64 - SCROLLBAR_W as f64 - d.grab
        };
        let off = raw.clamp(0.0, (g.track - g.len).max(0.0));
        Some(scrollbar::SbLive { vertical, off })
    }

    /// Release the thumb.
    ///
    /// Discards (not lands) a pending pointer sample: every *release* path
    /// flushes it explicitly before calling this — a teardown without a
    /// release (tab switch, locale switch) must not scroll the new surface.
    pub fn sb_end(&mut self) {
        crate::ui::scroll::sb_clear();
        self.sb_drag = None;
        self.sb_press = false;
    }

    /// One click on an arrow button: a line vertically, a few columns across.
    pub fn sb_step(&mut self, vertical: bool, dir: i32) {
        if vertical {
            self.scroll_top += dir as f64 * self.metrics.lh as f64;
        } else {
            self.scroll_left += dir as f32 * self.metrics.adv * 3.0;
        }
        self.clamp_scroll();
    }

    /// One click on the bare track: a page towards the click.
    pub fn sb_page(&mut self, vertical: bool, dir: i32) {
        if vertical {
            // one line of overlap keeps the reader's place, like every other
            // editor's page-up/page-down
            let page = (self.text_h() as f64 - self.metrics.lh as f64).max(self.metrics.lh as f64);
            self.scroll_top += dir as f64 * page;
        } else {
            let page = (self.text_w() - self.metrics.adv * 4.0).max(self.metrics.adv * 4.0);
            self.scroll_left += dir as f32 * page;
        }
        self.clamp_scroll();
    }

    /// Click on the bare track, `at` measured as in [`Self::sb_begin`]: page
    /// backwards when the click is before the thumb, forwards when after.
    pub fn sb_track_click(&mut self, vertical: bool, at: f64) {
        let g = if vertical {
            self.v_geom()
        } else {
            self.h_geom()
        };
        let dir = if at < SCROLLBAR_W as f64 + g.off {
            -1
        } else {
            1
        };
        self.sb_page(vertical, dir);
    }

    // ---- minimap ----------------------------------------------------------

    /// Whether the minimap is actually being drawn.
    ///
    /// The toggle is necessary but not sufficient: on a window too narrow to
    /// leave `MM_MIN_TEXT_W` of document the strip turns itself off, because
    /// a minimap that squeezes the text into a sliver is worse than none; and
    /// past `MM_MAX_BYTES` it stays off too, because the per-frame ink tax
    /// is not worth paying on a document whose shape nobody can read at that
    /// scale. The preference itself is untouched, so the strip comes back by
    /// itself on a smaller document.
    pub fn mm_on(&self) -> bool {
        self.minimap
            && self.tab().doc.total_bytes() <= MM_MAX_BYTES
            && self.view.w - self.gutter_w() - MINIMAP_W - SCROLLBAR_W >= MM_MIN_TEXT_W
    }

    /// Width taken by the minimap (0 when it is not drawn).
    pub fn mm_w(&self) -> f32 {
        if self.mm_on() {
            MINIMAP_W
        } else {
            0.0
        }
    }

    /// Height of the strip: it stands beside the text, so it is exactly as tall
    /// as the text area (the horizontal scrollbar already took its share).
    pub fn mm_h(&self) -> f32 {
        self.text_h()
    }

    /// Left edge of the strip in viewport coordinates: left of the vertical
    /// scrollbar, right of the text column.
    pub fn mm_left(&self) -> f32 {
        self.view.w - self.mm_w() - self.vbar_w()
    }

    pub fn mm_geom(&self) -> MmGeom {
        let h = self.mm_h();
        let prev = self.mm_top_latch(h);
        let g = minimap::mm_geom_ex(
            self.tab().doc.line_count(),
            h,
            self.scroll_top,
            h,
            self.metrics.lh,
            prev,
        );
        // Refresh the latch while it is honoured, so small scroll steps (wheel,
        // caret scrolling into view) keep the strip where a drag left it and
        // only a real jump re-centres it.
        if prev.is_some() {
            self.latch_mm_top(g.top);
        }
        g
    }

    /// The latched strip top, when it may still be honoured.
    ///
    /// The latch dies with the document (a tab switch must not inherit another
    /// document's map position) and when the scroll has been moved by something
    /// else further than the indicator can travel inside the strip — that is a
    /// jump (click, goto, find), and jumps re-centre exactly like they did
    /// before drags existed.
    fn mm_top_latch(&self, mm_h: f32) -> Option<f64> {
        let (doc, s0, t) = self.mm_top.borrow().as_ref()?.to_owned();
        if doc != self.active_doc().id || !t.is_finite() || !s0.is_finite() {
            return None;
        }
        let lh = if self.metrics.lh > 0.0 {
            self.metrics.lh as f64
        } else {
            1.0
        };
        let k = minimap::MM_LH as f64 / lh;
        // Free travel of the indicator inside the strip, in document px.
        let travel = (mm_h as f64 * (1.0 - k) / k).max(0.0);
        if (self.scroll_top - s0).abs() > travel {
            return None;
        }
        Some(t)
    }

    fn latch_mm_top(&self, top: f64) {
        *self.mm_top.borrow_mut() = Some((self.active_doc().id, self.scroll_top, top));
    }

    /// Document line under a point `local_y` px down the strip.
    pub fn mm_line_at(&self, local_y: f64) -> u64 {
        let g = self.mm_geom();
        minimap::mm_line_at(local_y, g.top, self.metrics.lh, self.tab().doc.line_count())
    }

    /// Put the document point `local_y` px down the strip in the **middle** of
    /// the viewport.
    ///
    /// `top` is the strip's own scroll at press time, passed in because the
    /// call site already holds the press-frame geometry — and because the press
    /// deliberately does **not** move the strip (see [`Self::mm_click`]).
    fn mm_scrub_to(&mut self, local_y: f64, top: f64) {
        let px = minimap::mm_doc_px(local_y, top, self.metrics.lh);
        // Centring (rather than aligning to the top) is what you want from an
        // overview: you click the shape you are looking *for*, and it lands in
        // front of you with context on both sides.
        self.scroll_top = px - self.mm_h() as f64 / 2.0;
        self.clamp_scroll();
    }

    /// Press on the strip: jump, then glue the pointer to the indicator.
    ///
    /// The jump scrolls so the clicked shape becomes the viewport centre — the
    /// same scroll it always did — but the **strip does not move**: with the
    /// strip latched where it was, the indicator lands *on the pointer* (its
    /// centre at the click point), which is exactly where VS Code puts it. The
    /// old behaviour re-centred the indicator in the strip instead, leaving the
    /// pointer and the indicator up to half a strip apart; the drag then mapped
    /// the pointer through a strip position the paint had already left, and the
    /// document scrolled with a constant offset of up to a full strip height —
    /// the "drag does not follow the hand" bug.
    pub fn mm_click(&mut self, local_y: f64) {
        let g = self.mm_geom();
        let top = g.top;
        self.mm_scrub_to(local_y, top);
        // The strip stands still across the jump; the next paint must read the
        // same top (and not re-centre) for the indicator to be on the pointer.
        self.latch_mm_top(top);
        let g = self.mm_geom();
        self.mm_drag = Some(MmDrag {
            grab: local_y - (g.slider.0 as f64 + g.slider.1 as f64 / 2.0),
            top: g.top,
        });
    }

    /// Pointer moved while the minimap is being scrubbed. `cy` is a client
    /// (window-content) y in CSS px — fed from the window-level move event,
    /// because a drag leaves the 84px strip constantly.
    ///
    /// The drag is **glued to the indicator**: its *centre* keeps the press-time
    /// offset under the pointer (`centre = pointer − grab`; the press put the
    /// centre on the pointer, so the offset is ~0), which makes the scroll
    /// `(pointer − grab − slider_h/2 + strip top) / k`. While the indicator
    /// stays inside the strip, `top` does not change — the map stands still and
    /// only the indicator moves. Once the drag pushes the indicator against an
    /// edge, `top` clamps into the same window the paint uses
    /// ([`minimap::mm_geom_ex`]), so the strip follows minimally and the mapping
    /// never disagrees with what is painted. Both ends of the document stop
    /// cleanly (the scroll clamps), and the latch keeps the strip standing still
    /// after the release instead of snapping back.
    pub fn mm_pointer(&mut self, cy: f64) {
        let Some(mut d) = self.mm_drag else {
            return;
        };
        let mm_h = self.mm_h().max(1.0) as f64;
        // Pointer position inside the strip: a drag that leaves it pins the
        // indicator to the edge it ran into.
        let local = (cy - self.view.y as f64).clamp(0.0, mm_h);
        let lh = if self.metrics.lh > 0.0 {
            self.metrics.lh as f64
        } else {
            1.0
        };
        let k = minimap::MM_LH as f64 / lh;
        let slider_h = mm_h * k; // the indicator's height, in strip px
        self.scroll_top = (local - d.grab - slider_h / 2.0 + d.top) / k;
        self.clamp_scroll();
        // Same window the paint clamps the latched top into.
        let lines = self.tab().doc.line_count() as f64;
        let max_top = (lines * minimap::MM_LH as f64 - mm_h).max(0.0);
        let lo = ((self.scroll_top + mm_h) * k - mm_h).max(0.0).min(max_top);
        let hi = (self.scroll_top * k).min(max_top).max(lo);
        d.top = d.top.clamp(lo, hi);
        if let Some(g) = self.mm_drag.as_mut() {
            g.top = d.top;
        }
        self.latch_mm_top(d.top);
    }

    pub fn mm_end(&mut self) {
        // The drag ends; the latch deliberately survives it — the strip stays
        // where the drag left it (no snap-back) until the next scroll that did
        // not come from the strip.
        self.mm_drag = None;
    }

    /// Wheel over the minimap scrolls the document, in the same direction as a
    /// wheel over the text.
    pub fn mm_wheel(&mut self, dy: f32) {
        self.scroll_top -= dy as f64;
        self.clamp_scroll();
    }

    /// View → Code Minimap.
    ///
    /// Turning it on for a document past `MM_MAX_BYTES` is refused (the
    /// JSON-tree-preview precedent): checking the menu item while `mm_on`
    /// paints nothing would just look broken, so the state stays off and the
    /// status line explains why. Turning it off is always allowed.
    pub fn toggle_minimap(&mut self) {
        if !self.minimap && self.active_doc().total_bytes() > MM_MAX_BYTES {
            self.status = Some(
                Fmt::StatusMinimapTooLarge {
                    mib: MM_MAX_BYTES / (1024 * 1024),
                }
                .into(),
            );
            return;
        }
        self.minimap = !self.minimap;
        self.status = Some(
            if self.minimap {
                Msg::StatusMinimapOn
            } else {
                Msg::StatusMinimapOff
            }
            .into(),
        );
        // the text column's width changed, so the horizontal scroll may now be
        // out of range in either direction
        self.clamp_scroll();
    }

    /// Build the ink model for the rows the strip is currently showing.
    ///
    /// Virtualized like the text: only the ~250 rows inside the strip are read,
    /// and each one is capped at `MM_LINE_CAP` bytes and
    /// [`minimap::MAX_RUNS`] runs, so the cost is bounded whatever the file.
    ///
    /// Per-line results go into the [`minimap::MmInk`] cache: on scroll the
    /// window overlaps ~90% with the previous frame, so only the few newly
    /// entered lines need text reads + scanning (see that type's comment).
    pub fn mm_model(&self) -> MmModel {
        let g = self.mm_geom();
        let doc = self.active_doc();
        let lines = doc.line_count();
        let gen = doc.gen;
        let doc_id = doc.id;
        let mut bars: Vec<minimap::MmBar> = Vec::new();
        let row_h = minimap::MM_LH as f64;

        for i in 0..g.rows {
            let ln = g.first + i as u64;
            if ln >= lines {
                break;
            }
            let runs = {
                let mut cache = self.mm_ink.borrow_mut();
                match cache.get(doc_id, gen, ln) {
                    Some(r) => r,
                    None => {
                        let text = mm_line(doc, ln);
                        let r = minimap::ink_runs(&text, TAB_SIZE, minimap::MAX_COLS);
                        cache.put(doc_id, gen, ln, r.clone());
                        r
                    }
                }
            };
            let y = (g.y0 + i as f64 * row_h) as f32;
            for (a, b) in runs.into_iter() {
                let x = a as f32 * minimap::MM_CHAR_W;
                let w = (b - a) as f32 * minimap::MM_CHAR_W;
                if w <= 0.0 {
                    continue;
                }
                bars.push(minimap::MmBar { x, y, w });
            }
        }

        // ---- caret row -----------------------------------------------------
        let cur_y = {
            let line = doc.line_of_pos(self.caret().pos);
            if line >= g.first && line < g.first + g.rows as u64 && line < lines {
                Some((g.y0 + (line - g.first) as f64 * row_h) as f32)
            } else {
                None
            }
        };

        // ---- search matches ------------------------------------------------
        // `find.results` is sorted by position, so the slice covering the
        // painted rows is a binary search, not a scan — cheap enough to redo
        // every frame.
        let mut hits: Vec<f32> = Vec::new();
        if !self.find.results.is_empty() && self.find.valid_for(doc.id, doc.gen) {
            let lo = doc.pos_of_line(g.first.min(lines.saturating_sub(1)));
            let last = (g.first + g.rows as u64).min(lines).saturating_sub(1);
            let hi = if lines == 0 {
                0
            } else {
                doc.line_span(last).end
            };
            let mut i = self.find.results.partition_point(|m| m.end <= lo);
            let mut prev: Option<u64> = None;
            while i < self.find.results.len() {
                let m = self.find.results[i];
                if m.start >= hi {
                    break;
                }
                let line = doc.line_of_pos(m.start);
                if line >= g.first && line < g.first + g.rows as u64 && Some(line) != prev {
                    hits.push((g.y0 + (line - g.first) as f64 * row_h) as f32);
                    prev = Some(line);
                }
                i += 1;
            }
        }

        MmModel { bars, cur_y, hits }
    }

    pub fn scroll_caret_into_view(&mut self) {
        let lh = self.metrics.lh.max(1.0) as f64;
        let view_h = self.text_h() as f64;
        let pos = self.caret().pos;
        // Visual row coordinates: JSONL expansion, hidden rows and soft wrap
        // are all folded in.
        let (vrow, _line, _sub, _row_a, _) = self.visual_row_of_pos(pos);
        let y = vrow as f64 * lh;
        if y < self.scroll_top {
            self.scroll_top = y;
        } else if y + lh > self.scroll_top + view_h {
            self.scroll_top = y + lh - view_h;
        }
        // horizontal (no horizontal overflow at all under soft wrap)
        if !self.wrap_enabled() {
            let line = self.tab().doc.line_of_pos(pos);
            let start = self.tab().doc.pos_of_line(line);
            let text = self.tab().doc.line_text(line);
            let x = self
                .metrics
                .x_of_offset(&text, (pos - start) as usize, TAB_SIZE);
            let vis_w = self.text_w().max(50.0);
            if x < self.scroll_left {
                self.scroll_left = x;
            } else if x > self.scroll_left + vis_w - 20.0 {
                self.scroll_left = x - vis_w + 60.0;
            }
        } else {
            self.scroll_left = 0.0;
        }
        self.clamp_scroll();
    }

    /// Advance the widest-line scan by up to `budget` lines.
    ///
    /// Called once per rendered frame (from `viewport::build`). The scan is
    /// *monotone*: an edit restarts it from line 0 but keeps the width already
    /// found, so the horizontal bar's extent can only grow within a document
    /// generation. That is deliberate — letting it shrink would make the bar
    /// (and with it the text column's width) flicker on every keystroke.
    pub fn pump_h_extent(&mut self, budget: usize) {
        let (doc_id, gen, lines) = {
            let d = &self.tab().doc;
            (d.id, d.gen, d.line_count())
        };
        if self.h_extent.doc_id != doc_id {
            self.h_extent = HExtent {
                doc_id,
                gen,
                ..Default::default()
            };
            self.sample_h_extent();
        }
        if self.h_extent.gen != gen {
            // Generation changed (any edit): **do not reset the scan
            // progress**.
            //
            // The old implementation zeroed `next`, so while typing into a
            // long document every keystroke re-scanned 256 lines from line 0
            // — the scan never finished and the horizontal width estimate
            // never converged (B13). Now `next` is kept and only "the visible
            // region being edited" is folded in; if the scan has not
            // finished, it keeps advancing over the remaining lines. HExtent
            // is a grow-only estimate; un-rescanned old lines cost at most
            // one notch of horizontal bar width, never correctness.
            self.h_extent.gen = gen;
            self.h_extent.visible = None;
            if self.h_extent.next >= lines {
                self.h_extent.next = lines;
                self.h_extent.done = true;
            } else {
                // Huge documents already closed out at sampling; an edit must
                // not spread them back into a full scan
                self.h_extent.done = lines > H_SCAN_LINES;
            }
            let (df, dl) = self.visible_doc_range();
            self.measure_lines(df, dl);
            // The caret line may have just been edited and not be in the
            // visible range (e.g. undo pulling the viewport elsewhere): fold
            // it in separately, O(1).
            let caret_line = self.tab().doc.line_of_pos(self.caret().pos);
            self.measure_lines(caret_line, caret_line.saturating_add(1));
        }
        if self.h_extent.done || budget == 0 {
            return;
        }
        let next = self.h_extent.next;
        if next >= lines {
            self.h_extent.done = true;
            return;
        }
        let end = (next + budget as u64).min(lines);
        self.measure_lines(next, end);
        self.h_extent.next = end;
        if end >= lines {
            self.h_extent.done = true;
        }
    }

    /// One strided pass over a newly seen document (see [`H_SAMPLE_LINES`]).
    ///
    /// Sampled lines scale up to [`H_SAMPLE_MAX`] with document size (1 in
    /// every 32) and the sample points **spread uniformly over the whole
    /// document** (including the first and last lines): the old formula
    /// `i * stride`, after integer truncation of `stride = lines / n`, could
    /// never reach the last line, so documents where "only the last line is
    /// unusually wide" (minified JSON without a trailing newline, log tails)
    /// kept a short horizontal bar until the user scrolled that line into
    /// view.
    fn sample_h_extent(&mut self) {
        let lines = self.tab().doc.line_count();
        if lines == 0 {
            self.h_extent.done = true;
            return;
        }
        let n = lines.min((lines.div_ceil(32)).clamp(H_SAMPLE_LINES, H_SAMPLE_MAX));
        let mut w = 0.0f32;
        {
            let tab = &self.tabs[self.active];
            for i in 0..n {
                // 0..n-1 maps onto 0..lines-1: both ends are in the sample
                // (n == 1 takes line 0).
                // u128 multiplication prevents overflow — no line count can
                // produce an out-of-range line number.
                let l = if n <= 1 {
                    0
                } else {
                    (i as u128 * (lines - 1) as u128 / (n - 1) as u128) as u64
                };
                let t = line_head(&tab.doc, l);
                let lw = self.metrics.line_width(&t, TAB_SIZE);
                if lw > w {
                    w = lw;
                }
            }
        }
        self.h_extent.width = w;
        // Small documents scan to the end; huge documents close out at
        // sampling (see [`H_SCAN_LINES`]).
        if n == lines || lines > H_SCAN_LINES {
            self.h_extent.done = true;
            self.h_extent.next = lines;
        }
    }

    /// Fold the widths of lines `[first, last)` into the horizontal extent.
    ///
    /// Used both by the background scan and, every frame, for the visible
    /// lines: the latter guarantees the bar can always reach the content on
    /// screen, even on a million-line document the scan has only just started.
    pub fn measure_lines(&mut self, first: u64, last: u64) {
        let mut w = self.h_extent.width;
        {
            let tab = &self.tabs[self.active];
            let lines = tab.doc.line_count();
            for l in first..last.min(lines) {
                // Read only the first `H_LINE_CAP` bytes instead of taking
                // the whole line via `line_text` and then truncating: a
                // single line of a minified/log file can reach several MB,
                // and a full per-line allocation would cause MB-scale heap
                // churn inside this "runs every frame" function.
                let t = line_head(&tab.doc, l);
                let lw = self.metrics.line_width(&t, TAB_SIZE);
                if lw > w {
                    w = lw;
                }
            }
        }
        self.h_extent.width = w;
    }

    // ---- mouse ------------------------------------------------------------

    /// Map client coordinates (window content space) to a document byte
    /// position. Returns `None` below the last line / outside the text.
    pub fn hit_test(&self, client_x: f64, client_y: f64) -> Option<u64> {
        // The whole-document JSON tree view is a read-only preview: it does
        // not receive caret landings (fold-marker clicks are handled
        // separately by `json_marker_click` and consumed before this).
        if self.tabs[self.active].json.enabled {
            return None;
        }
        let lh = self.metrics.lh.max(1.0);
        let doc = self.active_doc();
        let lines = doc.line_count();
        // compute the scroll offset in f64: rel_y stays exact even when
        // scroll_top is billions of pixels into a huge document
        let rel_y = (client_y - self.view.y as f64 - PAD_TOP as f64) + self.scroll_top;
        if rel_y < 0.0 {
            return Some(doc.pos_of_line(0));
        }
        let mut vrow = (rel_y / lh as f64).floor() as i64;
        if vrow < 0 {
            vrow = 0;
        }
        let vrow = vrow as u64;
        // Judge bounds by visual rows first (soft wrap/hidden rows/JSONL
        // virtual rows all folded in), then convert to a JSONL row and
        // in-row sub-row: virtual rows themselves are not editable (no
        // caret move).
        if vrow >= self.visual_rows() {
            let last = lines.saturating_sub(1);
            let span = doc.line_span(last);
            return Some(span.end);
        }
        let (screen, sub) = self.screen_of_visual(vrow);
        let line = self.jsonl_row_of(screen);
        let tab = self.tab();
        let jv = &tab.jsonl;
        let total = jv.total_rows(lines.max(1));
        if line >= total {
            let last = lines.saturating_sub(1);
            let span = doc.line_span(last);
            return Some(span.end);
        }
        let line = match jv.row_at(line) {
            crate::ui::state::jsonl::RowRef::Doc(l) => l,
            crate::ui::state::jsonl::RowRef::Virtual { .. } => return None,
        };
        // the gutter grows with the digit count of the line numbers, so the
        // constant would be wrong for documents with > ~100k lines
        let rel_x = client_x as f32 - self.view.x - self.gutter_w() - PAD_LEFT + self.scroll_left;
        let text = doc.line_text(line);
        let (a, b) = if self.wrap_enabled() {
            self.wrap_index.row_range(line, sub, text.len())
        } else {
            (0, text.len())
        };
        let off = if rel_x <= 0.0 {
            0
        } else {
            self.metrics
                .offset_of_x(&text[a.min(text.len())..b.min(text.len())], rel_x, TAB_SIZE)
        };
        let start = doc.pos_of_line(line);
        Some(start + (a + off) as u64)
    }

    /// Click on the fold column **right** of the line numbers (JSONL: expand
    /// a whole line / container nodes inside an expanded block).
    ///
    /// Returns `true` when the click was consumed by the fold column (the
    /// caller then does not move the caret / start a selection).
    pub fn jsonl_marker_click(&mut self, client_x: f64, client_y: f64) -> bool {
        if !self.tabs[self.active].jsonl.enabled {
            return false;
        }
        let Some(visual) = self.fold_marker_row_at(client_x, client_y) else {
            return false;
        };
        let (screen, soft_sub) = self.screen_of_visual(visual);
        let row = self.jsonl_row_of(screen);
        let ai = self.active;
        let target = {
            let tab = &mut self.tabs[ai];
            tab.jsonl.refresh(&tab.doc);
            let total = tab.jsonl.total_rows(tab.doc.line_count());
            if row >= total {
                return false;
            }
            match tab.jsonl.row_at(row) {
                crate::ui::state::jsonl::RowRef::Doc(line) => {
                    // Soft-wrap continuation rows draw no marker: markers
                    // only on the first sub-row
                    if soft_sub > 0 {
                        return false;
                    }
                    // Only lines whose first character is `{` / `[` get a
                    // marker
                    let text = tab.doc.line_text(line);
                    let foldable = text
                        .as_bytes()
                        .iter()
                        .position(|b| !b.is_ascii_whitespace())
                        .is_some_and(|i| matches!(text.as_bytes()[i], b'{' | b'['));
                    if !foldable {
                        return false;
                    }
                    JsonlTarget::Line(line)
                }
                crate::ui::state::jsonl::RowRef::Virtual { line, sub } => {
                    if soft_sub > 0 {
                        return false;
                    }
                    // A line that failed to parse yields no rows (`span()` still
                    // reports one so the ⚠ placeholder can render), and the
                    // marker strip spans the whole row height — clicking it on
                    // such a row used to index an empty Vec and abort the
                    // process.
                    let Some(entry) = tab.jsonl.open.get(&line) else {
                        return false;
                    };
                    if entry.err.is_some() {
                        return false;
                    }
                    let Some(r) = entry.rows.get(sub as usize) else {
                        return false;
                    };
                    if !r.foldable {
                        return false; // leaf rows have no marker to click
                    }
                    JsonlTarget::Node { line, node: r.node }
                }
            }
        };
        match target {
            JsonlTarget::Line(line) => self.jsonl_expand_line(line),
            JsonlTarget::Node { line, node } => {
                self.tabs[ai].jsonl.toggle_node(line, node);
            }
        }
        true
    }

    /// Expand/fold one document line (shared by marker clicks and
    /// `Ctrl+Alt+]`), with scroll anchoring.
    pub fn jsonl_expand_line(&mut self, line: u64) {
        let lh = self.metrics.lh.max(1.0) as f64;
        // MAX_OPEN's "farthest from the viewport" eviction measures from the
        // viewport centre (visual row → screen row)
        let center = self
            .screen_of_visual(((self.scroll_top + self.text_h() as f64 * 0.5) / lh) as u64)
            .0;
        let ai = self.active;
        let before = self.visual_start_of_screen(self.screen_vstart(line));
        let tab = &mut self.tabs[ai];
        tab.jsonl.refresh(&tab.doc);
        match tab.jsonl.toggle_line(&tab.doc, line, center) {
            crate::ui::state::jsonl::Outcome::Expanded { rows: k } => {
                // Scroll anchoring: expanding rows above the viewport ⇒
                // compensate, so content being read is not pushed away
                if before as f64 * lh < self.scroll_top {
                    self.scroll_top += k as f64 * lh;
                }
                self.status = tab.jsonl.status();
                self.clamp_scroll();
            }
            crate::ui::state::jsonl::Outcome::Collapsed => {
                self.status = tab.jsonl.status();
                self.clamp_scroll();
            }
            crate::ui::state::jsonl::Outcome::Failed(m) => {
                self.status = Some(m);
            }
        }
    }

    /// `Ctrl+Alt+]`: expand the caret's line (invalid JSON goes to the ⚠
    /// error row, never silently).
    pub fn jsonl_unfold_caret_line(&mut self) {
        if !self.tabs[self.active].jsonl.enabled {
            self.status = Some(Msg::StatusJsonlPreviewDisabled.into());
            return;
        }
        let line = self.active_doc().line_of_pos(self.caret().pos);
        self.jsonl_expand_line(line);
    }

    /// `Ctrl+Alt+[`: fold the caret's line (expanded ⇒ collapse; not open ⇒
    /// notice).
    pub fn jsonl_fold_caret_line(&mut self) {
        if !self.tabs[self.active].jsonl.enabled {
            self.status = Some(Msg::StatusJsonlPreviewDisabled.into());
            return;
        }
        let line = self.active_doc().line_of_pos(self.caret().pos);
        if self.tabs[self.active].jsonl.open.contains_key(&line) {
            self.jsonl_expand_line(line);
        } else {
            self.status = Some(Msg::StatusJsonlLineNotOpen.into());
        }
    }

    /// View → JSONL Preview (toggle; clears expansion state when turned
    /// off).
    pub fn toggle_jsonl(&mut self) {
        let ai = self.active;
        let on = !self.tabs[ai].jsonl.enabled;
        self.tabs[ai].jsonl.enabled = on;
        // The same preference is applied to the *next* JSON Lines document
        // opened in this session (`adopt_view_prefs`); updating only the tab
        // left the session default stale until restart.
        if self.tabs[ai].spec.name == "JSON Lines" {
            self.jsonl_pref = Some(on);
        }
        if !on {
            self.tabs[ai].jsonl.collapse_all();
        }
        // This used to miss relayout: the line-number slot gains/loses the
        // fold-marker column, and without resetting the viewport the first
        // frame after switching had the wrong width (only catching up via
        // the state layer's layout the next frame).
        // view_switched still takes a String for now; the status write is
        // done in place here (equivalent to it).
        self.status = Some(
            if on {
                Msg::StatusViewPreview
            } else {
                Msg::StatusViewText
            }
            .into(),
        );
        self.relayout();
    }

    /// Fold every expanded JSONL line.
    pub fn jsonl_collapse_all(&mut self) {
        self.tabs[self.active].jsonl.collapse_all();
        self.status = Some(Msg::StatusJsonlCollapsedAll.into());
        self.clamp_scroll();
    }

    /// Clamp every caret into the document. Cheap insurance against edits that
    /// shrink the buffer while a caret/anchor still points past the new end.
    ///
    /// Also snap to character boundaries: a few paths (e.g. Vim visual's
    /// `caret + 1`) write unaligned byte offsets, and consumers like the
    /// status bar's `position_info` that slice with `text[..off]` would
    /// panic afterwards. `table.round_down` is O(1) on mmap.
    pub fn sanitize_carets(&mut self) {
        let len = self.tab().doc.table.len();
        let pos = self.caret().pos;
        let anchor = self.caret().anchor;
        let snapped = {
            let doc = &self.tab().doc;
            (doc.snap(pos.min(len)), anchor.map(|a| doc.snap(a.min(len))))
        };
        let c = self.caret_mut();
        c.pos = snapped.0;
        c.anchor = snapped.1;
    }

    /// Width of the line-number gutter for the active document (px).
    ///
    /// The gutter hugs the widest line number the document actually has —
    /// `format_line_no` caps the display at 10 digits before switching to
    /// scientific notation, so the width saturates there instead of growing
    /// forever. `metrics.adv` is the real advance width of the editor font,
    /// which keeps the column visibly narrower than the old fixed 40 px floor
    /// for everyday documents (a 14 px Consolas lands at ~33 px for 3 digits).
    pub fn gutter_w(&self) -> f32 {
        let lines = self.active_doc().line_count().max(1);
        // For huge documents whose indexing is not finished, line_count is
        // "the actual number of lines loaded so far" and grows as the scan
        // advances — the gutter grows along with it; no extra "unknown line
        // count" state needed.
        // The digit count is pure arithmetic: pointer-movement paths call
        // this at high frequency, no more format! + count
        let digits = crate::ui::state::line_no_digits(lines).max(3);
        let mut w = digits as f32 * self.metrics.adv + 10.0;
        // Fold column right of the line numbers (shared by the JSON tree
        // preview / JSONL folding, see MARKER_W)
        let tab = &self.tabs[self.active];
        if tab.jsonl.enabled || tab.json.enabled {
            w += MARKER_W;
        }
        w
    }

    /// Fold column ("marker") hit test: the **visual** row under the client
    /// point, or `None` when the point is outside the column.
    ///
    /// The column is the fixed `MARKER_W`-wide slot right of the line numbers
    /// (`gutter - MARKER_W .. gutter`), shared by JSONL rows, JSON tree rows
    /// and soft-wrapped continuation rows; the test is therefore a single band
    /// comparison, independent of indent depth, horizontal scroll and wrap.
    ///
    /// Single home for that band — the JSONL / JSON marker clicks and the
    /// viewport's coarse pre-filter (which must not start an edit to find out
    /// that a click missed) all come through here. Callers map the returned
    /// row into their own row space: [`EditorState::screen_of_visual`] for
    /// wrapped JSONL rows, the row index itself for the tree.
    pub fn fold_marker_row_at(&self, client_x: f64, client_y: f64) -> Option<u64> {
        let x = client_x - self.view.x as f64;
        let gw = self.gutter_w() as f64;
        if !(x >= gw - MARKER_W as f64 && x <= gw) {
            return None;
        }
        let lh = self.metrics.lh.max(1.0) as f64;
        let rel_y = (client_y - self.view.y as f64 - PAD_TOP as f64) + self.scroll_top;
        if rel_y < 0.0 {
            return None;
        }
        Some((rel_y / lh).floor() as u64)
    }

    /// Absolute client (window-content) coordinates of the caret plus the line
    /// height. Used to park the single IME composer at the right place — either
    /// over the caret or over the focused find-bar box.
    pub fn caret_xy(&self) -> (f32, f32, f32) {
        let lh = self.metrics.lh.max(1.0);
        let pos = self.caret().pos;
        let doc = self.active_doc();
        let line = doc.line_of_pos(pos);
        let start = doc.pos_of_line(line);
        let text = doc.line_text(line);
        let cx = self
            .metrics
            .x_of_offset(&text, (pos - start) as usize, TAB_SIZE);
        let x = self.view.x + self.gutter_w() + PAD_LEFT + cx - self.scroll_left;
        // f64 for the vertical term: line * lh - scroll_top cancels a huge
        // scroll offset, and doing it in f64 keeps the small result exact
        // Virtual row coordinates (when not expanded, vstart == line,
        // identity)
        let y = (self.view.y as f64
            + PAD_TOP as f64
            + (self.tabs[self.active].jsonl.vstart(line) as f64 * lh as f64 - self.scroll_top))
            as f32;
        (x, y, lh)
    }

    /// Status-bar position info for the active caret.
    pub fn position_info(&self) -> (u64, u64, usize) {
        let doc = self.active_doc();
        let c = self.caret();
        let line = doc.line_of_pos(c.pos);
        let start = doc.pos_of_line(line);
        let text = doc.line_text(line);
        // Defensive: pos may come from the wys bridge's byte mapping (emit
        // offsets vs a CRLF document); in theory already snapped to a char
        // boundary, clamp once more here — slicing at a non-boundary byte
        // panics and
        // takes the whole process down (real machine: status bar renders
        // after clicking a Chinese title).
        let mut n = ((c.pos.saturating_sub(start)) as usize).min(text.len());
        while n > 0 && !text.is_char_boundary(n) {
            n -= 1;
        }
        let col = text[..n].chars().count() as u64;
        let sel = c
            .selection()
            .map(|r| (r.end - r.start) as usize)
            .unwrap_or(0);
        (line + 1, col + 1, sel)
    }

    // ---- line targeting ---------------------------------------------------

    /// The selected lines, or `None` when there is no selection.
    ///
    /// A selection that ends exactly at a line start does **not** pull in the
    /// following line — that is the behaviour of every editor that lets you
    /// drag complete lines.
    fn sel_lines(&self) -> Option<(u64, u64)> {
        let sel = self.sel_range()?;
        if sel.is_empty() {
            return None;
        }
        let doc = self.active_doc();
        let a = doc.line_of_pos(sel.start.min(doc.table.len()));
        let b = doc.line_of_pos(sel.end.saturating_sub(1));
        Some((a, b.max(a)))
    }

    /// Lines a *line* operation acts on: the selected lines when there is a
    /// selection, otherwise the caret's line (`Nothing selected` → current
    /// line, exactly like the standard rule).
    pub fn block_lines(&self) -> (u64, u64) {
        let doc = self.active_doc();
        if doc.line_count() == 0 {
            return (0, 0);
        }
        self.sel_lines().unwrap_or_else(|| {
            let l = doc.line_of_pos(self.caret().pos);
            (l, l)
        })
    }

    /// Lines a *document-wide* operation (blank operations, sorting, case
    /// conversion) acts on: the selected lines, or **every** line when nothing
    /// is selected — again the standard rule.
    ///
    /// With no selection, **phantom lines are excluded**: when a document
    /// ends with a terminator, the line index gains an empty trailing line
    /// (`"a\nb\n"` = 3 lines, line 3 being `""`). This repo treats a
    /// trailing EOL as part of a line, not as a line — letting it take part
    /// in sort/dedup as real content would conjure up blank lines (it was
    /// observed that `sort("b\na\n")` produced `"\na\nb\n"` and macro dedup
    /// of `"a\na\nb\n"` produced `"a\nb\n\n"`).
    pub fn target_lines(&self) -> (u64, u64) {
        let total = self.active_doc().line_count();
        if total == 0 {
            return (0, 0);
        }
        let (first, mut last) = self.sel_lines().unwrap_or((0, total - 1));
        if self.sel_range().is_none()
            && last + 1 == total
            && last > first
            && self.active_doc().line_text(last).is_empty()
        {
            last -= 1;
        }
        (first, last)
    }

    /// Whole-line byte range `[start, end)` covering lines `first..=last`,
    /// terminators included. The last line's terminator is only part of the
    /// span when another line follows it.
    pub fn lines_span(&self, first: u64, last: u64) -> (u64, u64) {
        let doc = self.active_doc();
        let total = doc.line_count();
        if total == 0 {
            return (0, 0);
        }
        let first = first.min(total - 1);
        let last = last.min(total - 1).max(first);
        let start = doc.pos_of_line(first);
        let end = if last + 1 < total {
            doc.pos_of_line(last + 1)
        } else {
            doc.table.len()
        };
        (start, end)
    }

    /// Owned text of lines `first..=last` (terminators stripped).
    pub fn read_lines(&self, first: u64, last: u64) -> Vec<String> {
        let doc = self.active_doc();
        (first..=last).map(|l| doc.line_text(l)).collect()
    }

    /// Replace lines `first..=last` with `new_lines` as a single undo step.
    ///
    /// The trailing terminator is only re-emitted when something follows the
    /// block, so the text after the block is left byte-identical. The caret
    /// ends up at the start of the new block with the block selected, and the
    /// view is deliberately **not** scrolled: a sort over the whole document
    /// must not yank the user to the bottom of the file.
    pub fn write_lines(&mut self, first: u64, last: u64, new_lines: &[String]) {
        self.write_lines_coalesced(first, last, new_lines, None);
    }

    /// [`Self::write_lines`] with a coalescing key: when `key` is `Some` the
    /// write goes through `apply_typing` (consecutive writes with the same
    /// key coalesce into one undo step — live CSV cell editing replaces the
    /// whole line on every keystroke; without coalescing a single edit would
    /// be a long chain of undo steps).
    pub fn write_lines_coalesced(
        &mut self,
        first: u64,
        last: u64,
        new_lines: &[String],
        key: Option<crate::buffer::CoalesceKey>,
    ) {
        let (start, end) = self.lines_span(first, last);
        let nl = nl_of(self.active_doc());
        let old = self.active_doc().text_range(start..end);
        // Does the old text end with a terminator? For the file's **last
        // line**, `lines_span` wraps the trailing terminator into the range
        // while `last + 1 < total` is already false — checking only the
        // latter would swallow the file's final newline (most text files
        // have one). OR the two.
        let had_term = old.ends_with(nl) || last + 1 < self.active_doc().line_count();
        let mut text = new_lines.join(nl);
        // Terminator reconciliation. `new_lines` holds line *contents*, and
        // `join` already emits the separator before a trailing empty element —
        // so `["b", ""]` joins to exactly `"b\n"` and an "insert a blank line
        // below" write became indistinguishable from a no-op. Callers that want
        // a real blank line therefore pass a trailing `""` (see
        // `insert_blank_line`), and the truncation below must not eat it.
        let ends_with_term = text.ends_with(nl);
        let wants_blank = new_lines.last().is_some_and(|l| l.is_empty());
        if had_term && !ends_with_term && !text.is_empty() {
            text.push_str(nl);
        } else if !had_term && ends_with_term && !wants_blank {
            text.truncate(text.len() - nl.len());
        }
        if text != old {
            if let Some(key) = key {
                let op = crate::buffer::EditOp {
                    pos: start,
                    deleted: old,
                    inserted: text.clone(),
                };
                self.tab_mut().doc.apply_typing(op, key);
            } else if end > start {
                self.tab_mut().doc.replace_range(start..end, &text);
            } else if !text.is_empty() {
                self.tab_mut().doc.insert_text(start, &text);
            }
        }
        let new_end = start + text.len() as u64;
        let c = self.caret_mut();
        c.pos = start;
        c.anchor = if new_end > start { Some(new_end) } else { None };
        self.clamp_scroll();
    }

    /// Mass operations materialise every line as a `String` and write the whole
    /// buffer back, so they are refused above this size unless the user has
    /// explicitly selected the range to process.
    pub const MASS_OP_MAX_BYTES: u64 = 128 * 1024 * 1024;

    pub(super) fn guard_mass_op(&mut self) -> bool {
        if self.sel_range().is_some() {
            return true;
        }
        let len = self.active_doc().table.len();
        if len <= Self::MASS_OP_MAX_BYTES {
            return true;
        }
        self.status = Some(Fmt::StatusMassOpTooLarge { bytes: len }.into());
        false
    }
}

/// EOL string of a document's effective newline style.
pub(super) fn nl_of(doc: &crate::buffer::TextDocument) -> &'static str {
    // `terminator()`, not `as_str()`: the latter is a display name for the
    // status bar ("LF"); splicing it into the document would turn every
    // line's newline into those two literal characters — which is exactly
    // what once happened.
    doc.meta
        .user_newline
        .unwrap_or(doc.meta.detected_newline)
        .terminator()
}

// ---- helpers ---------------------------------------------------------------

fn snap_boundary(doc: &crate::buffer::TextDocument, mut pos: u64) -> u64 {
    while pos > 0 && pos < doc.table.len() && !doc.table.is_boundary(pos) {
        pos -= 1;
    }
    pos
}

fn prev_boundary(doc: &crate::buffer::TextDocument, pos: u64) -> u64 {
    let mut p = pos - 1;
    while p > 0 && !doc.table.is_boundary(p) {
        p -= 1;
    }
    p
}

fn next_boundary(doc: &crate::buffer::TextDocument, pos: u64) -> u64 {
    let mut p = pos + 1;
    let len = doc.table.len();
    while p < len && !doc.table.is_boundary(p) {
        p += 1;
    }
    p
}

/// Scan window (bytes) for word motion. A single keypress materialises at
/// most this much text; over-long (rare) words stop at the window boundary
/// instead of the true word start/end — an acceptable approximation.
const WORD_SCAN_WINDOW: u64 = 64 * 1024;

/// Byte offset of the beginning of the last word run inside `text`.
///
/// Single pass, zero allocations (it once did `char_indices().collect()`
/// into a whole Vec — the caller `move_word_left` goes through here on
/// every keystroke).
fn prev_word(text: &str) -> usize {
    let mut run_start = text.len();
    let mut class: Option<bool> = None;
    for (o, c) in text.char_indices().rev() {
        match class {
            None => {
                if c.is_whitespace() {
                    run_start = o;
                    continue;
                }
                class = Some(is_word_char(c));
                run_start = o;
            }
            Some(w) => {
                if c.is_whitespace() || is_word_char(c) != w {
                    break;
                }
                run_start = o;
            }
        }
    }
    run_start
}

/// Byte offset just past the next word inside `text`. Single pass, zero
/// allocations.
fn next_word(text: &str) -> usize {
    let mut class: Option<bool> = None;
    let mut rest = text.len();
    for (o, c) in text.char_indices() {
        match class {
            None => {
                if c.is_whitespace() {
                    continue;
                }
                class = Some(is_word_char(c));
            }
            Some(w) => {
                if c.is_whitespace() || is_word_char(c) != w {
                    rest = o;
                    break;
                }
            }
        }
    }
    if class.is_none() {
        return text.len();
    }
    if rest >= text.len() {
        return text.len();
    }
    // move past trailing spaces so the next word starts under the caret
    for (o, c) in text[rest..].char_indices() {
        if !c.is_whitespace() {
            return rest + o;
        }
    }
    text.len()
}

#[cfg(test)]
mod edit_tests {

    use crate::ui::commands::testutil::st_of;

    /// Backspace must actually delete, not just move the caret back.
    ///
    /// Regression test: the second argument of `EditOp::delete` is the
    /// deleted text (undo needs it), and `apply_ops_raw` uses its length to
    /// compute the removed range — it used to pass `""`, so Backspace across
    /// the whole project only moved the caret without deleting a byte.
    #[test]
    fn backspace_deletes_the_character_before_the_caret() {
        let mut st = st_of("hello");
        st.set_caret(5, false);
        st.delete_back();
        assert_eq!(
            st.active_doc().read_all(),
            "hell",
            "backspace must delete the previous character"
        );
        assert_eq!(st.caret().pos, 4);
        st.delete_back();
        assert_eq!(st.active_doc().read_all(), "hel");
    }

    /// Same for Delete: removes the character **after** the caret.
    #[test]
    fn delete_removes_the_character_after_the_caret() {
        let mut st = st_of("hello");
        st.set_caret(0, false);
        st.delete_fwd();
        assert_eq!(
            st.active_doc().read_all(),
            "ello",
            "Delete must remove the next character"
        );
        assert_eq!(st.caret().pos, 0, "caret stays put");
    }

    /// Backspace must be undoable: the deleted text has to come back on
    /// undo.
    ///
    /// The other half of the same bug — the undo record had `deleted == ""`,
    /// so undo re-inserted an empty string too: "undo fails but reports
    /// success".
    #[test]
    fn backspace_is_undoable() {
        let mut st = st_of("abc");
        st.set_caret(3, false);
        st.delete_back();
        assert_eq!(st.active_doc().read_all(), "ab");
        st.undo();
        assert_eq!(
            st.active_doc().read_all(),
            "abc",
            "undo must restore the deleted text"
        );
        st.redo();
        assert_eq!(st.active_doc().read_all(), "ab");
    }

    /// Multi-byte characters (UTF-8) must be deleted whole, never leaving
    /// half a code point.
    #[test]
    fn backspace_deletes_whole_utf8_characters() {
        let mut st = st_of("a中b");
        st.set_caret(st.active_doc().table.len(), false);
        st.delete_back();
        assert_eq!(st.active_doc().read_all(), "a中");
        st.delete_back();
        assert_eq!(
            st.active_doc().read_all(),
            "a",
            "the 中 character must be deleted whole"
        );
    }
}

#[cfg(test)]
mod autoclose_tests {

    use crate::ui::commands::testutil::st_of;

    /// Typing an open bracket inserts the pair, caret in between.
    #[test]
    fn typing_an_open_bracket_inserts_the_pair() {
        let mut st = st_of("");
        st.type_text("(");
        assert_eq!(st.active_doc().read_all(), "()");
        assert_eq!(st.caret().pos, 1, "caret lands between the brackets");
        // keep typing; then ) should move over instead of inserting another
        st.type_text("a");
        st.type_text(")");
        assert_eq!(
            st.active_doc().read_all(),
            "(a)",
            "close bracket moves over the existing pair"
        );
        assert_eq!(st.caret().pos, 3);
    }

    /// Three bracket pairs + two quotes all pair; quotes do not pair after a
    /// word character (the it's case).
    #[test]
    fn quotes_pair_only_before_non_word_chars() {
        let mut st = st_of("");
        st.type_text("{");
        assert_eq!(st.active_doc().read_all(), "{}");
        let mut st2 = st_of("");
        st2.type_text("[");
        assert_eq!(st2.active_doc().read_all(), "[]");
        let mut st3 = st_of("");
        st3.type_text("\"");
        assert_eq!(
            st3.active_doc().read_all(),
            "\"\"",
            "quote at line start pairs"
        );
        // a quote typed at a word end still pairs; typing it again moves
        // over, so the actual feel of it's is: type it, ' → it'', then
        // s → it's.
        let mut st4 = st_of("it");
        st4.set_caret(2, false);
        st4.type_text("\'");
        assert_eq!(
            st4.active_doc().read_all(),
            "it''",
            "quote at word end pairs, caret in between"
        );
        assert_eq!(st4.caret().pos, 3);
        st4.type_text("s");
        st4.type_text("\'");
        assert_eq!(
            st4.active_doc().read_all(),
            "it's'",
            "the second quote moves over the pair"
        );
    }

    /// Auto-close is one undo step, and paste is unaffected.
    #[test]
    fn autoclose_is_one_undo_step_and_paste_is_verbatim() {
        let mut st = st_of("");
        st.type_text("(");
        assert_eq!(st.active_doc().read_all(), "()");
        st.undo();
        assert_eq!(
            st.active_doc().read_all(),
            "",
            "paired insert is one undo step"
        );

        // paste path (insert_text) does not trigger auto-close
        let mut st2 = st_of("");
        st2.insert_text("(");
        assert_eq!(
            st2.active_doc().read_all(),
            "(",
            "insert_text inserts verbatim"
        );
    }

    /// With the toggle off, everything is verbatim.
    #[test]
    fn the_toggle_disables_pairing() {
        let mut st = st_of("");
        st.auto_close = false;
        st.type_text("(");
        assert_eq!(st.active_doc().read_all(), "(");
    }
}

#[cfg(test)]
mod readonly_tests {
    use super::*;
    use crate::ui::commands::testutil::st_of;

    /// In read-only mode typing/backspace/delete all fail and the wording is
    /// only shown once.
    #[test]
    fn readonly_blocks_typing_paths() {
        let mut st = st_of("abc");
        st.read_only = true;
        st.set_caret(3, false);
        st.type_text("x");
        st.insert_text("y");
        st.delete_back();
        st.delete_fwd();
        assert_eq!(
            st.active_doc().read_all(),
            "abc",
            "read-only must not modify the document"
        );
        assert_eq!(st.caret().pos, 3, "the caret must not move either");
        assert_eq!(
            st.status.as_ref().map(|m| m.key()),
            Some(format!("Plain:{}", READONLY_MSG.key()))
        );
    }

    /// Edit commands are blocked by run's classifier; save/copy/navigation
    /// proceed normally.
    ///
    /// The document must carry a real path: a pathless `Cmd::Save` opens the
    /// "save as" dialog (rfd's blocking native window) and the test process
    /// would wait there forever for a button click.
    #[test]
    fn readonly_blocks_editing_commands_but_not_saving() {
        use crate::ui::actions::{run, Cmd};
        let mut st = st_of("abc");
        let tmp = std::env::temp_dir().join("infeditor_readonly_save_test.txt");
        std::fs::write(&tmp, "old").unwrap();
        st.tabs[0].doc.meta.path = Some(tmp.clone());
        st.read_only = true;
        st.select_all();
        run(&mut st, Cmd::Delete);
        run(&mut st, Cmd::Case(crate::textops::CaseKind::Upper));
        assert_eq!(st.active_doc().read_all(), "abc");

        // non-edit commands proceed: copy, navigation, find, save
        run(&mut st, Cmd::CopyAll);
        run(&mut st, Cmd::FindNext);
        run(&mut st, Cmd::Save);
        assert_ne!(
            st.status.as_ref().map(|m| m.key()),
            Some(format!("Plain:{}", READONLY_MSG.key())),
            "non-edit commands must not be blocked"
        );
        assert_eq!(
            std::fs::read_to_string(&tmp).unwrap(),
            "abc",
            "save should still write to disk"
        );
        let _ = std::fs::remove_file(&tmp);
    }

    /// Vim change plans are discarded in read-only mode.
    #[test]
    fn readonly_blocks_vim_plans() {
        let mut st = st_of("abc");
        st.read_only = true;
        st.set_caret(0, false);
        // empty plan (the Vim engine produces an empty plan for rejected
        // operations)
        st.vim.enabled = true;
        st.apply_vim_plan(&crate::ui::vim::Plan { ops: Vec::new() });
        assert_eq!(st.active_doc().read_all(), "abc");
    }

    /// Everything resumes once read-only is turned off.
    #[test]
    fn toggling_readonly_off_restores_editing() {
        let mut st = st_of("abc");
        st.read_only = true;
        st.type_text("x");
        assert_eq!(st.active_doc().read_all(), "abc");
        st.read_only = false;
        st.set_caret(3, false);
        st.type_text("x");
        assert_eq!(st.active_doc().read_all(), "abcx");
    }

    /// The whole-document JSON tree view is a read-only preview: `hit_test`
    /// no longer maps clicks to caret positions.
    #[test]
    fn json_preview_hit_test_returns_none() {
        let mut st = st_of(r#"{"a":1}"#);
        st.tabs[0].json.enabled = true;
        let doc = st.tabs[0].doc.clone();
        let loc = st.locale;
        st.tabs[0].json.refresh(&doc, loc);
        assert!(
            st.hit_test(200.0, 200.0).is_none(),
            "preview mode accepts no caret landing"
        );
    }

    /// With the whole-document JSON tree view on, `content_h`'s scroll bound
    /// becomes the expanded tree row count.
    #[test]
    fn json_preview_content_h_uses_tree_total() {
        let mut st = st_of(r#"{"a":[1,2],"b":3}"#);
        let doc_h = st.content_h();
        st.tabs[0].json.enabled = true;
        let doc = st.tabs[0].doc.clone();
        let loc = st.locale;
        st.tabs[0].json.refresh(&doc, loc);
        let tree_h = st.content_h();
        assert!(
            tree_h > doc_h,
            "preview content is taller: {tree_h} > {doc_h}"
        );
        // after collapsing all, height falls back but is still the tree's
        // (>= one row)
        st.tabs[0].json.collapse_all();
        let collapsed_h = st.content_h();
        assert!(collapsed_h < tree_h);
        assert!(collapsed_h >= 0.0);
    }
}

#[cfg(test)]
mod tests {
    use super::super::testutil::st_of;
    use super::*;

    fn long_doc(lines: u64) -> String {
        (0..lines).map(|i| format!("line {i}\n")).collect()
    }

    /// Touch panning of the find results panel scrolls by **rows** (see
    /// `apply_pending_scroll`'s d[2]).
    #[test]
    fn pending_scroll_moves_the_find_results_panel_by_rows() {
        use crate::ui::components::findbar::RESULT_ROW_H;
        crate::ui::scroll::reset();
        let mut st = st_of(&long_doc(200));
        st.open_find(false, true);
        st.find.pattern = "line".into();
        st.find_all_matches();
        assert_eq!(st.find.panel_top, 0);
        // content moves up one row (finger swipes up) → the list's first row
        // moves down one
        crate::ui::scroll::queue(
            crate::ui::scroll::Target::Find,
            0.0,
            -f64::from(RESULT_ROW_H),
        );
        assert!(st.apply_pending_scroll());
        assert_eq!(st.find.panel_top, 1);
        // back to top and push down again: clamps at 0
        crate::ui::scroll::queue(
            crate::ui::scroll::Target::Find,
            0.0,
            f64::from(RESULT_ROW_H),
        );
        assert!(st.apply_pending_scroll());
        assert_eq!(st.find.panel_top, 0);
        crate::ui::scroll::queue(
            crate::ui::scroll::Target::Find,
            0.0,
            f64::from(RESULT_ROW_H),
        );
        assert!(
            !st.apply_pending_scroll(),
            "pushing further at the top must not dirty"
        );
    }

    /// Scroll queue → state: deltas accumulated within one frame land in one
    /// step (see `ui::scroll`). Sign matches the wheel (`dy` is content
    /// displacement; positive = content down = position decreases).
    #[test]
    fn pending_scroll_applies_in_one_step_and_clamps() {
        crate::ui::scroll::reset();
        let mut st = crate::ui::testutil::st_of_view(&long_doc(200));
        st.scroll_top = 0.0;
        crate::ui::scroll::queue(crate::ui::scroll::Target::Doc, 0.0, -80.0);
        assert!(crate::ui::scroll::pending());
        assert!(
            st.apply_pending_scroll(),
            "content moving up 80px must actually scroll"
        );
        assert!((st.scroll_top - 80.0).abs() < 0.01, "{}", st.scroll_top);
        assert!(
            !crate::ui::scroll::pending(),
            "queue must be empty once flushed"
        );
        // back to top (content moves down 80px)
        crate::ui::scroll::queue(crate::ui::scroll::Target::Doc, 0.0, 80.0);
        assert!(st.apply_pending_scroll());
        assert_eq!(st.scroll_top, 0.0);
        // pushing down while already at the top: state unchanged (the caller
        // skips dirtying on this)
        crate::ui::scroll::queue(crate::ui::scroll::Target::Doc, 0.0, 50.0);
        assert!(!st.apply_pending_scroll());
        assert_eq!(st.scroll_top, 0.0);
    }

    /// The bar tests below are about the bars, so the minimap is switched off:
    /// it takes 84px out of the same text column and every width assertion
    /// would otherwise have to carry it along. `minimap_tests` covers it.
    fn st_bars(text: &str) -> EditorState {
        let mut st = st_of(text);
        st.minimap = false;
        st
    }

    /// Status-bar position info: mid-char byte offsets must clamp to a char
    /// boundary. The wys bridge sends emit byte offsets (`\n`) while the
    /// document may be CRLF (`\r\n`), so the whole thing can be skewed by a
    /// few bytes. Before the fix, `text[..off]` panicked directly and took the process down (real machine: clicking a Chinese title crashes).
    #[test]
    fn position_info_clamps_mid_char_offset() {
        let mut st = st_of("中文\n");
        st.caret_mut().pos = 4; // '文' spans 3..6, so 4 is mid-character
        let (line, col, _) = st.position_info();
        assert_eq!((line, col), (1, 2));
    }

    /// Soft-keyboard avoidance: when the keyboard "pops up" (the inset
    /// grows) the viewport shrinks and recovers its old height on collapse;
    /// jitter below 0.5px is ignored, so no relayout every frame.
    #[test]
    fn keyboard_inset_shrinks_the_viewport_and_recovers() {
        let mut st = st_of("");
        st.set_window_size(1200.0, 800.0);
        let base = st.view.h;
        st.set_keyboard_inset(300.0);
        assert!(
            st.view.h < base,
            "the keyboard inset must eat into the viewport"
        );
        assert!(
            (st.view.h - (base - 300.0)).abs() < 0.01,
            "the inset should be subtracted from the viewport height as-is"
        );
        st.set_keyboard_inset(0.0);
        assert_eq!(
            st.view.h, base,
            "the viewport must recover once the keyboard collapses"
        );
        // sub-pixel jitter does not enter layout (threshold 0.5px)
        st.set_keyboard_inset(0.3);
        assert_eq!(st.view.h, base, "a 0.3px inset should be ignored");
    }

    // ---- column-mode selection -------------------------------------------

    /// Text and coordinates: line 0 `hello world` (0..11), line 1 starts at
    /// 12, line 2 at 24.
    fn col_fixture() -> EditorState {
        st_of("hello world\nhello there\nhello again")
    }

    #[test]
    fn column_rect_covers_the_same_character_columns_on_every_line() {
        let mut st = col_fixture();
        st.set_caret(6, false); // line 0, col 6 (`w`)
        st.begin_end_select_column();
        assert!(st.col_anchor.is_some());
        st.set_caret(24 + 11, false); // line 2, col 11 (line end)

        let rect = st.col_rect().expect("column mode active");
        assert_eq!(rect.len(), 3);
        assert_eq!(rect[0], (0, 6, 11));
        assert_eq!(rect[1], (1, 6, 11));
        assert_eq!(rect[2], (2, 6, 11));
    }

    #[test]
    fn column_copy_joins_the_rectangles_line_by_line() {
        let mut st = col_fixture();
        st.set_caret(6, false);
        st.begin_end_select_column();
        st.set_caret(24 + 11, false);
        st.copy();
        assert_eq!(st.clip, "world\nthere\nagain");
    }

    #[test]
    fn column_typing_replaces_the_rectangle_on_every_line() {
        let mut st = col_fixture();
        st.set_caret(6, false);
        st.begin_end_select_column();
        st.set_caret(24 + 11, false);
        st.insert_text("X");
        assert_eq!(
            st.active_doc().read_all().replace("\r\n", "\n"),
            "hello X\nhello X\nhello X"
        );
        // the rectangle stays active: consecutive typing works
        assert!(st.col_anchor.is_some());
        st.insert_text("Y");
        assert_eq!(
            st.active_doc().read_all().replace("\r\n", "\n"),
            "hello XY\nhello XY\nhello XY"
        );
    }

    #[test]
    fn column_delete_removes_the_rectangle_and_keeps_the_rest() {
        let mut st = col_fixture();
        st.set_caret(6, false);
        st.begin_end_select_column();
        st.set_caret(24 + 11, false);
        st.delete_fwd();
        // the rectangle is columns [6,11): the space at col 5 stays
        assert_eq!(
            st.active_doc().read_all().replace("\r\n", "\n"),
            "hello \nhello \nhello "
        );
    }

    #[test]
    fn column_mode_clamps_short_lines_instead_of_panicking() {
        // line 0 is short: the rectangle clamps to the line end there
        let mut st = st_of("ab\nlonger line\nxy");
        st.set_caret(0, false);
        st.begin_end_select_column();
        st.set_caret(3 + 8, false); // line 1 starts at byte 3, col 8
        let rect = st.col_rect().expect("column mode active");
        assert_eq!(rect[0], (0, 0, 2), "short line clamps to the line end");
        assert_eq!(rect[1], (1, 0, 8));
        st.insert_text("#");
        let all = st.active_doc().read_all().replace("\r\n", "\n");
        assert_eq!(all, "#\n#ine\nxy");
    }

    #[test]
    fn backspace_after_column_typing_deletes_what_was_typed() {
        let mut st = col_fixture();
        st.set_caret(6, false);
        st.begin_end_select_column();
        st.set_caret(24 + 11, false);
        st.insert_text("X"); // the rectangle collapses to zero width, just after X
        assert_eq!(
            st.active_doc().read_all().replace("\r\n", "\n"),
            "hello X\nhello X\nhello X"
        );
        st.delete_back(); // zero-width backspace extends one left, deleting the just-typed X
        assert_eq!(
            st.active_doc().read_all().replace("\r\n", "\n"),
            "hello \nhello \nhello "
        );
    }

    #[test]
    fn column_paste_distributes_block_lines_across_the_rectangle() {
        let mut st = col_fixture();
        st.set_caret(6, false);
        st.begin_end_select_column();
        st.set_caret(24 + 11, false);
        // Test column paste's core directly: above paste() sits the system
        // clipboard (an earlier copy test just wrote to it), uncontrollable
        // in tests, so bypass it and call col_apply_map.
        let lines = ["1".to_string(), "2".to_string(), "3".to_string()];
        assert!(st.col_apply_map(ColOp::Insert, |i| lines.get(i).cloned()));
        assert_eq!(
            st.active_doc().read_all().replace("\r\n", "\n"),
            "hello 1\nhello 2\nhello 3"
        );
    }

    #[test]
    fn ending_column_mode_restores_linear_behaviour() {
        let mut st = col_fixture();
        st.set_caret(6, false);
        st.begin_end_select_column();
        st.begin_end_select_column(); // press again = end
        assert!(st.col_anchor.is_none());
        st.insert_text("Z");
        // the caret is still at column 6: the linear insert happens there
        assert_eq!(
            st.active_doc().read_all().replace("\r\n", "\n"),
            "hello Zworld\nhello there\nhello again"
        );
    }

    #[test]
    fn bars_appear_only_when_content_overflows() {
        // a three-line document fits in every direction
        let mut st = st_bars("a\nb\nc\n");
        st.pump_h_extent(1000);
        assert!(!st.vbar_on(), "short document: no vertical bar");
        assert!(!st.hbar_on(), "short document: no horizontal bar");
        assert_eq!(st.text_h(), st.view.h);
        assert_eq!(st.text_w(), st.view.w - st.gutter_w());

        // a tall document needs the vertical bar only
        let mut st = st_bars(&long_doc(5000));
        st.pump_h_extent(100_000);
        assert!(st.vbar_on(), "tall document: vertical bar");
        assert!(!st.hbar_on(), "tall document: still no horizontal bar");

        // one very long line needs the horizontal bar
        let mut st = st_bars(&format!("{}\nsecond\n", "x".repeat(400)));
        st.pump_h_extent(1000);
        assert!(st.hbar_on(), "long line: horizontal bar");
    }

    /// The bars must *take* space: the text area is what is left over, so the
    /// last line and the tail of the longest line stay reachable.
    #[test]
    fn bars_take_space_out_of_the_text_area() {
        let mut st = st_bars(&long_doc(5000));
        st.pump_h_extent(100_000);
        assert_eq!(st.text_w(), st.view.w - st.gutter_w() - SCROLLBAR_W);
        assert_eq!(st.text_h(), st.view.h);
        // ... and the maximum scroll is measured against the *text* area
        assert_eq!(st.max_scroll_top(), st.content_h() - st.text_h() as f64);

        let mut st = st_bars(&format!("{}\nsecond\n", "x".repeat(400)));
        st.pump_h_extent(1000);
        assert_eq!(st.text_h(), st.view.h - SCROLLBAR_W);
        assert!(st.max_scroll_left() > 0.0);
    }

    /// Both bars at once: the corner is reserved by each, and neither may
    /// leave the other's content unreachable.
    #[test]
    fn both_bars_together() {
        let mut text = long_doc(5000);
        text.push_str(&"y".repeat(400));
        text.push('\n');
        let mut st = st_bars(&text);
        st.pump_h_extent(100_000);
        assert!(st.vbar_on() && st.hbar_on(), "both bars");
        assert_eq!(st.text_h(), st.view.h - SCROLLBAR_W);
        assert_eq!(st.text_w(), st.view.w - st.gutter_w() - SCROLLBAR_W);
        // dragging to the end reaches the last line and the longest tail
        st.scroll_top = f64::MAX;
        st.scroll_left = f32::MAX;
        st.clamp_scroll();
        assert_eq!(st.scroll_top, st.max_scroll_top());
        assert_eq!(st.scroll_left, st.max_scroll_left());
        assert!(st.scroll_top + st.text_h() as f64 >= st.content_h() - 1.0);
        assert!(st.scroll_left + st.text_w() >= st.content_w() - 1.0);
    }

    /// The horizontal extent is a *scan*, and it must never shrink inside one
    /// document generation — otherwise the bar (and the text column's width)
    /// would flicker on every keystroke.
    #[test]
    fn horizontal_extent_is_monotone_within_a_generation() {
        let mut st = st_of(&format!("{}\nshort\n", "x".repeat(400)));
        st.pump_h_extent(2);
        assert!(st.h_extent.width >= 0.0);
        st.pump_h_extent(1000);
        assert!(st.h_extent.done, "the scan finishes on a small document");
        let before = st.h_extent.width;
        assert!(before > 0.0, "a 400-char line must measure non-zero");

        // after an edit the measured width is kept, and the visible/caret
        // lines being edited are folded in (may widen)
        st.insert_text("z");
        st.pump_h_extent(0);
        assert!(
            st.h_extent.width >= before,
            "extent survives an edit: {} >= {}",
            st.h_extent.width,
            before
        );
        st.pump_h_extent(1000);
        assert!(st.h_extent.width >= before, "extent never shrinks");
    }

    /// A line is never read past the cap, so a minified one-liner cannot stall
    /// the scan (and never allocates the whole line just to throw it away).
    #[test]
    fn huge_line_is_truncated_for_measurement() {
        let mut doc = crate::buffer::TextDocument::empty();
        doc.insert_text(0, &"x".repeat(10_000));
        assert_eq!(line_head(&doc, 0).len(), H_LINE_CAP);

        // a multi-byte character straddling the cap must not be split
        let mut doc = crate::buffer::TextDocument::empty();
        doc.insert_text(0, &format!("{}δδδ", "x".repeat(H_LINE_CAP - 4)));
        let t = line_head(&doc, 0);
        assert_eq!(t.len(), H_LINE_CAP);
        assert!(
            t.chars().all(|c| c == 'x' || c == 'δ'),
            "characters must not be split"
        );
    }

    /// Opening sampling must cover the last line: documents where "only the
    /// last line is unusually wide" (minified JSON without a trailing
    /// newline, log tails) always fell short under the old `i * stride`
    /// formula — after integer truncation of the stride, the sample could
    /// never reach the last line. The sample now spreads uniformly over
    /// 0..=lines-1.
    #[test]
    fn opening_sample_reaches_the_widest_last_line() {
        // 10k short lines + one extra-wide last line (far below H_SCAN_LINES,
        // so full sampling runs)
        let mut text = "ab\n".repeat(10_000);
        text.push_str(&"x".repeat(400));
        let mut st = st_of(&text);
        // Run only the opening-sample part (triggered by pump_h_extent on doc
        // switch), with no incremental scan budget: the wide line must be
        // found during sampling.
        st.pump_h_extent(0);
        let wide = st.metrics.line_width(&"x".repeat(400), TAB_SIZE);
        assert!(
            st.h_extent.width >= wide,
            "the wide last line must be in the sample: {} >= {}",
            st.h_extent.width,
            wide
        );
    }

    /// One press on an arrow button = one line (or three columns), and it
    /// clamps at both ends instead of running past them.
    #[test]
    fn arrow_step_moves_exactly_one_line() {
        let mut st = st_of(&long_doc(5000));
        st.pump_h_extent(100_000);
        let lh = st.metrics.lh;
        st.sb_step(true, 1);
        assert_eq!(st.scroll_top, lh as f64);
        st.sb_step(true, -1);
        st.sb_step(true, -1);
        assert_eq!(st.scroll_top, 0.0, "never scrolls above the first line");

        st.scroll_top = st.max_scroll_top();
        st.sb_step(true, 1);
        assert_eq!(st.scroll_top, st.max_scroll_top(), "never past the end");
    }

    /// A click on the bare track pages towards the pointer.
    #[test]
    fn track_click_pages() {
        let mut st = st_of(&long_doc(5000));
        st.pump_h_extent(100_000);
        let page = st.text_h() - st.metrics.lh;
        // below the thumb -> down a page
        st.sb_track_click(true, 400.0);
        assert!(
            (st.scroll_top - page as f64).abs() < 1e-3,
            "{}",
            st.scroll_top
        );
        // back up again
        st.sb_track_click(true, 0.0);
        assert!(st.scroll_top.abs() < 1e-3, "{}", st.scroll_top);
    }

    /// Dragging the thumb maps the pointer onto the whole scroll range: grab it
    /// anywhere, and the view follows one-for-one with the track.
    #[test]
    fn dragging_the_thumb_covers_the_whole_range() {
        let mut st = st_of(&long_doc(5000));
        st.pump_h_extent(100_000);
        let g = st.v_geom();
        // grab the thumb 5px from its leading edge, while at scroll 0
        st.sb_begin(true, SCROLLBAR_W as f64 + g.off + 5.0);
        // park the pointer at the very end of the track -> maximum scroll
        st.sb_apply(crate::ui::scroll::SbPointer {
            vertical: true,
            cx: st.view.x,
            cy: st.view.y + SCROLLBAR_W + 5.0 + (g.track - g.len) as f32,
        });
        assert!(
            (st.scroll_top - st.max_scroll_top()).abs() < 1.0,
            "end of track must be max scroll: {} vs {}",
            st.scroll_top,
            st.max_scroll_top()
        );
        // and back to the top
        st.sb_apply(crate::ui::scroll::SbPointer {
            vertical: true,
            cx: st.view.x,
            cy: st.view.y + SCROLLBAR_W + 5.0,
        });
        assert!(st.scroll_top.abs() < 1.0, "start of track must be 0");
        st.sb_end();
        assert!(st.sb_drag.is_none());
    }

    /// A thumb grabbed off-centre must not jump: the view only moves by the
    /// pointer's movement, not by the grab offset.
    #[test]
    fn grabbing_the_thumb_does_not_jump() {
        let mut st = st_of(&long_doc(5000));
        st.pump_h_extent(100_000);
        st.scroll_top = 10_000.0;
        st.clamp_scroll();
        let g = st.v_geom();
        let at = SCROLLBAR_W as f64 + g.off + g.len - 2.0;
        st.sb_begin(true, at);
        // no movement yet
        st.sb_apply(crate::ui::scroll::SbPointer {
            vertical: true,
            cx: st.view.x,
            cy: st.view.y + at as f32,
        });
        assert!(
            (st.scroll_top - 10_000.0).abs() < 2.0,
            "grabbing the thumb must not move the view: {}",
            st.scroll_top
        );
        // and it stays proportional on the way down
        let before = st.scroll_top;
        st.sb_apply(crate::ui::scroll::SbPointer {
            vertical: true,
            cx: st.view.x,
            cy: st.view.y + at as f32 + 100.0,
        });
        assert!(st.scroll_top > before, "dragging down scrolls down");
    }

    /// The **input-side** thumb tracks the pointer and clamps to the track;
    /// with no live drag there is no thumb position to invent.
    #[test]
    fn sb_live_off_tracks_pointer_and_clamps() {
        let mut st = st_of(&long_doc(5000));
        st.pump_h_extent(100_000);
        let g = st.v_geom();
        st.sb_begin(true, SCROLLBAR_W as f64 + g.off + 5.0);
        // midway pointer -> live off between 0 and track-len
        let mid = st.view.y + SCROLLBAR_W + (st.text_h() - 2.0 * SCROLLBAR_W) * 0.5;
        let live = st.sb_live_off(true, st.view.x, mid).expect("drag is live");
        assert!(
            (live.off - (g.track - g.len) * 0.5).abs() < 6.0,
            "mid-track pointer maps to mid-track thumb: {} vs {}",
            live.off,
            (g.track - g.len) * 0.5
        );
        // far past the end -> clamped
        let past = st.view.y + st.text_h() + 500.0;
        let live = st.sb_live_off(true, st.view.x, past).expect("drag is live");
        assert!(
            (live.off - (g.track - g.len)).abs() < 1e-6,
            "clamped at track end: {}",
            live.off
        );
        // teardown: no drag, no invented position
        st.sb_end();
        assert!(st.sb_live_off(true, st.view.x, mid).is_none());
    }

    /// Releasing must not jump: after the content side lands the same sample,
    /// `thumb_geom(scroll_top)` reproduces the input-side offset.
    #[test]
    fn sb_release_matches_input_side_thumb() {
        let mut st = st_of(&long_doc(5000));
        st.pump_h_extent(100_000);
        st.sb_begin(true, SCROLLBAR_W as f64 + 5.0);
        let mid = st.view.y + SCROLLBAR_W + (st.text_h() - 2.0 * SCROLLBAR_W) * 0.4;
        let live = st.sb_live_off(true, st.view.x, mid).expect("drag is live");
        st.sb_apply(crate::ui::scroll::SbPointer {
            vertical: true,
            cx: st.view.x,
            cy: mid,
        });
        let g = st.v_geom();
        assert!(
            (g.off - live.off).abs() < 0.01,
            "content side must reproduce the input-side thumb: {} vs {}",
            g.off,
            live.off
        );
    }

    /// A teardown without a release (tab switch) discards the pending sample:
    /// landing it would scroll the *new* surface.
    #[test]
    fn sb_end_discards_pending_sample() {
        let mut st = st_of(&long_doc(5000));
        st.pump_h_extent(100_000);
        st.sb_begin(true, SCROLLBAR_W as f64 + 5.0);
        crate::ui::scroll::sb_queue(crate::ui::scroll::SbPointer {
            vertical: true,
            cx: st.view.x,
            cy: st.view.y + 400.0,
        });
        st.sb_end();
        assert!(
            !crate::ui::scroll::sb_pending(),
            "teardown must discard, not land"
        );
        assert_eq!(st.scroll_top, 0.0, "a stale sample must not scroll");
    }
}

#[cfg(test)]
mod jsonl_tests {
    use super::super::testutil::st_of;
    use super::*;

    /// 200 plain lines + JSON on line 0, ensuring the scroll bound is large
    /// enough
    fn st_jsonl() -> EditorState {
        let mut text = String::from("{\"a\":[1,2,3]}\n");
        for i in 0..200 {
            text.push_str(&format!("line {i}\n"));
        }
        let mut st = st_of(&text);
        st.tabs[0].jsonl.enabled = true;
        st.view.x = 0.0;
        st.view.y = 0.0;
        st.view.w = 1280.0;
        st.view.h = 786.0;
        let doc = st.tabs[0].doc.clone();
        st.tabs[0].jsonl.refresh(&doc);
        st
    }

    #[test]
    fn toggling_the_preview_updates_the_session_preference() {
        let mut st = st_of("{}\n");
        let spec =
            crate::highlight::for_path_and_first_line(Some(std::path::Path::new("x.jsonl")), "");
        assert_eq!(spec.name, "JSON Lines");
        st.tabs[0] = crate::ui::state::Tab::with_spec(st.tabs[0].doc.clone(), spec);
        st.jsonl_pref = None;
        st.toggle_jsonl();
        assert_eq!(
            st.jsonl_pref,
            Some(false),
            "the next JSONL file inherits it"
        );
        st.toggle_jsonl();
        assert_eq!(st.jsonl_pref, Some(true));
    }

    #[test]
    fn hit_test_skips_virtual_rows_and_maps_doc_rows() {
        let mut st = st_jsonl();
        let doc = st.tabs[0].doc.clone();
        st.tabs[0].jsonl.toggle_line(&doc, 0, 5);
        let lh = st.metrics.lh.max(1.0) as f64;
        // virtual row (inside the expanded block) → None
        let y_virt = PAD_TOP as f64 + (st.tabs[0].jsonl.vstart(0) + 2) as f64 * lh + lh * 0.5;
        assert_eq!(
            st.hit_test(200.0, y_virt),
            None,
            "virtual rows are not editable"
        );
        // document line 1 after the expanded block → correct landing
        let y_doc1 = PAD_TOP as f64 + st.tabs[0].jsonl.vstart(1) as f64 * lh + lh * 0.5;
        let got = st.hit_test(200.0, y_doc1).unwrap();
        assert_eq!(st.tabs[0].doc.line_of_pos(got), 1);
        // unexpanded region unchanged
        let got0 = st
            .hit_test(
                200.0,
                PAD_TOP as f64 + st.tabs[0].jsonl.vstart(0) as f64 * lh + 0.5 * lh,
            )
            .unwrap();
        assert_eq!(st.tabs[0].doc.line_of_pos(got0), 0);
    }

    #[test]
    fn marker_click_expands_and_anchors_scroll() {
        let mut st = st_jsonl();
        let lh = st.metrics.lh.max(1.0) as f64;
        // viewport starts at document line 2 ⇒ line 0's marker is above the
        // viewport
        st.scroll_top = 2.0 * lh;
        // Click the fold column right of the line numbers — the `MARKER_W` slot
        // `fold_marker_row_at` tests, unmoved by indent/horizontal scroll
        let x = st.gutter_w() as f64 - MARKER_W as f64 / 2.0;
        // line 0 half visible (top edge cut in half):
        // before*lh < scroll_top ⇒ anchoring compensation triggers
        st.scroll_top = 0.5 * lh;
        let y = PAD_TOP as f64 + lh * 0.1; // line 0's top edge is half cut, still visible
        assert!(
            st.jsonl_marker_click(x, y),
            "marker column click should be consumed"
        );
        assert!(st.tabs[0].jsonl.open.contains_key(&0));
        let k = st.tabs[0].jsonl.open[&0].span() as f64;
        assert!(
            (st.scroll_top - (0.5 * lh + k * lh)).abs() < 1.0,
            "expanding rows above the viewport should anchor scroll: {} vs {}",
            st.scroll_top,
            0.5 * lh + k * lh
        );
        // back to the top and click again to collapse; scroll unaffected
        st.scroll_top = 0.0;
        assert!(st.jsonl_marker_click(x, y));
        assert!(st.tabs[0].jsonl.open.is_empty());
        assert!((st.scroll_top - 0.0).abs() < 1.0);
    }

    #[test]
    fn marker_click_on_virtual_row_toggles_node() {
        let mut st = st_jsonl();
        let doc = st.tabs[0].doc.clone();
        st.tabs[0].jsonl.toggle_line(&doc, 0, 5);
        let lh = st.metrics.lh.max(1.0) as f64;
        let rows_full = st.tabs[0].jsonl.open[&0].rows.len();
        // the expanded block's first row (sub=1) is the "a": [ container
        let sub_row = &st.tabs[0].jsonl.open[&0].rows[1];
        let sub_foldable = sub_row.foldable;
        // virtual rows share the fold column right of the line numbers with
        // document rows (independent of indent depth)
        let x = st.gutter_w() as f64 - MARKER_W as f64 / 2.0;
        let y = PAD_TOP as f64 + (st.tabs[0].jsonl.vstart(0) + 2) as f64 * lh + lh * 0.5;
        assert!(st.jsonl_marker_click(x, y));
        if sub_foldable {
            let rows_less = st.tabs[0].jsonl.open[&0].rows.len();
            assert!(
                rows_less < rows_full,
                "collapsing the sub-container should reduce the row count"
            );
        }
    }

    #[test]
    fn marker_click_outside_marker_column_is_ignored() {
        let mut st = st_jsonl();
        let lh = st.metrics.lh.max(1.0) as f64;
        // neither the line-number area (left of the fold column) nor the
        // text area counts as a hit
        for x in [
            st.gutter_w() as f64 - MARKER_W as f64 - 4.0,
            st.gutter_w() as f64 + 100.0,
        ] {
            assert!(!st.jsonl_marker_click(x, PAD_TOP as f64 + lh * 0.5));
        }
        assert!(st.tabs[0].jsonl.open.is_empty());
    }

    #[test]
    fn max_scroll_top_covers_expansions() {
        let mut st = st_jsonl();
        let lh = st.metrics.lh.max(1.0) as f64;
        let before = st.max_scroll_top();
        let doc = st.tabs[0].doc.clone();
        st.tabs[0].jsonl.toggle_line(&doc, 0, 5);
        let k = st.tabs[0].jsonl.open[&0].span() as f64;
        assert!((st.max_scroll_top() - (before + k * lh)).abs() < 1.0);
    }

    #[test]
    fn caret_xy_uses_virtual_row_of_caret_line() {
        let mut st = st_jsonl();
        let lh = st.metrics.lh.max(1.0) as f64;
        // the caret sits on document line 1 (below the expanded block)
        let pos = st.tabs[0].doc.pos_of_line(1);
        st.tabs[0].caret.pos = pos;
        let (_, y_before, _) = st.caret_xy();
        let doc = st.tabs[0].doc.clone();
        st.tabs[0].jsonl.toggle_line(&doc, 0, 5);
        let (_, y_after, _) = st.caret_xy();
        let k = st.tabs[0].jsonl.open[&0].span() as f64;
        assert!(
            (y_after - y_before - (k * lh) as f32).abs() < 0.01,
            "the caret's line moved down k*lh: {y_before} -> {y_after}"
        );
    }
}

#[cfg(test)]
mod minimap_tests {
    use super::super::testutil::st_of;
    use super::*;

    fn long_doc(lines: u64) -> String {
        (0..lines).map(|i| format!("line {i}\n")).collect()
    }

    /// The strip stands between the text and the vertical scrollbar and takes
    /// its width out of the text column.
    #[test]
    fn the_minimap_takes_width_out_of_the_text_column() {
        let mut st = st_of(&long_doc(5000));
        assert!(st.mm_on());
        assert_eq!(
            st.text_w(),
            st.view.w - st.gutter_w() - SCROLLBAR_W - MINIMAP_W
        );
        assert_eq!(
            st.mm_left(),
            st.view.w - MINIMAP_W - SCROLLBAR_W,
            "it sits left of the vertical bar"
        );
        assert_eq!(st.mm_h(), st.text_h(), "and is as tall as the text area");

        let wide = st.text_w();
        st.toggle_minimap();
        assert!(!st.mm_on());
        assert!(
            (st.text_w() - wide - MINIMAP_W).abs() < 0.01,
            "switching it off gives the width back"
        );
        st.toggle_minimap();
        assert!(st.mm_on(), "and the toggle is symmetric");
    }

    /// On a window too narrow to leave a usable text column the strip turns
    /// itself off — 84px of minimap is worse than no text. The 320px minimum
    /// window width itself now fits (the gutter got narrower), so the yielding
    /// rule is exercised below it by setting `win.w` directly.
    #[test]
    fn a_narrow_window_gets_no_minimap() {
        let mut st = st_of(&long_doc(5000));
        assert!(st.mm_on());
        // 320px minimum window width (the viewport no longer insets the zoom
        // band, no extra padding needed)
        st.set_window_size(320.0, 600.0);
        assert!(
            st.mm_on(),
            "at the 320px minimum the text column still fits beside the strip"
        );
        st.win.w = 200.0;
        st.relayout();
        assert!(!st.mm_on(), "no room for the text -> the strip yields");
        assert_eq!(st.mm_w(), 0.0);
        st.set_window_size(1280.0, 860.0);
        assert!(st.mm_on(), "and comes back when there is room again");
    }

    /// Past the byte limit the strip turns itself off even with the
    /// preference on: the per-frame ink tax is not worth paying on a
    /// document whose shape nobody can read at that scale. The preference
    /// itself survives, so a smaller document gets the strip back.
    #[test]
    fn an_oversized_document_gets_no_minimap() {
        let mut st = st_of(&long_doc(5000));
        assert!(st.mm_on());
        let big = "x".repeat(MM_MAX_BYTES as usize + 1);
        st.tabs[0].doc = crate::buffer::TextDocument::from(big.as_str());
        assert!(!st.mm_on(), "over the byte limit the strip yields");
        assert_eq!(st.mm_w(), 0.0);
        assert!(st.minimap, "the preference itself is untouched");
        // the limit is inclusive: exactly at it the strip still shows
        let edge = "x".repeat(MM_MAX_BYTES as usize);
        st.tabs[0].doc = crate::buffer::TextDocument::from(edge.as_str());
        assert!(st.mm_on(), "exactly at the limit the strip still shows");
    }

    /// Turning the minimap on for an oversized document is refused (the
    /// JSON-tree-preview precedent): checking the menu item while `mm_on`
    /// paints nothing would just look broken. Turning it off stays allowed.
    #[test]
    fn toggling_on_an_oversized_document_is_refused() {
        let big = "x".repeat(MM_MAX_BYTES as usize + 1);
        let mut st = st_of(&big);
        st.minimap = false;
        st.toggle_minimap();
        assert!(!st.minimap, "the preference must not flip");
        assert_eq!(
            st.status.as_ref().map(|m| m.key()),
            Some("With:StatusMinimapTooLarge".to_string()),
            "status bar should explain why: {:?}",
            st.status
        );
        // and the off direction is never refused
        st.minimap = true;
        st.toggle_minimap();
        assert!(!st.minimap);
        assert_eq!(
            st.status.as_ref().map(|m| m.key()),
            Some("Plain:StatusMinimapOff".to_string())
        );
    }

    /// A click centres what you clicked: you aim at a shape you are looking
    /// *for*, and it lands in front of you with context on both sides.
    #[test]
    fn clicking_the_minimap_centres_the_line_under_the_pointer() {
        let mut st = st_of(&long_doc(5000));
        let lh = st.metrics.lh as f64;
        let y = 100.0;
        let want = st.mm_line_at(y);
        st.mm_click(y);
        let mid = ((st.scroll_top + st.text_h() as f64 / 2.0) / lh).floor() as u64;
        assert_eq!(mid, want, "the clicked line must end up centred");
    }

    /// The drag is **glued to the indicator**: the pointer keeps the offset on
    /// the indicator it took at press (the press puts the indicator's centre on
    /// the pointer, so the offset is ~0), the strip stands still while the
    /// indicator travels inside it, and nothing snaps on release.
    ///
    /// This is the "跟手" property. The previous model froze the *pre-jump*
    /// strip scroll in the mapping while the paint re-centred the indicator
    /// every frame, so mapping and paint disagreed by up to a full strip height:
    /// the document scrolled with the pointer, but at a constant offset, and the
    /// map slid away underneath it.
    #[test]
    fn dragging_the_minimap_keeps_the_indicator_under_the_pointer() {
        let mut st = st_of(&long_doc(5000));
        st.mm_click(100.0);
        let top_at_press = st.mm_geom().top;
        // The indicator centre landed on the pointer at press.
        let g = st.mm_geom();
        let centre = g.slider.0 as f64 + g.slider.1 as f64 / 2.0;
        assert!(
            (centre - 100.0).abs() < 1.0,
            "press puts the indicator centre on the pointer: {centre}"
        );
        // Drag within the strip: the indicator follows 1:1 and the map stands
        // still.
        for y in [150.0, 300.0, 400.0] {
            st.mm_pointer(st.view.y as f64 + y);
            let g = st.mm_geom();
            let centre = g.slider.0 as f64 + g.slider.1 as f64 / 2.0;
            assert!(
                (centre - y).abs() < 1.0,
                "indicator glued at strip y={y}: centre {centre}"
            );
            assert_eq!(g.top, top_at_press, "the strip stands still mid-travel");
        }
        // Release: no snap — the strip stays where the drag left it.
        st.mm_end();
        let g = st.mm_geom();
        assert_eq!(g.top, top_at_press, "the strip must not jump on release");
        // Further moves are ignored.
        let held = st.scroll_top;
        st.mm_pointer(st.view.y as f64 + 20.0);
        assert_eq!(st.scroll_top, held);
    }

    /// The glue holds to the ends of the document: dragging past either stop
    /// pins the scroll there, the strip follows minimally (the indicator parks
    /// at the strip edge), and — the press having put the indicator's centre on
    /// the pointer — both ends stay reachable from one press.
    ///
    /// "Past the stop" is a stream of pointer events with the pointer clamped at
    /// the strip edge: each one lets the strip follow by up to the indicator's
    /// height, so the test repeats the event until the view settles.
    #[test]
    fn dragging_the_minimap_reaches_both_ends_and_stops_cleanly() {
        let mut st = st_of(&long_doc(5000));
        st.mm_click(100.0);
        // drag far past the bottom of the document
        for _ in 0..2000 {
            st.mm_pointer(st.view.y as f64 + 10_000.0);
            if (st.scroll_top - st.max_scroll_top()).abs() < 1e-6 {
                break;
            }
        }
        assert_eq!(
            st.scroll_top,
            st.max_scroll_top(),
            "dragging past the end stops at the last line"
        );
        let g = st.mm_geom();
        assert!(
            (f64::from(g.slider.0 + g.slider.1) - st.mm_h() as f64).abs() < 1.0,
            "indicator pinned at the strip's bottom edge: {:?}",
            g.slider
        );
        // and back up past the top
        for _ in 0..2000 {
            st.mm_pointer(st.view.y as f64 - 10_000.0);
            if st.scroll_top.abs() < 1e-6 {
                break;
            }
        }
        assert_eq!(st.scroll_top, 0.0, "dragging past the top stops at line 0");
        let g = st.mm_geom();
        assert!(g.slider.0.abs() < 1.0, "indicator pinned at the top edge");
        st.mm_end();
    }

    /// A click is still a jump to the clicked shape (the clicked line becomes
    /// the viewport centre), and after the jump the strip has **not** moved —
    /// the indicator moved to the pointer instead. That is what keeps the
    /// subsequent drag glued.
    #[test]
    fn clicking_the_minimap_does_not_move_the_strip() {
        let mut st = st_of(&long_doc(5000));
        let before = st.mm_geom().top;
        let want = st.mm_line_at(100.0);
        st.mm_click(100.0);
        let mid =
            ((st.scroll_top + st.text_h() as f64 / 2.0) / st.metrics.lh as f64).floor() as u64;
        assert_eq!(mid, want, "the clicked line must end up centred");
        assert_eq!(st.mm_geom().top, before, "the strip stands still at press");
    }

    /// A scroll that did not come from the strip re-centres the indicator (the
    /// pre-drag policy): the drag latch must not survive a goto-style jump.
    #[test]
    fn a_foreign_scroll_recentres_the_indicator() {
        let mut st = st_of(&long_doc(5000));
        st.mm_click(100.0);
        st.mm_pointer(st.view.y as f64 + 300.0);
        let dragged_top = st.mm_geom().top;
        // goto-style jump: far more than the indicator's free travel, landing
        // mid-document where centring is actually possible (at the very end the
        // indicator must sit at the strip's bottom — the document ends there).
        st.scroll_top = st.max_scroll_top() / 2.0;
        st.clamp_scroll();
        let g = st.mm_geom();
        assert_ne!(
            g.top, dragged_top,
            "a jump must re-centre the strip, not keep the drag position"
        );
        let centre = f64::from(g.slider.0) + f64::from(g.slider.1) / 2.0;
        assert!(
            (centre - st.mm_h() as f64 / 2.0).abs() < 2.0,
            "re-centred: indicator centre {centre}"
        );
    }

    /// The "you are here" box follows the viewport.
    #[test]
    fn the_indicator_tracks_the_viewport() {
        let mut st = st_of(&long_doc(5000));
        let a = st.mm_geom();
        assert!(a.scrolls, "a 5000-line document does not fit the strip");
        st.scroll_top = st.max_scroll_top();
        st.clamp_scroll();
        let b = st.mm_geom();
        assert!(
            b.slider.0 > a.slider.0,
            "indicator moved down: {} -> {}",
            a.slider.0,
            b.slider.0
        );
        assert!(b.slider.1 >= 2.0, "the box never collapses to nothing");
    }

    /// A wheel over the strip scrolls the document, same direction as over text.
    #[test]
    fn the_wheel_over_the_minimap_scrolls_the_document() {
        let mut st = st_of(&long_doc(5000));
        st.scroll_top = 1000.0;
        st.clamp_scroll();
        st.mm_wheel(-120.0);
        assert!((st.scroll_top - 1120.0).abs() < 1e-6, "{}", st.scroll_top);
        st.mm_wheel(1e9);
        assert_eq!(st.scroll_top, 0.0, "and clamps at the top");
    }

    /// Only the rows inside the strip are modelled, and each row is capped —
    /// so the cost per frame is bounded whatever the file looks like.
    #[test]
    fn the_model_is_bounded_whatever_the_document() {
        let mut st = st_of(&long_doc(20_000));
        st.scroll_top = st.max_scroll_top() / 2.0;
        st.clamp_scroll();
        let g = st.mm_geom();
        let m = st.mm_model();
        assert!(
            g.rows < 400,
            "only the painted window is modelled: {}",
            g.rows
        );
        assert!(
            m.bars.len() <= g.rows * minimap::MAX_RUNS,
            "{} bars for {} rows",
            m.bars.len(),
            g.rows
        );
        for b in &m.bars {
            assert!(b.w > 0.0);
            assert!(b.x >= 0.0 && b.x + b.w <= minimap::MAX_COLS as f32 + 0.5);
        }
    }

    /// A minified one-liner must not drag the whole line into memory: the strip
    /// is 84 columns wide, so only the head is ever read.
    #[test]
    fn a_minified_one_liner_costs_only_its_head() {
        let st = st_of(&format!("{}\ntail\n", "x".repeat(1_000_000)));
        let m = st.mm_model();
        assert!(!m.bars.is_empty(), "the head is still drawn");
        assert!(m.bars.len() <= minimap::MAX_RUNS);
        assert!(m.bars[0].w <= minimap::MAX_COLS as f32);
    }

    /// Every row the strip paints exists: the model's y range covers the strip,
    /// and the caret's row is reported when it is on screen.
    #[test]
    fn the_caret_row_is_marked_when_it_is_on_screen() {
        let mut st = st_of(&long_doc(5000));
        let m = st.mm_model();
        assert!(m.cur_y.is_some(), "the caret starts on line 0, which shows");
        // scroll far away: line 0 is no longer painted
        st.scroll_top = st.max_scroll_top();
        st.clamp_scroll();
        let m = st.mm_model();
        assert!(m.cur_y.is_none(), "line 0 is off the painted window");
    }

    /// Search matches become ticks on the strip, one per line.
    #[test]
    fn find_matches_become_ticks_on_the_strip() {
        let mut st = st_of(&long_doc(5000));
        st.find.pattern = "line 12".into();
        st.find_all_matches();
        assert!(!st.find.results.is_empty());
        // bring those lines into the strip's painted window
        let lh = st.metrics.lh as f64;
        st.scroll_top = 120.0 * lh;
        st.clamp_scroll();
        let m = st.mm_model();
        assert!(!m.hits.is_empty(), "matches inside the window are marked");
    }
}
