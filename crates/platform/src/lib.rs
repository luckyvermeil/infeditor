//! # platform — the single home for platform differences
//!
//! One rule: **`cfg(windows)` / `cfg!(windows)` must not appear in host crates**.
//! Every spot where "this is done differently per OS" — encoding codepages,
//! default language, file sharing on open, local time, file-manager reveal,
//! shell spawn, path comparison, file association — is funneled here into a
//! platform-agnostic function signature. The `win32` / `posix` implementation
//! modules are compiled one-per-target, and callers never see a branch.
//!
//! Division of labor with the sibling `winshell` crate: that crate owns
//! **the window and the Blitz shell** (drag/resize/popups/context menus,
//! needs winit type identity), while this crate owns **all OS integration
//! outside the window**, with zero rendering dependencies — unit tests can
//! run in full on any platform (implementations return "unsupported"
//! semantics; see each function's docs).
//!
//! Principle for non-Windows implementations: **equivalent when possible,
//! honest when not** — return `None` / `Err(Unsupported)` / an empty list,
//! never fake success. Callers route through their own degradation paths
//! (hide the UI entry, show a status-bar hint), which is the same handling
//! code as a real failure on Windows.

pub mod assoc;

// "One editor per user session": the named-mutex / flock guard the entry point
// takes before creating a window (see the module docs for why two instances
// cannot share the per-user state).
pub mod instance;

// The OS clipboard is a process-wide shared resource; `lock()` serializes
// every clipboard operation in every crate (see the module docs — concurrent
// open/close corrupts the heap on Windows).
pub mod clipboard;

#[cfg(not(windows))]
mod posix;
#[cfg(windows)]
mod win32;

#[cfg(not(windows))]
use posix as os;
#[cfg(windows)]
use win32 as os;

// ---------------------------------------------------------------------------
// Language and encoding
// ---------------------------------------------------------------------------

/// Best-guess BCP-47 tag for the system UI language (used for the first-run
/// default language; `None` when unavailable).
///
/// Windows reads the primary language ID from `GetUserDefaultUILanguage` and
/// maps only the two languages the app actually supports (`zh` → `zh-CN`,
/// `en` → `en-US`), everything else is `None` — between "can't guess" and
/// "guess wrong", always pick the former and let the caller fall back to the
/// default. POSIX applies the message-locale precedence (`LC_ALL`, then
/// `LC_MESSAGES`, then `LANG`, treating an empty value as unset) and returns
/// the tag verbatim, leaving the verdict to the caller's tag parser.
pub fn locale_tag() -> Option<String> {
    os::locale_tag()
}

/// The system ANSI codepage (fallback basis for legacy-encoding detection).
///
/// On Windows this is the real value of `GetACP()` (936=GBK, 950=Big5,
/// 932=SJIS…); other platforms have no such concept and return 65001
/// (UTF-8) — "no ANSI codepage" is expressed as "all text is UTF-8", so
/// encoding detection skips legacy-encoding guessing.
pub fn legacy_ansi_codepage() -> u32 {
    os::legacy_ansi_codepage()
}

// ---------------------------------------------------------------------------
// Files
// ---------------------------------------------------------------------------

/// Open a file for memory-mapping.
///
/// On Windows the handle shares read+delete but NOT write: the mapping stays
/// alive for the whole document lifetime (also cloned into background search
/// snapshots), and memmap2's safety contract forbids the file from shrinking
/// under the map — an external truncation would be UB (access violation).
/// Excluding write share makes concurrent writers fail with a sharing
/// violation, which is exactly how classic editors protect mapped files.
///
/// FILE_SHARE_DELETE is required for **saving back to the same path**:
/// saving does write-temp + rename, and `MOVEFILE_REPLACE_EXISTING` needs to
/// open the target with DELETE access. With a read-only share mode every
/// Ctrl+S on a >=64 MB mmap document failed with ERROR_SHARING_VIOLATION
/// (measured 2026-10: READ-only rename failed with os error 5, while
/// READ|DELETE succeeded and the mapping stayed readable — the replaced old
/// file object is kept alive by the section reference, and piece-table
/// offsets are unaffected).
///
/// Other platforms have no sharing bits: a plain open is the whole story.
pub fn open_semi_shared(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    os::open_semi_shared(path)
}

// ---------------------------------------------------------------------------
// Files
// ---------------------------------------------------------------------------

/// Create `tmp` as the stand-in for `target`, carrying `target`'s permissions.
/// Call [`finalize_replacement_file`] after writing and before the rename.
///
/// An atomic save writes a temp file next to the target and renames it over
/// it, so the *new* file object — not the old one — becomes the document. A
/// freshly created temp file gets whatever the platform hands out by default
/// (process umask on POSIX, the directory's inheritable ACEs on Windows), so
/// without this step the first save silently rewrote the file's permissions:
/// a `0600` key, `.env` or notes file came back as `0644` and readable by
/// every account on the machine.
///
/// The pair is split because the caller has to write the file in between, and
/// a read-only target's exact mode (`0444`) would make its own temp file
/// unwritable. During the write the temp file may therefore carry *owner*
/// write on top of the target's mode — nothing is exposed wider than the
/// target, and the exact bits land in [`finalize_replacement_file`].
///
/// `target` not existing yet (a new document) creates `tmp` with the default
/// permissions and makes the finalize step a no-op — there is nothing to
/// preserve.
///
/// POSIX carries the mode, set-id bits included. Windows copies the DACL, so a
/// per-file ACL survives; it does not copy the read-only attribute, because
/// renaming over a read-only target fails anyway and a read-only temp file
/// could not be deleted by the caller's failure path.
pub fn create_replacement_file(
    target: &std::path::Path,
    tmp: &std::path::Path,
) -> std::io::Result<()> {
    os::create_replacement_file(target, tmp)
}

/// Give `tmp` the exact permissions of the `target` it is about to replace —
/// the second half of [`create_replacement_file`], called after the content is
/// written and before the rename.
///
/// Failing here must fail the save: the rename is the point of no return, and
/// publishing the new content with the wrong permissions is the bug this pair
/// exists to prevent.
pub fn finalize_replacement_file(
    target: &std::path::Path,
    tmp: &std::path::Path,
) -> std::io::Result<()> {
    os::finalize_replacement_file(target, tmp)
}

/// Two paths that denote the same file, by the platform's own rules.
///
/// Windows file systems are case-insensitive and treat `\` and `/` as
/// synonyms, so a character-by-character comparison would judge
/// `C:\A\B.txt` and `c:/a/b.txt` to be two different files (external-change
/// detection would break); POSIX is exactly the opposite — case means two
/// different files. The comparison rule itself is a platform difference.
pub fn paths_equal(a: &std::path::Path, b: &std::path::Path) -> bool {
    os::paths_equal(a, b)
}

// ---------------------------------------------------------------------------
// Time
// ---------------------------------------------------------------------------

/// Civil date/time from a Unix timestamp, in **local** time:
/// `(year, month, day, hour, minute, second)`.
///
/// Windows goes through `GetLocalTime`; POSIX goes through the C library's
/// reentrant `localtime_r`, which honors `TZ` / `/etc/localtime`. Only a
/// timestamp the platform's `time_t` cannot represent falls back to UTC, so
/// "insert date and time" and crash-log timestamps follow the user's clock
/// everywhere.
pub fn local_civil(unix_secs: i64) -> (i32, u32, u32, u32, u32, u32) {
    os::local_civil(unix_secs)
}

// ---------------------------------------------------------------------------
// Processes and shell
// ---------------------------------------------------------------------------

/// Reveal `path` in the platform file manager.
///
/// `select = true` selects the file (Windows: `explorer /select`; macOS:
/// `open -R`; other platforms have no "select" semantic and degrade to
/// opening the containing directory); `false` opens the directory itself.
/// Failures are returned as `io::Error` (process failed to start, etc.);
/// the caller decides the user-facing message.
pub fn reveal_in_file_manager(path: &std::path::Path, select: bool) -> std::io::Result<()> {
    os::reveal_in_file_manager(path, select)
}

/// Open `url` in the platform's default browser.
pub fn open_url(url: &str) -> std::io::Result<()> {
    os::open_url(url)
}

/// Hand a command line to the platform shell (`cmd /C` vs `sh -c`), optionally
/// rooted at `cwd` (ignored when the directory doesn't exist, same as the old
/// behavior — the working directory is not part of the command, and a
/// directory that can't be entered should not block the command itself).
pub fn spawn_shell(command: &str, cwd: Option<&std::path::Path>) -> std::io::Result<()> {
    os::spawn_shell(command, cwd)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `local_civil` must convert the **given** timestamp, not merely report
    /// the current wall clock (the Windows implementation ignored its
    /// argument).
    #[test]
    fn local_civil_uses_the_given_timestamp() {
        let (year, ..) = local_civil(0);
        assert!(
            year == 1969 || year == 1970,
            "the Unix epoch is 1969/1970 in every time zone, got {year}"
        );
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let (year_now, ..) = local_civil(now);
        assert!(
            year_now >= 2024,
            "now must map to the current era: {year_now}"
        );
    }

    fn tmp_path(name: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("infeditor-platform-{}-{name}", std::process::id()));
        p
    }

    /// The temp file an atomic save renames over the document is a *new* file,
    /// so it must be created with the target's permissions — otherwise the
    /// first save silently widened them. A private file (0600) coming back as
    /// 0644 is a real disclosure, and an executable script losing +x breaks
    /// the file for its user.
    #[test]
    #[cfg(unix)]
    fn replacement_file_inherits_the_target_mode() {
        use std::os::unix::fs::PermissionsExt;
        for mode in [0o600u32, 0o640, 0o755, 0o444] {
            let target = tmp_path(&format!("mode-{mode:o}.txt"));
            let tmp = tmp_path(&format!("mode-{mode:o}.tmp"));
            std::fs::write(&target, b"old").unwrap();
            std::fs::set_permissions(&target, std::fs::Permissions::from_mode(mode)).unwrap();

            create_replacement_file(&target, &tmp).unwrap();
            // While the writer is running the temp file must be writable, and
            // must not be readable wider than the target.
            let during = std::fs::metadata(&tmp).unwrap().permissions().mode() & 0o7777;
            assert_eq!(
                during & !0o200,
                mode & !0o200,
                "temp file exposed {during:o} while the target was {mode:o}"
            );
            finalize_replacement_file(&target, &tmp).unwrap();

            let got = std::fs::metadata(&tmp).unwrap().permissions().mode() & 0o7777;
            assert_eq!(
                got, mode,
                "temp file got {got:o}, target had {mode:o} (saving would rewrite it)"
            );
            let _ = std::fs::remove_file(&target);
            let _ = std::fs::remove_file(&tmp);
        }
    }

    /// A read-only target must still be replaceable: only the *rename* is
    /// subject to the target's mode on POSIX, and the temp file is ours until
    /// then. The final file keeps `0444`.
    #[test]
    #[cfg(unix)]
    fn a_read_only_target_does_not_block_the_replacement() {
        use std::io::Write as _;
        use std::os::unix::fs::PermissionsExt;
        let target = tmp_path("ro-target.txt");
        let tmp = tmp_path("ro-target.tmp");
        std::fs::write(&target, b"old").unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o444)).unwrap();

        create_replacement_file(&target, &tmp).unwrap();
        {
            let mut f = std::fs::OpenOptions::new().write(true).open(&tmp).unwrap();
            f.write_all(b"new").unwrap();
        }
        finalize_replacement_file(&target, &tmp).unwrap();
        std::fs::rename(&tmp, &target).unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"new");
        let mode = std::fs::metadata(&target).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o444, "the read-only mode must survive the save");
        let _ = std::fs::remove_file(&target);
    }

    /// A brand-new document (no target yet) has no permissions to inherit: it
    /// gets the platform default, and both halves must succeed as a no-op.
    #[test]
    fn replacement_file_for_a_new_document_uses_the_default() {
        let target = tmp_path("brand-new.txt");
        let tmp = tmp_path("brand-new.tmp");
        let _ = std::fs::remove_file(&target);
        let _ = std::fs::remove_file(&tmp);

        create_replacement_file(&target, &tmp).unwrap();
        finalize_replacement_file(&target, &tmp).unwrap();
        assert!(tmp.exists(), "the temp file must exist for the writer");
        assert!(!target.exists(), "nothing may be created at the target yet");
        let _ = std::fs::remove_file(&tmp);
    }
}
