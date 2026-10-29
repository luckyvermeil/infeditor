//! WYSIWYG Markdown editor — a Dioxus Native (Blitz) component.
//!
//! Usage and design notes: `crates/wysiwyg/README.md`.
//!
//! Architecture in one sentence: **model-driven**. `model::Doc` (the block
//! tree) is the single source of truth; the DOM is purely derived (never
//! written back after render), and Blitz is used only as a read-only renderer
//! and layout querier — not the "browser mutates the DOM → round-trip
//! normalization" route: Blitz has no contenteditable, and there is no need
//! for `<wbr>` caret-anchor hacks.
//!
//! Module layering (the first two layers have zero GUI dependencies and can be
//! tested standalone with `cargo test`):
//! - [`model`] block tree + `Pos` logical coordinates
//! - [`md`]    Markdown ↔ Doc (pulldown-cmark parsing / hand-written emit)
//! - [`render`] Doc → HTML string (injected via `dangerous_inner_html`)
//! - [`geom`]  bidirectional coordinate queries (click→Pos; Pos→caret rect,
//!   parley `Cursor`)
//! - [`ui`]    the `Wysiwyg` component (event wiring, caret overlay)
//!
//! DOM conventions (geom depends on these; read geom's comments before
//! changing): **one block = one block-level element carrying `data-bid` = one
//! inline root**; `data-off` is reserved for multi-inline-root scenarios
//! (currently always 0).

pub mod chem;
mod dom_index;
pub mod edit;
pub mod geom;
pub mod math;
pub mod md;
pub mod model;
pub mod paste;
pub mod preview;
pub mod render;
pub mod ui;

pub use edit::{Cmd, Editor, Move, TableCmd, WrapKind};
// Rich paste: HTML clipboard → sanitize → Markdown (used by the host's Ctrl+V
// entry point)
pub use model::{Block, Doc, Inline, Pos, Sel};
pub use paste::html_to_markdown;
// Preview: the semantic-HTML exit sharing the edit block tree (formerly
// infeditor format/markdown.rs)
pub use preview::{escape_html, github_css, preview_html, safe_link_url};
// Preview-only parsing: chemical-formula / LaTeX delimiters are normalized
// before entering the block tree (the edit view does not go through here)
pub use chem::parse_for_preview;
pub mod hl;
// Host bridge + component: infeditor's composer keyboard path goes through
// host_* (see the ui.rs notes)
pub use geom::OverlayRect;
pub use ui::{
    host_apply_cmd, host_move_line_edge, host_move_visual, host_resync, parse_doc, Overlays,
    Wysiwyg,
};
