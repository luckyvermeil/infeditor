//! InfEditor patch 17: paint the window's client area before the first frame.
//!
//! # Why
//!
//! [`View::init`](crate::View::init) shows the window right after creating it,
//! and at that moment no pixel exists for it: the renderer has no surface yet
//! (that happens in `resume`/`complete_resume`) and neither Win32 nor winit
//! paints anything before the first present — winit's `WM_PAINT` handler
//! validates the update region without erasing, and the window class is
//! registered without a background brush. The compositor therefore has nothing
//! to show but the unpainted surface, which is the plain white first screen
//! ("dark theme flashes white on cold start").
//!
//! Holding the window back until the first frame would fix it too, but that
//! delays the window by the whole renderer initialization (hundreds of
//! milliseconds — device creation, shader compilation). Instead the window is
//! shown immediately and given the application's background color, which the
//! renderer's first frame replaces.
//!
//! # Order
//!
//! The two halves are **not** symmetric and both are needed:
//!
//! * [`prepare`] installs the color as the window **class** background brush
//!   and must run *before* the show — showing invalidates the whole client
//!   area, and the system asks for the background right after;
//! * [`paint`] fills the client area and must run *after* the show — showing a
//!   hidden window discards whatever was painted into it.
//!
//! # Scope
//!
//! Windows only. Other platforms keep their previous behavior (the window is
//! shown by `init`, and the first frame arrives as before); the API is
//! platform-neutral so `View::init` stays branch-free.
//!
//! # Shutdown
//!
//! [`paint`] is also what [`View::paint_background`](crate::View::paint_background)
//! uses while the application is being torn down, so closing a window looks the
//! same as opening it: the window keeps its theme background instead of showing
//! the unpainted surface. That use is belt-and-braces — on a GPU surface the
//! last presented frame normally survives the surface release, and GDI painting
//! over a window that owns a swapchain is not visible at all (measured; see
//! `INFEDITOR-PATCH.md`) — but it costs one `FillRect` and keeps the two paths
//! described by the same rule.

use winit::window::Window;

/// Install `background` as the window's class background brush (before the
/// show; see the module docs). No-op when no color was configured.
pub fn prepare(window: &dyn Window, background: Option<[u8; 3]>) {
    imp::prepare(window, background);
}

/// Suppress the platform's show/hide transition for this window, so it appears
/// fully formed instead of fading in over whatever is behind it.
///
/// Runs next to [`prepare`] (before the show) and is independent of the
/// background color: the fade-in is a compositor transition of the *window*
/// (partial opacity for the first ~100ms), which would show the desktop
/// through an otherwise correctly painted first screen.
pub fn suppress_show_transition(window: &dyn Window) {
    imp::suppress_show_transition(window);
}

/// Fill the window's client area with `background` (after the show; see the
/// module docs). No-op when no color was configured.
pub fn paint(window: &dyn Window, background: Option<[u8; 3]>) {
    imp::paint(window, background);
}

/// Show the window with `background` already in the client area.
///
/// The window is cloaked (Windows: `DWMWA_CLOAK`, which removes it from the
/// compositor's output without hiding it from Win32) for the few microseconds
/// between showing and painting: without that, the compositor can pick up the
/// window between `ShowWindow` and the fill and show one unpainted — white —
/// frame. The cloak is released before this function returns, so the window
/// still appears immediately; nothing waits for the renderer.
pub fn show(window: &dyn Window, background: Option<[u8; 3]>) {
    imp::show(window, background);
}

#[cfg(target_os = "windows")]
mod imp {
    use std::sync::Mutex;

    use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use winit::window::Window;

    type Hwnd = *mut core::ffi::c_void;
    type Hdc = *mut core::ffi::c_void;
    type Hbrush = *mut core::ffi::c_void;

    #[repr(C)]
    struct Rect {
        left: i32,
        top: i32,
        right: i32,
        bottom: i32,
    }

    // `GCLP_HBRBACKGROUND` (winuser.h): the class' background brush slot.
    const GCLP_HBRBACKGROUND: i32 = -10;

    #[link(name = "user32")]
    unsafe extern "system" {
        fn GetDC(hwnd: Hwnd) -> Hdc;
        fn ReleaseDC(hwnd: Hwnd, hdc: Hdc) -> i32;
        fn GetClientRect(hwnd: Hwnd, rect: *mut Rect) -> i32;
        fn SetClassLongPtrW(hwnd: Hwnd, index: i32, value: isize) -> isize;
    }

    #[link(name = "gdi32")]
    unsafe extern "system" {
        fn CreateSolidBrush(color: u32) -> Hbrush;
        fn FillRect(hdc: Hdc, rect: *const Rect, brush: Hbrush) -> i32;
        fn DeleteObject(obj: *mut core::ffi::c_void) -> i32;
    }

    #[link(name = "dwmapi")]
    unsafe extern "system" {
        fn DwmSetWindowAttribute(
            hwnd: Hwnd,
            attribute: u32,
            value: *const core::ffi::c_void,
            size: u32,
        ) -> i32;
    }

    // `DWMWA_TRANSITIONS_FORCEDISABLED` / `DWMWA_CLOAK` (dwmapi.h).
    const DWMWA_TRANSITIONS_FORCEDISABLED: u32 = 3;
    const DWMWA_CLOAK: u32 = 13;

    /// Hide the window from the compositor only (Win32 keeps painting it).
    fn set_cloaked(hwnd: Hwnd, on: bool) {
        let value: i32 = i32::from(on);
        unsafe {
            DwmSetWindowAttribute(
                hwnd,
                DWMWA_CLOAK,
                &value as *const i32 as *const core::ffi::c_void,
                std::mem::size_of::<i32>() as u32,
            );
        }
    }

    /// One cached brush per process: `(0x00RRGGBB, HBRUSH)`. `0` = none yet.
    ///
    /// A window class holds the brush by reference, so it must outlive every
    /// erase; the previous brush is only deleted once a *different* color
    /// replaces it (a theme switch), and only if we created it ourselves.
    static BRUSH: Mutex<(u32, isize)> = Mutex::new((0, 0));

    fn hwnd(window: &dyn Window) -> Hwnd {
        match window.window_handle() {
            Ok(handle) => match handle.as_raw() {
                RawWindowHandle::Win32(w) => w.hwnd.get() as Hwnd,
                _ => std::ptr::null_mut(),
            },
            Err(_) => std::ptr::null_mut(),
        }
    }

    /// `0xRRGGBB` → COLORREF (`0x00BBGGRR`).
    fn colorref([r, g, b]: [u8; 3]) -> u32 {
        (r as u32) | ((g as u32) << 8) | ((b as u32) << 16)
    }

    /// The cached brush for `rgb`, created on first use.
    ///
    /// The displaced brush is **returned, not deleted**: a window class holds
    /// the brush it was last given, so freeing it here would leave the class
    /// pointing at a dead object for the moment between this call and the
    /// `SetClassLongPtrW` that replaces it. [`prepare`] frees it after the
    /// install; callers that only paint ([`paint`]) match the class color and
    /// therefore never displace anything.
    fn brush_for(rgb: [u8; 3]) -> (Hbrush, Option<isize>) {
        let key = ((rgb[0] as u32) << 16) | ((rgb[1] as u32) << 8) | rgb[2] as u32;
        let mut cache = BRUSH.lock().unwrap_or_else(|e| e.into_inner());
        if cache.1 != 0 && cache.0 == key {
            return (cache.1 as Hbrush, None);
        }
        let brush = unsafe { CreateSolidBrush(colorref(rgb)) };
        if brush.is_null() {
            return (brush, None);
        }
        let displaced = (cache.1 != 0).then_some(cache.1);
        *cache = (key, brush as isize);
        (brush, displaced)
    }

    pub(super) fn prepare(window: &dyn Window, background: Option<[u8; 3]>) {
        let Some(rgb) = background else { return };
        let hwnd = hwnd(window);
        if hwnd.is_null() {
            return;
        }
        let (brush, displaced) = brush_for(rgb);
        if brush.is_null() {
            return;
        }
        unsafe {
            SetClassLongPtrW(hwnd, GCLP_HBRBACKGROUND, brush as isize);
            // Only now is the class off the old brush; a deleted handle in a
            // class slot would make the next `WM_ERASEBKGND` paint garbage.
            if let Some(old) = displaced {
                DeleteObject(old as *mut core::ffi::c_void);
            }
        }
    }

    pub(super) fn suppress_show_transition(window: &dyn Window) {
        let hwnd = hwnd(window);
        if hwnd.is_null() {
            return;
        }
        let disable: i32 = 1;
        unsafe {
            DwmSetWindowAttribute(
                hwnd,
                DWMWA_TRANSITIONS_FORCEDISABLED,
                &disable as *const i32 as *const core::ffi::c_void,
                std::mem::size_of::<i32>() as u32,
            );
        }
    }

    pub(super) fn show(window: &dyn Window, background: Option<[u8; 3]>) {
        let Some(rgb) = background else {
            // No color to paint with: keep the upstream behavior exactly.
            window.set_visible(true);
            return;
        };
        let hwnd = hwnd(window);
        let cloaked = !hwnd.is_null();
        if cloaked {
            set_cloaked(hwnd, true);
        }
        window.set_visible(true);
        super::paint(window, Some(rgb));
        if cloaked {
            set_cloaked(hwnd, false);
        }
    }

    pub(super) fn paint(window: &dyn Window, background: Option<[u8; 3]>) {
        let Some(rgb) = background else { return };
        let hwnd = hwnd(window);
        if hwnd.is_null() {
            return;
        }
        let (brush, _) = brush_for(rgb);
        if brush.is_null() {
            return;
        }
        unsafe {
            let dc = GetDC(hwnd);
            if dc.is_null() {
                return;
            }
            let mut rect = Rect {
                left: 0,
                top: 0,
                right: 0,
                bottom: 0,
            };
            if GetClientRect(hwnd, &mut rect) != 0 && rect.right > rect.left && rect.bottom > rect.top
            {
                FillRect(dc, &rect, brush);
            }
            ReleaseDC(hwnd, dc);
        }
    }
}

#[cfg(not(target_os = "windows"))]
mod imp {
    use winit::window::Window;

    pub(super) fn prepare(_window: &dyn Window, _background: Option<[u8; 3]>) {}

    pub(super) fn suppress_show_transition(_window: &dyn Window) {}

    pub(super) fn show(window: &dyn Window, _background: Option<[u8; 3]>) {
        window.set_visible(true);
    }

    pub(super) fn paint(_window: &dyn Window, _background: Option<[u8; 3]>) {}
}
