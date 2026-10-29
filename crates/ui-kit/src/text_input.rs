//! Single-line text input primitive (a **display widget**).
//!
//! [`TextInput`] **only renders the buffer**: character insertion, caret
//! movement, and IME composition are all routed by the host (the app keeps a
//! single transparent textarea as the real focus anchor — the composer
//! pattern — and draws the caret itself). The component holds no focus and
//! does not accept click-to-focus: in such hosts focus is a global resource,
//! and the input is just a visualization of "the box the composer is typing
//! into".
//!
//! For text inputs that need real interaction (native caret/selection/IME),
//! render a standard `<input>` directly — see `PopField` in the find dialog
//! (`ui::findwin`): a real `<input>` + controlled `value` attribute +
//! pass-through `oninput`. When focused, the border switches to the accent
//! color and a self-drawn block caret is shown (the real caret belongs to
//! the host's transparent textarea).

use dioxus_native::prelude::*;

use crate::motion;
use crate::tokens::FONT_MONO;
use crate::Theme;

/// A single-line text input (renders the buffer only; input is routed by the host).
#[component]
pub fn TextInput(
    value: String,
    #[props(default)] hint: String,
    #[props(default)] focused: bool,
    theme: Theme,
    #[props(default = true)] mono: bool,
) -> Element {
    let t = theme;
    let empty = value.is_empty();
    let shown = if empty { hint.as_str() } else { value.as_str() };
    // Placeholder text uses the "secondary foreground" token: `input_border`
    // is a border color, and in dark palettes it can sit at the same level as
    // the input background (the old DARK palette did exactly that), which
    // would make the placeholder entirely invisible.
    let color = if empty { t.gutter_fg } else { t.menu_fg };
    let border = if focused {
        t.focus_ring
    } else {
        t.input_border
    };
    let caret = if focused {
        let c = t.accent;
        rsx! {
            span { style: "color:{c};", "\u{258C}" }
        }
    } else {
        rsx! {}
    };
    let font = if mono {
        FONT_MONO
    } else {
        crate::tokens::FONT_UI
    };
    let style = format!(
        "display:flex; align-items:center; height:26px; padding:0 8px;
         font-size:12px; font-family:{font}; background:{bg};
         border:1px solid {border}; border-radius:3px; color:{color};
         white-space:pre; overflow:hidden; flex:none; user-select:none; {anim}",
        bg = t.input_bg,
        anim = motion::fast("border-color, color")
    );
    rsx! {
        div {
            style: "{style}",
            span { "{shown}" }
            { caret }
        }
    }
}
