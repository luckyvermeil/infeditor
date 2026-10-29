//! Text engine: piece table storage, sparse line index, document with undo.

pub mod doc;
pub mod line_index;
pub mod piece_table;

pub use doc::{CoalesceKey, DocMeta, DocSnapshot, EditOp, TextDocument};
pub use line_index::{LineIndex, LineSpan, Newline};
pub use piece_table::{Original, PieceTable};
