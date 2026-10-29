//! Application-side multi-window shell integration layer.
//!
//! The multi-window framework proper (`ShellApp`, `ShellEvt` routing, lazy
//! popup creation, soft-keyboard avoidance, touch simulation, accessibility
//! refresh) has been extracted into the standalone `winshell` crate (see
//! crates/winshell/README.md). This module only keeps the **application-side
//! integration points**:
//!
//! * re-exporting the framework's communication primitives ([`ShellEvt`] /
//!   [`send`] / [`use_window_event`] …), so the call paths of business code
//!   are unchanged;
//! * [`launch`] injects application-side hooks before entering the event
//!   loop — network image fetching, the diagnostic log path, menu-bar anchor
//!   measurement (all three are editor semantics; the framework uses them
//!   through `winshell::shell::set_*` callbacks and knows nothing about the
//!   specifics);
//! * registering the four popup instances ([`winshell::Popup`] implementations
//!   live in their respective popup modules: `findwin::FindPopup` etc.); the
//!   shell's show/hide/route/close all consult the registry.

use dioxus_native::prelude::*;
use dioxus_native::winit;

pub use winshell::shell::{
    bridge_ready, debug_log, send, shell_log_on, use_window_event, ShellEvt,
};

// ---------------------------------------------------------------------------
// Popup registry instances (`register_popups` requires `&'static dyn Popup`)
// ---------------------------------------------------------------------------

static FIND_POPUP: crate::ui::findwin::FindPopup = crate::ui::findwin::FindPopup;
static PREFS_POPUP: crate::ui::prefswin::PrefsPopup = crate::ui::prefswin::PrefsPopup;
static NUM_POPUP: crate::ui::numwin::NumPopup = crate::ui::numwin::NumPopup;
static ABOUT_POPUP: crate::ui::aboutwin::AboutPopup = crate::ui::aboutwin::AboutPopup;

// ---------------------------------------------------------------------------
// Application-side hooks (injected by the framework via `set_*`)
// ---------------------------------------------------------------------------

/// Diagnostic log path (writes `shell.log` under the config dir when
/// `INF_SHELL_LOG=1`; without a config dir the framework falls back to
/// `shell.log` in the cwd).
fn log_path() -> Option<std::path::PathBuf> {
    crate::ui::settings::settings_path().map(|p| p.with_file_name("shell.log"))
}

/// Menu-bar anchor measurement: write each menu caption's **true** left edge
/// from the main window's layout into the titlebar's measurement slot.
///
/// Opening a dropdown with <kbd>Alt</kbd>+letter has no pointer event, so the
/// anchor can only come from the layout itself. Every caption renders with
/// `inf-cap-{idx}` (see `titlebar::caption_dom_id`); blitz records the id
/// attribute in its `nodes_to_id` map, so `get_element_by_id` reaches it in
/// one hop; `get_client_bounding_rect` returns the same coordinate space as
/// `client_coordinates()` (CSS px, relative to the root element), identical
/// to the click path's `client_x - element_x` — window size, DPI and font
/// changes are followed automatically, with no more estimates.
///
/// The shell calls it once per idle pass on the main window's document: two
/// hash lookups plus one rect read, far cheaper than a frame of rendering.
/// Skips the write when the id is not found (the menu bar has not rendered
/// yet) or there is no layout rect; the estimate remains the fallback.
fn probe_caption_anchors(doc: &blitz_dom::BaseDocument) {
    const MAX_CAPTIONS: usize = 32;

    let mut xs: Vec<Option<f32>> = Vec::new();
    for idx in 0..MAX_CAPTIONS {
        // Ids are numbered consecutively from 0; the first miss means the
        // captions have all rendered.
        let Some(node_id) =
            doc.get_element_by_id(&crate::ui::components::titlebar::caption_dom_id(idx))
        else {
            break;
        };
        xs.push(doc.get_client_bounding_rect(node_id).map(|r| r.x as f32));
    }
    if !xs.is_empty() && crate::ui::components::titlebar::set_measured_caption_x(xs.clone()) {
        debug_log(&format!("caption anchors measured: {xs:?}"));
    }
    probe_dialog_buttons(doc);
}

/// The real geometry of dialog action buttons (`INF_UI_PROBE=1`): writes a
/// `[dialog]` line to shell.log.
///
/// Self-drawn modals have no Win32 handles, so UI automation used to guess
/// positions from "accent color blocks" — after primary/secondary buttons
/// were differentiated (primary fills the accent color, secondary a neutral
/// surface) that oracle broke. Here the same mechanism as the menu anchors
/// (`get_element_by_id` + `get_client_bounding_rect`, CSS px, consistent with
/// the click coordinate space) writes each button's center into the log;
/// tests only read the log, independent of theme, DPI and font.
///
/// Writes one line only when the content changed: the probe runs every idle
/// pass, and the log would otherwise be flooded.
fn probe_dialog_buttons(doc: &blitz_dom::BaseDocument) {
    if !winshell::shell::ui_probe_on() {
        return;
    }
    const MAX_BUTTONS: usize = 8;
    // The panel is centered with a transform, which the layout rect does not
    // include: the button's true position = layout position − half the panel
    // size (transform-origin defaults to the center).
    let panel = doc
        .get_element_by_id(&crate::ui::components::dialog::panel_dom_id())
        .and_then(|id| doc.get_client_bounding_rect(id));
    let Some(panel) = panel else {
        // No dialog → explicitly write a buttons=0 line. The old implementation
        // just returned, and the previous coordinate-bearing record stayed at
        // the end of the log, so after the test closed the box it would still
        // click blank space on that "ghost map". The labels are cleared too
        // (the clearing must be accepted by the setter, hence the bare write
        // rather than set_dialog_labels' probe-switch check).
        winshell::shell::set_dialog_labels(Vec::new());
        emit_probe_line("[dialog] buttons=0");
        return;
    };
    let (dx, dy) = (panel.width / 2.0, panel.height / 2.0);
    let mut centers: Vec<(f64, f64)> = Vec::new();
    for idx in 0..MAX_BUTTONS {
        let id = crate::ui::components::dialog::action_button_dom_id(idx);
        let Some(node_id) = doc.get_element_by_id(&id) else {
            break;
        };
        let Some(r) = doc.get_client_bounding_rect(node_id) else {
            // Layout not ready yet: skip this round (retry on the next idle
            // pass), so "box exists but no geometry" is not misreported as
            // buttons=0 overwriting a valid previous record.
            return;
        };
        centers.push((r.x + r.width / 2.0 - dx, r.y + r.height / 2.0 - dy));
    }
    let line = if centers.is_empty() {
        "[dialog] buttons=0".to_string()
    } else {
        let cs: Vec<String> = centers
            .iter()
            .map(|(x, y)| format!("{x:.1},{y:.1}"))
            .collect();
        format!(
            "[dialog] buttons={} centers={} labels={}",
            centers.len(),
            cs.join(";"),
            winshell::shell::dialog_labels().join("|")
        )
    };
    emit_probe_line(&line);
}

/// Deduplicating probe write: unchanged content is not written (the probe
/// runs every idle pass and would otherwise flood the log).
fn emit_probe_line(line: &str) {
    use std::sync::Mutex;
    static LAST: Mutex<String> = Mutex::new(String::new());
    if let Ok(mut last) = LAST.lock() {
        if *last == line {
            return;
        }
        *last = line.to_string();
    }
    winshell::shell::probe_log(line);
}

/// Enter the multi-window event loop (the single entry point of `main`).
///
/// The order matters: the three hooks + the popup registry must all be in
/// place before `launch` — the document config reads the `NetProvider` when
/// the main window is created, and the popup registry is consulted at the
/// first `PopupShow`.
pub fn launch(app: fn() -> Element, attrs: winit::window::WindowAttributes) {
    // Resource fetching for MD preview <img> (local file:// and http/https,
    // with a scheme allowlist)
    winshell::shell::set_net_provider(std::sync::Arc::new(crate::ui::mdnet::InfNetProvider));
    // Where the diagnostic log lands (the config dir, not the cwd)
    winshell::shell::set_log_path(log_path);
    // Menu-bar caption anchor measurement (application-side callback, called
    // once per idle pass)
    winshell::shell::set_anchor_probe(probe_caption_anchors);
    // The four popups' Popup implementations (each defined in its own module:
    // id/title/min size/light-dark/root component/flush/cleanup/close)
    winshell::register_popups(&[&FIND_POPUP, &PREFS_POPUP, &NUM_POPUP, &ABOUT_POPUP]);
    // The main window's clear color follows the saved theme (only peeking at
    // the dark flag, one small file IO): the window becomes visible before
    // the first frame, and the renderer's default white clear is exactly what
    // causes the white flash when starting with a dark theme.
    let dark = crate::ui::settings::peek_dark();
    let t = if dark {
        &crate::highlight::theme::DARK
    } else {
        &crate::highlight::theme::LIGHT
    };
    winshell::shell::launch(app, attrs, parse_hex_bg(t.bg));
}

/// "#rrggbb" → renderer clear color (only for the startup clear color;
/// invalid input falls back to white).
fn parse_hex_bg(hex: &str) -> dioxus_native::Color {
    let h = hex.trim_start_matches('#');
    let v = |i: usize| u8::from_str_radix(h.get(i..i + 2).unwrap_or("ff"), 16).unwrap_or(255);
    dioxus_native::Color::from_rgba8(v(0), v(2), v(4), 255)
}
