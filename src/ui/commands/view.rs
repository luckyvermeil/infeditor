//! The one shared epilogue for view-mode switches.
//!
//! Table (CSV), JSON tree, JSONL and Markdown (preview / WYSIWYG) are all the
//! same switch: **flip a per-tab flag → write the status bar → let the viewport
//! make room again**. The first two steps are easy to write in each module, but
//! skipping the third (`relayout`) does not fail immediately — the toolbar
//! appearing or disappearing moves the editor's origin, and only the state
//! layer's next-frame layout fallback catches up, which shows up as the first
//! frame being positioned wrong or covered by the toolbar. The JSONL preview
//! switch was exactly that one line short.
//!
//! Folding "the message + relayout" into a single exit point makes skipping the
//! third step structurally impossible: a new view only calls
//! [`EditorState::view_switched`] and never has to remember a second epilogue.

use crate::ui::state::EditorState;

impl EditorState {
    /// The epilogue of a view-mode switch: status-bar message + the viewport making
    /// room again (the caller flips the flag).
    ///
    /// The message is a **key** (`impl Into<i18n::Message>`): most views pass a
    /// constant, while the JSON tree view carries parse-error details (`Fmt` fields)
    /// — both go through this one entry point.
    pub(crate) fn view_switched(&mut self, msg: impl Into<i18n::Message>) {
        self.status = Some(msg.into());
        self.relayout();
    }
}
