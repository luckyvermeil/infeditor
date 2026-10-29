//! # winshell — window and multi-window shell infrastructure (the former
//! `winframe` + `popshell` merged)
//!
//! Extracted from `infeditor`, with no relation to editor business logic,
//! split into two layers along **dependency direction**:
//!
//! ## `win` layer — Win32 infrastructure for frameless windows (former winframe)
//!
//! Only deals with "windows, handles, geometry, theme-agnostic native
//! affordances" and depends on no blitz rendering concept (the only external
//! type dependency is winit re-exported through `dioxus-native`, see the
//! type-identity comment in `Cargo.toml`):
//!
//! * [`win::frameless`] — drag/resize (`WM_NCHITTEST` resize bands + the
//!   component-rect-set adjudication), double-click title bar, touch move loop,
//!   min/max/close/topmost, DPI, owner attaching, `DialogOwner`, title/
//!   activation;
//! * [`win::snap`] — subclassed wndproc observing
//!   `WM_ENTERSIZEMOVE`/`WM_MOVING`/`WM_EXITSIZEMOVE` to complete Win+↑ /
//!   drag-to-top snap behavior, and answering `WM_NCHITTEST`;
//! * [`win::softkb`] — touch keyboard (TabTip) detection.
//!
//! ## `shell` layer — Blitz multi-window popup framework (former popshell)
//!
//! * [`shell::bridge`] — the `Bridge<S, C>` two-way state bridge (deduplicated
//!   flush of the main-window mirror + an mpsc command channel), composer focus
//!   generations, popup cursor blink bookkeeping;
//! * [`shell::controls`] — the popup widget library (buttons/checkbox/radio/
//!   input rows/group/list/titlebar), whose theme only knows the [`PopTheme`]
//!   pure palette;
//! * [`shell::motion`] — motion tokens (hover/press transitions), shared by the
//!   main window and popups;
//! * [`shell`] — our own multi-window `ApplicationHandler`: lazy popup creation
//!   (system-dialog shape + Blitz painting), `ShellEvt` routing, soft-keyboard
//!   avoidance, touch simulation, accessibility refresh.
//!
//! # Semantic assets (behavioral contracts that must travel with the code when
//! extracting; do not rely on the code alone)
//!
//! 1. **`attach_owner` must never be swapped for winit's `with_owner_window`** —
//!    that turns the window into a `WS_POPUP` and loses the system title bar /
//!    border; it must be attached after creation with
//!    `SetWindowLongPtrW(GWLP_HWNDPARENT)`.
//! 2. **`attach_owner` must be re-run before every show**: winit `set_visible`
//!    rewrites `GWL_EXSTYLE` from its internal flags and puts
//!    `WS_EX_APPWINDOW` back (which puts it in the taskbar).
//! 3. Drag gestures must go through `PostMessageW(WM_NCLBUTTONDOWN)` instead of
//!    winit's `drag_window` — the latter's `dragging` reset flag gets stuck in
//!    nested modal loops (right-click menus), after which every drag/resize
//!    silently fails.
//! 4. Frameless windows need `with_undecorated_shadow(true)` for the DWM
//!    shadow (the window attributes in the application's `main.rs`).
//! 5. The interactive component rect set for `WM_NCHITTEST`
//!    ([`win::frameless::set_hit_exempt`]) is pushed by the application layout
//!    layer: hit-testing runs inside the WndProc and **must not** touch dioxus
//!    `Signal`s.
//! 6. **Popup cursor blinking must be event-driven**, never a CSS animation:
//!    blitz self-sustains redraws every frame for windows with running
//!    animations, saturating the winit message pump and starving the main
//!    window's WM_PAINT (see the `shell::bridge` module comments).
//!
//! # Application-side integration points
//!
//! 1. [`register_popups`]: register every popup instance ([`Popup`] trait) at
//!    startup; the shell's show/hide/route/close all consult this table — no
//!    more matching on business enums;
//! 2. [`shell::set_net_provider`] / [`shell::set_log_path`] /
//!    [`shell::set_anchor_probe`]: network images, the diagnostic log path and
//!    main-window layout anchor measurement are all hooks injected by the
//!    application side; the framework knows nothing about specific business
//!    logic;
//! 3. [`dialog`] — the stable entry point for file-dialog owner / DPI awareness
//!    (a facade over the corresponding items in `win::frameless`).

pub mod shell;
pub mod win;

/// Stable entry point for file-dialog owner / DPI awareness (a facade over the
/// corresponding items in `win::frameless`, so callers do not have to remember
/// "these functions happen to live in the frameless module"). Owner semantics
/// exist only on Windows, and every call site sits under `cfg(windows)`.
pub mod dialog {
    pub use crate::win::frameless::ensure_dpi_awareness;
    #[cfg(windows)]
    pub use crate::win::frameless::{attach_owner, dialog_owner, DialogOwner};

    /// Attach the main window as owner of an rfd dialog (modal, on the right
    /// monitor, not covered by the main window).
    ///
    /// Only Windows has owner semantics — other platforms return the dialog
    /// unchanged (Linux goes through xdg-desktop-portal, which has no parent
    /// window concept anyway), so call sites stay cfg-free.
    pub fn owned(dlg: rfd::FileDialog) -> rfd::FileDialog {
        #[cfg(windows)]
        if let Some(owner) = crate::win::frameless::dialog_owner() {
            return dlg.set_parent(&owner);
        }
        dlg
    }
}

/// Whether owned popups (child windows shaped like system dialogs) exist on
/// this platform.
///
/// Windows only: owner attaching, taskbar suppression and Alt+F4 routing are
/// all Win32 semantics. The application layer on other platforms falls back to
/// in-page dialogs accordingly (each `open_*_window` returns `false` and the
/// caller takes the in-page path) — a runtime predicate rather than `cfg`, so
/// host code stays branch-free.
pub fn owned_popups() -> bool {
    cfg!(windows)
}

/// Whether a native context menu (`TrackPopupMenu`, which can extend beyond the
/// window and flips automatically at screen edges) exists on this platform.
/// Other platforms fall back to an in-page menu overlay — a runtime predicate
/// rather than `cfg`, so host code stays branch-free.
pub fn native_ctx_menu() -> bool {
    cfg!(windows)
}

pub use shell::bridge::{bump_focus_epoch, focus_epoch};
pub use shell::controls::PopTheme;

use dioxus_native::prelude::Element;
use std::sync::OnceLock;

/// Stable identity of a popup (a string ID; replaces infeditor's old business
/// enum `PopupKind`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PopupId(pub &'static str);

/// All registration information for a popup (replaces the three `PopupKind`
/// matches in the old shell).
///
/// Each popup module exports a zero-sized implementation plus a
/// `pub const ID`; at startup they are registered into the global table via
/// [`register_popups`], and the shell routes by [`PopupId`] lookups.
pub trait Popup: Sync {
    /// Stable identity (used by shell routing)
    fn id(&self) -> PopupId;
    /// Initial title of the system title bar (the popup updates it itself via
    /// `set_title` when its content mode changes)
    fn title(&self) -> &'static str;
    /// Minimum size of the popup content (so the layout cannot be crushed)
    fn min_size(&self) -> (f32, f32);
    /// Dark or light of the system title bar (follows the application theme)
    fn dark(&self) -> bool;
    /// Root component of the popup vdom
    fn root(&self) -> fn() -> Element;
    /// Flush the shared mirror into the popup Signal (called by the shell at a
    /// safe point)
    fn flush(&self);
    /// Cleanup when the popup receives OS focus (called by the shell before its
    /// supplementary flush)
    fn on_focused(&self);
    /// Close path for the system `×` / Esc (hide + notify the main window for
    /// cleanup; does not destroy the window)
    fn request_close(&self);
}

/// Popup registry (injected once at application startup).
static POPUPS: OnceLock<Vec<&'static dyn Popup>> = OnceLock::new();

/// Register all popup instances (`main` calls this once before `launch`).
pub fn register_popups(list: &[&'static dyn Popup]) {
    let _ = POPUPS.set(list.to_vec());
}

/// Look up the registry by id (for shell routing; `None` when not registered).
pub fn popup_by_id(id: PopupId) -> Option<&'static dyn Popup> {
    POPUPS
        .get()
        .and_then(|v| v.iter().copied().find(|p| p.id() == id))
}

#[cfg(test)]
mod tests {
    use super::*;
    // The `rsx!` macro expansion body references several prelude re-exports
    // (dioxus_signals etc.), so a glob import is required (importing only rsx
    // reports unresolved dioxus_signals).
    use dioxus_native::prelude::*;

    struct Dummy;
    impl Popup for Dummy {
        fn id(&self) -> PopupId {
            PopupId("dummy")
        }
        fn title(&self) -> &'static str {
            "dummy"
        }
        fn min_size(&self) -> (f32, f32) {
            (100.0, 80.0)
        }
        fn dark(&self) -> bool {
            false
        }
        fn root(&self) -> fn() -> Element {
            || rsx! {}
        }
        fn flush(&self) {}
        fn on_focused(&self) {}
        fn request_close(&self) {}
    }

    /// The registry has either never been written (tests may write it) or the
    /// id is findable — a miss must not be silently treated as "hidden
    /// successfully"; the shell side uses `None` to detect not-registered.
    #[test]
    fn lookup_is_exact_and_id_typed() {
        // Parallel tests may already have written the registry; only assert
        // that looking up an id that was definitely never registered gives None.
        assert!(popup_by_id(PopupId("definitely-not-registered")).is_none());
        let d = Dummy;
        assert_eq!(d.id(), PopupId("dummy"));
        assert_eq!(d.title(), "dummy");
        assert!(!d.dark());
    }
}
