//! Syntax highlighting: language definitions, row lexer and themes.

pub mod code;
pub mod lang;
pub mod lexer;
pub mod theme;

pub use lang::{by_name, for_path, for_path_and_first_line, names, LangSpec, TokenKind};
pub use lexer::{Highlighter, LexState, Token};
pub use theme::{Theme, DARK, LIGHT};

/// Byte size above which highlighting is disabled by default (the user can
/// force it on from the View menu).
///
/// The lexer is budgeted (`lexer::FRAME_BYTES` per frame), so highlighting a
/// large file no longer stalls frames outright — but every cold-region replay
/// still burns its whole budget before the frame is honest, and on a
/// multi-megabyte file that tax lands on every jump and drag. The line is
/// drawn at the same "large document" size the outline parser and the minimap
/// use, so all three side features give up together.
pub const AUTO_DISABLE_BYTES: u64 = 4 * 1024 * 1024;
