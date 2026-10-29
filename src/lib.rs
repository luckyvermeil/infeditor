//! InfEditor library: the editor engine ([`buffer`] piece table + line index,
//! [`io`] encoding/detect/save, [`search`], [`mod@format`], [`highlight`],
//! [`textops`], [`jsonl`], [`digest`]) plus the whole Blitz UI layer in [`ui`].
//!
//! `src/main.rs` is only the binary shell: DPI awareness, window attributes,
//! the panic hook (writing through [`panic_log`]), and the launch/cleanup pair.

pub mod buffer;
// Shared with `build.rs` (compiled into the build script through `#[path]`), so
// the `.res`/ICO byte format is covered by `cargo test` — a build script's own
// tests never run.
#[doc(hidden)]
pub mod build_res;
pub mod digest;
pub mod format;
pub mod highlight;
pub mod io;
pub mod jsonl;
pub mod panic_log;
pub mod search;
pub mod textops;
pub mod ui;
