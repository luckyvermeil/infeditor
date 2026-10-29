//! Design tokens: the **single source of truth** for font stacks, font size
//! scale, corner radii, and base control dimensions.
//!
//! The component library consolidates magic numbers for the same semantics
//! scattered across components into one place — restyling only requires
//! changes here. Apps can wholesale `use ui_kit::tokens::*;` to reuse them
//! and keep visual rhythm consistent.

/// UI font stack: CJK-first (YaHei UI covers CJK and Latin), falls back to
/// Segoe UI for Western text.
pub const FONT_UI: &str = "'Microsoft YaHei UI','Segoe UI',sans-serif";

/// Monospace font stack (editing area / tables / code).
pub const FONT_MONO: &str = "Consolas,'Cascadia Mono',monospace";

// ---- Font size scale (px) ----
/// Checkmark glyphs (menu check, checkbox check)
pub const FS_GLYPH: f32 = 9.0;
/// Small: status bar hints, find-bar small buttons, dialog labels
pub const FS_SM: f32 = 11.0;
/// Standard: menu rows, toolbars, tabs, dialog body
pub const FS_MD: f32 = 12.0;
/// Dialog titles / emphasis
pub const FS_LG: f32 = 13.0;
/// Window button glyphs (× etc.)
pub const FS_GLYPH_LG: f32 = 15.0;

// ---- Corner radius scale (px) ----
/// Inputs, small buttons
pub const RAD_SM: f32 = 2.0;
/// Panels, dialogs
pub const RAD_LG: f32 = 6.0;

// ---- Control dimensions (px) ----
/// Standard height of flat buttons / inputs (unified across toolbars, find
/// bar, dialog controls)
pub const BTN_H: f32 = 22.0;
