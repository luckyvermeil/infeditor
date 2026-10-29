//! Square icon button primitive: a real `<button class="k-ibtn">`.
//!
//! Converges "a square tap target + hover/press feedback + an accessibility
//! name" into one component. The three-state feedback is driven by the class
//! stylesheet (`:hover`/`:active`); the component holds no state signal.
//!
//! `name` is the accessibility name: the accesskit tree names elements by
//! their **text child nodes** (blitz does not yet honor a pure `aria-label`),
//! and the glyph is separately wrapped in `aria-hidden` so it does not pollute
//! the name — hence glyph and name each occupy their own span, the name hidden
//! with `font-size:0` without affecting naming.

use dioxus_native::prelude::*;

use crate::Theme;

/// A square icon button (hover/press feedback goes through the class
/// stylesheet).
#[component]
pub fn IconButton(
    glyph: String,
    name: &'static str,
    theme: Theme,
    on_click: EventHandler<MouseEvent>,
    #[props(default = 22.0)] size: f32,
    #[props(default = 15.0)] font_size: f32,
) -> Element {
    let _ = theme; // all colors come from the class stylesheet; the theme
                   // argument is kept to stabilize the API
    let style = format!("width:{size}px; height:{size}px; font-size:{font_size}px;");
    rsx! {
        button {
            class: "k-ibtn",
            style: "{style}",
            onpointerdown: move |evt| {
                evt.stop_propagation();
                evt.prevent_default();
            },
            onclick: move |evt| {
                evt.stop_propagation();
                evt.prevent_default();
                on_click.call(evt);
            },
            span { aria_hidden: "true", "{glyph}" }
            span { style: "font-size:0; line-height:0;", "{name}" }
        }
    }
}
