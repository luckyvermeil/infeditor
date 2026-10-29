//! File I/O: encoding detection, opening, saving.

pub mod encoding;
pub mod file;

pub use encoding::{decode_all, detect, Detected};
pub use file::{open_file, open_file_progressive, save_file, Opened, SaveOptions, HUGE_THRESHOLD};
