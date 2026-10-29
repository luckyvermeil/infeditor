//! Pure formatters and parsers behind the Tools / View features:
//!
//! - [`json_pretty`] — JSON formatting (streaming validation + byte-level re-indent)
//! - [`xml_pretty`] — XML formatting (tag-based indentation)
//! - [`parse_csv`] — RFC 4180 CSV/TSV parsing for the CSV table view
//!
//! Markdown preview (the old `markdown`/`math` submodules) moved to the
//! `wysiwyg` crate and is unified with visual editing: `wysiwyg::md::parse`
//! (pulldown-cmark, CommonMark + GFM) → block tree →
//! [`wysiwyg::preview_html`] (semantic HTML). Formulas render in
//! `wysiwyg::math` (`MathSpec`: `svg`/`cached`, process cache `CACHE_CAP`).
//!
//! One submodule per formatter (no cross-dependencies, each carries its own
//! tests); this module only re-exports, so the external paths
//! `crate::format::xxx` stay unchanged. Everything works on plain
//! `String`s so it is unit-testable without a document or UI, same contract as
//! [`crate::textops`].

mod csv;
mod json;
mod xml;

pub use csv::{
    csv_quotes_balanced, csv_sort_key, parse_csv, parse_csv_indexed, parse_csv_spans,
    serialize_csv_row,
};
pub use json::json_pretty;
pub use xml::xml_pretty;
