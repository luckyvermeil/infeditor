//! Win32-exclusive popup glue: owned-window creation/placement/owner
//! attachment.
//!
//! Owned popups (system-dialog shape, Blitz-rendered content) depend on Win32
//! owner semantics (`GWLP_HWNDPARENT`) and taskbar suppression
//! (`WS_EX_APPWINDOW`), concepts other platforms do not have — the whole module
//! compiles only under `cfg(windows)`, while [`ShellApp`]'s public surface
//! (PopupShow routing) stays platform-neutral.

use super::*;

impl ShellApp {
    /// Create a popup window: an owned window in **system-dialog shape**,
    /// with blitz rendering the content.
    ///
    /// A normal top-level window (system title bar + border) gets an owner
    /// attached after creation via
    /// [`crate::win::frameless::attach_owner`]: the title bar is draggable, `×`
    /// closes (`window_event` intercepts CloseRequested into hide + close
    /// command instead of blitz's default destroy path), Alt+F4 / the system
    /// menu take the same path, and the system border is resizable. There is
    /// only one button, `×` (`WindowButtons::CLOSE`; dialog convention: no
    /// minimize/maximize).
    pub(super) fn open_child(
        &mut self,
        el: &dyn ActiveEventLoop,
        popup: &'static dyn Popup,
        parent: isize,
        rect: [f32; 4],
    ) {
        let (x, y, w, h) = (rect[0], rect[1], rect[2], rect[3]);
        let (min_w, min_h) = popup.min_size();
        let mut attrs = winit::window::WindowAttributes::default()
            .with_resizable(true)
            .with_enabled_buttons(winit::window::WindowButtons::CLOSE)
            .with_min_surface_size(winit::dpi::LogicalSize::new(min_w, min_h))
            .with_title(popup.title())
            .with_surface_size(winit::dpi::LogicalSize::new(w, h))
            .with_position(winit::dpi::LogicalPosition::new(x, y))
            .with_visible(false);
        if parent != 0 {
            use winit::platform::windows::WindowAttributesWindows;
            // No taskbar entry. The owner does not go through winit's
            // `with_owner_window` — that path turns the window into a
            // title-less, borderless `WS_POPUP`; the ownership is attached
            // after creation by `win32::attach_owner` (`GWLP_HWNDPARENT`), see
            // `place_show`.
            attrs = attrs.with_platform_attributes(Box::new(
                WindowAttributesWindows::default().with_skip_taskbar(true),
            ));
        }
        let vdom = dioxus_core::VirtualDom::new(popup.root());
        let doc = DioxusDocument::new(vdom, document_config());
        let cfg = WindowConfig::with_attributes(
            Box::new(doc) as _,
            DioxusNativeWindowRenderer::default(),
            attrs,
        );
        let mut view = View::init(cfg, el, &self.inner.proxy);
        // The child window only injects the window handle / renderer context
        // (components do not use document/history).
        let winit_window = std::sync::Arc::clone(&view.window);
        // The composer is the popup's only keyboard/IME landing point, so it
        // is always on at window level (see enable_ime's long doc: the winit
        // fork fixes IME off at window creation and blitz's enable point is
        // bound by timing to miss it).
        enable_ime(&*winit_window);
        let renderer = view.renderer.clone();
        let window_id = view.window_id();
        let doc = view.downcast_doc_mut::<DioxusDocument>();
        // Same cursor/repaint forwarding as the main window (the popup's
        // buttons and inputs otherwise show the plain arrow).
        doc.inner
            .borrow_mut()
            .set_shell_provider(std::sync::Arc::new(super::WindowShellProvider {
                window: std::sync::Arc::clone(&winit_window),
            }));
        // Runtime accounting for raw winit callbacks (see
        // dispatch_in_window_runtime): a popup is its own vdom and must use its
        // own runtime.
        note_window_runtime(window_id, doc.vdom.runtime());
        doc.vdom.in_scope(ScopeId::ROOT, move || {
            provide_context(winit_window);
            provide_context(renderer);
        });
        doc.initial_build();
        view.resume();
        // The is_visible recorded by View::init is based on the window being
        // invisible at creation; if left uncorrected, redraw() skips rendering
        // because is_visible=false and the popup stays frozen on the first
        // frame.
        view.is_visible = true;
        let id = window_id;
        self.inner.windows.insert(id, view);
        self.children.push(ChildWin {
            id,
            popup,
            focus_pending: std::cell::Cell::new(false),
            kb_saved: std::cell::Cell::new(None),
        });
        if let Some(v) = self.inner.windows.get_mut(&id) {
            place_show(&v.window, parent, x, y, w, h, popup.dark());
            v.request_redraw();
        }
        // The component's use_hook has already registered the Snap Signal;
        // push the current state in immediately, without waiting for the main
        // window's next-frame poll_dialogs.
        popup.flush();
    }

    /// Cloak every window the shell still owns — the shutdown half of the
    /// cold-start cloak. Called once from `ShellApp`'s destructor, before the
    /// renderer surfaces and the HWNDs are torn down (see the long note
    /// there).
    pub(super) fn cloak_all_windows(&self) {
        // Logged (only with INF_SHELL_LOG) because the result is unobservable
        // after the fact: a visual "did it flash?" report cannot be reproduced
        // from a log, but "was the shutdown cloak reached at all" can.
        if shell_log_on() {
            slog(&format!(
                "shutdown: cloaking {} window(s)",
                self.inner.windows.len()
            ));
        }
        for v in self.inner.windows.values() {
            cloak(&v.window, true);
        }
    }
}

fn place(window: &std::sync::Arc<dyn winit::window::Window>, x: f32, y: f32, w: f32, h: f32) {
    window.set_outer_position(winit::dpi::LogicalPosition::new(x, y).into());
    let _ = window.request_surface_size(winit::dpi::LogicalSize::new(w, h).into());
}

/// Place + show the popup, and keep it in "system-dialog shape" (see
/// [`own_dialog`]).
pub(super) fn place_show(
    window: &std::sync::Arc<dyn winit::window::Window>,
    parent: isize,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    dark: bool,
) {
    place(window, x, y, w, h);
    own_dialog(window, parent, dark);
    window.set_visible(true);
    // Attach once more after showing: winit's `set_visible` rewrites
    // `GWL_EXSTYLE` wholesale according to internal flags (winit-win32's
    // `apply_diff`), re-adding the `WS_EX_APPWINDOW` that the owner attachment
    // cleared (which would put it in the taskbar).
    own_dialog(window, parent, dark);
}

/// Attach the popup as the main window's owned dialog, and make the system
/// title bar's light/dark follow the app theme.
fn own_dialog(window: &std::sync::Arc<dyn winit::window::Window>, parent: isize, dark: bool) {
    if parent != 0 {
        crate::win::frameless::attach_owner(hwnd_of(window), parent);
    }
    window.set_theme(Some(if dark {
        winit::window::Theme::Dark
    } else {
        winit::window::Theme::Light
    }));
}

/// DWM-cloak a window: `on` hides it from composition without hiding it from
/// Win32, so the first frames still paint while the class brush (white) stays
/// invisible. See [`crate::win::cloak`].
pub(super) fn cloak(window: &std::sync::Arc<dyn winit::window::Window>, on: bool) {
    crate::win::cloak::set_cloaked(hwnd_of(window), on);
}

/// The window's Win32 `HWND` (0 when unavailable). Used only by the Windows
/// popup path.
fn hwnd_of(window: &std::sync::Arc<dyn winit::window::Window>) -> isize {
    use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
    match window.window_handle() {
        Ok(h) => match h.as_raw() {
            RawWindowHandle::Win32(w) => w.hwnd.get(),
            _ => 0,
        },
        Err(_) => 0,
    }
}
