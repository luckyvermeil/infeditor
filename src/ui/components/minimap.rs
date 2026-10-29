//! The VS Code-style minimap: a bird's-eye view of the whole document on the
//! right of the text column.
//!
//! Real minimaps draw the document a second time at ~2px, in token colours.
//! Blitz can only draw boxes, so each line is collapsed into a handful of
//! horizontal *ink* bars — runs of non-whitespace with the indentation left
//! blank. That keeps the thing a minimap is actually for: you navigate by
//! **shape** (where the dense blocks are, where the blank stretches are, how
//! the indentation steps), not by reading the text.
//!
//! Layout: the strip takes space out of the text column like the scrollbars do
//! (see [`EditorState::mm_w`]), sits between the text and the vertical bar, and
//! is exactly as tall as the text area.
//!
//! # The ink is one SVG, not a pile of divs
//!
//! One screenful is 257 lines × a few ink bars each; drawing every bar as an absolutely
//! positioned div would put more DOM nodes in the minimap than in the body viewport (771+), and
//! each extra bar measured ≈ +1.4ms/frame. Now all rectangles are packed into **one SVG path**
//! (one subpath per ink bar) hung on a single child div under the strip: DOM nodes collapse to
//! 1, and painting runs through blitz's usvg pipeline (the same one as toolbar icons and MD
//! formulas). The geometry (x/y/w) is pixel-for-pixel identical to the old drawing.
//!
//! Interaction, all of it on the strip itself:
//!
//! * a press jumps — the pointed line lands in the middle of the viewport —
//!   and then follows the pointer until release;
//! * the wheel scrolls the document, in the same direction as over the text;
//! * the translucent box is the viewport indicator ("you are here").
//!
//! Every handler calls `prevent_default()`, because an un-cancelled press makes
//! blitz blur the composer that is the editor's only keyboard target, and
//! `stop_propagation()` so the viewport underneath does not also run its hit
//! test and move the caret.

use dioxus_native::prelude::*;

use crate::ui::minimap::{MM_LH, MM_W};
use crate::ui::state::{EditorState, MINIMAP_W};

/// Height of one ink bar (px).
///
/// One px shorter than the 3px row so consecutive lines stay readable as
/// separate lines instead of fusing into a solid block — the row *rhythm* is
/// half of what makes a minimap scannable.
const BAR_H: f32 = 2.0;

/// Width of a search-match tick (px), pinned to the right edge of the strip.
const TICK_W: f32 = 3.0;

/// Lines moved per wheel notch, matching the text viewport.
const WHEEL_LINES: f32 = 3.0;

/// The minimap, drawn over the right edge of the viewport.
#[component]
pub fn Minimap() -> Element {
    let mut state = use_context::<Signal<EditorState>>();

    // Geometry, theme and the ink model are read in one guard: the model walks
    // every painted row of the document, and holding a `write()` across it
    // would mark the signal dirty twice.
    let (left, w, h, theme, geom, model, lh) = {
        let st = state.read();
        if !st.mm_on() {
            return rsx! {};
        }
        (
            st.mm_left(),
            st.mm_w(),
            st.mm_h(),
            st.theme(),
            st.mm_geom(),
            st.mm_model(),
            st.metrics.lh,
        )
    };

    // `position:absolute` children resolve against *this* box, not the
    // viewport: every coordinate below is therefore strip-relative. Getting
    // that wrong is what made the scrollbar's thumb invisible.
    //
    // No border and no separate background: VS Code's minimap is just the
    // editor surface scaled down — a ruled-off strip reads as a second panel.
    let strip_style = format!(
        "position:absolute; left:{left}px; top:0px; width:{w}px; height:{h}px; \
         box-sizing:border-box; overflow:hidden; background:{bg}; \
         user-select:none; cursor:default; touch-action:none; z-index:15;",
        bg = theme.mm_bg,
    );
    let ink = theme.mm_ink;
    let cur_bg = theme.mm_cur;
    let hit_bg = theme.match_hi;
    let tick_x = MM_W - TICK_W - 1.0;

    let (sy, sh) = geom.slider;
    // Hover coloring lives in `.mm-strip:hover .mm-slider` (components::style) — a child
    // follows the parent's hover, so no signal machine is mounted per slider.
    let slider_style = format!(
        "position:absolute; left:0px; top:{sy}px; width:{w}px; height:{sh}px; \
         pointer-events:none; z-index:6;"
    );

    // ---- ink / current line / hit ticks → one SVG ----
    // Rationale in the rsx comments below: the old drawing used one div per ink bar, and node
    // count was the frame cost.
    use std::fmt::Write as _;
    let mut ink_d = String::new();
    for b in model.bars.iter() {
        let _ = write!(ink_d, "M{} {}h{}v{}h{}z", b.x, b.y, b.w, BAR_H, -b.w);
    }
    let mut tick_d = String::new();
    for y in model.hits.iter() {
        let _ = write!(
            tick_d,
            "M{tick_x} {y}h{TICK_W}v{BAR_H}h{wneg}",
            wneg = -TICK_W
        );
    }
    let mut ink_svg = format!(
        "<svg width=\"{w}\" height=\"{h}\" viewBox=\"0 0 {w} {h}\" \
         xmlns=\"http://www.w3.org/2000/svg\">"
    );
    if let Some(y) = model.cur_y {
        let _ = write!(
            ink_svg,
            "<rect x=\"0\" y=\"{y}\" width=\"{w}\" height=\"{mm}\" fill=\"{cur_bg}\"/>",
            mm = MM_LH
        );
    }
    if !ink_d.is_empty() {
        let _ = write!(ink_svg, "<path d=\"{ink_d}\" fill=\"{ink}\"/>");
    }
    if !tick_d.is_empty() {
        let _ = write!(ink_svg, "<path d=\"{tick_d}\" fill=\"{hit_bg}\"/>");
    }
    ink_svg.push_str("</svg>");

    rsx! {
        div {
            class: "mm-strip",
            style: "{strip_style}",
            onpointerdown: move |evt| {
                evt.stop_propagation();
                evt.prevent_default();
                // A right press opens the context menu on button-up and must
                // leave the view exactly where the user left it.
                if !super::composer::is_primary_press(&*evt.data) {
                    return;
                }
                let cp = evt.data.client_coordinates();
                let mut st = state.write();
                let vy = st.view.y as f64;
                st.menu.open = None;
                st.mm_click(cp.y - vy);
            },
            // Conditional write: pointer-up with no drag does not mark dirty
            onpointerup: move |_| {
                // Conditional write (update_if): pointer-up with no drag does not mark dirty
                crate::ui::state::update_if(
                    &mut state,
                    |st| st.mm_drag.is_some(),
                    |st| st.mm_end(),
                );
            },
            onwheel: move |evt| {
                evt.stop_propagation();
                let dy = match evt.data.delta() {
                    dioxus_elements::geometry::WheelDelta::Pixels(v) => v.y as f32,
                    dioxus_elements::geometry::WheelDelta::Lines(v) => {
                        v.y as f32 * lh * WHEEL_LINES
                    }
                    dioxus_elements::geometry::WheelDelta::Pages(v) => v.y as f32 * h,
                };
                if dy == 0.0 {
                    return; // empty wheel events are not queued
                }
                // One scroll queue shared with the body viewport (see `ui::scroll`): the many
                // events of one wheel gesture fuse into one frame; nothing queues at a bound
                // (nothing to scroll = no frame scheduled).
                let can = state.read().can_scroll(0.0, dy as f64);
                if can
                    && crate::ui::scroll::queue(crate::ui::scroll::Target::Doc, 0.0, dy as f64)
                {
                    state.read().request_frame();
                }
            },

            // ---- ink / current line / hit ticks: **one** inline SVG ----
            //
            // With the old one-absolutely-positioned-div-per-ink-bar drawing, DOM node count
            // *was* the frame cost: measured (60-notch wheel, a 300k-line document) each extra
            // ink bar/line ≈ +1.4ms/frame (1 bar 12.8ms, 5 bars 19.0ms). The minimap shows 257
            // lines × 3 bars per screenful = 771 nodes — more expensive than the entire body
            // viewport. Packing everything into one SVG path (one subpath per bar) collapses the
            // DOM to 1 node, paints through blitz's usvg pipeline (same as toolbar icons and MD
            // formulas), and leaves positions and rectangle geometry exactly unchanged.
            //
            // `dangerous_inner_html` must not be the empty string (blitz's mutation writer
            // replaces empty inner_html children with a zero node and the next template
            // placeholder panics out of bounds), so the `<svg>` shell is always present — an
            // empty path just draws nothing.
            div {
                style: "position:absolute; left:0px; top:0px; width:{w}px; height:{h}px; \
                        z-index:2; pointer-events:none;",
                dangerous_inner_html: "{ink_svg}",
            }

            // ---- viewport indicator ----
            div { class: "mm-slider", style: "{slider_style}" }
        }
    }
}

/// Compile-time proof that the strip's width constant and the one the geometry
/// uses cannot drift apart: `MINIMAP_W` exists because `state` must not depend
/// on the view-side `minimap` module.
const _: () = assert!(MINIMAP_W == MM_W);
