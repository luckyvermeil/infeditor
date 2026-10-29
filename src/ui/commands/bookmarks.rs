//! Line bookmarks and external-modification detection.

use crate::ui::state::EditorState;
use crate::ui::{Fmt, Msg};

// ---- Line bookmarks and external-change detection ----------------------------

impl EditorState {
    // ---- Bookmarks ----------------------------------------------------------

    /// The bookmark set (current document).
    fn marks(&self) -> std::collections::BTreeSet<u64> {
        self.bookmarks
            .get(&self.active_doc().id)
            .cloned()
            .unwrap_or_default()
    }

    /// Toggle a bookmark on the caret's line.
    pub fn toggle_bookmark(&mut self) {
        let id = self.tab().doc.id;
        let line = self.active_doc().line_of_pos(self.caret().pos);
        let set = self.bookmarks.entry(id).or_default();
        if set.contains(&line) {
            set.remove(&line);
            self.status = Some(
                Fmt::StatusBookmarkCleared {
                    line: (line + 1) as usize,
                }
                .into(),
            );
        } else {
            set.insert(line);
            self.status = Some(
                Fmt::StatusBookmarkSet {
                    line: (line + 1) as usize,
                }
                .into(),
            );
        }
    }

    /// Jump to the next/previous bookmark (wrapping around). `dir` is 1 or -1.
    pub fn goto_bookmark(&mut self, dir: i32) {
        let marks = self.marks();
        if marks.is_empty() {
            self.status = Some(Msg::StatusNoBookmarks.into());
            return;
        }
        let cur = self.active_doc().line_of_pos(self.caret().pos) as i64;
        // Flatten the circular set and find the first entry strictly greater/smaller than the current line.
        let lines: Vec<i64> = marks.iter().map(|l| *l as i64).collect();
        let target = if dir > 0 {
            lines
                .iter()
                .copied()
                .find(|l| *l > cur)
                .or_else(|| lines.first().copied())
        } else {
            lines
                .iter()
                .rev()
                .copied()
                .find(|l| *l < cur)
                .or_else(|| lines.last().copied())
        };
        let Some(line) = target else { return };
        let pos = self.active_doc().pos_of_line(line.max(0) as u64);
        self.goto_line(line.max(0) as u64 + 1);
        self.set_caret(pos, false);
        self.status = Some(
            Fmt::StatusBookmarkGoto {
                line: (line.max(0) + 1) as usize,
            }
            .into(),
        );
    }

    // ---- External-change detection --------------------------------------------

    /// Record the mtime of tab `i` (ignored when it has no path); the old hint goes stale after a reload.
    pub fn note_mtime_of(&mut self, i: usize) {
        let Some(tab) = self.tabs.get(i) else { return };
        let Some(path) = tab.doc.meta.path.clone() else {
            return;
        };
        let id = tab.doc.id;
        if let Some(t) = std::fs::metadata(&path)
            .ok()
            .and_then(|m| m.modified().ok())
        {
            self.mtimes.insert(id, t);
        }
        if i == self.active {
            self.external_hint = false;
        }
    }

    /// Record the active document's current mtime (called after open/save/reload as the comparison baseline).
    pub fn note_mtime(&mut self) {
        let path = self.active_doc().meta.path.clone();
        if let Some(t) = path
            .and_then(|p| std::fs::metadata(&p).ok())
            .and_then(|m| m.modified().ok())
        {
            let id = self.tab().doc.id;
            self.mtimes.insert(id, t);
        }
    }

    /// Check whether the active document was modified by an external program.
    ///
    /// Throttled to about once per second (hung on RedrawRequested, which can fire
    /// very often). On a hit **only hint, never act**: the conflict between an
    /// external modification and unsaved local changes is the user's call to make
    /// (reloading would drop the local edits); an editor making that decision for
    /// the user is how data-loss accidents happen.
    pub fn poll_external_change(&mut self) {
        let now = std::time::Instant::now();
        if let Some(t) = self.last_watch {
            if now.duration_since(t).as_millis() < 1000 {
                return;
            }
        }
        self.last_watch = Some(now);
        let Some(path) = self.active_doc().meta.path.clone() else {
            return;
        };
        let id = self.tab().doc.id;
        let cur = std::fs::metadata(&path)
            .ok()
            .and_then(|m| m.modified().ok());
        match (self.mtimes.get(&id).copied(), cur) {
            (None, Some(t)) => {
                self.mtimes.insert(id, t);
            }
            (Some(old), Some(new)) if old != new && !self.external_hint => {
                self.external_hint = true;
                self.status = Some(Msg::StatusExternalModified.into());
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod bookmark_tests {

    use crate::ui::commands::testutil::st_of;

    /// Toggle → jump → clear cycle.
    #[test]
    fn bookmarks_toggle_and_navigate() {
        let mut st = st_of("l1\nl2\nl3\nl4\n");
        st.set_caret(0, false);
        st.toggle_bookmark();
        st.set_caret(3, false); // line 2 (3 bytes per line)
        st.toggle_bookmark();
        assert_eq!(st.marks(), [0, 1].into_iter().collect());

        // Searching backwards from line 4 should land on line 2, then line 1, then wrap back to line 2.
        st.set_caret(9, false);
        st.goto_bookmark(-1);
        assert_eq!(st.active_doc().line_of_pos(st.caret().pos), 1);
        st.goto_bookmark(-1);
        assert_eq!(st.active_doc().line_of_pos(st.caret().pos), 0);
        st.goto_bookmark(-1);
        assert_eq!(
            st.active_doc().line_of_pos(st.caret().pos),
            1,
            "wrapped to the end"
        );

        // Same going backwards.
        st.goto_bookmark(1);
        assert_eq!(st.active_doc().line_of_pos(st.caret().pos), 0);

        // Cleared.
        st.set_caret(0, false);
        st.toggle_bookmark();
        assert_eq!(st.marks(), [1].into_iter().collect());
    }

    /// Jumping with no bookmarks reports a status message instead of panicking.
    #[test]
    fn goto_without_marks_reports() {
        let mut st = st_of("a\nb");
        st.goto_bookmark(1);
        assert!(
            st.status_text().contains(
                i18n::current()
                    .tr(crate::ui::Msg::StatusNoBookmarks)
                    .as_ref()
            ),
            "{}",
            st.status_text()
        );
    }
}
