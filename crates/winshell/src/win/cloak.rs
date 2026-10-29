//! DWM cloak: keep a window out of the composition while the Win32 surface is
//! not the thing that should be on screen.
//!
//! Used at both ends of a window's life, for the same underlying reason: the
//! Win32 fallback surface is white, so any moment where the HWND is visible but
//! the GPU surface is not the composed one shows white.
//!
//! * **Startup.** The window must stay *visible* to Win32 (a hidden winit window
//!   never receives `RedrawRequested` — `request_redraw` goes through
//!   `RedrawWindow(RDW_INTERNALPAINT)`, which is not delivered while hidden),
//!   but a visible window shows that fallback surface before the first present —
//!   *the* "dark theme flashes white on cold start" report.
//! * **Shutdown.** `View::drop` releases the renderer surface before the window
//!   is dropped, so the same white surface is composed again for as long as
//!   tearing down the document takes — "dark theme flashes white when closing".
//!
//! Cloaking hides the window from DWM only: painting still happens, and the
//! frame appears composed the moment the cloak is dropped.

/// Cloak (or uncloak) `hwnd`. Best effort: an unsupported system returns an error
/// that is intentionally ignored (the window then behaves as before).
#[cfg(windows)]
pub fn set_cloaked(hwnd: isize, on: bool) {
    use windows_sys::Win32::Graphics::Dwm::{DwmSetWindowAttribute, DWMWA_CLOAK};
    let value: i32 = i32::from(on);
    unsafe {
        DwmSetWindowAttribute(
            hwnd as _,
            DWMWA_CLOAK as u32,
            &value as *const i32 as *const std::ffi::c_void,
            std::mem::size_of::<i32>() as u32,
        );
    }
}

#[cfg(not(windows))]
pub fn set_cloaked(_hwnd: isize, _on: bool) {}
