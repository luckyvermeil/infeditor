//! Shared [`EditorState`] fixtures for unit tests (`#[cfg(test)]`, excluded
//! from release builds).
//!
//! Why centralized: `st_of` was copied into 9 test modules, while "use
//! `minimal()` not `default()`" is a discipline that **must** be followed —
//! `default()` reads machine settings (theme/font size/jsonl·CSV switches),
//! queues command-line files into the open list, and even runs the
//! crash-recovery check, so unit test results drifted with the machine (the
//! old copy in `viewport` was exactly a `default()`). The fixture is written
//! once here, and so is the discipline.
//!
//! Only the "build a state" common parts live here; fixtures with business
//! preconditions (e.g. needing a large document or pre-seeded search results)
//! stay in their own test modules.

use crate::buffer::TextDocument;
use crate::ui::state::{Caret, EditorState, CHROME_TOP};

/// Minimal state: one tab holding `text`, caret at 0, viewport with bootstrap
/// geometry (`ViewRect::default()`, 1280×~800, enough for render-like tests
/// to produce real lines).
pub(crate) fn st_of(text: &str) -> EditorState {
    let mut st = EditorState::minimal();
    st.tabs[0].doc = TextDocument::from(text);
    st.tabs[0].caret = Caret::default();
    st
}

/// Same as above, but with the viewport pinned to 800×200 (top edge =
/// [`CHROME_TOP`], i.e. title bar + tab bar).
///
/// For geometry assertions: hit-testing, scroll self-healing, visible-line
/// count and similar cases need a **deterministic** edit-area size instead of
/// following `ViewRect::default()`'s bootstrap value or the machine's window
/// size.
///
/// The top edge references a constant instead of hard-coded pixels: what used
/// to be copied here was `52.0` (a leftover from the title bar 34 + tab bar
/// 18 era), and the moment a layout constant changed the fixture silently
/// drifted 12px. Geometry tests are insensitive to offsets, so assert never
/// catches such drift — the only cure is not writing magic numbers.
pub(crate) fn st_of_view(text: &str) -> EditorState {
    let mut st = st_of(text);
    st.view.x = 0.0;
    st.view.y = CHROME_TOP;
    st.view.w = 800.0;
    st.view.h = 200.0;
    st
}
