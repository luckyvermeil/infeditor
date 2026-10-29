//! POSIX implementation (Linux/macOS etc., compiled only under `cfg(not(windows))`; public API in [`crate`]).

use std::path::Path;

pub fn locale_tag() -> Option<String> {
    // POSIX precedence for the *message* locale: LC_ALL overrides everything,
    // then LC_MESSAGES (the category this app cares about), then LANG.
    // An empty value means "unset" in POSIX, not "empty locale" — the old
    // `var(...).or_else(var("LANG"))` chain returned Some("") for `LC_ALL=`
    // and never consulted LANG, so a shell that exported an empty LC_ALL
    // silently lost the system language.
    fn non_empty(name: &str) -> Option<String> {
        std::env::var(name).ok().filter(|v| !v.trim().is_empty())
    }
    // Return verbatim and let the caller's tag parser decide (may be a
    // composite like `zh_CN.UTF-8`; parse failure = `None`, falls back to the
    // default language).
    non_empty("LC_ALL")
        .or_else(|| non_empty("LC_MESSAGES"))
        .or_else(|| non_empty("LANG"))
}

pub fn legacy_ansi_codepage() -> u32 {
    // No "ANSI codepage" concept: UTF-8 expresses "no text needs legacy
    // encoding guessing".
    65001
}

pub fn open_semi_shared(path: &Path) -> std::io::Result<std::fs::File> {
    // No sharing bits: mmap shrink protection is naturally provided by the
    // write side (temp+rename) and inode semantics — after rename replaces
    // the old file, the mapping still points at the old inode and reads the
    // content from the moment the file was opened.
    std::fs::File::open(path)
}

/// See [`crate::create_replacement_file`].
///
/// The mode is handed to `open(2)` as well as applied afterwards, in that
/// order: `open` applies the umask, so the file never exists with *wider*
/// bits than the target had (a `0600` target is never briefly `0644`), but
/// the umask can also *narrow* it (a `0664` target under a `0022` umask).
/// Owner-write is added for the duration of the write — the caller must be
/// able to open this file, and a `0444` target has to stay readable-only in
/// the end, not unwritable in the middle; `finalize_replacement_file` lands
/// the exact bits.
pub fn create_replacement_file(target: &Path, tmp: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;

    let mode = inherited_mode(target)?;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    if let Some(mode) = mode {
        opts.mode(mode | 0o200);
    }
    let file = opts.open(tmp)?;
    drop(file);
    Ok(())
}

/// See [`crate::finalize_replacement_file`].
pub fn finalize_replacement_file(target: &Path, tmp: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    if let Some(mode) = inherited_mode(target)? {
        std::fs::set_permissions(tmp, std::fs::Permissions::from_mode(mode))?;
    }
    Ok(())
}

/// The mode to inherit from `target`: `None` when it does not exist yet (a new
/// document), `Err` when it exists but cannot be inspected.
///
/// The error case is deliberately fatal for the save rather than "fall back to
/// the default": writing a temp file with default permissions over a file we
/// cannot stat is exactly the leak this pair exists to prevent.
fn inherited_mode(target: &Path) -> std::io::Result<Option<u32>> {
    use std::os::unix::fs::PermissionsExt;

    match std::fs::metadata(target) {
        Ok(meta) => Ok(Some(meta.permissions().mode())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// macOS' default volume format (APFS, and HFS+ before it) is **case-
/// insensitive**, like the Windows file systems — the same file opened as
/// `Notes.md` and `notes.md` must not become two tabs, and external-change
/// detection must still match. `\` remains a legal filename character there,
/// so only the case rule is borrowed, not Windows' separator folding.
///
/// A case-sensitive APFS volume is the rare exception; the platform already
/// makes the same fixed assumption on Windows.
#[cfg(target_os = "macos")]
pub fn paths_equal(a: &Path, b: &Path) -> bool {
    a == b || a.to_string_lossy().to_lowercase() == b.to_string_lossy().to_lowercase()
}

/// Linux (and the other POSIX targets) is case-sensitive and `\` is a legal
/// filename character: exact byte-wise comparison.
#[cfg(not(target_os = "macos"))]
pub fn paths_equal(a: &Path, b: &Path) -> bool {
    a == b
}

/// Civil date/time from a Unix timestamp in **local** time.
///
/// `localtime_r` is the C library's reentrant local-time conversion: it honors
/// `TZ` / `/etc/localtime`, so the inserted timestamp and the crash-log date
/// match the user's clock instead of being hours off (the old implementation
/// returned UTC on every POSIX platform).
///
/// Timestamps the platform's `time_t` cannot represent (32-bit targets past
/// 2038) fall back to [`utc_civil`].
pub fn local_civil(unix_secs: i64) -> (i32, u32, u32, u32, u32, u32) {
    let secs = unix_secs as libc::time_t;
    if secs as i64 != unix_secs {
        return utc_civil(unix_secs);
    }
    // SAFETY: `tm` is a valid, properly aligned `libc::tm`; `localtime_r`
    // fills it on success and returns null on failure (leaving it untouched).
    // The pointer never escapes this function.
    let mut tm = std::mem::MaybeUninit::<libc::tm>::uninit();
    if unsafe { libc::localtime_r(&secs, tm.as_mut_ptr()) }.is_null() {
        return utc_civil(unix_secs);
    }
    let tm = unsafe { tm.assume_init() };
    (
        tm.tm_year + 1900,
        (tm.tm_mon + 1) as u32,
        tm.tm_mday as u32,
        tm.tm_hour as u32,
        tm.tm_min as u32,
        tm.tm_sec as u32,
    )
}

/// UTC civil date/time (Howard Hinnant's civil_from_days), used only when the
/// platform cannot convert the timestamp.
fn utc_civil(unix_secs: i64) -> (i32, u32, u32, u32, u32, u32) {
    let days = unix_secs.div_euclid(86_400);
    let rem = unix_secs.rem_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (
        y as i32,
        m as u32,
        d as u32,
        (rem / 3600) as u32,
        ((rem % 3600) / 60) as u32,
        (rem % 60) as u32,
    )
}

pub fn reveal_in_file_manager(path: &Path, select: bool) -> std::io::Result<()> {
    #[cfg(target_os = "macos")]
    {
        // `open -R` reveals the file in Finder; a directory itself is opened directly.
        if select {
            std::process::Command::new("open")
                .arg("-R")
                .arg(path)
                .spawn()
                .map(|_| ())
        } else {
            std::process::Command::new("open")
                .arg(path)
                .spawn()
                .map(|_| ())
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        // xdg-open has no "select" semantic: the file degrades to its containing directory.
        let target = if select {
            path.parent().unwrap_or(path).to_path_buf()
        } else {
            path.to_path_buf()
        };
        std::process::Command::new("xdg-open")
            .arg(target)
            .spawn()
            .map(|_| ())
    }
}

pub fn open_url(url: &str) -> std::io::Result<()> {
    #[cfg(target_os = "macos")]
    const BROWSER: &str = "open";
    #[cfg(not(target_os = "macos"))]
    const BROWSER: &str = "xdg-open";
    std::process::Command::new(BROWSER)
        .arg(url)
        .spawn()
        .map(|_| ())
}

pub fn spawn_shell(command: &str, cwd: Option<&Path>) -> std::io::Result<()> {
    let mut c = std::process::Command::new("sh");
    c.arg("-c").arg(command);
    if let Some(dir) = cwd {
        if dir.is_dir() {
            c.current_dir(dir);
        }
    }
    c.spawn().map(|_| ())
}
