//! The document outline panel (the entry list of the sidebar's "Outline" panel).
//!
//! # Rendering
//!
//! Pure read-snapshot mode (same as `sidebar.rs` / `tabs.rs`): a single `state.read()` copies out
//! the items, highlight index and theme colors, then renders outside the block. The data comes from
//! [`EditorState::outline_items`]' prebuilt cache ([`crate::ui::state::outline`]); before rendering
//! only the two-phase probe runs once
//! ([`EditorState::outline_build_needed`] / [`EditorState::outline_rebuild`]):
//! the cache rebuilds only when the document gen invalidates it — zero writes during steady-state
//! rendering (the heal iron rule).
//!
//! # Interaction
//!
//! * Row click = jump to the heading's source line ([`EditorState::outline_click`] branches
//!   internally across text/preview/wys — see the `state::outline` module comment)
//! * The active item (the section under the caret) is bolded and follows the caret
//! * Empty states: non-Markdown / no headings / oversized document each get their own hint
//!
//! The panel frame (geometry, header, panel-switch buttons) lives in [`super::sidebar::SideBar`];
//! this module owns only the entry list.
//!
//! # Manual verification (beyond what headless GUI render tests cover)
//!
//! Open a .md with multi-level headings → View → Document Outline: entries indent by H1–H6;
//! move the caret through the body and the matching section bolds; click an entry to jump to its
//! heading line (the caret lands in text mode, preview mode scrolls to the block, wys mode puts the
//! caret on it); switch to a non-Markdown tab to see "Only Markdown documents support the outline".

use std::rc::Rc;

use dioxus_native::prelude::*;

use crate::highlight::Theme;
use crate::ui::state::outline::outline_active_index;
use crate::ui::state::{EditorState, OutlineItem};
use crate::ui::Msg;

/// Row height (px, same as `sidebar.rs`).
const ROW_H: f32 = 22.0;
/// Indent per heading level (px).
const INDENT: f32 = 14.0;
/// In-row font size (px), same step as the explorer tree.
const ROW_FS: f32 = 13.0;

/// A read-only snapshot for one frame.
struct OutlineSnap {
    items: Rc<Vec<OutlineItem>>,
    /// Index of the item for the caret's section (active highlight)
    active: Option<usize>,
    /// Not a Markdown document (the panel shows its empty state)
    markdown: bool,
    /// Document oversized; parsing disabled
    too_large: bool,
    theme: &'static Theme,
}

#[component]
pub fn OutlinePanel() -> Element {
    let mut state = use_context::<Signal<EditorState>>();

    // Two-phase conditional write (the heal iron rule): the cache rebuilds only when gen
    // invalidates it; probe and fix early-exit identically, so this write never happens in
    // steady state.
    if state.read().outline_build_needed() {
        state.write().outline_rebuild();
    }

    let snap = {
        let st = state.read();
        let items = st.outline_items();
        let active = outline_active_index(&items, st.md_caret_line());
        OutlineSnap {
            items,
            active,
            markdown: st.spec().name == "Markdown",
            too_large: st.outline_too_large(),
            theme: st.theme(),
        }
    };
    let t = snap.theme;
    let rows_style = "flex:1 1 auto; overflow-y:auto; overflow-x:hidden; padding:2px 0 6px;";
    // Empty-state text (non-Markdown / no headings / oversized document)
    let empty = if !snap.markdown {
        state.read().tr(Msg::OutlineUnsupported).into_owned()
    } else if snap.too_large {
        state.read().tr(Msg::OutlineTooLarge).into_owned()
    } else {
        state.read().tr(Msg::OutlineNoHeadings).into_owned()
    };

    rsx! {
        div { style: "{rows_style}",
            if snap.items.is_empty() {
                div {
                    style: "padding:10px 12px; color:{t.menu_fg}; opacity:0.7;
                            font-size:{ROW_FS}px; line-height:1.6;",
                    "{empty}"
                }
            }
            for (i, item) in snap.items.iter().enumerate() {
                OutlineRow {
                    key: "{i}",
                    state,
                    index: i,
                    item: item.clone(),
                    active: snap.active == Some(i),
                }
            }
        }
    }
}

/// One outline row: indent + heading text; click to jump.
///
/// Hover/selected states **share** the explorer tree's `.ws-row` class (`components::style`):
/// both panels in the sidebar use one set of row heights, indents and colors; writing them
/// separately would only drift.
#[component]
fn OutlineRow(
    state: Signal<EditorState>,
    index: usize,
    item: OutlineItem,
    active: bool,
) -> Element {
    let indent = 6.0 + item.depth as f32 * INDENT;
    let class = if active { "ws-row on" } else { "ws-row" };
    // An empty heading (nothing after `#`) gets a placeholder, or the row looks broken
    let label = if item.title.is_empty() {
        state.read().tr(Msg::OutlineUntitled).into_owned()
    } else {
        item.title.clone()
    };

    rsx! {
        div {
            class: "{class}",
            role: "treeitem",
            style: "padding-left:{indent}px; height:{ROW_H}px;",
            title: "{label}",
            onpointerdown: move |evt| {
                // Per the app-root convention: once you prevent the click default you must
                // cancel it yourself, or the composer loses focus and the keyboard disconnects
                // (same as sidebar.rs's WsLine)
                evt.prevent_default();
            },
            onclick: move |evt| {
                evt.stop_propagation();
                evt.prevent_default();
                state.write().outline_click(index);
            },
            span { class: "ws-name", "{label}" }
        }
    }
}
