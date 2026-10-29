//! Design tokens — the **implementation has moved to the shared component
//! library [`ui_kit::tokens`]**; this module only re-exports it.
//!
//! The single implementation of the font stack, font-size scale, radii and
//! control sizes lives in `crates/ui-kit/src/tokens.rs`; keeping the
//! `crate::ui::design::…` paths means existing references need zero changes.

pub use ui_kit::tokens::*;
