//! File association: the registry (HKCU) half of the abstraction.
//!
//! The semantics documentation (why HKCU, why defaults are not stolen, the
//! PrevDefault restore protocol) lives with the implementation — see the
//! `win32` module header. This module holds only **key/value operations**;
//! "which extensions are supported" (the language list) and "what the result
//! messages say" (i18n strings) are host-side concerns:
//!
//! * The extension list is passed in by the caller ([`registered_exts`]) or
//!   registered one by one ([`register_ext`]);
//! * Display strings (`doc_name` / `app_desc` / `mui_label`) are resolved by
//!   the caller in the current language at call time — the platform layer
//!   does not know i18n exists.
//!
//! On unsupported platforms (POSIX): queries are always empty, writes are
//! always `Err(Unsupported)`, and [`supported`] is `false` — the host renders
//! the UI as disabled instead of pretending success.

#[cfg(not(windows))]
mod posix;
#[cfg(windows)]
mod win32;

#[cfg(not(windows))]
use posix as os;
#[cfg(windows)]
use win32 as os;

/// ProgID: every extension points at this single "file type"; uninstall and
/// troubleshooting only need to look at this one key.
pub const PROG_ID: &str = "InfEditor_file";

/// Verb name for the explorer context menu (also the key name; invisible to users).
pub const CTX_VERB: &str = "InfEditor";

/// Whether this platform can write associations (`false` means the host
/// renders the entire settings page as disabled).
pub fn supported() -> bool {
    os::supported()
}

/// Open command line: `"<exe>" "%1"`.
///
/// The quotes are not optional — a program launched from `C:\Program
/// Files\...` without quotes gets split into two arguments by the "Open
/// with" list entry, and double-clicking a file fails outright.
pub fn open_command(exe: &str) -> String {
    format!("\"{exe}\" \"%1\"")
}

/// Establish "this app exists": ProgID, `Applications\<exe>`, `Capabilities`,
/// `RegisteredApplications`. Idempotent — every registration runs the whole
/// thing again, so even if the user moves the exe, the command path follows.
///
/// Display names are resolved by the caller in the current language.
pub fn ensure_app_registered(
    exe: &std::path::Path,
    app_name: &str,
    doc_name: &str,
    app_desc: &str,
) -> std::io::Result<()> {
    os::ensure_app_registered(exe, app_name, doc_name, app_desc)
}

/// Register one extension. Returns whether anything actually changed
/// ("already in the list" does not count as a new registration).
///
/// `take_default` also changes the `<ext>` default to us (the old value is
/// archived first and restored on unregister).
pub fn register_ext(ext: &str, take_default: bool) -> std::io::Result<bool> {
    os::register_ext(ext, take_default)
}

/// Unregister one extension. If the default handler still points at us, the
/// archived value is restored.
pub fn unregister_ext(ext: &str) -> std::io::Result<bool> {
    os::unregister_ext(ext)
}

/// The subset of `candidates` (lowercase, with dot) that is registered, in
/// the given order.
pub fn registered_exts(candidates: &[String]) -> Vec<String> {
    os::registered_exts(candidates)
}

/// The current user's default-handler ProgID for an extension (`None` = the
/// user never changed it; the system default applies).
pub fn default_handler(ext: &str) -> Option<String> {
    os::default_handler(ext)
}

/// Whether the explorer context menu "Open with InfEditor" is installed.
pub fn context_menu_installed() -> bool {
    os::context_menu_installed()
}

/// Install/uninstall the explorer context menu. `mui_label` is the text shown
/// on the menu (in the current language).
pub fn set_context_menu(
    on: bool,
    mui_label: &str,
    exe: Option<&std::path::Path>,
) -> std::io::Result<()> {
    os::set_context_menu(on, mui_label, exe)
}

/// Make explorer re-read associations immediately (call once after batch
/// register/unregister; `set_context_menu` already does this internally, so
/// no need to repeat it).
pub fn notify_shell() {
    os::notify_shell()
}
