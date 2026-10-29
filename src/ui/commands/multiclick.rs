//! Multi-click selection: double-click selects the word under the mouse,
//! triple-click selects the line.

use crate::textops::is_word_char;
use crate::ui::state::{EditorState, FindFocus};

// ---- multi-click selection (double-click word / triple-click line) -------

impl EditorState {
    /// Touch tap: place the caret at client-area `(x, y)` (CSS px).
    ///
    /// Shares hit testing and multi-click semantics with the mouse path
    /// ([`Self::hit_test`] + [`Self::editor_click`]); only the **timing**
    /// differs: on finger-down the caret does not move yet (that touch may
    /// still become a swipe); only on lift without swiping does this run
    /// (see [`crate::ui::touch`]). A tap also hands the keyboard back to the
    /// document — on a touchscreen there is no other way to take focus back
    /// from the find box.
    ///
    /// Returns whether the tap landed on text.
    pub fn touch_tap(&mut self, x: f64, y: f64) -> bool {
        self.menu.open = None;
        // bare Alt → click → typing must not let the first letter trigger a
        // mnemonic
        self.menu.alt = false;
        self.find.focus = FindFocus::Doc;
        self.doc_focused = true;
        let Some(p) = self.hit_test(x, y) else {
            return false;
        };
        self.editor_click(p, x as f32, y as f32, false);
        true
    }

    /// Handle one **primary-button** press in the editor area: fold the
    /// multi-click semantics (word/line selection) into normal caret
    /// setting; both the viewport and composer hit entries route through
    /// here, so the behavior is consistent by construction.
    ///
    /// Returns whether the press produced a selection.
    ///
    /// The click counter lives on `EditorState` rather than a global slot:
    /// click cadence should be a property of "the mouse", but a global slot
    /// gets polluted across parallel unit tests (two tests "double-clicking"
    /// at once became a triple-click), and writing two scalar fields per
    /// click is negligible.
    pub fn editor_click(&mut self, pos: u64, x: f32, y: f32, shift: bool) -> bool {
        // clicking text ends column-mode selection (same convention as
        // Windows column selection: a click = restart)
        self.col_anchor = None;
        const WITHIN_MS: u128 = 500;
        /// Maximum distance between the two landing points. Aligned with the
        /// touch slop (8px, see `ui::touch`): the two taps of a finger
        /// double-click naturally drift 3–6px, so the old 6px was borderline
        /// and double-click word selection worked intermittently on touch
        /// screens; mouse double-clicks usually land 0–2px apart and are
        /// unaffected.
        const NEAR: f32 = 8.0;
        let now = std::time::Instant::now();
        let consecutive = self
            .click_at
            .map(|(t, lx, ly, c)| {
                now.duration_since(t).as_millis() < WITHIN_MS
                    && (x - lx).abs() <= NEAR
                    && (y - ly).abs() <= NEAR
                    && c < 3
            })
            .unwrap_or(false);
        self.click_at = Some((
            now,
            x,
            y,
            if consecutive {
                self.click_at.unwrap().3 + 1
            } else {
                1
            },
        ));
        let count = self.click_at.unwrap().3;
        match count {
            2 => {
                // double-click selects the word: set_caret lands first, then
                // the word is circled.
                self.set_caret(pos, false);
                let r = self.word_range_at(pos);
                let c = self.caret_mut();
                c.anchor = Some(r.start);
                c.pos = r.end;
                true
            }
            3 => {
                self.set_caret(pos, false);
                let (s, e) = self.line_span_at(pos);
                let c = self.caret_mut();
                c.anchor = Some(s);
                c.pos = e;
                true
            }
            _ => {
                self.set_caret(pos, shift);
                false
            }
        }
    }

    /// Byte range of the "word" containing `pos` (the double-click
    /// selection rule).
    ///
    /// Three classes (matching mainstream editors):
    /// * identifier characters ([`is_word_char`]: letters/digits/underscore,
    ///   Han and other alphanumeric scripts) — the whole run expands, which is
    ///   what makes one double-click in Chinese select a whole word instead of
    ///   one character;
    /// * whitespace — the whole run of whitespace (double-clicking whitespace
    ///   selects that run of whitespace);
    /// * remaining (punctuation, symbols) — **a consecutive same-type run is
    ///   one word**.
    pub fn word_range_at(&self, pos: u64) -> std::ops::Range<u64> {
        let doc = self.active_doc();
        let len = doc.table.len();
        if len == 0 {
            return 0..0;
        }
        let pos = doc.snap(pos.min(len));
        let class = |p: u64| -> u8 {
            match doc.char_at(p) {
                Some(c) if is_word_char(c) => 0,
                Some(c) if c.is_whitespace() => 1,
                Some(_) => 2,
                None => 3,
            }
        };
        // A caret at the very end of the document (`pos == len`) has no
        // character of its own: anchor on the last one, so the click still
        // selects the trailing word instead of collapsing to an empty range.
        let anchor = if pos < len { pos } else { doc.snap(len - 1) };
        let cls = class(anchor);
        if cls == 3 {
            return pos..pos;
        }
        let mut start = anchor;
        while start > 0 {
            let p = doc.snap(start - 1);
            if class(p) != cls {
                break;
            }
            start = p;
        }
        let mut end = anchor;
        while end < len && class(end) == cls {
            end += doc.char_at(end).map(|c| c.len_utf8() as u64).unwrap_or(1);
        }
        start..end.max(anchor)
    }

    /// Byte range of the line containing `pos` (triple-click line selection,
    /// excluding the line terminator).
    pub fn line_span_at(&self, pos: u64) -> (u64, u64) {
        let doc = self.active_doc();
        let line = doc.line_of_pos(pos);
        let s = doc.pos_of_line(line);
        let e = doc.line_span(line).end;
        (s, e)
    }
}

#[cfg(test)]
mod multiclick_tests {

    use crate::ui::commands::testutil::st_of;

    /// Three-class rules of double-click word selection: identifiers whole,
    /// whitespace whole, CJK as a consecutive same-type run.
    #[test]
    fn word_ranges_cover_identifiers_whitespace_and_cjk() {
        let st = st_of("hello world");
        // mid-hello
        assert_eq!(st.word_range_at(2), 0..5);
        // world
        assert_eq!(st.word_range_at(8), 6..11);
        // past the end of the word (pos=5 is the space)
        assert_eq!(
            st.word_range_at(5),
            5..6,
            "double-clicking whitespace selects that whitespace run"
        );
    }

    #[test]
    fn cjk_runs_select_as_one_word() {
        let st = st_of("你好世界");
        assert_eq!(
            st.word_range_at(3),
            0..12,
            "consecutive Han characters select as one word"
        );
        let st2 = st_of("中文 abc");
        assert_eq!(st2.word_range_at(0), 0..6);
        assert_eq!(st2.word_range_at(9), 7..10);
    }

    #[test]
    fn punctuation_selects_its_run() {
        let st = st_of("foo....bar");
        assert_eq!(
            st.word_range_at(4),
            3..7,
            "consecutive punctuation is one word"
        );
        let st2 = st_of("a_b1 x");
        assert_eq!(
            st2.word_range_at(1),
            0..4,
            "underscore counts as an identifier character"
        );
    }

    /// double-click → select and extend; triple-click → whole line (without
    /// the line terminator).
    #[test]
    fn editor_click_second_and_third_click_select_word_then_line() {
        let mut st = st_of("hello world\nsecond line\n");
        // single click
        assert!(!st.editor_click(8, 100.0, 10.0, false));
        assert_eq!(st.caret().pos, 8);
        // quick second click at the same point (click_count is driven inside
        // editor_click; the distance/time parameters here fall inside the
        // window)
        assert!(st.editor_click(8, 100.0, 10.0, false));
        assert_eq!(st.sel_range(), Some(6..11), "double-click selects world");
        // triple-click → whole line
        assert!(st.editor_click(8, 100.0, 10.0, false));
        assert_eq!(
            st.sel_range(),
            Some(0..11),
            "triple-click selects the first line (without the \\n)"
        );
        // the fourth click returns to a single click
        assert!(!st.editor_click(8, 100.0, 10.0, false));
        assert_eq!(st.caret().pos, 8);
        assert_eq!(st.sel_range(), None);
    }

    /// shift+click still respects extend-selection in the multi-click path.
    #[test]
    fn shift_click_extends_even_in_the_multiclick_path() {
        let mut st = st_of("hello world");
        st.set_caret(0, false);
        assert!(!st.editor_click(11, 100.0, 10.0, true));
        assert_eq!(st.sel_range(), Some(0..11));
    }
}
