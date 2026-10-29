//! Separator primitive: a real `<hr>` element.
//!
//! `<hr>` is the standard HTML semantic for a thematic break; the UA styles
//! (inset border, 0.5em margin) are zeroed by the `.k-sep` class rule
//! (`border:none; margin:0`), and geometry and color go through inline styles
//! — inline beats class in specificity, so the `color` override parameter
//! still takes effect.
//!
//! Non-interactive element: `pointer-events:none` (in the class rule), so it
//! does not take part in hit-testing (and cannot swallow clicks below it).

use dioxus_native::prelude::*;

use crate::Theme;

/// A separator. `horizontal` defaults to a vertical line (between toolbar
/// groups); a horizontal line is for groupings inside a panel.
#[component]
pub fn Separator(
    theme: Theme,
    /// Length (height for a vertical line, width for a horizontal one), px.
    #[props(default = 18.0)]
    length: f32,
    #[props(default = false)] horizontal: bool,
    /// Explicit color override (defaults to the theme's menu border color).
    #[props(default)]
    color: Option<&'static str>,
) -> Element {
    let _ = theme; // the default color comes from the class stylesheet; the
                   // theme argument is kept to stabilize the API
                   // Vertical: 1px wide, the given height; horizontal: full-width
                   // 1px. The color defaults to the .k-sep class; on an explicit
                   // override the inline background wins (an empty string hands
                   // it fully back to the class).
    let bg = color
        .map(|c| format!("background:{c};"))
        .unwrap_or_default();
    let style = if horizontal {
        format!("width:100%; height:1px; {bg}")
    } else {
        format!("width:1px; height:{length}px; {bg}")
    };
    rsx! {
        hr { class: "k-sep", style: "{style}" }
    }
}
