//! Edge/corner resize strips for the frameless window on non-Windows platforms.
//!
//! The window is created with `decorations(false)` (see `main.rs`), so there is
//! no native frame. Windows keeps real non-client borders: the left/right/bottom
//! resize bands are `WM_NCCALCSIZE`'d non-client area and `DefWindowProc`
//! answers the hit test, and `ui::snap` covers the top band — no DOM is
//! involved.
//!
//! winit platforms have no such hook. Without a hit area, a borderless window
//! can only be resized from the Window menu ("Size"), which is not how anyone
//! resizes a window. These eight transparent strips are that missing border:
//! each press hands the gesture to winit's `drag_resize_window`, i.e. the
//! window manager's own interactive resize loop (edge snapping and the correct
//! cursor come with it, exactly as `drag_window` does for moving).
//!
//! Windows deliberately renders nothing here — a DOM strip would sit *inside*
//! the client area and steal clicks from the real border and the title-bar
//! buttons.
//!
//! The layer sits below the modal dialog overlay (1400) on purpose: while a
//! modal is up, its dim layer owns the whole window, edges included.

use dioxus_native::prelude::*;
use dioxus_native::winit::window::ResizeDirection;

use crate::ui::state::{EditorState, WINBTN_W};

/// Thickness of the edge strips (CSS px). Thinner than the native Windows
/// border (8px) so the strips take as little as possible from the content /
/// title-bar buttons they overlap.
const EDGE: f32 = 4.0;
/// Size of the corner handles (CSS px).
const CORNER: f32 = 8.0;

/// The transparent edge/corner strips (see the module docs).
#[component]
pub fn ResizeEdges() -> Element {
    let state = use_context::<Signal<EditorState>>();
    // A maximized window has no edges to drag (native frames behave the same).
    // The memo keeps this component out of the per-keystroke re-render path:
    // reading the state signal directly would make every editor edit re-run
    // this body (eight fresh style strings) even though only the maximized flag
    // matters here.
    let maximized = use_memo(move || state.read().win.maximized);
    if cfg!(windows) || *maximized.read() {
        return rsx! {};
    }

    let e = EDGE;
    let c = CORNER;
    // The window-control cluster is flush against the right edge. The top
    // strip stops short of it (a native frame's caption buttons own that
    // corner too); the right strip still runs the full height, which only
    // costs the buttons their outermost 4px.
    let buttons = 3.0 * WINBTN_W;

    // Edges first, corners second: with one z-index, later siblings win the
    // overlap, so the diagonal handles are on top at the corners.
    let strips = [
        (
            "n",
            format!("left:{c}px; right:{buttons}px; top:0; height:{e}px; cursor:n-resize;"),
            ResizeDirection::North,
        ),
        (
            "s",
            format!("left:{c}px; right:{c}px; bottom:0; height:{e}px; cursor:s-resize;"),
            ResizeDirection::South,
        ),
        (
            "w",
            format!("top:{c}px; bottom:{c}px; left:0; width:{e}px; cursor:w-resize;"),
            ResizeDirection::West,
        ),
        (
            "e",
            format!("top:{c}px; bottom:{c}px; right:0; width:{e}px; cursor:e-resize;"),
            ResizeDirection::East,
        ),
        (
            "nw",
            format!("left:0; top:0; width:{c}px; height:{c}px; cursor:nw-resize;"),
            ResizeDirection::NorthWest,
        ),
        (
            "ne",
            format!("right:0; top:0; width:{c}px; height:{c}px; cursor:ne-resize;"),
            ResizeDirection::NorthEast,
        ),
        (
            "sw",
            format!("left:0; bottom:0; width:{c}px; height:{c}px; cursor:sw-resize;"),
            ResizeDirection::SouthWest,
        ),
        (
            "se",
            format!("right:0; bottom:0; width:{c}px; height:{c}px; cursor:se-resize;"),
            ResizeDirection::SouthEast,
        ),
    ];

    rsx! {
        for (name, pos, dir) in strips {
            div {
                key: "{name}",
                // Below the dialog overlay (1400), above menus (900) and the
                // title bar (50): the edge has to win over both.
                style: "position:absolute; z-index:1390; {pos}",
                onpointerdown: move |evt| {
                    // The press must not reach the root (composer refocus) or
                    // select text under the strip.
                    evt.stop_propagation();
                    evt.prevent_default();
                    winshell::win::frameless::start_resize(dir);
                },
            }
        }
    }
}
