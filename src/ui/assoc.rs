//! System file association and Explorer context menu — the **UI-side thin
//! shell**.
//!
//! # Layering
//!
//! * All registry/key operations (HKCU `Software\Classes`, ProgID, `PrevDefault`
//!   restore protocol, `SHChangeNotify`) live in [`platform::assoc`] (the single
//!   home for platform differences); display strings (type names, descriptions,
//!   context-menu wording) are resolved by this module from the current language
//!   at call time — the platform layer does not know i18n exists.
//! * This module keeps the **pure logic** (extension normalization, the
//!   supported list) and the **result → wording** mapping. Registry IO must not
//!   happen inside a render frame: prefswin ships the [`AssocCmd`] back to the
//!   main window through the command bridge, and it is executed here.
//!
//! # Placement
//!
//! The UI exposes this under **Settings → Preferences → File associations**:
//! the left list is "supported extensions" (grouped by language); opening a
//! language lists its extensions in the middle; `->` / `<-` move entries
//! between the two lists. This module is the entry point for the backend half
//! of those lists.

use std::path::PathBuf;

use crate::ui::{Fmt, Msg};

pub use platform::assoc::{open_command, PROG_ID};

/// Extensions that must appear in the list but have **no language definition**.
///
/// `.txt` has no `LangSpec` in this app (it falls to "plain text" via
/// `LangSpec::plain()`), yet it is exactly the one most worth associating —
/// so a set of common text extensions is hand-added here.
const TEXT_EXTS: &[&str] = &["txt", "text", "cfg", "conf", "log"];

/// One category of files on the left of the preferences "Associations" page:
/// `name` comes from the language table (or the text-extension set above),
/// `exts` are all extensions it supports (with leading dot, lower case).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AssocGroup {
    pub name: &'static str,
    pub exts: Vec<String>,
}

/// One UI-side action. Like the settings toggles, it goes through the
/// `prefswin` command bridge back to the main window for execution —
/// registry IO must not happen inside a render frame.
#[derive(Debug, Clone, PartialEq)]
pub enum AssocCmd {
    /// Associate a set of extensions (written to the parent key's OpenWithProgids).
    Register {
        exts: Vec<String>,
        /// Also set as the default open-with handler (changes `<ext>`'s default
        /// value; the old value is remembered so it can be restored)
        take_default: bool,
    },
    /// Un-associate a set of extensions (removed from OpenWithProgids,
    /// restoring the default program when necessary).
    Unregister { exts: Vec<String> },
    /// Explorer right-click "Open with InfEditor".
    SetContextMenu(bool),
}

// ---------------------------------------------------------------------------
// Pure logic (cross-platform, unit-test friendly)
// ---------------------------------------------------------------------------

/// Normalize into an extension form a shell can recognize:
/// `"TXT"` / `"txt"` / `".txt"` → `".txt"`.
///
/// Windows extension keys are case-insensitive; an empty string or an illegal
/// '.' result is `None`.
pub fn normalize_ext(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    let body = trimmed.strip_prefix('.').unwrap_or(trimmed);
    let body = body.trim();
    if body.is_empty() || body.contains(char::is_whitespace) || body.contains(['\\', '/', ':']) {
        return None;
    }
    Some(format!(".{}", body.to_ascii_lowercase()))
}

/// "Supported extensions": the first group is common text files, the rest map
/// one-to-one to the language table's order.
pub fn supported_groups() -> Vec<AssocGroup> {
    let mut out = Vec::new();
    out.push(AssocGroup {
        name: "Text Document",
        exts: TEXT_EXTS.iter().filter_map(|e| normalize_ext(e)).collect(),
    });
    for spec in crate::highlight::lang::all() {
        let exts: Vec<String> = spec.exts.iter().filter_map(|e| normalize_ext(e)).collect();
        if exts.is_empty() {
            continue;
        }
        out.push(AssocGroup {
            name: spec.name,
            exts,
        });
    }
    out
}

/// All supported extensions (deduplicated, list order preserved) — the
/// iteration order used when querying "which are registered", keeping the
/// right-hand list's order stable.
pub fn supported_exts() -> Vec<String> {
    let mut seen = Vec::new();
    for g in supported_groups() {
        for e in g.exts {
            if !seen.contains(&e) {
                seen.push(e);
            }
        }
    }
    seen
}

// ---------------------------------------------------------------------------
// Platform IO dispatch (key operations live in `platform::assoc`; this only
// maps results to wording)
// ---------------------------------------------------------------------------

/// Currently registered supported extensions (lower case, with dot).
pub fn registered_exts() -> Vec<String> {
    platform::assoc::registered_exts(&supported_exts())
}

/// Whether the Explorer right-click "Open with InfEditor" is registered.
pub fn context_menu_installed() -> bool {
    platform::assoc::context_menu_installed()
}

/// The default handler ProgID for an extension under the current user
/// (`None` = the user never changed it; the system default applies).
///
/// All side effects of "set as default open-with" land on this value, so it is
/// also the value "given back" on unregister — exposed so the restore logic
/// can be tested.
pub fn default_handler(ext: &str) -> Option<String> {
    platform::assoc::default_handler(ext)
}

/// Executable path used for association (`None` = unavailable; the UI should
/// report it rather than crash).
pub fn app_exe() -> Option<PathBuf> {
    std::env::current_exe().ok()
}

/// Whether this page can write associations (`false` → the UI draws the
/// toggles as disabled).
pub fn supported() -> bool {
    platform::assoc::supported()
}

/// Execute an [`AssocCmd`], returning a one-line result for the status bar.
///
/// Failure also returns a string rather than a `Result`: the only handling the
/// preferences page needs is showing the reason to the user, and a partially
/// successful run also says how many entries were changed.
pub fn apply(cmd: AssocCmd) -> Option<i18n::Message> {
    let loc = i18n::current();
    let Some(exe) = app_exe() else {
        return Some(Msg::AssocNoExe.into());
    };
    if let Err(e) = platform::assoc::ensure_app_registered(
        &exe,
        "InfEditor",
        i18n::Msg::AssocDocName.get(loc),
        i18n::Msg::AssocAppDesc.get(loc),
    ) {
        return Some(Fmt::AssocRegisterFailed { err: e.to_string() }.into());
    }
    match cmd {
        AssocCmd::Register { exts, take_default } => {
            let mut n = 0;
            let mut fails: Vec<String> = Vec::new();
            for raw in &exts {
                let Some(ext) = normalize_ext(raw) else {
                    continue;
                };
                match platform::assoc::register_ext(&ext, take_default) {
                    Ok(true) => n += 1,
                    Ok(false) => {}
                    Err(e) => fails.push(format!("{ext} ({e})")),
                }
            }
            if !fails.is_empty() {
                return Some(
                    Fmt::AssocRegisterPartialFail {
                        fails: fails.join("、"),
                    }
                    .into(),
                );
            }
            platform::assoc::notify_shell();
            if n == 0 {
                Some(Msg::AssocAlreadyRegistered.into())
            } else if take_default {
                Some(Fmt::AssocRegisteredDefault { n }.into())
            } else {
                Some(Fmt::AssocRegistered { n }.into())
            }
        }
        AssocCmd::Unregister { exts } => {
            let mut n = 0;
            let mut fails: Vec<String> = Vec::new();
            for raw in &exts {
                let Some(ext) = normalize_ext(raw) else {
                    continue;
                };
                match platform::assoc::unregister_ext(&ext) {
                    Ok(true) => n += 1,
                    Ok(false) => {}
                    Err(e) => fails.push(format!("{ext} ({e})")),
                }
            }
            if !fails.is_empty() {
                return Some(
                    Fmt::AssocUnregisterPartialFail {
                        fails: fails.join("、"),
                    }
                    .into(),
                );
            }
            platform::assoc::notify_shell();
            if n == 0 {
                Some(Msg::AssocNotAssociated.into())
            } else {
                Some(Fmt::AssocUnregistered { n }.into())
            }
        }
        AssocCmd::SetContextMenu(on) => {
            match platform::assoc::set_context_menu(
                on,
                i18n::Msg::AssocCtxMUIVerb.get(loc),
                app_exe().as_deref(),
            ) {
                Ok(()) => {
                    let m: i18n::Message = if on {
                        Msg::AssocCtxAdded.into()
                    } else {
                        Msg::AssocCtxRemoved.into()
                    };
                    Some(m)
                }
                Err(e) => Some(Fmt::AssocCtxFailed { err: e.to_string() }.into()),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The three spellings of an extension must converge to the same shape
    /// (registry comparisons are case-insensitive, but the strings we store in
    /// settings should stay uniform).
    #[test]
    fn extension_names_are_normalized() {
        assert_eq!(normalize_ext("txt").as_deref(), Some(".txt"));
        assert_eq!(normalize_ext(".TXT").as_deref(), Some(".txt"));
        assert_eq!(normalize_ext("  .CSV ").as_deref(), Some(".csv"));
        assert_eq!(normalize_ext("").as_deref(), None);
        assert_eq!(normalize_ext(".").as_deref(), None);
        assert_eq!(normalize_ext("a b").as_deref(), None);
        assert_eq!(normalize_ext("a\\b").as_deref(), None);
    }

    /// `.txt` has no language definition, but must be the first group of the
    /// associable list — it is the most-used item of the whole feature.
    #[test]
    fn the_text_group_comes_first_and_carries_txt() {
        let groups = supported_groups();
        assert_eq!(groups.first().map(|g| g.name), Some("Text Document"));
        assert!(
            groups
                .first()
                .map(|g| g.exts.iter().any(|e| e == ".txt"))
                .unwrap_or(false),
            "first group must contain .txt"
        );
    }

    /// Every language in the language table that carries extensions must appear
    /// in the supported list (Rust → .rs), otherwise "files the highlighter
    /// accepts" and "files that can be associated" silently drift apart.
    #[test]
    fn every_language_with_extensions_is_associable() {
        let groups = supported_groups();
        for spec in crate::highlight::lang::all() {
            if spec.exts.is_empty() {
                continue;
            }
            let g = groups
                .iter()
                .find(|g| g.name == spec.name)
                .unwrap_or_else(|| panic!("{} should have an assoc group", spec.name));
            for e in spec.exts {
                let ext = normalize_ext(e).unwrap();
                assert!(
                    g.exts.contains(&ext),
                    "{spec_name} is missing {ext}",
                    spec_name = spec.name
                );
            }
        }
    }

    /// Extension order is stable and duplicate-free: `registered_exts()`
    /// iterates in `supported_exts()` order, so the list order does not jump
    /// around every frame.
    #[test]
    fn supported_exts_are_unique_and_lower_case() {
        let exts = supported_exts();
        let mut seen = std::collections::HashSet::new();
        for e in &exts {
            assert!(e.starts_with('.'), "{e} must start with a leading dot");
            assert_eq!(e, &e.to_ascii_lowercase(), "{e} must be lower case");
            assert!(seen.insert(e.clone()), "{e} appears more than once");
        }
        assert!(exts.iter().any(|e| e == ".rs"));
        assert!(exts.iter().any(|e| e == ".md"));
    }

    /// The open command must quote the path: `C:\Program Files\...` without
    /// quotes would be split into two arguments and double-clicking a file
    /// would fail.
    #[test]
    fn the_open_command_quotes_the_executable() {
        assert_eq!(
            open_command("C:\\Program Files\\InfEditor\\infeditor.exe"),
            "\"C:\\Program Files\\InfEditor\\infeditor.exe\" \"%1\""
        );
    }

    /// Real-machine round trip: register → read back → unregister → read back,
    /// finally restoring the context menu too.
    ///
    /// `#[ignore]` because it **touches the local registry** (its own keys under
    /// HKCU). CI runs pure logic only; `cargo test -- --ignored` runs this one,
    /// to confirm "clicked → actually took effect".
    #[test]
    #[ignore = "writes to the local HKCU; run manually on a real machine only"]
    fn a_real_round_trip_registers_then_restores() {
        // The probe must be a **supported** extension: `registered_exts()` only
        // looks inside the supported set, so a made-up name would never read
        // back. Pick one almost nobody uses (.psv in the csv group) and do not
        // take the default handler, so nothing is left behind beyond the round
        // trip itself.
        const PROBE: &str = ".psv";
        let before = registered_exts();
        if before.iter().any(|e| e == PROBE) {
            // Self-healing when a previous run was interrupted: unregister
            // first, then verify from scratch.
            apply(AssocCmd::Unregister {
                exts: vec![PROBE.into()],
            });
        }
        assert!(
            !registered_exts().iter().any(|e| e == PROBE),
            "probe extension should be unregistered before we start"
        );

        let msg = apply(AssocCmd::Register {
            exts: vec![PROBE.into()],
            take_default: false,
        });
        let msg = msg
            .map(|m| i18n::current().tr(m).into_owned())
            .unwrap_or_default();
        assert!(
            registered_exts().iter().any(|e| e == PROBE),
            "should read back after registering: {msg}"
        );

        let msg = apply(AssocCmd::Unregister {
            exts: vec![PROBE.into()],
        });
        let msg = msg
            .map(|m| i18n::current().tr(m).into_owned())
            .unwrap_or_default();
        assert!(
            !registered_exts().iter().any(|e| e == PROBE),
            "should not read back after unregistering: {msg}"
        );

        // The "set as default open-with" path gets its own pass: registering
        // grabs the default value, and unregistering must give the original
        // one back — cancelling an association must not turn the extension
        // into an "unknown type".
        apply(AssocCmd::Register {
            exts: vec![PROBE.into()],
            take_default: true,
        });
        assert!(
            default_handler(PROBE).as_deref() == Some(PROG_ID),
            "after ticking \"set as default\", {PROBE}'s default handler should be this app"
        );
        apply(AssocCmd::Unregister {
            exts: vec![PROBE.into()],
        });
        assert!(
            default_handler(PROBE).as_deref() != Some(PROG_ID),
            "after unregistering it must no longer point at this app"
        );

        // Context menu: flipping twice must return to the starting state.
        let was = context_menu_installed();
        apply(AssocCmd::SetContextMenu(!was));
        assert_ne!(context_menu_installed(), was, "first flip should change it");
        apply(AssocCmd::SetContextMenu(was));
        assert_eq!(
            context_menu_installed(),
            was,
            "second flip should restore it"
        );
    }
}
