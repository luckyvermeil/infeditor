//! Motion tokens — **implementation has moved to the shared component library
//! [`ui_kit::motion`]**; this module only re-exports it.
//!
//! `crate::ui::motion::…` is kept so existing references across the repo need no
//! change; the single implementation of durations, easing and the `Press`
//! three-state lives in `crates/ui-kit/src/motion.rs`.

pub use ui_kit::motion::*;
