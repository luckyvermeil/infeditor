#![cfg_attr(all(not(test), target_os = "windows"), windows_subsystem = "windows")]

use dioxus_native::prelude::*;
use dioxus_native::{LogicalSize, WindowAttributes};
use infeditor::ui::editor::EditorApp;
use infeditor::ui::shell;

fn main() {
    winshell::shell::timing("main");
    // DPI awareness must be set as early as possible: the sharpness of the
    // native file dialogs depends on the process awareness level.
    // Diagnostics go through [`diag`]: during development they hit the
    // console; in release builds output only appears when logging is
    // explicitly enabled (a `windows_subsystem = "windows"` process has no
    // stderr receiver).
    diag(&format!(
        "[main] dpi awareness: {}",
        winshell::win::frameless::ensure_dpi_awareness()
    ));
    install_panic_hook();
    diag("[main] starting");
    // One editor per user session (see `platform::instance` and `ui::single`).
    //
    // Two instances would share one recovery journal, one settings file and one
    // macro library — and the newcomer would find the *running* instance's
    // session journal and offer to "restore" it, which is the alarming prompt
    // this guard exists to remove. A later launch therefore hands its command
    // line (a file dropped onto the exe, an "Open with" invocation) to the
    // running instance and exits **before** creating a window or touching any
    // per-user state.
    //
    // The lock must stay alive for the whole process: it is the "I am the
    // editor for this config directory" claim, released when the returned guard
    // drops at the end of main.
    let Some(instance) = infeditor::ui::single::claim() else {
        diag("[main] another instance owns this config directory: handing over and exiting");
        // Hand the foreground right over first: this process is the one the user
        // just clicked, and the running instance needs the right to raise its own
        // window (Windows otherwise only flashes the taskbar button).
        platform::instance::yield_foreground();
        infeditor::ui::single::submit(&infeditor::ui::cli_open_files());
        return;
    };
    // winit 0.31 renamed `inner_size` to `surface_size`.
    let attrs = WindowAttributes::default()
        .with_title("InfEditor")
        .with_surface_size(LogicalSize::new(1280.0, 860.0))
        // Taskbar / Alt-Tab / system menu icon. Windows only reads it from
        // `WM_SETICON`, which winit fills from this field — the `.ico` that
        // ships next to the exe is for Explorer's shortcuts, not for the live
        // window. Without it a frameless window keeps the generic executable
        // icon.
        .with_window_icon(window_icon())
        // Frameless: the title bar and menu bar are merged into one
        // row drawn by the app (see `ui::components::titlebar`), which is why
        // the window controls, dragging and resizing are re-implemented in
        // `winshell::win::frameless`.
        .with_decorations(false);
    // Draw the system shadow back.
    //
    // DWM only draws a drop shadow for top-level windows with a visible
    // non-client area, while the frameless implementation does the opposite:
    // winit expands the client area to the full window rect inside
    // `WM_NCCALCSIZE`, leaving the non-client area at 0 — the system title
    // bar disappears, and the shadow goes with it. `with_undecorated_shadow`
    // makes winit keep a 1px non-client strip at the top of the window in
    // that same message (the client area shifts down 1px and the bottom row
    // gets clipped); seeing that bit of visible frame, DWM redraws the
    // shadow around all four sides.
    //
    // Known cost (stated in winit's docs): a 1px hairline at the top of the
    // window. When maximized, `WM_NCCALCSIZE` takes the work-area clamping
    // branch, so neither the hairline nor the shadow appears — maximized
    // windows do not show shadows anyway, matching system behavior.
    //
    // Popups (find/preferences/numbering/about) are ordinary top-level
    // windows with a system title bar; DWM provides their shadows for free,
    // so this flag is not needed there.
    #[cfg(windows)]
    let attrs = attrs.with_platform_attributes(Box::new(
        dioxus_native::winit::platform::windows::WindowAttributesWindows::default()
            .with_undecorated_shadow(true),
    ));
    // Multi-window shell: besides the main window, find/replace is created
    // on demand as a Blitz-rendered child window (see `ui::shell` /
    // `ui::findwin`).
    shell::launch(app, attrs);
    // Normal exit (event loop returns): clean up the recovery session. Any
    // session/ directory left behind can only be debris from an abnormal
    // shutdown — this is exactly the signal used on next startup to decide
    // "the last run did not exit cleanly".
    infeditor::ui::recovery::clear_session_best_effort();
    // Only now may another editor start: released explicitly (rather than by
    // dropping at the end of `main`) so a relaunch right after quitting is not
    // turned away by a lock this process still happens to hold.
    instance.release();
}

/// Diagnostic output: only reaches the console when `INFEDITOR_LOG` is set.
///
/// The production binary is a GUI subsystem (no console); an unconditional
/// `eprintln!` would be dead code and would pollute the output when the
/// process is spawned by a console-bearing host (debugger, CI).
fn diag(msg: &str) {
    if std::env::var_os("INFEDITOR_LOG").is_some() {
        eprintln!("{msg}");
    }
}

/// Panic hook: the console (if any) + a `crash.log` in the config directory.
///
/// A release build is a `windows_subsystem = "windows"` process with no stderr
/// receiver — writing only to stderr means zero evidence on crash (the user can
/// only say "it just disappeared"). The record writer lives in
/// [`infeditor::panic_log`] so the `catch_unwind` firewalls can append a
/// "recovered" line to the same file and the log stays one place to look at.
fn install_panic_hook() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        diag(&format!("[PANIC] {info}"));
        infeditor::panic_log::record_panic(info);
        default_hook(info);
    }));
}

/// The window icon handed to winit (see [`main`]).
///
/// The artwork is the same mark the installers ship as `infeditor.ico`, but the
/// *live* window only sees what the toolkit passes to `WM_SETICON`, so the PNG
/// is compiled into the binary instead of being read from disk (a `cargo run`
/// checkout has no installed `.ico` next to the exe).
///
/// `None` — undecodable PNG, wrong byte count — degrades to the system default
/// icon: a missing icon is not worth failing startup over.
fn window_icon() -> Option<dioxus_native::winit::icon::Icon> {
    let bytes = include_bytes!("../packaging/icon/png/infeditor-256.png");
    let img = image::load_from_memory(bytes).ok()?.to_rgba8();
    let (w, h) = img.dimensions();
    // `RgbaIcon` keeps the straight-alpha buffer; the platform layer converts it
    // to the DIB the shell wants.
    dioxus_native::winit::icon::RgbaIcon::new(img.into_raw(), w, h)
        .ok()
        .map(Into::into)
}

#[component]
fn app() -> Element {
    rsx! { EditorApp {} }
}
