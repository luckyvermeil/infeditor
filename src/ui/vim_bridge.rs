//! The bridge between the pure Vim engine and the real editor.
//!
//! [`crate::ui::vim`] decides *what* a key means; this module supplies the two
//! things it deliberately does not own:
//!
//! 1. [`EditorState::vim_buffer`](crate::ui::state::EditorState::vim_buffer) — a snapshot of the facts the engine needs about the active
//!    document, rebuilt per keystroke.
//! 2. [`EditorState::apply_vim_plan`](crate::ui::state::EditorState::apply_vim_plan) — the executor that turns the engine's [`Op`]s into
//!    calls on the ordinary editing commands, so Vim reuses the same code paths
//!    (and the same undo grouping) as every other command.
//!
//! Keeping the split here means a Vim key can never bypass the invariants the
//! rest of the editor relies on — clamped carets, coalesced typing, selection
//! invariants — because there is only one implementation of each of those.

use crate::textops::is_word_char;
use crate::ui::state::{EditorState, TAB_SIZE};
use crate::ui::vim::{Buffer, Op, Plan};
use crate::ui::Msg;

impl EditorState {
    /// Replace `[from, to)` in the active document with `text`.
    ///
    /// A single funnel for every Vim mutation: the caret lands *after* the
    /// inserted text (so typing keeps going forward), the selection is
    /// collapsed, and the pending typing group is broken so one undo removes the
    /// whole operator rather than one character of it.
    fn replace(&mut self, from: u64, to: u64, text: &str) {
        let len = self.active_doc().table.len();
        let from = from.min(len);
        let to = to.clamp(from, len);
        self.active_doc_mut().replace_range(from..to, text);
        let after = from + text.len() as u64;
        self.clear_selection();
        self.set_caret(after, false);
        self.active_doc_mut().break_undo_group();
    }

    /// Snapshot the active document for the Vim engine.
    pub fn vim_buffer(&self) -> Buffer {
        let caret = self.caret().pos;
        let doc = self.active_doc();
        let len = doc.table.len();
        let line = doc.line_of_pos(caret.min(len));
        let line_start = doc.pos_of_line(line);
        let span = doc.line_span(line);
        let text = doc.line_text(line);
        // `line_span.end` already excludes the terminator
        let line_end = span.end.min(len);
        let next_line_start = span.next.min(len);
        let prev_line_start = if line == 0 {
            0
        } else {
            doc.pos_of_line(line - 1)
        };
        let first_nonblank =
            line_start + text.len() as u64 - text.trim_start_matches([' ', '\t']).len() as u64;
        Buffer {
            line_start,
            line_end,
            line_end_with_term: next_line_start,
            next_line_start,
            prev_line_start,
            len,
            sel: self.sel_range().map(|r| (r.start, r.end)),
            line: text,
            at_eof: line + 1 >= doc.line_count(),
            first_nonblank,
        }
    }

    /// Execute a plan produced by [`crate::ui::vim::Vim::handle_key`].
    ///
    /// `caret` is the caret position the plan was computed against; ops are
    /// applied in order and each one re-reads the live state, so a sequence
    /// like `yank` + `delete` cannot go stale.
    pub fn apply_vim_plan(&mut self, plan: &Plan) {
        // Read-only mode: any Vim modification (including delete/paste) is
        // blocked here.
        if self.read_only && !plan.ops.is_empty() {
            self.status = Some(Msg::StatusReadOnlyHint.into());
            return;
        }
        for op in &plan.ops {
            self.apply_vim_op(op);
        }
        self.sanitize_carets();
        // every offset-based search result may now point at shifted bytes.
        // Mark them *stale* — refreshing `gen` here used to launder the old
        // offsets into the new document generation, so result-panel navigation
        // and `find_next` selected the wrong text after a Vim edit.
        self.find.stale = true;
    }

    fn apply_vim_op(&mut self, op: &Op) {
        let caret = self.caret().pos;
        match op {
            Op::Goto(p) => self.set_caret(*p, false),

            Op::GotoLine(n) => {
                let doc = self.active_doc();
                let len = doc.table.len();
                let lines = doc.line_count();
                let line = if *n == u64::MAX {
                    lines.saturating_sub(1)
                } else {
                    (*n).saturating_sub(1).min(lines.saturating_sub(1))
                };
                let pos = doc.pos_of_line(line).min(len);
                self.set_caret(pos, false);
            }

            Op::Search(pattern) => {
                self.find.pattern = pattern.clone();
            }

            Op::Lines(n) => {
                let n = *n;
                if n < 0 {
                    for _ in 0..(-n) {
                        self.move_up(false);
                    }
                } else {
                    for _ in 0..n {
                        self.move_down(false);
                    }
                }
            }

            Op::Chars(n) => {
                let n = *n;
                // Cap: the count is at most 6 digits, but the old `e` ≈6-chars-
                // per-word approximation would loop millions of times here. Real
                // word motions are now handled by WordMove/WordEnd.
                //
                // Line-boundary clamping (Vim `l`/`h` do not cross lines): stop
                // at the end of the line's content going forward, at the line
                // start going backward — reusing move_* would carry the caret to
                // an adjacent line (the normal-mode arrow semantics), which is
                // a behavior deviation under Vim mode.
                //
                // With a selection, first collapse to the end on the motion's
                // side (consistent with the editing semantics of arrow keys
                // cancelling a selection), then move at most n characters.
                if let Some(r) = self.sel_range() {
                    let target = if n >= 0 { r.end } else { r.start };
                    self.set_caret(target, false);
                }
                if n < 0 {
                    for _ in 0..(-n).min(100_000) {
                        if self.caret().pos <= self.vim_line_lo() {
                            break;
                        }
                        self.move_left(false);
                    }
                } else {
                    for _ in 0..n.min(100_000) {
                        if self.caret().pos >= self.vim_line_hi() {
                            break;
                        }
                        self.move_right(false);
                    }
                }
            }

            Op::WordMove(n) => {
                let forward = *n >= 0;
                // count is bounded and every step makes progress (or stops
                // the loop), so a huge `999999w` can no longer spin millions
                // of per-character moves
                for _ in 0..(*n).abs().min(100_000) {
                    let moved = if forward {
                        self.word_forward()
                    } else {
                        self.word_backward()
                    };
                    if !moved {
                        break;
                    }
                }
            }

            Op::WordEnd(n) => {
                for _ in 0..(*n).clamp(0, 100_000) {
                    if !self.word_end_forward() {
                        break;
                    }
                }
            }

            Op::SelectTo(p) => {
                // anchor stays where it was, the caret becomes `p`
                let c = self.caret_mut();
                if c.anchor.is_none() {
                    c.anchor = Some(caret);
                }
                c.pos = *p;
            }

            Op::Delete { from, to } => {
                let (from, to) = (*from, *to);
                if from < to {
                    self.replace(from, to, "");
                }
            }

            Op::DeleteChar(at) => match self.sel_range() {
                Some(r) => self.replace(r.start, r.end, ""),
                None => {
                    let from = at.unwrap_or(caret);
                    let next = self.char_after(from);
                    if from < next {
                        self.replace(from, next, "");
                    }
                }
            },

            Op::DeleteLines(count) => self.delete_current_lines(*count),

            Op::ChangeLines(count) => self.change_lines(*count),

            Op::Change(at) => {
                // delete the range (or one character) then let typing continue
                if let Some(r) = self.sel_range() {
                    self.replace(r.start, r.end, "");
                    return;
                }
                match at {
                    Some(p) => {
                        let p = *p;
                        let next = self.char_after(p);
                        if p < next {
                            self.replace(p, next, "");
                        }
                        self.set_caret(p, false);
                    }
                    None => self.set_caret(caret, false),
                }
            }

            Op::Yank { from, to } => {
                let (from, to) = (*from, *to);
                let text =
                    String::from_utf8_lossy(&self.active_doc().table.read(from..to)).into_owned();
                self.vim.set_register(text, false);
            }

            Op::YankLines(count) => {
                let (s, e) = self.vim_lines_span(*count);
                let mut text =
                    String::from_utf8_lossy(&self.active_doc().table.read(s..e)).into_owned();
                // When the last line of the file has no terminator (very common
                // after editing) `yy`'s register lacks a `\n`, but linewise
                // semantics require the whole line — a line-level `p` would glue
                // the two lines together. Pad to the document's line end.
                if !text.ends_with('\n') && !text.ends_with('\r') {
                    let nl = self.active_doc().newline().terminator();
                    text.push_str(nl);
                }
                self.vim.set_register(text, true);
            }

            Op::Paste { after, linewise } => {
                let reg = self.vim.register.clone();
                if reg.is_empty() {
                    self.status = Some(Msg::VimStRegisterEmpty.into());
                    return;
                }
                if *linewise {
                    // the register always carries its terminator (YankLines
                    // pads a missing EOF terminator), so `p` and `P` differ
                    // only in *where* the whole line(s) go
                    let (start, next) = self.current_line_span();
                    let at = if *after { next } else { start };
                    self.replace(at, at, &reg);
                    self.set_caret(at, false);
                } else {
                    let at = if *after {
                        self.char_after(caret)
                    } else {
                        caret
                    };
                    self.replace(at, at, &reg);
                    self.set_caret(at + reg.len() as u64, false);
                }
            }

            Op::Insert(s) => {
                self.insert_text(s);
            }

            Op::OpenLine { below } => {
                self.insert_newline();
                if !*below {
                    // `O` opens *above*: the newline landed after the caret, so
                    // swap the caret to the fresh line above
                    let pos = self.caret().pos;
                    if pos > 0 {
                        self.set_caret(pos - 1, false);
                    }
                }
            }

            Op::JoinLine => self.join_lines(),

            Op::ShiftIndent { outdent } => {
                if *outdent {
                    self.dedent_lines();
                } else {
                    self.indent_lines();
                }
            }

            Op::Run(cmd) => crate::ui::actions::run(self, *cmd),

            Op::FindNext => self.find_next(),
            Op::FindPrev => self.find_prev(),

            Op::Status(s) => self.status = Some(s.clone()),
        }
    }

    /// Byte range of the caret's line **including** its terminator.
    fn current_line_span(&self) -> (u64, u64) {
        let caret = self.caret().pos;
        let doc = self.active_doc();
        let len = doc.table.len();
        let line = doc.line_of_pos(caret.min(len));
        let start = doc.pos_of_line(line);
        let end = doc.line_span(line).next.min(len);
        (start, end)
    }

    /// Vim `w`: to the start of the next word. A word is a run of
    /// alphanumeric/`_` characters or a run of punctuation; whitespace
    /// separates. Scans the real document in 64 KB windows, so `dw` on
    /// `foo.bar` correctly stops between the words. Returns false at EOF.
    fn word_forward(&mut self) -> bool {
        match self.word_scan(false) {
            Some(p) if p != self.caret().pos => {
                self.set_caret(p, false);
                true
            }
            Some(_) => false,
            None => {
                // no further word start: `w` still lands on the document end
                let len = self.active_doc().table.len();
                if self.caret().pos < len {
                    self.set_caret(len, false);
                    true
                } else {
                    false
                }
            }
        }
    }

    /// Vim `e`: to the *last character* of the current / next word
    /// (`word_scan(true)`); returns false when there is nothing ahead.
    fn word_end_forward(&mut self) -> bool {
        match self.word_scan(true) {
            Some(p) if p != self.caret().pos => {
                self.set_caret(p, false);
                true
            }
            _ => false,
        }
    }

    /// Shared word scan. `to_end = false` → offset of the next word *start*
    /// (`w`); `to_end = true` → offset of the current / next word's *last
    /// character* (`e`). `None` when the scan runs out of non-blank text.
    ///
    /// Reads backwards only through the current caret, so the cost is
    /// proportional to the distance to the target, not to the document size.
    fn word_scan(&mut self, to_end: bool) -> Option<u64> {
        const WIN: u64 = 64 * 1024;
        let len = self.active_doc().table.len();
        let start = self.caret().pos;
        if start >= len {
            return None;
        }
        let mut from = start;
        let mut first = true;
        loop {
            let end = len.min(from + WIN);
            let text = self.active_doc().text_range(from..end);
            let chars: Vec<(usize, char)> = text.char_indices().collect();
            if chars.is_empty() {
                return None;
            }
            let mut i = 0usize;
            if first {
                first = false;
                if !chars[0].1.is_whitespace() {
                    // walk to the end of the caret's own run
                    let cls = is_word_char(chars[0].1);
                    let mut j = 0usize;
                    while j < chars.len()
                        && !chars[j].1.is_whitespace()
                        && is_word_char(chars[j].1) == cls
                    {
                        j += 1;
                    }
                    if to_end {
                        // `e` from inside a word: land on that word's last
                        // character — unless the caret already *is* that
                        // character, in which case Vim moves on to the end of
                        // the next word (fall through to the whitespace skip
                        // below instead of returning the caret itself).
                        if j >= chars.len() {
                            if end < len {
                                // the run continues past the window: rescan
                                // from its tail so the next iteration sees
                                // fresh text
                                from += chars[j - 1].0 as u64;
                                continue;
                            }
                            return Some(from + chars[j - 1].0 as u64);
                        }
                        let last = from + chars[j - 1].0 as u64;
                        if last != start {
                            return Some(last);
                        }
                        i = j; // at the run end: continue with the next run
                    } else {
                        i = j;
                    }
                }
                while i < chars.len() && chars[i].1.is_whitespace() {
                    i += 1;
                }
            } else {
                while i < chars.len() && chars[i].1.is_whitespace() {
                    i += 1;
                }
            }
            if i < chars.len() {
                if !to_end {
                    return Some(from + chars[i].0 as u64);
                }
                // `e` from whitespace: walk to the end of the next run
                let cls = is_word_char(chars[i].1);
                let mut j = i;
                while j + 1 < chars.len()
                    && !chars[j + 1].1.is_whitespace()
                    && is_word_char(chars[j + 1].1) == cls
                {
                    j += 1;
                }
                if j + 1 == chars.len() && end < len {
                    // may continue into the next window
                    from += chars[j].0 as u64;
                    continue;
                }
                return Some(from + chars[j].0 as u64);
            }
            if end >= len || end <= from {
                return None;
            }
            from = end;
        }
    }

    /// Vim `b`: to the start of the previous word. Returns false at BOF.
    fn word_backward(&mut self) -> bool {
        const WIN: u64 = 64 * 1024;
        let start = self.caret().pos;
        if start == 0 {
            return false;
        }
        let mut to = start;
        loop {
            let from = to.saturating_sub(WIN);
            let text = self.active_doc().text_range(from..to);
            let chars: Vec<(usize, char)> = text.char_indices().collect();
            let mut i = chars.len();
            // skip the whitespace right before the caret
            while i > 0 && chars[i - 1].1.is_whitespace() {
                i -= 1;
            }
            if i == 0 {
                if from == 0 {
                    self.set_caret(0, false);
                    return true;
                }
                to = from;
                continue;
            }
            // walk back over the word / punctuation run to its first char
            let class = is_word_char(chars[i - 1].1);
            while i > 0 && !chars[i - 1].1.is_whitespace() && is_word_char(chars[i - 1].1) == class
            {
                i -= 1;
            }
            if i == 0 && from > 0 && !chars[0].1.is_whitespace() {
                // the run continues past the window start — widen
                to = from;
                continue;
            }
            let target = from + chars.get(i).map(|(o, _)| *o as u64).unwrap_or(0);
            if target >= start {
                return false;
            }
            self.set_caret(target, false);
            return true;
        }
    }

    /// The line range `(l0, l1)` touched by the selection (or the caret's
    /// line) — shared by vim_lines_span / shift_lines.
    fn sel_line_span(&self) -> (u64, u64) {
        let doc = self.active_doc();
        match self.sel_range() {
            Some(r) => {
                let a = doc.line_of_pos(r.start);
                let b = doc.line_of_pos(r.end.saturating_sub(1).max(r.start));
                (a, b)
            }
            None => {
                let l = doc.line_of_pos(self.caret().pos.min(doc.table.len()));
                (l, l)
            }
        }
    }

    /// Byte span of `count` whole lines starting at the caret's line — or the
    /// lines the selection touches — **including** their terminators, clamped
    /// to the buffer end.
    ///
    /// Named distinctly from [`EditorState::lines_span`] (which takes an
    /// explicit line range); this resolves the range from the caret/count and
    /// delegates to it.
    fn vim_lines_span(&self, count: u64) -> (u64, u64) {
        let (l0, l1) = match self.sel_range() {
            Some(_) => self.sel_line_span(),
            None => {
                let doc = self.active_doc();
                let l0 = doc.line_of_pos(self.caret().pos.min(doc.table.len()));
                (l0, l0.saturating_add(count.saturating_sub(1)))
            }
        };
        self.lines_span(l0, l1)
    }

    /// Delete `count` whole lines from the caret's line (or every line the
    /// selection touches); the caret lands on the first deleted line.
    fn delete_current_lines(&mut self, count: u64) {
        let (from, to) = self.vim_lines_span(count);
        if to > from {
            self.replace(from, to, "");
        }
        self.set_caret(from, false);
    }

    /// Vim `cc`: clear the contents of `count` lines from the caret's line.
    ///
    /// The *content* end of the last line is used — its terminator stays — so
    /// exactly one empty line remains and Insert mode (set by the engine)
    /// starts at its beginning.
    fn change_lines(&mut self, count: u64) {
        let doc = self.active_doc();
        let len = doc.table.len();
        let l0 = doc.line_of_pos(self.caret().pos.min(len));
        let l1 = l0
            .saturating_add(count.saturating_sub(1))
            .min(doc.line_count().saturating_sub(1))
            .max(l0);
        let from = doc.pos_of_line(l0);
        let to = doc.line_span(l1).end.min(len).max(from);
        self.replace(from, to, "");
    }

    /// Indent every line the selection touches, or the caret line.
    fn indent_lines(&mut self) {
        self.shift_lines(true);
    }

    /// Remove one indent step from the selection, or the caret line.
    fn dedent_lines(&mut self) {
        self.shift_lines(false);
    }

    fn shift_lines(&mut self, add: bool) {
        let (l0, l1) = self.sel_line_span();
        if add {
            // back-to-front so earlier offsets stay valid.
            // Collected into a single transaction for the write-back: N lines
            // of indent = 1 undo step (it used to be one transaction per line,
            // so `>>` on 50 lines took 50 Ctrl+Z presses).
            let mut ops: Vec<crate::buffer::doc::EditOp> = Vec::new();
            let indent = " ".repeat(TAB_SIZE);
            for l in (l0..=l1).rev() {
                let doc = self.active_doc();
                let s = doc.pos_of_line(l);
                ops.push(crate::buffer::doc::EditOp::insert(s, &indent));
            }
            self.active_doc_mut().apply_transaction(ops);
        } else {
            let mut ops: Vec<crate::buffer::doc::EditOp> = Vec::new();
            for l in (l0..=l1).rev() {
                let doc = self.active_doc();
                let s = doc.pos_of_line(l);
                let text = doc.line_text(l);
                let strip = text.len() - text.trim_start_matches([' ', '\t']).len();
                let take = strip.min(TAB_SIZE);
                if take > 0 {
                    ops.push(crate::buffer::doc::EditOp {
                        pos: s,
                        deleted: text[..take].to_string(),
                        inserted: String::new(),
                    });
                }
            }
            if !ops.is_empty() {
                self.active_doc_mut().apply_transaction(ops);
            }
        }
        // keep the caret on its own line
        let doc_len = self.active_doc().table.len();
        let line = self.active_doc().line_of_pos(self.caret().pos.min(doc_len));
        let s = self.active_doc().pos_of_line(line);
        self.set_caret(s, false);
    }
}

/// Offset just past the character at `pos`, respecting UTF-8 boundaries.
///
/// A Vim `x` or `l` must move one *character*, not one byte: on a CJK line a
/// byte-wise step lands in the middle of a code point and produces invalid UTF-8
/// when the editor deletes it.
impl EditorState {
    pub fn char_after(&self, pos: u64) -> u64 {
        let doc = self.active_doc();
        let len = doc.table.len();
        if pos >= len {
            return len;
        }
        let line = doc.line_of_pos(pos);
        let ls = doc.pos_of_line(line);
        let text = doc.line_text(line);
        let off = (pos - ls) as usize;
        if off >= text.len() {
            return (ls + text.len() as u64).min(len);
        }
        let mut i = off + 1;
        while i < text.len() && !text.is_char_boundary(i) {
            i += 1;
        }
        (ls + i as u64).min(len)
    }

    /// Offset of the character *before* `pos`, or `pos` at the line start.
    pub fn char_before(&self, pos: u64) -> u64 {
        let doc = self.active_doc();
        let line = doc.line_of_pos(pos.min(doc.table.len()));
        let ls = doc.pos_of_line(line);
        if pos <= ls {
            return ls;
        }
        let text = doc.line_text(line);
        let mut i = ((pos - ls) as usize).saturating_sub(1);
        while i > 0 && !text.is_char_boundary(i) {
            i -= 1;
        }
        ls + i as u64
    }

    /// The line start offset of the caret's line (Vim `h`'s lower bound).
    pub fn vim_line_lo(&self) -> u64 {
        let doc = self.active_doc();
        let line = doc.line_of_pos(self.caret().pos.min(doc.table.len()));
        doc.pos_of_line(line)
    }

    /// The **content** end offset of the caret's line (excluding the line
    /// terminator; Vim `l`'s upper bound).
    ///
    /// The caret may rest one past the line's content (this editor's end-of-
    /// line convention), so that position is a legal docking point; any
    /// further would cross the terminator into the next line.
    pub fn vim_line_hi(&self) -> u64 {
        let doc = self.active_doc();
        let len = doc.table.len();
        let line = doc.line_of_pos(self.caret().pos.min(len));
        let span = doc.line_span(line);
        span.end.min(len)
    }
}

#[cfg(test)]
mod tests {
    use crate::ui::state::EditorState;
    use crate::ui::testutil::st_of;
    use crate::ui::vim::Vim;

    fn text(st: &EditorState) -> String {
        let doc = st.active_doc();
        String::from_utf8_lossy(&doc.table.read(0..doc.table.len())).into_owned()
    }

    #[test]
    fn counted_dd_removes_three_lines_in_one_undo_step() {
        let mut st = st_of("one\ntwo\nthree\nfour");
        let mut v = crate::ui::vim::test_vim();
        press3(&mut st, &mut v, &['3', 'd', 'd']);
        assert_eq!(text(&st), "four");
        // one undo removes the whole operator
        st.undo();
        assert_eq!(text(&st), "one\ntwo\nthree\nfour");
    }

    #[test]
    fn dj_deletes_two_lines_in_the_real_buffer() {
        let mut st = st_of("one\ntwo\nthree");
        let mut v = crate::ui::vim::test_vim();
        press3(&mut st, &mut v, &['d', 'j']);
        assert_eq!(text(&st), "three");
    }

    #[test]
    fn counted_dd_on_the_last_line_clamps_to_the_end() {
        let mut st = st_of("one\ntwo\nthree");
        let mut v = crate::ui::vim::test_vim();
        // move to the last line first (`G` is not modelled, so goto directly)
        st.set_caret(st.active_doc().pos_of_line(2), false);
        press3(&mut st, &mut v, &['5', 'd', 'd']);
        // the count clamps to the lines that exist; the editor-wide
        // `lines_span` convention keeps the previous terminator, so one
        // trailing empty line is left (same as the menu's Delete line)
        assert_eq!(text(&st), "one\ntwo\n");
    }

    #[test]
    fn counted_yy_yanks_three_lines_linewise() {
        let mut st = st_of("one\ntwo\nthree\nfour");
        let mut v = crate::ui::vim::test_vim();
        press3(&mut st, &mut v, &['3', 'y', 'y']);
        // apply_vim_plan stores into the *state's* Vim, not the local one
        assert_eq!(st.vim.register, "one\ntwo\nthree\n");
        // the buffer is untouched by a yank
        assert_eq!(text(&st), "one\ntwo\nthree\nfour");
    }

    #[test]
    fn cc_clears_the_line_and_keeps_one_empty_line() {
        let mut st = st_of("aaa\nbbb\nccc");
        let mut v = crate::ui::vim::test_vim();
        press3(&mut st, &mut v, &['c', 'c']);
        assert_eq!(text(&st), "\nbbb\nccc");
        assert_eq!(v.mode, crate::ui::vim::Mode::Insert);
        // typing lands on the cleared line
        st.insert_text("X");
        assert_eq!(text(&st), "X\nbbb\nccc");
    }

    #[test]
    fn counted_cc_clears_three_lines_leaving_one_empty() {
        let mut st = st_of("aaa\nbbb\nccc\nddd");
        let mut v = crate::ui::vim::test_vim();
        press3(&mut st, &mut v, &['3', 'c', 'c']);
        assert_eq!(text(&st), "\nddd");
    }

    /// Type a short key sequence, re-deriving the buffer snapshot per key the
    /// way the live editor does.
    fn press3(st: &mut EditorState, v: &mut Vim, keys: &[char]) {
        for &k in keys {
            let buf = st.vim_buffer();
            if let Some(plan) = v.handle_key(crate::ui::vim::Ev::Char(k), st.caret().pos, &buf) {
                st.apply_vim_plan(&plan);
            }
        }
    }

    /// Real-document word motion: `w` on `foo.bar` lands on the punctuation
    /// word start (Vim counts a run of punctuation as one "word") and can
    /// jump across lines to the next line's word start.
    #[test]
    fn w_scans_real_word_boundaries() {
        let mut st = st_of("foo.bar baz\nnext");
        let mut v = crate::ui::vim::test_vim();
        press3(&mut st, &mut v, &['w']);
        assert_eq!(st.caret().pos, 3, "foo → '.'");
        press3(&mut st, &mut v, &['w']);
        assert_eq!(st.caret().pos, 4, "'.' → 'bar'");
        press3(&mut st, &mut v, &['w']);
        assert_eq!(st.caret().pos, 8, "'bar' → 'baz'");
        press3(&mut st, &mut v, &['w']);
        assert_eq!(st.caret().pos, 12, "across lines → 'next'");
    }

    /// `b` sweeps the same string backwards; `e` lands on the word-end
    /// character and, when already at a word end, advances to the next word's
    /// end (Vim semantics).
    #[test]
    fn b_and_e_handle_run_ends() {
        let mut st = st_of("foo.bar baz");
        let mut v = crate::ui::vim::test_vim();
        st.set_caret(8, false); // 'b' of baz
        press3(&mut st, &mut v, &['b']);
        assert_eq!(st.caret().pos, 4, "baz → bar word start");
        press3(&mut st, &mut v, &['b']);
        assert_eq!(st.caret().pos, 3, "bar → '.'");
        press3(&mut st, &mut v, &['b']);
        assert_eq!(st.caret().pos, 0, "'.' → 'foo'");

        // e: 0 → the last character of foo (2)
        let mut st = st_of("foo.bar baz");
        let mut v = crate::ui::vim::test_vim();
        press3(&mut st, &mut v, &['e']);
        assert_eq!(st.caret().pos, 2);
        // already at a word end → the end of the next run ('.') (still 3)
        press3(&mut st, &mut v, &['e']);
        assert_eq!(st.caret().pos, 3);
        // the end of '.' → the end of bar (6)
        press3(&mut st, &mut v, &['e']);
        assert_eq!(st.caret().pos, 6);
    }

    /// `dw`/`de` deletion ranges on a real document.
    #[test]
    fn dw_and_de_delete_real_word_spans() {
        // dw from 0: delete to the next word start (3) — "foo"
        let mut st = st_of("foo.bar baz");
        let mut v = crate::ui::vim::test_vim();
        press3(&mut st, &mut v, &['d', 'w']);
        assert_eq!(text(&st), ".bar baz");
        // de from 0: delete to the word end including the end char (3)
        let mut st = st_of("foo.bar baz");
        let mut v = crate::ui::vim::test_vim();
        press3(&mut st, &mut v, &['d', 'e']);
        assert_eq!(text(&st), ".bar baz");
        // de stops on the word-end character: only that char is deleted
        let mut st = st_of("foo.bar baz");
        let mut v = crate::ui::vim::test_vim();
        st.set_caret(2, false);
        press3(&mut st, &mut v, &['d', 'e']);
        assert_eq!(text(&st), "fo.bar baz");
    }

    /// `l`/`h` do not cross lines (Vim semantics): pressing `l` at the line
    /// end does not move, pressing `h` at the line start does not move.
    #[test]
    fn l_and_h_do_not_cross_line_boundaries() {
        let mut st = st_of("ab\ncd");
        let mut v = crate::ui::vim::test_vim();
        st.set_caret(1, false);
        press3(&mut st, &mut v, &['l']);
        assert_eq!(st.caret().pos, 2, "l reaches the end of the line content");
        press3(&mut st, &mut v, &['l']);
        assert_eq!(
            st.caret().pos,
            2,
            "l at the line end does not move (no crossing to the next line)"
        );
        st.set_caret(3, false); // line start of 'c'
        press3(&mut st, &mut v, &['h']);
        assert_eq!(st.caret().pos, 3, "h at the line start does not move");
        st.set_caret(4, false);
        press3(&mut st, &mut v, &['h']);
        assert_eq!(st.caret().pos, 3);
    }
}
