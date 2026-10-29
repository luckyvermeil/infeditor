//! Generic modal dialog shell.
//!
//! [`Modal`] only provides the "scrim + centered panel + title bar (title +
//! close button) + content slot + action row" shell — the content, buttons
//! and commands are filled in by the host (content via `children`, the action
//! row via the `actions` slot). It holds no business state.
//!
//! # Why dialogs are self-drawn
//!
//! A native message box would block the Blitz event loop and does not float
//! on top on frameless windows, so hosts always self-draw: a dimming scrim +
//! a centered panel, with `z-index` above other overlays.
//!
//! # Entrance animation
//!
//! Scrim fade-in + panel fade-in-and-scale (the Fluent "decelerate" curve).
//! **Only for the main window** — child windows (multi-window popups) running
//! animations would starve the message pump through blitz's per-frame
//! self-sustained redraws. The host must inject [`MODAL_CSS`] into the global
//! CSS (keyframes names are fixed, see the constant).
//!
//! # Two click contracts
//!
//! - Scrim `onpointerdown`: clicking outside closes when dismissable; the
//!   panel itself swallows presses (they do not reach the scrim).
//! - Every clickable element must `prevent_default()`: otherwise blitz's
//!   default click action makes the host's only keyboard focus anchor lose
//!   focus (iron rule 1 of the crate docs).

use dioxus_native::prelude::*;

use crate::Theme;

/// Scrim fade-in duration (ms).
const FADE_MS: u32 = 120;
/// Panel entrance duration (ms).
const PANEL_MS: u32 = 170;
/// The Fluent "decelerate" curve: fast out of the gate, gentle landing.
const EASE: &str = "cubic-bezier(0.1, 0.9, 0.2, 1)";

/// The CSS the host must inject (keyframes). Matches the animation names
/// referenced in this section.
pub const MODAL_CSS: &str = "
@keyframes overlay-fade { from { opacity: 0; } to { opacity: 1; } }
@keyframes dialog-in {
    from { opacity: 0; transform: translate(-50%, -50%) scale(0.96); }
    to   { opacity: 1; transform: translate(-50%, -50%) scale(1); }
}
";

/// A modal dialog shell.
///
/// `children` is the content slot between the title bar and the action row
/// (**not scrollable**: the host puts its own `flex:1 1 auto; overflow:auto`
/// container there to control scrolling). `actions` is an optional right-
/// aligned action row (with a top separator).
#[component]
pub fn Modal(
    title: String,
    theme: Theme,
    /// Accessibility text of the close button (the component library carries
    /// no localized text; the host passes it in).
    close_label: String,
    /// Close request (scrim click / title-bar × click).
    on_close: EventHandler<()>,
    /// Whether the scrim can be clicked to close (confirmation dialogs should
    /// pass false — an explicit answer is required).
    #[props(default = true)]
    dismissable: bool,
    /// Panel `z-index` (the scrim is one below).
    #[props(default = 1400)]
    z: i32,
    /// Panel minimum width (px).
    #[props(default = 380.0)]
    min_width: f32,
    children: Element,
    #[props(default)] actions: Option<Element>,
) -> Element {
    let t = theme;
    let (bg, border, dim, shadow) = (t.menu_bg, t.menu_border, t.dim_backdrop, t.shadow);
    let backdrop = format!(
        "position:absolute; left:0; top:0; right:0; bottom:0; z-index:{z}; \
         background:{dim}; animation: overlay-fade {FADE_MS}ms {EASE};"
    );
    let panel = format!(
        "position:absolute; left:50%; top:50%; transform:translate(-50%,-50%); \
         min-width:{min_width}px; max-width:76vw; max-height:78vh; z-index:{z1}; \
         background:{bg}; border:1px solid {border}; border-radius:6px; \
         box-shadow:0 8px 32px {shadow}; display:flex; flex-direction:column; \
         overflow:hidden; animation: dialog-in {PANEL_MS}ms {EASE};",
        z1 = z + 1
    );
    let title_style = format!(
        "display:flex; align-items:center; height:34px; padding:0 4px 0 14px; \
         border-bottom:1px solid {border}; flex:none;"
    );
    let title_fg = t.menu_fg;
    let title_text = format!("flex:1 1 auto; font-size:13px; font-weight:600; color:{title_fg};");
    let body_style = "display:flex; flex-direction:column; flex:1 1 auto; min-height:0;";
    let action_row: Option<Element> = actions.map(|a| {
        rsx! {
            div {
                style: "display:flex; justify-content:flex-end; align-items:center; height:46px;
                        padding:0 14px; border-top:1px solid {border}; flex:none;",
                { a }
            }
        }
    });
    rsx! {
        div {
            style: "{backdrop}",
            onpointerdown: move |evt| {
                evt.prevent_default();
                if dismissable {
                    on_close.call(());
                }
            },
        }
        div {
            style: "{panel}",
            onpointerdown: move |evt| {
                evt.stop_propagation();
                evt.prevent_default();
            },
            div {
                style: "{title_style}",
                div { style: "{title_text}", "{title}" }
                ModalClose { theme: t, close_label, on_close }
            }
            div { style: "{body_style}", { children } }
            { action_row }
        }
    }
}

/// The title bar's `×`: a real `<button class="k-close">`; the three states
/// (red on hover) are driven by the class stylesheet's `:hover`/`:active`.
#[component]
fn ModalClose(theme: Theme, close_label: String, on_close: EventHandler<()>) -> Element {
    let _ = theme; // all colors come from the class stylesheet; the theme
                   // argument is kept to stabilize the API
    rsx! {
        button {
            class: "k-close",
            onpointerdown: move |evt| {
                evt.stop_propagation();
                evt.prevent_default();
            },
            onclick: move |evt| {
                evt.stop_propagation();
                evt.prevent_default();
                on_close.call(());
            },
            span { aria_hidden: "true", "\u{00D7}" }
            span { style: "font-size:0; line-height:0;", {close_label} }
        }
    }
}
