//! POSIX implementation of file association: the feature does not exist,
//! everything degrades honestly.
//!
//! Queries are always empty / writes are always `Err(Unsupported)` /
//! [`super::supported`] is `false`; the host renders the settings page as
//! disabled — sharing the same message path as a real failure on Windows.

use std::io;
use std::path::Path;

pub fn supported() -> bool {
    false
}

pub fn ensure_app_registered(
    _exe: &Path,
    _app_name: &str,
    _doc_name: &str,
    _app_desc: &str,
) -> io::Result<()> {
    Err(unsupported())
}

pub fn register_ext(_ext: &str, _take_default: bool) -> io::Result<bool> {
    Err(unsupported())
}

pub fn unregister_ext(_ext: &str) -> io::Result<bool> {
    Err(unsupported())
}

pub fn registered_exts(_candidates: &[String]) -> Vec<String> {
    Vec::new()
}

pub fn default_handler(_ext: &str) -> Option<String> {
    None
}

pub fn context_menu_installed() -> bool {
    false
}

pub fn set_context_menu(_on: bool, _mui_label: &str, _exe: Option<&Path>) -> io::Result<()> {
    Err(unsupported())
}

pub fn notify_shell() {}

fn unsupported() -> io::Error {
    io::Error::new(
        io::ErrorKind::Unsupported,
        "file association requires Windows",
    )
}
