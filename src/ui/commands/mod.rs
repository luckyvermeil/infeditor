//! Editing commands operating on [`EditorState`](crate::ui::state::EditorState),
//! split by responsibility: core editing and line targeting (`edit`), line/tool
//! operations (`lines`), multi-click selection (`multiclick`), bracket matching
//! (`brackets`) and bookmarks / external-change polling (`bookmarks`).
//!
//! The methods are inherent impls, so call sites are unaffected: `st.copy()`
//! works no matter which file the impl block lives in.

mod bookmarks;
mod brackets;
pub mod csv;
mod edit;
pub mod json;
mod lines;
pub mod markdown;
mod multiclick;
mod view;

use crate::ui::Msg;

/// The unified read-only notice (a key, not a rendered string; the status bar
/// renders it in the current language).
pub const READONLY_MSG: Msg = Msg::StatusReadOnlyHint;

/// Test fixture shared by the command-module suites: see
/// [`crate::ui::testutil`] — the single implementation lives there, this
/// module only keeps the historical `commands::testutil::st_of` path so the
/// thirteen suites that import it stay untouched.
#[cfg(test)]
pub(crate) mod testutil {
    pub(crate) use crate::ui::testutil::st_of;
}
