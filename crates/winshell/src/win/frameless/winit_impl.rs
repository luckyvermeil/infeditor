//! Non-Windows implementation (winit's cross-platform capabilities; compiled
//! only under `cfg(not(windows))`; function signatures match
//! [`super::win32`], public surface documented in [`super`]).
//!
//! The window is always resolved from the [`super::WINDOW`] slot; with no
//! window in the slot every operation is a harmless no-op (the call window
//! before the app registers on its first frame).
//!
//! What winit lacks is filled by small mirrors: topmost is only ever set by
//! us and the OS never changes it behind our back — the [`TOPMOST`] mirror is
//! the truth. Closing goes through [`super::CLOSE_HOOK`] (see the `close`
//! doc).

use super::Window;
use dioxus_native::winit::window::WindowLevel;

/// A local mirror of [`WindowLevel::AlwaysOnTop`] (winit only writes it, never
/// reads it).
static TOPMOST: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn with_main(f: impl FnOnce(&dyn Window)) {
    if let Some(w) = super::window() {
        f(w.as_ref());
    }
}

// ---------------------------------------------------------------------------
// Client geometry
// ---------------------------------------------------------------------------

/// Client = the whole outer frame (a winit `decorations(false)` window has no
/// native border), so the outer frame's origin is the client's origin.
pub fn client_origin_physical() -> Option<(i32, i32)> {
    super::window()?.outer_position().ok().map(|p| (p.x, p.y))
}

/// Client = the content size (winit 0.31-beta renamed `inner_size` to
/// `surface_size`); origin = the outer frame's origin.
pub fn client_rect_screen() -> Option<(i32, i32, i32, i32)> {
    let w = super::window()?;
    let p = w.outer_position().ok()?;
    let s = w.surface_size();
    Some((p.x, p.y, s.width as i32, s.height as i32))
}

pub fn move_window_screen(window: &dyn Window, x: i32, y: i32) {
    use dioxus_native::winit::dpi::Position;
    // In this repo's winit fork the Into impl for `PhysicalPosition →
    // Position` is incomplete; go through Position::Physical explicitly (see
    // the E0308 when cross-compiling).
    window.set_outer_position(Position::Physical(
        dioxus_native::winit::dpi::PhysicalPosition::new(x, y),
    ));
}

// ---------------------------------------------------------------------------
// Drag / resize gestures
// ---------------------------------------------------------------------------

/// Title-bar drag → the window manager's native move gesture.
///
/// On Windows `drag_window` **must not** be used (its `dragging` re-entrancy
/// flag gets wedged by the nested modal loop; see the doc of
/// `win32::begin_nc_drag`); here there is no such modal loop (the context menu
/// is an in-page overlay and runs no OS message pump), so winit's entry point
/// is the right path.
pub fn start_move() {
    with_main(|w| {
        let _ = w.drag_window();
    });
}

/// Window menu "Move" → native interactive move (X11's `_NET_WM_MOVERESIZE`
/// is exactly the "no buttons held, starts at the pointer" semantics).
pub fn start_system_move() {
    with_main(|w| {
        let _ = w.drag_window();
    });
}

/// Window menu "Size" → native interactive resize gesture.
pub fn start_system_resize() {
    with_main(|w| {
        let _ = w.drag_resize_window(dioxus_native::winit::window::ResizeDirection::SouthEast);
    });
}

/// Edge/corner resize from the app's DOM strips (see [`super::start_resize`]).
///
/// `drag_resize_window` is the window manager's own interactive resize
/// (`_NET_WM_MOVERESIZE` on X11, `xdg_toplevel.resize` on Wayland, the live
/// resize loop on macOS), so snapping and the edge cursor stay native.
pub fn start_resize(dir: dioxus_native::winit::window::ResizeDirection) {
    with_main(|w| {
        let _ = w.drag_resize_window(dir);
    });
}

// ---------------------------------------------------------------------------
// Window state
// ---------------------------------------------------------------------------

pub fn minimize() {
    with_main(|w| w.set_minimized(true));
}

pub fn maximize() {
    with_main(|w| w.set_maximized(true));
}

/// Restoring from minimized and restoring from maximized are two independent
/// state axes; flip each once — the extra flip is a harmless idempotent
/// operation.
pub fn restore() {
    with_main(|w| {
        w.set_minimized(false);
        w.set_maximized(false);
    });
}

pub fn is_zoomed() -> bool {
    super::window().is_some_and(|w| w.is_maximized())
}

pub fn set_topmost(on: bool) -> bool {
    match super::window() {
        Some(w) => {
            w.set_window_level(if on {
                WindowLevel::AlwaysOnTop
            } else {
                WindowLevel::Normal
            });
            TOPMOST.store(on, std::sync::atomic::Ordering::Relaxed);
            true
        }
        None => false,
    }
}

pub fn is_topmost() -> bool {
    TOPMOST.load(std::sync::atomic::Ordering::Relaxed)
}

pub fn activate() {
    // winit's focus_window implies "restore and bring to front" (each backend
    // handles the semantic differences for minimized windows internally);
    // without focus it is merely a request and the window manager may refuse —
    // that is platform policy, not an error.
    with_main(|w| {
        w.focus_window();
    });
}

pub fn focus_main() {
    with_main(|w| {
        w.focus_window();
    });
}

pub fn set_title(title: &str) {
    with_main(|w| w.set_title(title));
}

/// Close has no winit equivalent: the shell layer installs a hook
/// ([`super::set_close_hook`]) that routes the request through the event loop
/// as `ShellEvt::CloseMain`, which becomes a synthetic `CloseRequested` for
/// the main window — the exact same path a real `×` takes.
pub fn close() {
    if let Some(f) = super::CLOSE_HOOK.get() {
        f();
    }
}

pub fn ensure_dpi_awareness() -> &'static str {
    // The awareness level is decided by the window system/session; the process
    // side neither needs (nor can) set it.
    "n/a (host-managed)"
}
