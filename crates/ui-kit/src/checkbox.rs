//! Checkbox primitive: a self-drawn box + a vector check mark, while the
//! **semantics** are still carried by a real `<input type="checkbox">`.
//!
//! # Why self-draw
//!
//! blitz's native checkbox painting hard-codes the "unchecked" fill to
//! `Color::WHITE` (`blitz-paint/src/render/form_controls.rs`) — under a dark
//! theme every unchecked checkbox is a pure-white square, brighter than any
//! theme color. The border can only use the element's `color`, so there is no
//! way to express "gray border when unchecked, blue fill when checked" at the
//! same time. The look is therefore self-drawn.
//!
//! # Boundary: native semantics, CSS + vector check mark for the look
//!
//! * **Semantics**: a real `<input type="checkbox">` — the `checked` attribute
//!   remains the controlled source of truth, and the accesskit role remains
//!   CheckBox; the input is squashed to 0×0 **and `opacity:0`**
//!   (`display:none` / `visibility:hidden` would remove the input from the
//!   accessibility tree; squashing the size alone still lets blitz paint a
//!   native color block), and all clicks are handled by the row;
//! * **Look**: the `.k-check-box` square + an SVG tick when checked. The tick
//!   is an **inline vector path** (same pipeline as the toolbar icons and the
//!   minimap's usvg), not a `✓` glyph — U+2713 is not in Consolas's cmap, and
//!   glyph fallback would render a tofu box.
//!
//! # Controlled semantics
//!
//! The default click action is intercepted by `prevent_default`: the checked
//! state is only ever written back from host state (an uncontrolled native
//! toggle would let the DOM and the host state permanently diverge when the
//! "host refuses the toggle"), and it also avoids blitz `click`'s default
//! action calling `clear_focus()` for clicks outside focusable targets.

use dioxus_native::prelude::*;

use crate::Theme;

/// The check tick: a **filled** 10×10 viewBox polygon (no stroke, to avoid
/// usvg's handling differences on stroke attributes). White on the accent
/// color, clear in both themes.
const TICK_SVG: &str = concat!(
    // `display:block` is mandatory: an inline `<svg>` lays out on the line
    // box baseline and gets pushed below the box by `line-height` (measured:
    // the tick sinks entirely to the box bottom and is clipped off).
    "<svg width=\"10\" height=\"10\" viewBox=\"0 0 10 10\" ",
    "style=\"display:block\" xmlns=\"http://www.w3.org/2000/svg\">",
    "<path fill=\"#ffffff\" d=\"M0.9 5.3 4.2 8.5 9.1 1.6 8.1 0.8 4.2 6.1 2 4Z\"/>",
    "</svg>"
);

/// The **purely visual** part of a check box: the square + the vector tick
/// when checked; no `<input>`, not interactive either (`pointer-events:none`,
/// clicks pass through to the host).
///
/// [`Checkbox`] reuses it internally; places like a menu's check column that
/// "only need a checkbox-looking mark whose clicks are handled by the host
/// row" use it directly too — so the two always look identical (14px box,
/// 3px radius, all colors from the class stylesheet).
#[component]
pub fn CheckMark(checked: bool) -> Element {
    let box_class = if checked {
        "k-check-box k-check-box-on"
    } else {
        "k-check-box"
    };
    rsx! {
        div {
            class: "{box_class}",
            aria_hidden: "true",
            if checked {
                // Inline SVG goes through `dangerous_inner_html`: an empty
                // string triggers an out-of-bounds in blitz's mutation writer
                // (see the comments in `ui_kit::style`), so this layer is only
                // mounted when checked.
                div { class: "k-check-tick", dangerous_inner_html: "{TICK_SVG}" }
            }
        }
    }
}

/// A checkbox (with optional label). `checked` is the controlled state; the
/// click callback is reported upward.
#[component]
pub fn Checkbox(
    checked: bool,
    theme: Theme,
    on_toggle: EventHandler<MouseEvent>,
    #[props(default)] label: String,
    #[props(default)] disabled: bool,
) -> Element {
    let _ = theme; // all colors come from the class stylesheet; the theme
                   // argument is kept to stabilize the API
    let row_style = if disabled { "opacity:0.45;" } else { "" };
    rsx! {
        div {
            class: "k-check",
            style: "{row_style}",
            onpointerdown: move |evt| {
                // Stop press-drag text selection; clicks uniformly go through
                // the row's onclick.
                evt.stop_propagation();
                evt.prevent_default();
            },
            onclick: move |evt| {
                evt.stop_propagation();
                // Controlled semantics: intercept the default action; the
                // checked state is only written back from host state.
                evt.prevent_default();
                if !disabled {
                    on_toggle.call(evt);
                }
            },
            // Semantic widget: takes no space, paints nothing (see module
            // docs)
            input {
                class: "k-cb",
                r#type: "checkbox",
                checked: checked,
                disabled: disabled.then_some(true),
            }
            // Look: the box + the vector tick when checked (same widget as
            // the menu's check column)
            CheckMark { checked }
            if !label.is_empty() {
                span { class: "k-check-label", "{label}" }
            }
        }
    }
}
