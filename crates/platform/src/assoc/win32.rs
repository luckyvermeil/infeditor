//! Windows implementation of file association.
//!
//! # Where to write: HKCU, not HKCR/HKLM
//!
//! Writing `HKCR` directly (= `HKLM\Software\Classes`, machine-wide) requires
//! a UAC-elevated restart as administrator — too steep a price for an editor
//! opened by double-click from explorer, and the failure mode is ugly (the
//! whole page grays out).
//!
//! Everything here writes the **per-user Classes Root**
//! (`HKCU\Software\Classes`, per-user file association supported since Win 7):
//!
//! * No admin needed; the page is always clickable;
//! * Uninstall = unregister + delete keys, leaving no garbage that needs
//!   privileges to clean;
//! * The cost is that it only affects the current user, which on a
//!   single-user machine equals the whole machine.
//!
//! # Association is not "stealing the default"
//!
//! Writing only `OpenWithProgids` (adding this app to the "Open with" list)
//! is the safe approach that does **not** touch the existing default program;
//! only `take_default` changes the `<ext>` default, and it archives the old
//! value in `Software\InfEditor\PrevDefault` and restores it on unregister —
//! avoiding irreversible damage like ".csv becoming an unknown type after
//! the association is canceled".

use std::ffi::OsString;
use std::io;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::Path;
use std::ptr;

use windows_sys::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyW, RegDeleteKeyValueW, RegDeleteTreeW, RegDeleteValueW, RegOpenKeyExW,
    RegQueryValueExW, RegSetValueExW, HKEY, HKEY_CURRENT_USER, KEY_READ, KEY_WRITE, REG_NONE,
    REG_SZ,
};
use windows_sys::Win32::UI::Shell::{
    SHChangeNotify, SHCNE_ASSOCCHANGED, SHCNF_FLUSH, SHCNF_IDLIST,
};

use super::{CTX_VERB, PROG_ID};

pub fn supported() -> bool {
    true
}

/// UTF-16 NUL-terminated string (Win32's unspoken rule: all name parameters
/// take this shape).
fn wide(s: &str) -> Vec<u16> {
    std::ffi::OsStr::new(s)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

fn os_err(rc: u32) -> io::Error {
    io::Error::from_raw_os_error(rc as i32)
}

/// An open key; `Drop` handles `RegCloseKey`, so this module never closes a
/// handle explicitly.
struct Key(HKEY);

impl Key {
    /// Create (or open) a key, generating intermediate paths, writable. Path
    /// is relative to HKCU.
    ///
    /// `RegCreateKeyW` instead of `RegCreateKeyExW`: the latter needs a
    /// `SECURITY_ATTRIBUTES` parameter, which windows-sys puts behind the
    /// `Win32_Security` feature — and we don't need a separate security
    /// descriptor at all.
    fn create(path: &str) -> io::Result<Self> {
        let p = wide(path);
        let mut hk: HKEY = ptr::null_mut();
        let rc = unsafe { RegCreateKeyW(HKEY_CURRENT_USER, p.as_ptr(), &mut hk) };
        if rc == 0 {
            Ok(Key(hk))
        } else {
            Err(os_err(rc))
        }
    }

    /// Open an existing key; missing or any other failure is `None` ("no
    /// such key" and "error" are the same thing to the caller: the key
    /// doesn't exist, treat it as absent).
    fn try_open(path: &str, write: bool) -> Option<Self> {
        let p = wide(path);
        let mut hk: HKEY = ptr::null_mut();
        // Write handles must also carry `KEY_READ`: `KEY_WRITE` does not
        // include `KEY_QUERY_VALUE`, so requesting only it makes the same
        // handle unable to **read** the default value (unregister must first
        // read "is the current default us" to decide whether to restore).
        let access = KEY_READ | if write { KEY_WRITE } else { 0 };
        let rc = unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, p.as_ptr(), 0, access, &mut hk) };
        if rc == 0 {
            Some(Key(hk))
        } else {
            None
        }
    }

    fn exists(path: &str) -> bool {
        Self::try_open(path, false).is_some()
    }

    /// `None` as name = the key's default value (the entry that says "who
    /// owns this extension").
    fn set_sz(&self, name: Option<&str>, val: &str) -> io::Result<()> {
        let n = wide(name.unwrap_or(""));
        let v = wide(val);
        let rc = unsafe {
            RegSetValueExW(
                self.0,
                if name.is_some() {
                    n.as_ptr()
                } else {
                    ptr::null()
                },
                0,
                REG_SZ,
                v.as_ptr().cast(),
                v.len() as u32 * 2,
            )
        };
        if rc == 0 {
            Ok(())
        } else {
            Err(os_err(rc))
        }
    }

    /// The standard way to write `OpenWithProgids`: the value name is the
    /// ProgID, **data empty, type REG_NONE**. Windows only cares that "the
    /// name exists".
    fn set_none(&self, name: &str) -> io::Result<()> {
        let n = wide(name);
        let rc = unsafe { RegSetValueExW(self.0, n.as_ptr(), 0, REG_NONE, ptr::null(), 0) };
        if rc == 0 {
            Ok(())
        } else {
            Err(os_err(rc))
        }
    }

    /// Whether a value exists (existence only, content not read).
    ///
    /// This is the only correct question to ask of `OpenWithProgids`: values
    /// there are **empty-data** (see [`Self::set_none`]); asking "can I read
    /// a string" will always answer "no".
    fn has_value(&self, name: &str) -> bool {
        let n = wide(name);
        let mut size: u32 = 0;
        let rc = unsafe {
            RegQueryValueExW(
                self.0,
                n.as_ptr(),
                ptr::null(),
                ptr::null_mut(),
                ptr::null_mut(),
                &mut size,
            )
        };
        rc == 0
    }

    fn get_sz(&self, name: Option<&str>) -> Option<String> {
        let n = wide(name.unwrap_or(""));
        let mut size: u32 = 0;
        let mut rc = unsafe {
            RegQueryValueExW(
                self.0,
                if name.is_some() {
                    n.as_ptr()
                } else {
                    ptr::null()
                },
                ptr::null(),
                ptr::null_mut(),
                ptr::null_mut(),
                &mut size,
            )
        };
        if rc != 0 {
            return None;
        }
        // A present but empty string (`size == 0`) still counts as "has a
        // value"; return the empty string.
        if size == 0 {
            return Some(String::new());
        }
        let mut buf = vec![0u8; size as usize];
        rc = unsafe {
            RegQueryValueExW(
                self.0,
                if name.is_some() {
                    n.as_ptr()
                } else {
                    ptr::null()
                },
                ptr::null(),
                ptr::null_mut(),
                buf.as_mut_ptr(),
                &mut size,
            )
        };
        if rc != 0 {
            return None;
        }
        let units = (size as usize / 2).min(buf.len() / 2);
        let chars = unsafe { std::slice::from_raw_parts(buf.as_ptr().cast::<u16>(), units) };
        let s = OsString::from_wide(chars);
        let s = s.to_string_lossy().trim_end_matches('\0').to_string();
        Some(s)
    }

    fn del_value(&self, name: Option<&str>) -> io::Result<()> {
        let n = wide(name.unwrap_or(""));
        let rc = unsafe {
            RegDeleteValueW(
                self.0,
                if name.is_some() {
                    n.as_ptr()
                } else {
                    ptr::null()
                },
            )
        };
        if rc == 0 {
            Ok(())
        } else {
            Err(os_err(rc))
        }
    }
}

impl Drop for Key {
    fn drop(&mut self) {
        unsafe { RegCloseKey(self.0) };
    }
}

fn delete_tree(path: &str) -> io::Result<()> {
    let p = wide(path);
    let rc = unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, p.as_ptr()) };
    if rc == 0 {
        Ok(())
    } else {
        Err(os_err(rc))
    }
}

fn delete_value(path: &str, name: &str) -> io::Result<()> {
    let p = wide(path);
    let n = wide(name);
    let rc = unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, p.as_ptr(), n.as_ptr()) };
    if rc == 0 {
        Ok(())
    } else {
        Err(os_err(rc))
    }
}

/// Make explorer re-read associations immediately (otherwise it only takes
/// effect at next logon).
pub fn notify_shell() {
    unsafe {
        SHChangeNotify(
            SHCNE_ASSOCCHANGED as i32,
            SHCNF_IDLIST | SHCNF_FLUSH,
            ptr::null(),
            ptr::null(),
        )
    };
}

/// The classes key path for an extension, e.g. `.csv` → `Software\Classes\.csv`.
fn ext_key(ext: &str) -> String {
    format!("Software\\Classes\\{ext}")
}

/// Key remembering the original default values. Unregister relies on it to
/// hand extensions back.
const PREV_DEFAULT: &str = "Software\\InfEditor\\PrevDefault";

pub fn ensure_app_registered(
    exe: &Path,
    app_name: &str,
    doc_name: &str,
    app_desc: &str,
) -> io::Result<()> {
    let exe_s = exe.to_string_lossy().into_owned();
    let cmd = super::open_command(&exe_s);
    let icon = format!("\"{exe_s}\",0");
    let file_name = exe
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "infeditor.exe".to_string());
    let app_root = format!("Software\\Classes\\Applications\\{file_name}");

    // ProgID: both OpenWithProgids and the default association point at it.
    let progid = Key::create(&format!(
        "Software\\Classes\\{PROG_ID}\\shell\\open\\command"
    ))?;
    progid.set_sz(None, &cmd)?;
    let pid_root = Key::create(&format!("Software\\Classes\\{PROG_ID}"))?;
    pid_root.set_sz(Some("FriendlyTypeName"), doc_name)?;
    pid_root.set_sz(Some("DefaultIcon"), &icon)?;

    // Applications\<exe>: this is the source for the explorer
    // "Open with → Choose another app" list; `Capabilities` +
    // `RegisteredApplications` make it appear in Windows Settings
    // "Default apps".
    Key::create(&format!("{app_root}\\shell\\open\\command"))?.set_sz(None, &cmd)?;
    let app = Key::create(&app_root)?;
    app.set_sz(Some("FriendlyAppName"), app_name)?;
    app.set_sz(Some("DefaultIcon"), &icon)?;

    let caps = Key::create("Software\\InfEditor\\Capabilities")?;
    caps.set_sz(Some("ApplicationName"), app_name)?;
    caps.set_sz(Some("ApplicationDescription"), app_desc)?;
    Key::create("Software\\InfEditor")?
        .set_sz(Some("Capabilities"), "Software\\InfEditor\\Capabilities")?;
    Key::create(&format!("{app_root}\\Capabilities"))?.set_sz(Some("ApplicationName"), app_name)?;
    // The Capabilities key alone is inert: Windows Settings → Default apps
    // enumerates the applications listed under `RegisteredApplications`
    // (value name = display name, data = the Capabilities key path). The
    // comment used to claim this step existed while the registry never got it,
    // so the app never appeared in the "choose by app" list.
    Key::create("Software\\RegisteredApplications")?
        .set_sz(Some("InfEditor"), "Software\\InfEditor\\Capabilities")?;
    Ok(())
}

/// Update the capability manifest declared to Windows
/// (`Capabilities\FileAssociations`): by default it lists the types this app
/// claims it can open; "Settings → Default apps → choose by app" reads it.
fn set_capability(ext: &str, on: bool) -> io::Result<()> {
    let file_name = std::env::current_exe()
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .unwrap_or_else(|| "infeditor.exe".to_string());
    for base in [
        "Software\\InfEditor\\Capabilities\\FileAssociations".to_string(),
        format!("Software\\Classes\\Applications\\{file_name}\\Capabilities\\FileAssociations"),
    ] {
        if on {
            Key::create(&base)?.set_sz(Some(ext), PROG_ID)?;
        } else {
            let _ = delete_value(&base, ext);
        }
    }
    Ok(())
}

fn ext_is_registered(ext: &str) -> bool {
    Key::try_open(&format!("{}\\OpenWithProgids", ext_key(ext)), false)
        .map(|k| k.has_value(PROG_ID))
        .unwrap_or(false)
}

pub fn register_ext(ext: &str, take_default: bool) -> io::Result<bool> {
    let ow_path = format!("{}\\OpenWithProgids", ext_key(ext));
    let already = ext_is_registered(ext);
    let was_default = Key::try_open(&ext_key(ext), false)
        .and_then(|k| k.get_sz(None))
        .map(|v| v == PROG_ID)
        .unwrap_or(false);

    Key::create(&ow_path)?.set_none(PROG_ID)?;
    set_capability(ext, true)?;

    if take_default && !was_default {
        // Archive the old value before taking the default; the "restore" at
        // unregister time depends entirely on this record.
        let prev = Key::create(PREV_DEFAULT)?;
        if prev.get_sz(Some(ext)).is_none() {
            if let Some(old) = Key::try_open(&ext_key(ext), false).and_then(|k| k.get_sz(None)) {
                prev.set_sz(Some(ext), &old)?;
            }
        }
        Key::create(&ext_key(ext))?.set_sz(None, PROG_ID)?;
    }
    Ok(!already || (take_default && !was_default))
}

pub fn unregister_ext(ext: &str) -> io::Result<bool> {
    let ow_path = format!("{}\\OpenWithProgids", ext_key(ext));
    let was = ext_is_registered(ext);
    let _ = delete_value(&ow_path, PROG_ID);
    set_capability(ext, false)?;

    if let Some(k) = Key::try_open(&ext_key(ext), true) {
        if k.get_sz(None).as_deref() == Some(PROG_ID) {
            let prev = Key::try_open(PREV_DEFAULT, false).and_then(|p| p.get_sz(Some(ext)));
            match prev {
                Some(old) => k.set_sz(None, &old)?,
                None => k.del_value(None)?,
            }
        }
    }
    let _ = delete_value(PREV_DEFAULT, ext);
    Ok(was)
}

pub fn registered_exts(candidates: &[String]) -> Vec<String> {
    candidates
        .iter()
        .filter(|e| ext_is_registered(e))
        .cloned()
        .collect()
}

pub fn default_handler(ext: &str) -> Option<String> {
    Key::try_open(&ext_key(ext), false).and_then(|k| k.get_sz(None))
}

/// Whether the context menu is installed: check the verb key's existence
/// (`Software\Classes\*\shell\InfEditor`).
pub fn context_menu_installed() -> bool {
    Key::exists(&format!("Software\\Classes\\*\\shell\\{CTX_VERB}\\command"))
}

pub fn set_context_menu(on: bool, mui_label: &str, exe: Option<&Path>) -> io::Result<()> {
    let base = format!("Software\\Classes\\*\\shell\\{CTX_VERB}");
    if !on {
        let _ = delete_tree(&base);
        notify_shell();
        return Ok(());
    }
    let icon = match exe {
        Some(p) => format!("\"{}\",0", p.to_string_lossy()),
        None => String::from("imageres.dll,163"),
    };
    let cmd = match exe {
        Some(p) => super::open_command(&p.to_string_lossy()),
        None => super::open_command("infeditor.exe"),
    };
    Key::create(&format!("{base}\\command"))?.set_sz(None, &cmd)?;
    let k = Key::create(&base)?;
    // MUIVerb = the text actually shown on the menu; `Icon` makes it
    // recognizable at a glance even in Win11's collapsed menu.
    k.set_sz(Some("MUIVerb"), mui_label)?;
    k.set_sz(Some("Icon"), &icon)?;
    notify_shell();
    Ok(())
}
