//! The editor right-click menu, drawn by the **operating system** rather than
//! inside the page.
//!
//! An overlay inside the document is clipped by the window, but this
//! context menu is a real `WS_POPUP` window: it can extend past the application
//! frame and even flips to the other side of the cursor near a screen edge
//! (which a compositor-free absolute layout can never do). The only way to match
//! that is `TrackPopupMenu`, so this module turns the same [`Item`] tree the menu
//! bar renders into an `HMENU` and runs it.
//!
//! **Where it is called from matters.** `TrackPopupMenu` runs its own modal
//! message loop, so it must not be invoked while a borrow of the Blitz document
//! is alive. The menu is therefore raised from the borrow-free winit
//! `use_window_event` hook in [`crate::ui::editor`], which runs *before*
//! `BlitzApplication` sees the same event. Nested messages produced by the modal
//! loop are *buffered* by `winit` (`EventLoopRunner::should_buffer`) and
//! dispatched once the outer handler returns, so the re-entrancy detector never
//! fires.

use crate::ui::actions::{Cmd, Item};

/// Show `items` at the cursor and return the command the user picked.
///
/// `client_x` / `client_y` are physical pixels relative to the window's client
/// area; they are converted with `ClientToScreen` so the popup lands exactly
/// under the pointer.
///
/// `checked` supplies the check marks (Always-on-Top in the title-bar menu, the
/// toggles in View / Encoding / Language). The parameter mirrors what the
/// in-page menu bar renders, so the same [`Item`] tree draws a check in either
/// host — `Item::Cmd` has no room for state, which is why it is passed alongside.
#[cfg(windows)]
pub fn show(
    items: &[Item<'_>],
    hwnd: isize,
    client_x: i32,
    client_y: i32,
    dark: bool,
    checked: impl Fn(&Cmd) -> bool,
) -> Option<Cmd> {
    use windows_sys::Win32::Foundation::{HWND, POINT};
    use windows_sys::Win32::Graphics::Gdi::ClientToScreen;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        DestroyMenu, PostMessageW, SetForegroundWindow, TrackPopupMenu, TPM_LEFTALIGN,
        TPM_NONOTIFY, TPM_RETURNCMD, TPM_RIGHTBUTTON, TPM_TOPALIGN, WM_NULL,
    };

    let hwnd: HWND = hwnd as HWND;
    if hwnd.is_null() {
        return None;
    }

    unsafe {
        imp::apply_menu_theme(hwnd, dark);

        let mut cmds: Vec<Cmd> = Vec::with_capacity(64);
        let hmenu = imp::build_menu(items, &mut cmds, &checked);
        if hmenu.is_null() {
            return None;
        }

        let mut pt = POINT {
            x: client_x,
            y: client_y,
        };
        if ClientToScreen(hwnd, &mut pt) == 0 {
            DestroyMenu(hmenu);
            return None;
        }

        // A popup menu has to be owned by the foreground window, otherwise it
        // will not be dismissed when the user clicks outside of it.
        SetForegroundWindow(hwnd);
        let flags = TPM_LEFTALIGN | TPM_TOPALIGN | TPM_RIGHTBUTTON | TPM_RETURNCMD | TPM_NONOTIFY;
        let id = TrackPopupMenu(hmenu, flags, pt.x, pt.y, 0, hwnd, std::ptr::null());
        DestroyMenu(hmenu);
        // Part of the documented idiom: without this a click that dismissed the
        // menu can be swallowed by the popup's still-active message loop.
        PostMessageW(hwnd, WM_NULL, 0, 0);

        if id > 0 && (id as usize) <= cmds.len() {
            Some(cmds[id as usize - 1])
        } else {
            None
        }
    }
}

/// Non-Windows placeholder: there is no cross-platform popup that can leave the
/// window, so the caller keeps whatever in-page fallback it has.
#[cfg(not(windows))]
pub fn show(
    _items: &[Item<'_>],
    _hwnd: isize,
    _client_x: i32,
    _client_y: i32,
    _dark: bool,
    _checked: impl Fn(&Cmd) -> bool,
) -> Option<Cmd> {
    None
}

// ---------------------------------------------------------------------------
// Windows implementation
// ---------------------------------------------------------------------------

#[cfg(windows)]
mod imp {
    use super::*;
    use windows_sys::Win32::Foundation::HWND;
    use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleA, GetProcAddress};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        AppendMenuW, CreatePopupMenu, HMENU, MF_POPUP, MF_SEPARATOR, MF_STRING,
    };

    /// NUL-terminated UTF-16, the encoding every `...W` Win32 string API wants.
    pub fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// Recursively turn an [`Item`] slice into an `HMENU`.
    ///
    /// Every command gets the next id (1-based) and pushes itself onto `cmds`,
    /// so the id is simply an index into that vector — no hash map, and the
    /// recursion order is the only thing that has to stay in step with the id
    /// counter. [`tests::ids_map_back_to_commands_in_order`] pins that down.
    ///
    /// `checked` adds `MF_CHECKED`, which is how a native menu shows the state of
    /// a toggle (Always-on-Top) without the label having to carry it.
    ///
    /// When none of the rows this popup layer emits are checkable commands,
    /// `MNS_NOCHECK` tells the system **not to reserve a check column on the
    /// left**: themed menus reserve a check/icon column for every popup layer
    /// by default, and pure-command submenus (Sort / Mark styling…) would show
    /// a dead blank strip on the left. The criterion is the same convention as
    /// the menu bar's DOM menu — "has any checkable row" ([`is_toggle`]), not
    /// "is anything currently checked": the check state changes with document
    /// state, so the column width must not jump with it. Sub-layers' checks
    /// belong to the sub-layer's own column and do not affect this one.
    pub unsafe fn build_menu(
        items: &[Item<'_>],
        cmds: &mut Vec<Cmd>,
        checked: &impl Fn(&Cmd) -> bool,
    ) -> HMENU {
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            SetMenuInfo, MENUINFO, MF_CHECKED, MIM_STYLE, MNS_NOCHECK,
        };
        let hmenu = CreatePopupMenu();
        let any_checkable = items
            .iter()
            .any(|it| matches!(it, Item::Cmd { cmd, .. } if crate::ui::actions::is_toggle(*cmd)));
        if !any_checkable {
            let mut mi: MENUINFO = std::mem::zeroed();
            mi.cbSize = std::mem::size_of::<MENUINFO>() as u32;
            mi.fMask = MIM_STYLE;
            mi.dwStyle = MNS_NOCHECK;
            SetMenuInfo(hmenu, &mi);
        }
        for it in items {
            match it {
                Item::Sep => {
                    AppendMenuW(hmenu, MF_SEPARATOR, 0, std::ptr::null());
                }
                Item::Cmd { label, accel, cmd } => {
                    // Windows right-aligns whatever follows a tab.
                    let text = if accel.is_empty() {
                        label.to_string()
                    } else {
                        format!("{label}\t{accel}")
                    };
                    let w = wide(&text);
                    let id = cmds.len() + 1;
                    cmds.push(*cmd);
                    let flags = if checked(cmd) {
                        MF_STRING | MF_CHECKED
                    } else {
                        MF_STRING
                    };
                    AppendMenuW(hmenu, flags, id, w.as_ptr());
                }
                Item::Sub { label, items } => {
                    let sub = build_menu(items, cmds, checked);
                    let w = wide(label);
                    AppendMenuW(hmenu, MF_STRING | MF_POPUP, sub as usize, w.as_ptr());
                }
            }
        }
        hmenu
    }

    type AllowDarkModeForWindowFn = unsafe extern "system" fn(HWND, i32) -> i32;
    type SetPreferredAppModeFn = unsafe extern "system" fn(i32) -> i32;
    type RefreshPolicyFn = unsafe extern "system" fn();

    /// Resolve an anonymous `uxtheme.dll` export by ordinal.
    unsafe fn uxtheme_fn<T>(ordinal: u16) -> Option<T> {
        let name = b"uxtheme.dll\0";
        let h = GetModuleHandleA(name.as_ptr());
        if h.is_null() {
            return None;
        }
        let p = GetProcAddress(h, ordinal as usize as *const u8)?;
        Some(std::mem::transmute_copy(&p))
    }

    /// Make the system menu follow the editor theme.
    ///
    /// These are undocumented exports, but they are stable since Windows 10
    /// 1903 and are what every dark-mode Win32 application uses. Any of the
    /// lookups failing just leaves the default (light) system menu in place.
    pub unsafe fn apply_menu_theme(hwnd: HWND, dark: bool) {
        if let Some(allow) = uxtheme_fn::<AllowDarkModeForWindowFn>(133) {
            allow(hwnd, if dark { 1 } else { 0 });
        }
        if let Some(set) = uxtheme_fn::<SetPreferredAppModeFn>(135) {
            // 2 = ForceDark, 3 = ForceLight, 0 = Default, 1 = AllowDark
            set(if dark { 2 } else { 3 });
        }
        if let Some(refresh) = uxtheme_fn::<RefreshPolicyFn>(104) {
            refresh();
        }
    }
}

#[cfg(windows)]
#[cfg(test)]
mod tests {
    use super::imp::build_menu;
    use super::*;
    use crate::ui::actions::editor_context_items;
    use windows_sys::Win32::UI::WindowsAndMessaging::DestroyMenu;

    /// The id → command mapping is only correct if the recursion order of
    /// `build_menu` matches the order the ids were handed out in. Build the
    /// menu head-less (no window needed) and check every id resolves.
    #[test]
    fn ids_map_back_to_commands_in_order() {
        let items = editor_context_items(crate::ui::Locale::DEFAULT);
        let mut flat = Vec::new();
        collect(&items, &mut flat);
        let mut cmds = Vec::new();
        let hmenu = unsafe { build_menu(&items, &mut cmds, &|_| false) };
        assert!(!hmenu.is_null());
        assert_eq!(cmds, flat, "id order drifted from the recursive walk");
        unsafe {
            DestroyMenu(hmenu);
        }
    }

    fn collect(items: &[Item<'_>], out: &mut Vec<Cmd>) {
        for it in items {
            match it {
                Item::Cmd { cmd, .. } => out.push(*cmd),
                Item::Sub { items, .. } => collect(items, out),
                Item::Sep => {}
            }
        }
    }
}
