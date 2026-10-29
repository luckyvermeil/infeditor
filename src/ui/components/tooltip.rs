//! Hover tooltips.
//!
//! blitz **does not render the HTML `title` attribute** (it only reaches the accessibility tree),
//! so tooltips have to be drawn by hand.
//! The shape follows the browser: hover for a moment and a small label appears; move the
//! pointer away and it is gone.
//!
//! # Why a standalone Signal instead of hanging it on EditorState
//!
//! Writing [`Tip`] on hover re-renders only [`TipLayer`] — the button side only writes,
//! it is not a subscriber. Hanging it on the giant `EditorState` degenerates into
//! "mouse sweeps the toolbar = whole-app re-render", exactly the cost retiring the
//! Press machinery was meant to remove.
//!
//! # How the button side uses it
//!
//! ```ignore
//! onmouseenter: move |evt| {
//!     let c = evt.data.client_coordinates();
//!     tip.write().show(tip_text("Bold", cmd), c.x, c.y);
//! },
//! onmouseleave: move |_| tip.write().hide(),
//! onpointerdown: move |_| tip.write().hide(),   // no tooltip should survive a press
//! ```
//!
//! The tip layer hangs on the application root (outside every `overflow:hidden`) — the toolbar is itself
//! a clipping container, so drawing the tip as a child of the button would clip it away.

use dioxus_native::prelude::*;

use crate::ui::actions::Cmd;
use crate::ui::state::EditorState;

/// One hover tip: text + anchor (client-area CSS px, taken from the pointer position).
#[derive(Clone, PartialEq, Default)]
pub struct Tip {
    text: String,
    x: f64,
    y: f64,
}

impl Tip {
    /// Show: record the text and pointer position (pointer position + offset = the tip's top-left corner).
    pub fn show(&mut self, text: impl Into<String>, x: f64, y: f64) {
        let text = text.into();
        // Re-hovering the same spot (or jittering by a pixel) does not mark dirty: content and anchor are unchanged.
        if self.text == text && (self.x - x).abs() < 0.5 && (self.y - y).abs() < 0.5 {
            return;
        }
        self.text = text;
        self.x = x;
        self.y = y;
    }

    /// Hide. No dirty mark when already hidden.
    pub fn hide(&mut self) {
        if !self.text.is_empty() {
            self.text.clear();
        }
    }

    pub fn visible(&self) -> bool {
        !self.text.is_empty()
    }
}

/// Button tooltip text: body + accelerator (no tail when there is no accelerator).
pub fn tip_text(label: &str, cmd: Cmd) -> String {
    let accel = crate::ui::shortcuts::accel_of(cmd);
    if accel.is_empty() {
        label.to_string()
    } else {
        format!("{label}  {accel}")
    }
}

/// The tip layer: it subscribes to [`Tip`] only, so it re-renders only when the tip changes. Window size is read with `peek`
/// (not subscribed — the clamping delta from a resize exists only on frames where the tip is visible, not worth
/// turning window resizes into a re-render source for the tip layer).
#[component]
pub fn TipLayer() -> Element {
    let tip = use_context::<Signal<Tip>>();
    let t = tip();
    if !t.visible() {
        return rsx! {};
    }
    // The anchor is client-area CSS px; leave slack for the tip's width at the right edge so it cannot slide out of the window.
    let (win_w, win_h) = {
        let st = use_context::<Signal<EditorState>>();
        let st = st.peek();
        (f64::from(st.win.w), f64::from(st.win.h))
    };
    let below = t.y + 22.0 < win_h - 40.0;
    let top = if below { t.y + 22.0 } else { t.y - 42.0 };
    let left = t.x.min((win_w - 240.0).max(8.0)).max(8.0);
    rsx! {
        div {
            class: "tip",
            style: "left:{left}px; top:{top}px;",
            "{t.text}"
        }
    }
}
