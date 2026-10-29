//! Windows implementation (compiled only under `cfg(windows)`; public API in [`crate`]).

use std::path::Path;

/// LANGID returned by `GetUserDefaultUILanguage`: the low 10 bits are the
/// primary language ID.
const PRIMARY_LANG_MASK: u16 = 0x3ff;
const LANG_CHINESE: u16 = 0x04;
const LANG_ENGLISH: u16 = 0x09;

pub fn locale_tag() -> Option<String> {
    // SAFETY: no parameters, no side effects, callable on any thread; the
    // return value is the LANGID itself.
    let langid = unsafe { windows_sys::Win32::Globalization::GetUserDefaultUILanguage() };
    match langid & PRIMARY_LANG_MASK {
        LANG_CHINESE => Some("zh-CN".into()),
        LANG_ENGLISH => Some("en-US".into()),
        _ => None,
    }
}

pub fn legacy_ansi_codepage() -> u32 {
    unsafe extern "system" {
        fn GetACP() -> u32;
    }
    unsafe { GetACP() }
}

pub fn open_semi_shared(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::windows::fs::OpenOptionsExt;
    const FILE_SHARE_READ: u32 = 0x0000_0001;
    const FILE_SHARE_DELETE: u32 = 0x0000_0004;
    std::fs::OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_DELETE)
        .open(path)
}

/// Windows file system: case-insensitive, `\` and `/` are synonyms —
/// normalize both before comparing.
pub fn paths_equal(a: &Path, b: &Path) -> bool {
    let norm = |p: &Path| p.to_string_lossy().replace('/', "\\").to_lowercase();
    norm(a) == norm(b)
}

/// Mirror of `GetLocalTime` (declared directly against kernel32, without
/// depending on a windows-sys feature).
#[repr(C)]
#[derive(Default, Clone, Copy)]
struct SystemTime {
    year: u16,
    month: u16,
    day_of_week: u16,
    day: u16,
    hour: u16,
    minute: u16,
    second: u16,
    milliseconds: u16,
}

/// Civil date/time from a Unix timestamp in **local** time.
///
/// Windows converts the timestamp to a `FILETIME`, then lets the OS map UTC to
/// the current time zone. The previous implementation ignored its argument and
/// always returned "now" — harmless only while every caller happened to pass
/// the current instant, and silently wrong for any other timestamp (e.g. a
/// crash log's recorded time).
pub fn local_civil(unix_secs: i64) -> (i32, u32, u32, u32, u32, u32) {
    const WINDOWS_EPOCH_OFFSET_SECS: i64 = 11_644_473_600;

    #[repr(C)]
    #[derive(Default, Clone, Copy)]
    struct FileTime {
        low: u32,
        high: u32,
    }

    unsafe extern "system" {
        fn FileTimeToSystemTime(file_time: *const FileTime, system_time: *mut SystemTime) -> i32;
        // Null `tz` = "use the current local time zone".
        fn SystemTimeToTzSpecificLocalTime(
            tz: *const core::ffi::c_void,
            utc: *const SystemTime,
            local: *mut SystemTime,
        ) -> i32;
    }

    fn now_local() -> (i32, u32, u32, u32, u32, u32) {
        unsafe extern "system" {
            fn GetLocalTime(out: *mut SystemTime);
        }
        let mut st = SystemTime::default();
        unsafe { GetLocalTime(&mut st) };
        to_parts(&st)
    }

    fn to_parts(st: &SystemTime) -> (i32, u32, u32, u32, u32, u32) {
        (
            st.year as i32,
            st.month as u32,
            st.day as u32,
            st.hour as u32,
            st.minute as u32,
            st.second as u32,
        )
    }

    let secs_1601 = unix_secs.saturating_add(WINDOWS_EPOCH_OFFSET_SECS);
    // Before 1601 (or beyond the FILETIME range) there is no representable
    // local time; falling back to "now" keeps the function total.
    let (low, high) = if secs_1601 < 0 {
        return now_local();
    } else {
        let ticks = (secs_1601 as u128) * 10_000_000;
        if ticks > u64::MAX as u128 {
            return now_local();
        }
        let ticks = ticks as u64;
        (ticks as u32, (ticks >> 32) as u32)
    };
    let ft = FileTime { low, high };
    let mut utc = SystemTime::default();
    // SAFETY: both pointers point at live, properly aligned values of the
    // documented layout; the API only writes the out parameter.
    if unsafe { FileTimeToSystemTime(&ft, &mut utc) } == 0 {
        return now_local();
    }
    let mut local = SystemTime::default();
    if unsafe { SystemTimeToTzSpecificLocalTime(std::ptr::null(), &utc, &mut local) } == 0 {
        // A time zone conversion failure still has the UTC value; report it
        // rather than inventing a different instant.
        return to_parts(&utc);
    }
    to_parts(&local)
}

pub fn reveal_in_file_manager(path: &Path, select: bool) -> std::io::Result<()> {
    let mut c = std::process::Command::new("explorer");
    if select {
        // No space is allowed between `/select` and the path (an old explorer
        // quirk), and it only accepts backslash paths.
        let win_path = path.to_string_lossy().replace('/', "\\");
        c.arg(format!("/select,{win_path}"));
    } else {
        c.arg(path);
    }
    c.spawn().map(|_| ())
}

pub fn open_url(url: &str) -> std::io::Result<()> {
    // ShellExecuteW instead of `cmd /C start "" <url>`: cmd expands shell
    // metacharacters inside the URL, so `&`, `|`, `^` truncated the link and
    // — worse — `https://x/a&calc` ran `calc`. ShellExecute takes the URL as
    // a single verbatim argument.
    unsafe extern "system" {
        fn ShellExecuteW(
            hwnd: isize,
            operation: *const u16,
            file: *const u16,
            parameters: *const u16,
            directory: *const u16,
            show: i32,
        ) -> isize;
    }
    const SW_SHOWNORMAL: i32 = 1;
    let wide = |s: &str| -> Vec<u16> { s.encode_utf16().chain(std::iter::once(0)).collect() };
    let op = wide("open");
    let file = wide(url);
    // SAFETY: all strings are NUL-terminated UTF-16 buffers alive for the
    // call; the remaining pointers/parameters are null, as the API allows.
    let rc = unsafe {
        ShellExecuteW(
            0,
            op.as_ptr(),
            file.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        )
    };
    // ShellExecuteW's documented success test: a value greater than 32.
    if rc <= 32 {
        Err(std::io::Error::other("ShellExecuteW failed"))
    } else {
        Ok(())
    }
}

/// See [`crate::create_replacement_file`].
///
/// Windows has no mode bits: the equivalent of a POSIX mode is the file's
/// discretionary ACL. A temp file created in the target's directory inherits
/// the *directory's* ACEs, which usually — but not always — match the ones the
/// target got when it was created there; a file whose ACL was customised
/// afterwards (a shared/CI checkout, a file copied from another volume) would
/// come back with the directory defaults. So the DACL is copied explicitly,
/// together with its "protected" flag, which decides whether the ACL keeps
/// following the directory.
///
/// Best effort by design: a security descriptor that cannot be read or written
/// (no `READ_DAC` / `WRITE_DAC` on either side, an ACL the caller is not
/// allowed to grant) leaves the inherited ACL in place — the same result the
/// old code produced. Failing the save over it would trade the user's edits
/// for a permission detail.
pub fn create_replacement_file(target: &Path, tmp: &Path) -> std::io::Result<()> {
    // Create first: this also truncates leftovers from a crashed save, and
    // `write_out`'s `File::create` then only reopens it. The DACL copy has no
    // writability problem to solve (unlike the POSIX mode), so there is
    // nothing left for the finalize half.
    std::fs::File::create(tmp)?;
    copy_dacl(target, tmp);
    Ok(())
}

/// See [`crate::finalize_replacement_file`]. Windows has nothing to defer: the
/// DACL is already on the temp file, and the read-only attribute is
/// deliberately not inherited (a read-only target fails the rename on its own,
/// and a read-only temp file would defeat the cleanup path).
pub fn finalize_replacement_file(_target: &Path, _tmp: &Path) -> std::io::Result<()> {
    Ok(())
}

/// Copy `from`'s DACL (and nothing else) onto `to`. Errors are swallowed; see
/// [`create_replacement_file`].
fn copy_dacl(from: &Path, to: &Path) {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::{LocalFree, ERROR_SUCCESS};
    use windows_sys::Win32::Security::Authorization::{
        GetNamedSecurityInfoW, SetNamedSecurityInfoW, SE_FILE_OBJECT,
    };
    use windows_sys::Win32::Security::{
        DACL_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION, SECURITY_DESCRIPTOR,
        SE_DACL_PROTECTED, UNPROTECTED_DACL_SECURITY_INFORMATION,
    };

    let wide = |p: &Path| -> Vec<u16> {
        p.as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    };
    let from_w = wide(from);
    let to_w = wide(to);

    let mut dacl = std::ptr::null_mut();
    let mut sd = std::ptr::null_mut();
    // SAFETY: both names are NUL-terminated buffers that outlive the call; the
    // owner/group/SACL out-parameters are unused and null, which the API
    // allows. On success the returned descriptor is allocated with LocalAlloc
    // and owned by us (freed below).
    let rc = unsafe {
        GetNamedSecurityInfoW(
            from_w.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut dacl,
            std::ptr::null_mut(),
            &mut sd,
        )
    };
    if rc != ERROR_SUCCESS || sd.is_null() {
        return;
    }

    // The returned descriptor is *self-relative*, so `Control` is readable
    // directly. `SE_DACL_PROTECTED` records whether the ACL still inherits
    // from the parent directory, and the same flag must be passed back when
    // setting it — otherwise the copy would silently re-enable inheritance and
    // the copied ACEs would be duplicated by the inherited ones.
    let protected = unsafe { (*sd.cast::<SECURITY_DESCRIPTOR>()).Control & SE_DACL_PROTECTED != 0 };
    let inheritance = if protected {
        PROTECTED_DACL_SECURITY_INFORMATION
    } else {
        UNPROTECTED_DACL_SECURITY_INFORMATION
    };
    // SAFETY: `sd` is a live descriptor from the call above; `dacl` points
    // into it, so the set happens before the free. A null DACL is meaningful
    // (a file granting everyone full access) and is passed through as-is.
    unsafe {
        SetNamedSecurityInfoW(
            to_w.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | inheritance,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            dacl,
            std::ptr::null_mut(),
        );
        let _ = LocalFree(sd);
    }
}

pub fn spawn_shell(command: &str, cwd: Option<&Path>) -> std::io::Result<()> {
    let mut c = std::process::Command::new("cmd");
    c.arg("/C").arg(command);
    if let Some(dir) = cwd {
        if dir.is_dir() {
            c.current_dir(dir);
        }
    }
    c.spawn().map(|_| ())
}
