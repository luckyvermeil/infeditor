//! Bracket matching and jumping (Edit → Go to Matching Bracket).

use crate::ui::state::EditorState;
use crate::ui::Msg;

// ---- bracket matching and jumping ----------------------------------------

/// Bracket pair table. Open brackets map by index to their close brackets.
const BRACKET_PAIRS: &[(char, char)] = &[('(', ')'), ('[', ']'), ('{', '}')];

/// Upper bound (bytes) for a one-directional scan. Bracket matching runs on
/// the keystroke path; a full scan of a huge document would stall typing. A
/// bounded range is searched here too, and past the limit it counts as no
/// match.
const BRACKET_SCAN_LIMIT: u64 = 1 << 21; // 2 MiB

impl EditorState {
    /// The matching position of the bracket at the caret.
    ///
    /// Three landing points are accepted: the caret on an open bracket, on
    /// a close bracket, or **just after** an open bracket (in insert mode
    /// the caret usually sits right of `{`, which is the normal typing
    /// state).
    pub fn match_bracket(&self) -> Option<u64> {
        let doc = self.active_doc();
        let len = doc.table.len();
        let pos = self.caret().pos;

        let is_bracket = |c: char| BRACKET_PAIRS.iter().any(|(o, x)| *o == c || *x == c);
        let at = doc.char_at(pos).filter(|c| is_bracket(*c));
        let before = doc
            .char_at(doc.snap(pos.saturating_sub(1)))
            .filter(|c| is_bracket(*c));

        if let Some(c) = at {
            return self.scan_bracket(c, pos);
        }
        if let Some(c) = before {
            return self.scan_bracket(c, doc.snap(pos - 1));
        }
        let _ = len;
        None
    }

    /// Scan for the pair starting from bracket `c` at `pos`.
    fn scan_bracket(&self, c: char, pos: u64) -> Option<u64> {
        let doc = self.active_doc();
        let len = doc.table.len();
        let is_open = |ch: char| BRACKET_PAIRS.iter().any(|(o, _)| *o == ch);
        let (open, close) = if is_open(c) {
            (c, mirror(c))
        } else {
            (mirror(c), c)
        };
        // Depth starts at 1: the bracket at the starting point has already
        // been "seen", and the scan starts beside it, so the nesting count
        // comes out naturally correct with no special-casing of the start.
        let mut depth = 1i32;
        if is_open(c) {
            let limit = (pos + BRACKET_SCAN_LIMIT).min(len);
            let mut p = pos + c.len_utf8() as u64;
            while p < limit {
                match doc.char_at(p) {
                    Some(ch) if ch == open => depth += 1,
                    Some(ch) if ch == close => {
                        depth -= 1;
                        if depth == 0 {
                            return Some(p);
                        }
                    }
                    Some(ch) => p += ch.len_utf8() as u64 - 1,
                    None => {}
                }
                p += 1;
            }
            None
        } else {
            let limit = pos.saturating_sub(BRACKET_SCAN_LIMIT);
            let mut p = pos;
            while p > limit {
                let prev = doc.snap(p - 1);
                match doc.char_at(prev) {
                    Some(ch) if ch == close => depth += 1,
                    Some(ch) if ch == open => {
                        depth -= 1;
                        if depth == 0 {
                            return Some(prev);
                        }
                    }
                    _ => {}
                }
                p = prev;
            }
            None
        }
    }

    /// Edit → Go to Matching Bracket: move the caret to the counterpart
    /// (status-bar notice when there is no match).
    pub fn goto_matching_bracket(&mut self) {
        let Some(target) = self.match_bracket() else {
            self.status = Some(Msg::StatusNoMatchingBracket.into());
            return;
        };
        // Select the bracket itself (one character), matching common editor
        // behaviour and easing replacement.
        let width = self
            .active_doc()
            .char_at(target)
            .map(|ch| ch.len_utf8() as u64)
            .unwrap_or(0);
        self.set_caret(target, false);
        let c = self.caret_mut();
        c.anchor = Some(target + width);
        self.scroll_caret_into_view();
    }
}

/// Mirror bracket: `) ↔ (` and so on.
fn mirror(c: char) -> char {
    match c {
        '(' => ')',
        ')' => '(',
        '[' => ']',
        ']' => '[',
        '{' => '}',
        '}' => '{',
        other => other,
    }
}

#[cfg(test)]
mod bracket_tests {
    use super::*;
    use crate::ui::commands::testutil::st_of;

    /// Open bracket scans forward, close bracket scans backward, nesting
    /// counts correctly.
    #[test]
    fn brackets_match_in_both_directions_with_nesting() {
        // "fn f() { if (a[0] == 1) { b(); } }"
        //  f0 n1 sp2 f3 (4 )5 sp6 {7 sp8 i9 f10 sp11 (12 a13 [14 015 ]16
        //  sp17 =18 =19 sp20 121 )22 sp23 {24 sp25 b26 (27 )28 ;29 sp30 }31 sp32 }33
        let st = st_of("fn f() { if (a[0] == 1) { b(); } }");
        assert_eq!(st_of_pos(&st, 4), Some(5), "( pairs with )");
        assert_eq!(
            st_of_pos(&st, 5),
            Some(4),
            "close bracket searches backward"
        );
        assert_eq!(st_of_pos(&st, 7), Some(33), "outermost curly braces pair");
        assert_eq!(st_of_pos(&st, 33), Some(7));
        assert_eq!(st_of_pos(&st, 14), Some(16), "[ nested inside (");
        assert_eq!(
            st_of_pos(&st, 12),
            Some(22),
            "inner ( skips the nested [ and finds )"
        );
    }

    fn st_of_pos(st: &EditorState, pos: u64) -> Option<u64> {
        let mut s = clone_of(st);
        s.set_caret(pos, false);
        s.match_bracket()
    }

    fn clone_of(st: &EditorState) -> EditorState {
        // minimal + same text (test helper, avoids the Clone bound)
        st_of(&st.active_doc().read_all())
    }

    /// A caret just **right** of an open bracket (normal typing state)
    /// still matches.
    #[test]
    fn a_caret_right_after_an_opening_bracket_still_matches() {
        let mut st = st_of("(x)");
        st.set_caret(1, false);
        assert_eq!(st.match_bracket(), Some(2), "caret right of (");
    }

    /// No guessing when there is no pair.
    #[test]
    fn an_unmatched_bracket_has_no_match() {
        let mut st = st_of("(oops");
        st.set_caret(0, false);
        assert_eq!(st.match_bracket(), None);
        let mut st2 = st_of("no brackets here");
        st2.set_caret(3, false);
        assert_eq!(st2.match_bracket(), None);
    }

    /// The jump moves the caret to the counterpart and selects that
    /// bracket.
    #[test]
    fn goto_bracket_moves_and_selects_the_counterpart() {
        let mut st = st_of("{a}");
        st.set_caret(0, false);
        st.goto_matching_bracket();
        assert_eq!(st.sel_range(), Some(2..3));
    }
}
