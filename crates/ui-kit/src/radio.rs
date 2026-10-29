//! Radio primitive: a self-drawn ring + dot, while the **semantics** are
//! still carried by a real `<input type="radio">`.
//!
//! # Why self-draw
//!
//! blitz's native radio painting hard-codes the "gap inside the ring" to
//! pure white (`blitz-paint/src/render/form_controls.rs`: `Color::WHITE`),
//! and the unchecked outer ring is a fixed `GRAY` — under a dark theme every
//! radio is a glaring white dot in a white ring, and the ring color does not
//! follow the theme at all. The native painting offers no CSS knob (the
//! `accent-color` in the TODO comment is not implemented yet), so the look
//! was changed to self-drawn.
//!
//! # Boundary: native semantics, CSS for the look
//!
//! * **Semantics**: a real `<input type="radio">` — same-`name` exclusion is
//!   native in blitz (`handle_click`'s radio branch → `toggle_radio`),
//!   `<label>` clicks are forwarded to the input, the accesskit role remains
//!   RadioButton, and the `checked` attribute remains the controlled source of
//!   truth;
//! * **Look**: two ordinary `div`s — the ring (`.k-radio-ring`) and the dot
//!   (`.k-radio-dot`) — with all colors and sizes from the class stylesheet;
//!   hover / focus / checked / disabled are all standard CSS (ancestor
//!   `:hover`, input `:focus` + adjacent sibling).
//!
//! The native input is squashed to **0×0 and `opacity:0`** (`.k-radio`): 0×0
//! keeps it out of the inline flow, and `opacity:0` silences blitz's native
//! widget painting completely — zero size alone is not enough, it still
//! paints a small color block to the left of the ring (a pale blue once
//! focused); while `display:none` / `visibility:hidden` would remove the input
//! from the accessibility tree and cannot be used. Clicks on the ring or the
//! text are forwarded by the `<label>` to the input, and native exclusion
//! works as usual.
//!
//! The accent color still comes from the theme's `accent` (checked ring +
//! dot); the unchecked ring uses the "secondary foreground" token — a medium
//! gray in the dark palette (#7d8590), #8a8a8a in the light one, with enough
//! contrast in both themes and no "glow" in the dark like fixed white.

use dioxus_native::prelude::*;

use crate::Theme;

/// A radio row. Rows sharing a `name` are mutually exclusive; `checked` is
/// the controlled state, clicks are reported upward.
#[component]
pub fn Radio(
    /// Radio group name (same-group exclusion, implemented natively by blitz)
    name: String,
    checked: bool,
    theme: Theme,
    on_pick: EventHandler<MouseEvent>,
    #[props(default)] label: String,
    #[props(default)] disabled: bool,
) -> Element {
    let _ = theme; // all colors come from the class stylesheet; the theme
                   // argument is kept to stabilize the API
    let row_style = if disabled { "opacity:0.45;" } else { "" };
    let ring_class = if checked {
        "k-radio-ring k-radio-on"
    } else {
        "k-radio-ring"
    };
    rsx! {
        label {
            class: "k-radio-row",
            style: "{row_style}",
            onpointerdown: move |evt| {
                // Stop press-drag text selection; the click goes to the native
                // radio default action.
                evt.stop_propagation();
                evt.prevent_default();
            },
            // on_pick is mounted on the **row container**: hitting the ring or
            // the text both trigger it (mounted on the input, a text click's
            // target is the label's text node, with no input on the chain, so
            // the handler never fires). Native exclusion (handle_click's radio
            // branch) is unaffected.
            onclick: move |evt| {
                evt.stop_propagation();
                if !disabled {
                    on_pick.call(evt);
                }
            },
            // Semantic widget: takes no space, paints nothing (see module
            // docs)
            input {
                class: "k-radio",
                r#type: "radio",
                name: "{name}",
                checked: checked,
                disabled: disabled.then_some(true),
            }
            // Look: the ring + the dot when checked
            div {
                class: "{ring_class}",
                if checked {
                    div { class: "k-radio-dot" }
                }
            }
            if !label.is_empty() {
                span { class: "k-check-label", "{label}" }
            }
        }
    }
}
