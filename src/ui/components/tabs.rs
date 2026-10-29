//! Tab bar: one tab per document, click to switch, dirty marker, close button.
//!
//! # Interaction
//!
//! Every clickable part (the tab body, `×`, `+`) has its tri-state in the stylesheet (`.tab` / `.tab-btn`,
//! see `components::style`): blitz's stylo parses `:hover` / `:active` natively, so no
//! `Press` signal machine is mounted per button. A hover triggers a single style recalc, so
//! the strip does not re-render wholesale even with dozens of tabs under the mouse.
//!
//! The `×` and `+` are deliberately larger than a text glyph: they are the only
//! way to close a tab or add one, and at the old 12–14px they were hard to hit
//! without aiming. They also grow their hit area past the glyph, which is what
//! makes them usable on a 28px strip.

use dioxus_native::prelude::*;

use dioxus_elements::input_data::MouseButton;

use crate::ui::state::{EditorState, TABS_H, TITLEBAR_H};
use crate::ui::Msg;

/// The square hit area of `×` / `+` lives in the `.tab-btn` class (22px: bigger than the glyph, still inside the 28px
/// tab strip).
/// Font size of the close glyph.
const CLOSE_FS: f32 = 17.0;
/// Font size of the new-tab glyph.
const NEW_FS: f32 = 18.0;

/// A square icon button (tri-state in the `.tab-btn` class).
///
/// `name` is the accessibility name: the accesskit tree names nodes from text children (blitz does not support
/// `aria-label` yet), so the glyphs (× / +) are wrapped in `aria-hidden` to keep the name clean.
#[component]
fn TabIcon(
    state: Signal<EditorState>,
    glyph: String,
    name: Msg,
    font_size: f32,
    on_click: EventHandler<MouseEvent>,
) -> Element {
    let mut tip: Signal<crate::ui::components::Tip> = use_context();
    let style = format!("font-size:{font_size}px; line-height:1;");
    let name = state.read().tr(name).into_owned();
    rsx! {
        div {
            class: "tab-btn",
            role: "button",
            style: "{style}",
            onmouseenter: move |evt| {
                let c = evt.data.client_coordinates();
                tip.write().show(name.clone(), c.x, c.y);
            },
            onmouseleave: move |_| tip.write().hide(),
            onpointerdown: move |evt| {
                tip.write().hide();
                // The press must not start a text selection, and — see the app
                // root — a hidden click has to cancel its own default action or
                // the composer loses focus and the keyboard dies.
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

/// One document tab: dirty dot, name, close button.
#[component]
fn Tab(
    state: Signal<EditorState>,
    index: usize,
    name: String,
    dirty: bool,
    active: bool,
) -> Element {
    let theme = { state.read().theme() };

    // The active tab is joined to the text area: no bottom border, so the
    // highlight reads as one continuous surface. Tri-state in the `.tab` / `.tab.on`
    // classes (components::style).
    let border = format!("border:1px solid {}; border-bottom:none;", theme.tab_border);

    let style = border;

    let dot_color = if dirty { theme.dirty_dot } else { theme.tab_fg };

    rsx! {
        div {
            class: if active { "tab on" } else { "tab" },
            role: "tab",
            style: "{style}",
            onpointerdown: move |evt| {
                // Pressing a tab hands the keyboard back to the document: the
                // composer never lost focus, but the *document* should own it
                // again so the caret paints and the selection is live.
                evt.prevent_default();
                // Middle click = close (the common convention), and it must not trigger
                // the system's press-and-hold scroll.
                if evt.data.trigger_button() == Some(MouseButton::Auxiliary) {
                    state.write().close_doc(index);
                }
            },
            onclick: move |_| {
                let mut st = state.write();
                // set_active_tab first flushes pending wys sync, then revalidates and re-seeds the singleton
                st.set_active_tab(index);
                st.menu_close();
                st.doc_focused = true;
            },
            if dirty {
                span { style: "color:{dot_color}; margin-right:6px;", "\u{25CF}" }
            }
            span { style: "flex:1; overflow:hidden; white-space:nowrap;", "{name}" }
            // The name comes from content: blitz-dom's name-from-content collects subtree text
            // recursively (except aria-hidden), and the visible file name is itself a readable
            // name — the zero-size duplicate added in round ten for the "direct text child"
            // workaround is no longer needed; keeping it would stutter the name as
            // "UnnamedUnnamedCloseTab".
            TabIcon {
                state,
                glyph: "\u{00D7}",
                name: Msg::WindowCloseTab,
                font_size: CLOSE_FS,
                on_click: move |evt: MouseEvent| {
                    evt.stop_propagation();
                    evt.prevent_default();
                    state.write().close_doc(index);
                },
            }
        }
    }
}

#[component]
pub fn TabBar() -> Element {
    let mut state = use_context::<Signal<EditorState>>();
    let (names, dirty, active) = {
        let st = state.read();
        let names: Vec<String> = st
            .tabs
            .iter()
            .map(|t| t.doc.meta.display_name.clone())
            .collect();
        let dirty: Vec<bool> = st.tabs.iter().map(|t| t.doc.is_dirty()).collect();
        (names, dirty, st.active)
    };
    let theme = { state.read().theme() };
    let bar_style = format!(
        "position:absolute; top:{TITLEBAR_H}px; left:0; right:0; height:{TABS_H}px; z-index:40;
         background:{}; display:flex; align-items:center;
         padding:0 2px; gap:1px; overflow:hidden; touch-action:none;
         user-select:none; border-bottom:1px solid {};",
        theme.tab_bg, theme.tab_border
    );
    // `touch-action:none`: a finger tap always drifts more than blitz's 2px
    // pan threshold, and a pan suppresses the `click` on release — tabs and
    // their buttons would ignore touch without it. The strip has nothing to
    // scroll (tab overflow clips, see the plan to scroll it *only* if it ever
    // grows a scroller). Same rationale as the title bar's declaration.

    rsx! {
        div {
            role: "tablist",
            style: "{bar_style}",

            for (i, name) in names.iter().enumerate() {
                Tab {
                    key: "{i}",
                    state,
                    index: i,
                    name: name.clone(),
                    dirty: dirty[i],
                    active: i == active,
                }
            }

            TabIcon {
                state,
                glyph: "+",
                name: Msg::TabNew,
                font_size: NEW_FS,
                on_click: move |_evt: MouseEvent| { state.write().new_doc(); },
            }
        }
    }
}
