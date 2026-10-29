//! Text marks (editor right-click → Style... / Clear style).
//!
//! Applies one of five colors to the selection (or the word under the
//! caret); `Style all` covers every occurrence in the document, `Style
//! current` only this one.
//!
//! # Positions drift, so marks carry their own evidence
//!
//! A mark stores an **absolute byte range**. Once the document changes,
//! later ranges misalign — a misaligned highlight is far worse than none
//! (it lies). So each mark also stores the text at creation time
//! ([`Mark::text`]) and is re-checked before drawing (see `viewport::build`):
//! if the range's content no longer matches the snapshot, it is not drawn.
//! Marks therefore "vanish automatically after edits" instead of "painting
//! over the wrong characters" — the same trade-off as bookmarks, which store
//! line numbers and re-align by line number after edits.

use crate::buffer::doc::TextDocument;
use crate::ui::state::EditorState;

use super::{Fmt, Msg};

/// Single mark. `text` is the interval content snapshot at creation time (see module comment).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mark {
    /// Absolute byte start (inclusive)
    pub start: u64,
    /// Absolute byte end (exclusive)
    pub end: u64,
    /// Palette index 1..=5 ([`crate::highlight::Theme::mark`])
    pub style: u8,
    /// Text snapshot at creation time; re-checked before drawing
    pub text: String,
}

/// Cap on full-text scanning. Above it, "Style all" marks only the current
/// occurrence — a full-text match over a 500 MB log plus thousands of stored
/// ranges stutters more visibly than the highlight itself.
const MAX_MARK_SCAN: usize = 16 * 1024 * 1024;

impl Mark {
    /// Is this mark currently overlaying its created text (see module comment)?
    pub fn still_valid(&self, doc: &TextDocument) -> bool {
        if self.start >= self.end || self.end > doc.table.len() {
            return false;
        }
        doc.text_range(self.start..self.end) == self.text
    }
}

impl EditorState {
    /// Marks of the current document (never marked ⇒ empty slice).
    pub fn doc_marks(&self) -> &[Mark] {
        self.hl_marks
            .get(&self.active_doc().id)
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }

    /// The range to mark/clear: the selection first, else the word under the
    /// caret (empty if none).
    fn mark_target(&self) -> (u64, u64) {
        if let Some(r) = self.sel_range() {
            if r.start < r.end {
                return (r.start, r.end);
            }
        }
        let pos = self.caret().pos;
        let r = self.word_range_at(pos);
        (r.start, r.end)
    }

    /// Right-click → Style...: `all` = every occurrence in the document, else
    /// only this one under the caret.
    pub fn mark_style(&mut self, style: u8, all: bool) {
        let style = style.clamp(1, 5);
        let (start, end) = self.mark_target();
        if start >= end {
            self.status = Some(Msg::StatusMarkNoTarget.into());
            return;
        }
        let doc_id = self.active_doc().id;
        let needle = self.active_doc().text_range(start..end);
        if needle.is_empty() {
            self.status = Some(Msg::StatusMarkNoText.into());
            return;
        }

        let mut ranges: Vec<(u64, u64)> = vec![(start, end)];
        let mut huge = false;
        if all {
            let doc = self.active_doc();
            if doc.table.len() as usize <= MAX_MARK_SCAN {
                let hay = doc.read_all();
                ranges = match_indices(&hay, &needle);
            } else {
                huge = true;
            }
        }

        let marks = self.hl_marks.entry(doc_id).or_default();
        let mut added = 0usize;
        for (s, e) in ranges {
            // Same range already marked ⇒ recolor (clicking another style again should recolor, not stack)
            marks.retain(|m| !(m.start == s && m.end == e));
            marks.push(Mark {
                start: s,
                end: e,
                style,
                text: needle.clone(),
            });
            added += 1;
        }
        self.status = Some(if huge {
            Fmt::StatusMarkHugeDoc {
                mb: MAX_MARK_SCAN / 1024 / 1024,
                style: style as usize,
            }
            .into()
        } else if all {
            Fmt::StatusMarked {
                n: added,
                style: style as usize,
            }
            .into()
        } else {
            Fmt::StatusMarkedCurrent {
                style: style as usize,
            }
            .into()
        });
    }

    /// Right-click → Clear style: `all` = everything in the current document,
    /// else clear at the selection/word.
    pub fn mark_clear(&mut self, all: bool) {
        let doc_id = self.active_doc().id;
        if all {
            let n = self.hl_marks.get(&doc_id).map(|v| v.len()).unwrap_or(0);
            self.hl_marks.remove(&doc_id);
            self.status = Some(Fmt::StatusClearedMarks { n }.into());
            return;
        }
        let (start, end) = self.mark_target();
        let Some(marks) = self.hl_marks.get_mut(&doc_id) else {
            self.status = Some(Msg::StatusNoMarks.into());
            return;
        };
        let before = marks.len();
        // Anything intersecting the target range is cleared: a click inside a
        // mark or a selection covering part of one both count
        marks.retain(|m| m.end <= start || m.start >= end.max(start + 1));
        let n = before - marks.len();
        if marks.is_empty() {
            self.hl_marks.remove(&doc_id);
        }
        self.status = Some(if n == 0 {
            Msg::StatusNoMarkHere.into()
        } else {
            Fmt::StatusClearedMarks { n }.into()
        });
    }
}

/// All occurrences of `needle` in the full text (byte ranges).
fn match_indices(hay: &str, needle: &str) -> Vec<(u64, u64)> {
    if needle.is_empty() {
        return Vec::new();
    }
    hay.match_indices(needle)
        .map(|(i, _)| (i as u64, (i + needle.len()) as u64))
        .collect()
}

#[cfg(test)]
mod tests {
    use crate::ui::testutil::st_of;

    #[test]
    fn mark_all_covers_every_occurrence_and_current_only_one() {
        // The word under the caret = `foo`, three occurrences in the document
        let mut st = st_of("foo bar foo\nfoo baz");
        st.set_caret(1, false);
        st.mark_style(2, true);
        assert_eq!(st.doc_marks().len(), 3, "three `foo` in the document");
        assert!(st.doc_marks().iter().all(|m| m.style == 2));

        let mut st = st_of("foo bar foo");
        st.set_caret(0, false);
        st.mark_style(5, false);
        assert_eq!(st.doc_marks().len(), 1, "current marks only one");
        assert_eq!(st.doc_marks()[0].style, 5);
        assert_eq!((st.doc_marks()[0].start, st.doc_marks()[0].end), (0, 3));
    }

    #[test]
    fn re_marking_the_same_range_changes_the_colour_instead_of_stacking() {
        let mut st = st_of("foo foo");
        st.set_caret(0, false);
        st.mark_style(1, true);
        st.mark_style(3, true);
        assert_eq!(st.doc_marks().len(), 2);
        assert!(st.doc_marks().iter().all(|m| m.style == 3));
    }

    #[test]
    fn clear_all_and_clear_current() {
        let mut st = st_of("foo foo foo");
        st.set_caret(0, false);
        st.mark_style(1, true);
        assert_eq!(st.doc_marks().len(), 3);

        // Clear the occurrence under the caret
        st.set_caret(4, false); // inside the second foo
        st.mark_clear(false);
        assert_eq!(st.doc_marks().len(), 2);

        st.mark_clear(true);
        assert!(st.doc_marks().is_empty());
    }

    #[test]
    fn a_mark_that_no_longer_matches_its_text_is_reported_invalid() {
        let mut st = st_of("hello world");
        st.select_all();
        st.mark_style(1, false);
        let doc = st.active_doc();
        let m = st.doc_marks()[0].clone();
        assert!(m.still_valid(doc));
    }

    #[test]
    fn nothing_selected_marks_nothing_and_says_so() {
        let mut st = st_of("");
        st.mark_style(1, true);
        assert!(st.doc_marks().is_empty());
        let key = st.status.as_ref().expect("status set").key();
        assert!(key.contains("StatusMarkNoTarget"), "{key}");
    }
}
