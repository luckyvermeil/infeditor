//! Win32 implementation (compiled only under `cfg(windows)`; function
//! signatures match [`super::winit_impl`], public surface documented in
//! [`super`]).
//!
//! The window is always resolved from the [`super::WINDOW`] slot into an HWND
//! ([`super::hwnd_of_w`]), so the functions here take no HWND parameter — the
//! handle is an internal detail of this module.

use super::Window;

/// Resolve the main window into an `HWND` (0 when the window is not registered
/// or the handle is unavailable).
fn main_hwnd() -> isize {
    match super::window() {
        Some(w) => super::hwnd_of_w(w.as_ref()),
        None => 0,
    }
}

// ---------------------------------------------------------------------------
// Client geometry
// ---------------------------------------------------------------------------

pub fn client_origin_physical() -> Option<(i32, i32)> {
    use windows_sys::Win32::Foundation::POINT;
    use windows_sys::Win32::Graphics::Gdi::ClientToScreen;
    let hwnd = super::current_hwnd();
    if hwnd == 0 {
        return None;
    }
    let mut pt = POINT { x: 0, y: 0 };
    if unsafe { ClientToScreen(hwnd as _, &mut pt) } == 0 {
        return None;
    }
    Some((pt.x, pt.y))
}

pub fn client_rect_screen() -> Option<(i32, i32, i32, i32)> {
    use windows_sys::Win32::Foundation::{POINT, RECT};
    use windows_sys::Win32::Graphics::Gdi::ClientToScreen;
    use windows_sys::Win32::UI::WindowsAndMessaging::GetClientRect;
    let hwnd = super::current_hwnd();
    if hwnd == 0 {
        return None;
    }
    let mut r: RECT = unsafe { std::mem::zeroed() };
    if unsafe { GetClientRect(hwnd as _, &mut r) } == 0 {
        return None;
    }
    let mut tl = POINT {
        x: r.left,
        y: r.top,
    };
    if unsafe { ClientToScreen(hwnd as _, &mut tl) } == 0 {
        return None;
    }
    Some((tl.x, tl.y, r.right - r.left, r.bottom - r.top))
}

pub fn move_window_screen(window: &dyn Window, x: i32, y: i32) {
    let hwnd = super::hwnd_of_w(window);
    if hwnd != 0 {
        super::move_window(hwnd, x, y);
    }
}

// ---------------------------------------------------------------------------
// Drag / resize gestures
// ---------------------------------------------------------------------------

/// Start the system's own modal move/size loop for the given non-client hit
/// code (`HTCAPTION` to move, `HT*` edge/corner codes to resize).
///
/// This deliberately does **not** go through winit's `drag_window` /
/// `drag_resize_window`: both share one `dragging` re-entrancy flag that is
/// only cleared by the next `WM_EXITSIZEMOVE`. When a drag is armed while a
/// nested modal loop is still unwinding (e.g. right after the title-bar
/// `TrackPopupMenu` context menu), the flag is left set and *every* later
/// drag/resize silently no-ops — exactly the "drag stops working after the
/// context menu" bug. Posting `WM_NCLBUTTONDOWN` straight to `DefWindowProc`
/// is what a real caption (and the system menu's Move/Size) use, and it keeps
/// no state between gestures.
fn begin_nc_drag(ht: isize) {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::ReleaseCapture;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetCursorPos, PostMessageW, WM_NCLBUTTONDOWN, WM_NULL,
    };
    let Some(w) = super::window() else {
        return;
    };
    let hwnd = super::hwnd_of_w(w.as_ref());
    if hwnd == 0 {
        return;
    }
    unsafe {
        // Defensive: clear any lingering menu-mode state (the documented
        // TrackPopupMenu idiom) so the posted NC message can't be eaten.
        PostMessageW(hwnd as _, WM_NULL, 0, 0);
        // Same recipe as winit's `drag_window` (ReleaseCapture + posted
        // `WM_NCLBUTTONDOWN`, so the system loop starts after the current
        // dispatch unwinds) — minus winit's `dragging` re-entrancy flag (see
        // the function doc).
        ReleaseCapture();
        // `WM_NCLBUTTONDOWN`'s lParam is MAKELPARAM(x, y) screen
        // coordinates, not a pointer — the grab point the system modal
        // loop anchors the drag at.
        let mut pos = std::mem::zeroed();
        GetCursorPos(&mut pos);
        let lparam = ((pos.y as i32 & 0xFFFF) << 16) | (pos.x as i32 & 0xFFFF);
        PostMessageW(hwnd as _, WM_NCLBUTTONDOWN, ht as usize, lparam as isize);
    }
}

pub fn start_move() {
    // A system-menu "Move" (or a caption drag started from the API) must be
    // labelled as a move: a left-over RESIZE arming from hovering the top
    // edge would otherwise lose the release-to-maximize rule.
    crate::win::snap::arm_move();
    begin_nc_drag(super::ht::HTCAPTION);
}

pub fn start_system_move() {
    crate::win::snap::arm_move();
    begin_nc_drag(super::ht::HTCAPTION);
}

pub fn start_system_resize() {
    crate::win::snap::arm_resize();
    begin_nc_drag(super::ht::HTBOTTOMRIGHT);
}

/// Edge/corner resize from a DOM strip (see [`super::start_resize`]).
///
/// Windows normally gets here through `WM_NCHITTEST`, but the same system
/// modal loop can be entered explicitly — that is what the window menu's
/// "Size" does. Keeping the mapping here means the non-Windows UI component
/// can call one facade function on every platform.
pub fn start_resize(dir: dioxus_native::winit::window::ResizeDirection) {
    use dioxus_native::winit::window::ResizeDirection;
    let ht = match dir {
        ResizeDirection::East => super::ht::HTRIGHT,
        ResizeDirection::North => super::ht::HTTOP,
        ResizeDirection::NorthEast => super::ht::HTTOPRIGHT,
        ResizeDirection::NorthWest => super::ht::HTTOPLEFT,
        ResizeDirection::South => super::ht::HTBOTTOM,
        ResizeDirection::SouthEast => super::ht::HTBOTTOMRIGHT,
        ResizeDirection::SouthWest => super::ht::HTBOTTOMLEFT,
        ResizeDirection::West => super::ht::HTLEFT,
    };
    // Edge drag from a DOM strip is a resize, not a title-bar move: without
    // this the top band could snap/maximize on release.
    crate::win::snap::arm_resize();
    begin_nc_drag(ht);
}

// ---------------------------------------------------------------------------
// Window state
// ---------------------------------------------------------------------------

/// Common `ShowWindow` commands (in windows-sys they hang off the SW
/// constants under `Win32_UI_WindowsAndMessaging`; only the three used here
/// are picked, to avoid pulling in the whole feature surface).
mod sw {
    pub const MAXIMIZE: i32 = 3;
    pub const MINIMIZE: i32 = 6;
    pub const RESTORE: i32 = 9;
}

pub fn minimize() {
    let h = main_hwnd();
    if h != 0 {
        unsafe {
            windows_sys::Win32::UI::WindowsAndMessaging::ShowWindow(h as _, sw::MINIMIZE);
        }
    }
}

pub fn maximize() {
    let h = main_hwnd();
    if h != 0 {
        unsafe {
            windows_sys::Win32::UI::WindowsAndMessaging::ShowWindow(h as _, sw::MAXIMIZE);
        }
    }
}

pub fn restore() {
    let h = main_hwnd();
    if h != 0 {
        unsafe {
            windows_sys::Win32::UI::WindowsAndMessaging::ShowWindow(h as _, sw::RESTORE);
        }
    }
}

/// Close via `WM_CLOSE` so the app's normal close path runs (blitz raises
/// `CloseRequested`, which the window layer honours).
pub fn close() {
    use windows_sys::Win32::Foundation::{LPARAM, WPARAM};
    const WM_CLOSE: u32 = 0x0010;
    let h = main_hwnd();
    if h != 0 {
        unsafe {
            windows_sys::Win32::UI::WindowsAndMessaging::PostMessageW(
                h as _,
                WM_CLOSE,
                0 as WPARAM,
                0 as LPARAM,
            );
        }
    }
}

/// `IsZoomed`: whether the window is currently maximized.
pub fn is_zoomed() -> bool {
    let h = main_hwnd();
    h != 0 && unsafe { windows_sys::Win32::UI::WindowsAndMessaging::IsZoomed(h as _) != 0 }
}

/// Put the window above (or below) every other window.
///
/// `HWND_TOPMOST` / `HWND_NOTOPMOST` is the whole mechanism — there is no
/// separate "always on top" flag. `SWP_NOMOVE | SWP_NOSIZE` keeps the
/// current rectangle, because the *only* reason to call this is to change
/// the z-order and a jump across the screen would be a bug, not a feature.
pub fn set_topmost(on: bool) -> bool {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        SetWindowPos, HWND_NOTOPMOST, HWND_TOPMOST, SWP_NOMOVE, SWP_NOSIZE,
    };
    let h = main_hwnd();
    if h == 0 {
        return false;
    }
    let after = if on { HWND_TOPMOST } else { HWND_NOTOPMOST };
    // Report the real result: callers surface a failure in the status bar
    // (e.g. a modal dialog that could not be kept above its owner).
    unsafe { SetWindowPos(h as _, after, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE) != 0 }
}

/// Is the window currently in the topmost band?
///
/// `GetWindowLong(GWL_EXSTYLE) & WS_EX_TOPMOST` — the OS has no direct
/// "query", and this is the documented way to ask.
pub fn is_topmost() -> bool {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetWindowLongPtrW, GWL_EXSTYLE, WS_EX_TOPMOST,
    };
    let h = main_hwnd();
    if h == 0 {
        return false;
    }
    unsafe {
        let ex = GetWindowLongPtrW(h as _, GWL_EXSTYLE) as isize;
        ex & WS_EX_TOPMOST as isize == WS_EX_TOPMOST as isize
    }
}

pub fn activate() {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        SetForegroundWindow, ShowWindow, SW_RESTORE,
    };
    let h = main_hwnd();
    if h == 0 {
        return;
    }
    unsafe {
        // Restore first: `SetForegroundWindow` on a minimized window only
        // flashes it in the taskbar.
        if windows_sys::Win32::UI::WindowsAndMessaging::IsZoomed(h as _) == 0 {
            let _ = ShowWindow(h as _, SW_RESTORE);
        }
        SetForegroundWindow(h as _);
    }
}

pub fn focus_main() {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::SetFocus;
    let h = super::current_hwnd();
    if h != 0 {
        unsafe {
            SetFocus(h as _);
        }
    }
}

pub fn set_title(title: &str) {
    let h = main_hwnd();
    if h == 0 {
        return;
    }
    let text: Vec<u16> = title.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        windows_sys::Win32::UI::WindowsAndMessaging::SetWindowTextW(h as _, text.as_ptr());
    }
}

// ---------------------------------------------------------------------------
// DPI
// ---------------------------------------------------------------------------

pub fn ensure_dpi_awareness() -> &'static str {
    unsafe {
        use windows_sys::Win32::UI::HiDpi::{
            SetProcessDpiAwareness, SetProcessDpiAwarenessContext,
            DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, PROCESS_PER_MONITOR_DPI_AWARE,
        };
        // V2 is the only level that fixes both "dialogs scale with the
        // monitor" and "blurry non-client area" at once.
        if SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) != 0 {
            return "per-monitor-v2";
        }
        // Note: SetProcessDpiAwareness returns an HRESULT (S_OK==0); failure
        // (including E_ACCESSDENIED when the manifest already locked the
        // awareness) is a non-zero value, so it must be checked with == 0.
        if SetProcessDpiAwareness(PROCESS_PER_MONITOR_DPI_AWARE) == 0 {
            return "per-monitor";
        }
        // Both failing = awareness is already locked (manifest/compatibility
        // override); query the actual value.
        //
        // Note: `GetAwarenessFromDpiAwarenessContext` can only return 0/1/2
        // (unaware/system/per-monitor); PerMonitorV2 is a context rather than
        // an awareness enum value — v1 and v2 both return 2. To tell them
        // apart, the contexts must be compared one by one with
        // `AreDpiAwarenessContextsEqual`.
        use windows_sys::Win32::UI::HiDpi::{
            AreDpiAwarenessContextsEqual, GetThreadDpiAwarenessContext,
            DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE, DPI_AWARENESS_CONTEXT_SYSTEM_AWARE,
            DPI_AWARENESS_CONTEXT_UNAWARE,
        };
        let ctx = GetThreadDpiAwarenessContext();
        if AreDpiAwarenessContextsEqual(ctx, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) != 0 {
            "already: per-monitor-v2 (manifest)"
        } else if AreDpiAwarenessContextsEqual(ctx, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE) != 0 {
            "already: per-monitor"
        } else if AreDpiAwarenessContextsEqual(ctx, DPI_AWARENESS_CONTEXT_SYSTEM_AWARE) != 0 {
            "already: system"
        } else if AreDpiAwarenessContextsEqual(ctx, DPI_AWARENESS_CONTEXT_UNAWARE) != 0 {
            "already: unaware"
        } else {
            "already: unknown"
        }
    }
}

// ---------------------------------------------------------------------------
// Dialog owner (rfd's modal semantics depend on the Win32 owner)
// ---------------------------------------------------------------------------

/// Owner wrapper for file dialogs (wraps a bare HWND into the
/// `HasWindowHandle` rfd wants).
///
/// With an owner, the dialog becomes the main window's **modal** child: it can
/// no longer end up behind the main window (without an owner, a single focus
/// switch and the user cannot find the dialog), and it lands on the owner's
/// monitor.
pub struct DialogOwner(isize);

impl dioxus_native::winit::raw_window_handle::HasDisplayHandle for DialogOwner {
    fn display_handle(
        &self,
    ) -> Result<
        dioxus_native::winit::raw_window_handle::DisplayHandle<'_>,
        dioxus_native::winit::raw_window_handle::HandleError,
    > {
        use dioxus_native::winit::raw_window_handle::{
            DisplayHandle, RawDisplayHandle, WindowsDisplayHandle,
        };
        // Windows has only one display; the bare constructor is a usage
        // endorsed by the raw-window-handle docs.
        let handle = RawDisplayHandle::Windows(WindowsDisplayHandle::new());
        Ok(unsafe { DisplayHandle::borrow_raw(handle) })
    }
}

impl dioxus_native::winit::raw_window_handle::HasWindowHandle for DialogOwner {
    fn window_handle(
        &self,
    ) -> Result<
        dioxus_native::winit::raw_window_handle::WindowHandle<'_>,
        dioxus_native::winit::raw_window_handle::HandleError,
    > {
        use dioxus_native::winit::raw_window_handle::{
            RawWindowHandle, Win32WindowHandle, WindowHandle,
        };
        let hwnd = std::num::NonZeroIsize::new(self.0)
            .ok_or(dioxus_native::winit::raw_window_handle::HandleError::NotSupported)?;
        let mut handle = Win32WindowHandle::new(hwnd);
        handle.hinstance = None;
        Ok(unsafe { WindowHandle::borrow_raw(RawWindowHandle::Win32(handle)) })
    }
}

/// The current window as a file dialog's owner; `None` while the window is
/// not yet created.
pub fn dialog_owner() -> Option<DialogOwner> {
    let h = super::current_hwnd();
    (h != 0).then_some(DialogOwner(h))
}

/// Attach a top-level window as another window's **owner** (dialog
/// semantics).
///
/// winit's `with_owner_window` provides ownership for free, but turns the
/// window into a `WS_POPUP` (winit-win32's `fallback_parent` →
/// `to_window_styles`): no system title bar, no resize border — the "looks like
/// a system dialog" shape is unattainable. Here Win32's owner semantics
/// (`GWLP_HWNDPARENT`) are attached **after** the window is created, so the
/// window keeps a normal top-level window's system title bar + border while
/// gaining ownership: the z-order is always above the owner, it auto-hides
/// when the owner minimizes, and it is destroyed together with the owner.
///
/// It also clears `WS_EX_APPWINDOW` along the way: a window with an owner does
/// not appear in the taskbar / Alt+Tab by default, but winit adds that style
/// when creating a normal window. winit rewrites `GWL_EXSTYLE` wholesale
/// according to internal flags (`apply_diff`) on show/hide operations and
/// re-adds it, so it **must be called again before every show** (see
/// `shell::place_show`).
pub fn attach_owner(hwnd: isize, owner: isize) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetWindowLongPtrW, SetWindowLongPtrW, GWLP_HWNDPARENT, GWL_EXSTYLE, WS_EX_APPWINDOW,
    };
    if hwnd == 0 || owner == 0 {
        return;
    }
    unsafe {
        SetWindowLongPtrW(hwnd as _, GWLP_HWNDPARENT, owner as _);
        let ex = GetWindowLongPtrW(hwnd as _, GWL_EXSTYLE);
        SetWindowLongPtrW(hwnd as _, GWL_EXSTYLE, ex & !WS_EX_APPWINDOW as isize);
    }
}
