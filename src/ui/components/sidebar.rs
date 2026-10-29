//! The left sidebar: a VS Code-style **single panel** (explorer directory tree / document outline).
//!
//! # Shape
//!
//! ```text
//! ┌──────────────────────────┐
//! │ Panel head (35px): title + actions │
//! ├──────────────────────────┤
//! │ Tree rows (22px, indent guides)     │
//! └──────────────────────────┘
//! ```
//!
//! **No icon strip**: the sidebar toggles all live at the status bar's lower left (the explorer /
//! outline icons of `components::status`), and collapsing zeroes the whole sidebar width (see
//! [`EditorState::sidebar_w`]). Outline opens only for Markdown
//! ([`EditorState::outline_supported`]); switching to another document hands the sidebar back to
//! the explorer via `set_active_tab`.
//!
//! # Rendering
//!
//! Pure read-snapshot mode (same as `tabs.rs`): one `state.read()` copies out the geometry, the
//! row cache and the active document path, and rendering happens outside the block. Row data comes
//! from [`EditorState::ws_rows`] — a precomputed flat cache (with indent depths and expanded
//! states); nothing is derived here.
//!
//! # Interaction and tri-state
//!
//! Row hover/selection and the header's action buttons all run on **CSS pseudo-classes** in the
//! `<style>` sheet (`components::style::AppStyle`): blitz's stylo supports `:hover` natively, so
//! rows need no `Press` machinery and no inline background — a hover no longer re-renders a
//! component, and even a tree of thousands of rows costs only one extra style recalc. The row
//! components therefore **read no signals at all** (colors all live in the stylesheet, row data
//! all in props).
//!
//! * Directory row click = collapse/expand (▸/▾; glyphs from the Consolas cmap, see the round-18 lesson)
//! * File row click = open in the background as a tab (`open_path_async` dedupes/focuses itself)
//! * Panel head: title + refresh (re-read the expanded levels) + × (close workspace / collapse panel)
//!
//! Scrolling is left to blitz's `overflow-y:auto`: rows are usually a few dozen to a few hundred,
//! native scrolling is plenty; virtualizing a directory of thousands of rows would be overengineering.

use std::path::PathBuf;
use std::rc::Rc;

use dioxus_native::prelude::*;

use crate::highlight::Theme;
use crate::ui::components::outline::OutlinePanel;
use crate::ui::state::{EditorState, SidePanel, WsRow};
use crate::ui::Msg;

/// Panel head height (px — VS Code's 35px panel title).
const HEADER_H: f32 = 35.0;
/// Indent per level (px).
const INDENT: f32 = 14.0;
/// Width of the twisty slot (px) — file rows keep the slot too so the names line up.
const ARROW_W: f32 = 16.0;
/// Leading padding of a row (px).
const ROW_PAD: f32 = 4.0;

/// A read-only snapshot for one frame.
struct SideSnap {
    y: f32,
    h: f32,
    w: f32,
    root: Option<String>,
    rows: Rc<Vec<WsRow>>,
    /// Path of the active document (used to highlight its file row in the sidebar)
    active: Option<PathBuf>,
    /// The current panel (workspace tree / document outline)
    panel: SidePanel,
    theme: &'static Theme,
}

#[component]
pub fn SideBar() -> Element {
    let mut state = use_context::<Signal<EditorState>>();

    let snap = {
        let st = state.read();
        let v = st.view;
        let w = st.sidebar_w();
        // A width of 0 means "no sidebar": nothing is rendered, so blitz has no panel to hit.
        if w <= 0.0 {
            return rsx! {};
        }
        SideSnap {
            // Flush against the window's left edge: overlap with the resize band is settled by
            // the "interactive component rectangle set" (components win, see
            // `EditorState::hit_exempt_rects`); the viewport no longer insets.
            y: v.y,
            h: v.h,
            w,
            root: st.ws_root.as_ref().map(|p| {
                p.file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| p.to_string_lossy().into_owned())
            }),
            rows: st.ws_rows.clone(),
            active: st.tabs.get(st.active).and_then(|t| t.doc.meta.path.clone()),
            panel: st.side_panel,
            theme: st.theme(),
        }
    };
    let t = snap.theme;
    let outline = snap.panel == SidePanel::Outline;
    // The panel head's text renders per frame (the snapshot carries only data): the outline
    // title, the × tooltip.
    let title = if outline {
        state.read().tr(Msg::SideOutline).into_owned()
    } else {
        snap.root.clone().unwrap_or_default()
    };
    // The head ×'s tooltip follows the panel (tree = close the workspace; outline = just
    // collapse the sidebar)
    let close_tip: String = if outline {
        state.read().tr(Msg::SideCollapse).into_owned()
    } else {
        state.read().tr(Msg::SideCloseWorkspace).into_owned()
    };
    let refresh_tip = state.read().tr(Msg::SideRefreshWorkspace).into_owned();
    let refresh_label = state.read().tr(Msg::SideRefresh).into_owned();

    let panel_style = format!(
        "position:absolute; left:0; top:{y}px; width:{w}px; height:{h}px;
         background:{bg}; border-right:1px solid {bd}; display:flex;
         flex-direction:column; overflow:hidden; z-index:30; user-select:none;",
        y = snap.y,
        h = snap.h,
        w = snap.w,
        bg = t.panel_bg,
        bd = t.border
    );
    let header_style = format!(
        "height:{HEADER_H}px; flex:0 0 auto; display:flex; align-items:center;
         padding-left:12px; padding-right:6px; gap:2px; overflow:hidden;
         border-bottom:1px solid {bd};",
        bd = t.border
    );
    // The row container needs a **definite height** to scroll (`h - panel head`, both known px).
    // It used to shrink on its own via `flex:1 1 auto`: a flex item's automatic minimum size
    // defaults to its content height, so too many rows grew it past the panel and the panel's
    // overflow:hidden clipped them — the box itself never overflowed, so `overflow-y:auto` only
    // clipped without scrolling (neither wheel nor finger could move it, and files further down
    // could not be clicked at all). `min-height:0` is the standard fix, but it does not work here
    // (see the test below), so the height is given explicitly: box height < content height →
    // blitz's scrollable overflow condition holds.
    let rows_style =
        "flex:1 1 auto; overflow-y:auto; overflow-x:hidden; padding:2px 0 6px;".to_string();

    rsx! {
        div { style: "{panel_style}",
            div { style: "{header_style}",
                span { class: "ws-title", "{title}" }
                if !outline {
                    div {
                        class: "ws-btn",
                        role: "button",
                        title: "{refresh_tip}",
                        onclick: move |evt| {
                            evt.stop_propagation();
                            evt.prevent_default();
                            state.write().ws_refresh();
                        },
                        "{refresh_label}"
                    }
                }
                div {
                    class: "ws-btn",
                    role: "button",
                    title: "{close_tip}",
                    onclick: move |evt| {
                        evt.stop_propagation();
                        evt.prevent_default();
                        if outline {
                            // The outline panel's × only collapses the sidebar; it does not close
                            // the workspace
                            state.write().sidebar_hide();
                        } else {
                            state.write().ws_close();
                        }
                    },
                    "\u{00D7}"
                }
            }
            if outline {
                OutlinePanel {}
            } else {
                div { style: "{rows_style}",
                    for (i, row) in snap.rows.iter().enumerate() {
                        WsLine {
                            key: "{i}",
                            state,
                            index: i,
                            row: row.clone(),
                            active: snap.active.as_deref() == Some(row.path.as_path()),
                        }
                    }
                }
            }
        }
    }
}

/// One sidebar row: indent guides + twisty + name. Directories and files share the row.
#[component]
fn WsLine(state: Signal<EditorState>, index: usize, row: WsRow, active: bool) -> Element {
    let indent = ROW_PAD + row.depth as f32 * INDENT;
    // VS Code draws the guides under each ancestor's twisty: adjacent sibling rows join into one
    // continuous vertical line. Depth caps at 8 levels — deeper trees are still readable from the
    // indent alone and do not deserve dozens of extra divs per row.
    let guides: Vec<f32> = (0..row.depth.min(8))
        .map(|d| ROW_PAD + d as f32 * INDENT + ARROW_W / 2.0)
        .collect();
    let arrow = if row.is_dir {
        if row.expanded {
            "\u{25BE}" // ▾
        } else {
            "\u{25B8}" // ▸
        }
    } else {
        ""
    };
    let class = if active { "ws-row on" } else { "ws-row" };

    rsx! {
        div {
            class: "{class}",
            role: "treeitem",
            style: "padding-left:{indent}px;",
            onpointerdown: move |evt| {
                // Per the app-root convention: once you prevent the click default you must
                // cancel it yourself, or the composer loses focus and the keyboard disconnects
                evt.prevent_default();
            },
            onclick: move |evt| {
                evt.stop_propagation();
                evt.prevent_default();
                state.write().ws_click(index);
            },
            for (i, gx) in guides.iter().enumerate() {
                div {
                    key: "g{i}",
                    class: "ws-guide",
                    style: "left:{gx}px;",
                }
            }
            span { class: "ws-caret", "{arrow}" }
            span { class: "ws-name", "{row.name}" }
        }
    }
}
