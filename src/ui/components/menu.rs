//! Menu bar plus its dropdown, drawn from the same [`Item`](crate::ui::actions::Item)
//! tree the native editor context menu uses (see [`crate::ui::actions`]).
//!
//! ## Why the dropdown is not nested in the caption
//!
//! The obvious implementation — `position:relative` on the caption, the panel
//! as its child — is broken here, and silently so. The menu bar needs a
//! `z-index` to paint above the editor viewport (both are positioned, and the
//! viewport comes later in the DOM). A `z-index` **creates a stacking
//! context**, so every descendant of the bar is now confined below the find bar
//! and the results panel no matter how large its own `z-index` grows: opening
//! Edit while the find bar is open draws the panel *behind* it.
//!
//! So the captions ([`crate::ui::components::titlebar`]) only render the bar, and
//! [`MenuOverlay`] renders the panel at the root of the app with a high `z-index`.
//! The anchor comes from the
//! click event, which reports both client and element coordinates, so the panel
//! lines up with its caption without measuring text.
//!
//! ## Height and scrolling
//!
//! The panel's rows are **absolutely positioned**, so they contribute nothing
//! to the panel's own height — the height has to be stated explicitly, and it
//! must be stated as `rows × ROW_H` with **no padding**: an absolutely
//! positioned child is laid out against the padding box, so a `padding:5px 0`
//! on the panel never offset the first row, it only added 10 px of dead space
//! under the last one (the "blank strip" bug).
//!
//! A menu can also be taller than the window (Language has dozens of entries).
//! Native menus clip themselves and grow a scrollbar, and so does this one:
//! the panel is clamped to [`panel_avail_h`], the rows are drawn at
//! `i·ROW_H − scroll`, and a scrollbar appears on the right edge whenever
//! [`max_scroll`] is non-zero. It is a real scrollbar — the wheel scrolls it,
//! the thumb drags, clicking the track pages, and the keyboard cursor keeps
//! itself visible ([`crate::ui::state::MenuUi::ensure_cursor_visible`]).
//!
//! ## Motion
//!
//! Hover and press feedback are inline styles, not `:hover` rules: a stylesheet
//! rule loses to the inline `background` the caption already sets, which is why
//! the light theme used to flash dark blue on hover. Inline styles also let
//! [`crate::ui::motion`] drive real CSS transitions, which blitz animates on its
//! own — `blitz-shell` keeps calling `request_redraw()` while
//! `Document::is_animating()` is true.

use dioxus_native::prelude::*;

use crate::highlight::Theme;
use crate::ui::actions;
use crate::ui::motion;
use crate::ui::state::{EditorState, MenuDrag, TITLEBAR_H};
use ui_kit::{thumb_geom, wheel_step};

/// Height of one dropdown row (px) — definition lives in [`crate::ui::state::menu`]
/// (the geometry's single source of truth lives in the state layer; re-exported here for the
/// render-side legacy paths).
pub use crate::ui::state::menu::{max_scroll, panel_avail_h, panel_height, visible_rows, ROW_H};
/// Panel width (px).
pub const PANEL_W: f32 = 252.0;
/// Width reserved for the check-mark column.
pub const CHECK_W: f32 = 22.0;
/// Left padding for a menu with no checkable rows at all (never leave an empty check column).
pub const MENU_PAD_L: f32 = 12.0;
/// Width of the scroll strip drawn on the right edge of a tall panel.
pub const SCROLLBAR_W: f32 = 9.0;
/// z-index of the panel; above the find bar (60) and the results panel (60).
const PANEL_Z: i32 = 900;

// ---------------------------------------------------------------------------
// Panel geometry — lives in state::menu; re-exported above. The scrollbar
// thumb / wheel arithmetic is shared with the standalone component library
// (`ui_kit::menu`), so the two panels cannot drift apart.
// ---------------------------------------------------------------------------

/// Panel chrome shared by the dropdown and the fly-out.
///
/// `touch-action:none` keeps a jittery finger tap a *tap*: without it a finger
/// drift past the slop makes blitz pan instead of click, and a menu row is a
/// one-shot command — the slop/`scrolled` rules (blitz patches thirteen/fourteen) would
/// rescue the tap, but a menu is the last place to bet on "the pan scrolled
/// nothing". The walk that resolves the effective value starts at the row and
/// passes through this panel, so one declaration covers every row.
///
/// The height passed in is the **drawn** height ([`panel_height`]), which
/// already accounts for the window: rows are absolutely positioned against the
/// padding box, so any `padding` here would only grow the panel — see the
/// module doc.
fn panel_style(top: f32, left: f32, h: f32, t: &'static Theme, z: i32) -> String {
    format!(
        "position:absolute; top:{top}px; left:{left}px; width:{PANEL_W}px; height:{h}px;
         z-index:{z}; background:{bg}; border:1px solid {border};
         border-radius:4px; box-shadow:0 6px 20px {shadow};
         overflow:hidden; touch-action:none; user-select:none; {anim}",
        bg = t.menu_bg,
        border = t.menu_border,
        shadow = t.shadow,
        anim = motion::normal("box-shadow")
    )
}

// The captions themselves moved to [`crate::ui::components::titlebar`]: the
// menu tree now lives inside the merged title bar, and what is left in this
// module is the root-level dropdown overlay they open.

// ---------------------------------------------------------------------------
// Dropdown overlay (root level)
// ---------------------------------------------------------------------------

#[component]
pub fn MenuOverlay() -> Element {
    let mut state = use_context::<Signal<EditorState>>();

    let (rows, sub_rows, anchor_x, cursor, hover, sub_open, hover_sub, win_w, win_h, t) = {
        let st = state.read();
        // The open caption is an identity (`Caption`) and the fly-out is
        // resolved from it. Label text is for painting only, never matched.
        let top = st.menu.open;
        let sub_rows = match (top, st.menu.sub.as_deref()) {
            (Some(cap), Some(_)) => actions::submenu_rows(&st, cap),
            _ => Vec::new(),
        };
        (
            actions::menu_rows(&st),
            (!sub_rows.is_empty()).then_some(sub_rows),
            st.menu.anchor_x,
            st.menu.cursor,
            st.menu.hover,
            st.menu.sub.clone(),
            st.menu.hover_sub,
            st.win.w,
            st.win.h,
            st.theme(),
        )
    };

    if rows.is_empty() {
        return rsx! {};
    }

    // The panel is clamped to the space below the caption, so it can never
    // "flip above" — there is no room above the caption to flip into. It still
    // slides left when it would overflow to the right.
    let content_h = rows.len() as f32 * ROW_H;
    let panel_h = panel_height(rows.len(), win_h);
    let max_s = max_scroll(rows.len(), win_h);
    let top = TITLEBAR_H;
    let left = anchor_x.min((win_w - PANEL_W).max(0.0));
    let scroll = state.read().menu.scroll.clamp(0.0, max_s);

    let mut main: Vec<RowGeom> = Vec::with_capacity(rows.len());
    for (i, r) in rows.iter().enumerate() {
        main.push(RowGeom {
            label: r.label.clone(),
            accel: r.accel.clone(),
            header: r.header,
            sep: r.sep,
            checkable: r.checkable,
            checked: r.checked,
            y: i as f32 * ROW_H - scroll,
            hot: hover == Some(i),
            cursor: cursor == i,
        });
    }

    // the fly-out sits beside the row that opened it, one z-step above, and
    // scrolls independently
    let fly: Option<Element> = match (sub_rows.as_ref(), sub_open.as_deref()) {
        (Some(sub), Some(cap)) => {
            let parent_idx = rows
                .iter()
                .position(|r| r.header && r.label == cap)
                .unwrap_or(0);
            let sub_content_h = sub.len() as f32 * ROW_H;
            let sub_h = panel_height(sub.len(), win_h);
            let sub_max = max_scroll(sub.len(), win_h);
            let sub_scroll = state.read().menu.sub_scroll.clamp(0.0, sub_max);
            // right of the parent panel
            let sx = left + PANEL_W - 4.0;
            // top of the *panel*, not of the cursor row: a scrolled fly-out
            // would otherwise be dragged up with its parent row
            let sy = (top + parent_idx as f32 * ROW_H)
                .min(win_h - sub_h - 2.0)
                .max(top);
            let mut rows_v: Vec<RowGeom> = Vec::with_capacity(sub.len());
            for (i, r) in sub.iter().enumerate() {
                rows_v.push(RowGeom {
                    label: r.label.clone(),
                    accel: r.accel.clone(),
                    header: r.header,
                    sep: r.sep,
                    checkable: r.checkable,
                    checked: r.checked,
                    y: i as f32 * ROW_H - sub_scroll,
                    hot: hover_sub == Some(i),
                    cursor: false,
                });
            }
            let sub_style = panel_style(sy, sx, sub_h, t, PANEL_Z + 1);
            Some(rsx! {
                div {
                    role: "menu",
                    style: "{sub_style}",
                    // leaving the fly-out closes it, like every native menu。
                    // Conditional write: hover_sub / sub_drag being empty already means no dirty
                    // mark.
                    onmouseleave: move |_| {
                        // Conditional write (update_if): hover_sub / sub_drag being empty already
                        // means no dirty mark
                        crate::ui::state::update_if(
                            &mut state,
                            |st| st.menu.hover_sub.is_some() || st.menu.sub_drag.is_some(),
                            |st| {
                                st.menu.hover_sub = None;
                                st.menu.sub_drag = None;
                            },
                        );
                    },
                    onwheel: move |evt| {
                        evt.stop_propagation();
                        let dy = wheel_delta_px(&evt);
                        let mut st = state.write();
                        st.menu.sub_scroll = (st.menu.sub_scroll - wheel_step(dy))
                            .clamp(0.0, sub_max);
                    },
                    onpointermove: move |evt| {
                        let y = evt.data.client_coordinates().y as f32;
                        let mut st = state.write();
                        if let Some(d) = st.menu.sub_drag {
                            st.menu.sub_scroll =
                                (d.start_scroll - (y - d.start_y) * d.per_px).clamp(0.0, d.max);
                        }
                    },
                    onpointerup: move |_| {
                        // Conditional write (update_if): no dirty mark when the lift has no drag
                        crate::ui::state::update_if(
                            &mut state,
                            |st| st.menu.sub_drag.is_some(),
                            |st| st.menu.sub_drag = None,
                        );
                    },
                    { scrollbar(state, t, ScrollGeom { panel_top: sy, panel_h: sub_h, content_h: sub_content_h, scroll: sub_scroll, max_s: sub_max }, true) }
                    { row_list(state, rows_v, true) }
                }
            })
        }
        _ => None,
    };

    let main_style = panel_style(top, left, panel_h, t, PANEL_Z);

    rsx! {
        div {
            role: "menu",
            style: "{main_style}",
            // leaving the panel clears the hover so the highlight does not stick
            // after the pointer travels into the fly-out. The conditional write avoids marking
            // dirty without a change.
            onmouseleave: move |_| {
                // Conditional write (update_if): the same reason as the fly-out above
                crate::ui::state::update_if(
                    &mut state,
                    |st| st.menu.hover.is_some() || st.menu.scroll_drag.is_some(),
                    |st| {
                        st.menu.hover = None;
                        st.menu.scroll_drag = None;
                    },
                );
            },
            onwheel: move |evt| {
                evt.stop_propagation();
                let dy = wheel_delta_px(&evt);
                let mut st = state.write();
                st.menu.scroll = (st.menu.scroll - wheel_step(dy)).clamp(0.0, max_s);
            },
            onpointermove: move |evt| {
                let y = evt.data.client_coordinates().y as f32;
                let mut st = state.write();
                if let Some(d) = st.menu.scroll_drag {
                    st.menu.scroll =
                        (d.start_scroll - (y - d.start_y) * d.per_px).clamp(0.0, d.max);
                }
            },
            onpointerup: move |_| {
                crate::ui::state::update_if(
                    &mut state,
                    |st| st.menu.scroll_drag.is_some(),
                    |st| st.menu.scroll_drag = None,
                );
            },
            { scrollbar(state, t, ScrollGeom { panel_top: top, panel_h, content_h, scroll, max_s }, false) }
            { row_list(state, main, false) }
        }
        {fly}
    }
}

/// One wheel event expressed in CSS px (positive = towards the top).
fn wheel_delta_px(evt: &Event<WheelData>) -> f32 {
    match evt.data.delta() {
        dioxus_elements::geometry::WheelDelta::Pixels(v) => v.y as f32,
        dioxus_elements::geometry::WheelDelta::Lines(v) => v.y as f32 * ROW_H,
        dioxus_elements::geometry::WheelDelta::Pages(v) => v.y as f32 * ROW_H * 8.0,
    }
}

// ---------------------------------------------------------------------------
// menu bar overflow list (the more button)
// ---------------------------------------------------------------------------

/// The menubar's overflow list: captions that no longer fit the window stack vertically here
/// (a root-level overlay at the same level as the dropdowns, dodging the title bar's stacking
/// context).
///
/// Behavior mirrors a browser toolbar's `»` overflow menu: clicking an entry opens that
/// caption's dropdown (anchored near the more button — a hidden caption has no measurable
/// position to begin with).
#[component]
pub fn MoreMenu() -> Element {
    let state = use_context::<Signal<EditorState>>();

    let (open, more_from, win_w, win_h, t) = {
        let st = state.read();
        (
            st.menu.more_open,
            st.menu.more_from,
            st.win.w,
            st.win.h,
            st.theme(),
        )
    };
    if !open {
        return rsx! {};
    }
    // (identity, label in the current language): the identity drives commands.
    let caps: Vec<(actions::Caption, String)> = {
        let st = state.read();
        actions::menu_bar_items(&st)
            .into_iter()
            .zip(actions::Caption::ALL)
            .skip(more_from)
            .map(|(it, cap)| (cap, it.label().to_string()))
            .collect()
    };
    if caps.is_empty() {
        return rsx! {};
    }
    let panel_h = panel_height(caps.len(), win_h);
    // Anchor to the first hidden caption's estimated position (it is not rendered, so its real
    // value cannot be measured); the further right it sits, the more it pulls leftward,
    // keeping the panel inside the window.
    let left = {
        let st = state.read();
        crate::ui::components::titlebar::caption_x(&st, actions::Caption::ALL[0])
            .min((win_w - PANEL_W).max(0.0))
    };
    let top = TITLEBAR_H;
    let style = panel_style(top, left, panel_h, t, PANEL_Z);

    rsx! {
        div {
            role: "menu",
            style: "{style}",
            for (idx, (cap, label)) in caps.iter().enumerate() {
                MoreRow {
                    key: "m{idx}",
                    state,
                    cap: *cap,
                    label: label.clone(),
                    anchor_x: left,
                    idx,
                    t,
                }
            }
        }
    }
}

/// One row in the overflow list: hover feedback + click to open that caption's dropdown.
#[component]
fn MoreRow(
    state: Signal<EditorState>,
    cap: actions::Caption,
    label: String,
    anchor_x: f32,
    idx: usize,
    t: &'static Theme,
) -> Element {
    // Tri-state lives in the `.mrow` class (components::style)
    // The overflow list holds only captions, never checkable rows → use the plain left padding,
    // leaving no check-column gap (it used to indent by CHECK_W, putting 22px of blank space
    // next to the identically named caption in a dropdown).
    let style = format!(
        "position:absolute; left:0; right:0; top:{y}px; height:{ROW_H}px;
         line-height:{ROW_H}px; font-size:12px;
         padding-left:{MENU_PAD_L}px; white-space:nowrap; overflow:hidden;",
        y = idx as f32 * ROW_H,
    );
    rsx! {
        div {
            class: "mrow",
            role: "menuitem",
            style: "{style}",
            onpointerdown: move |evt| {
                evt.stop_propagation();
                evt.prevent_default();
            },
            onclick: move |evt| {
                evt.stop_propagation();
                evt.prevent_default();
                // Close the more list and open that caption's dropdown (menu_toggle clears
                // more_open internally — the two are mutually exclusive)
                state.write().menu_toggle(cap, anchor_x);
            },
            "{label}"
        }
    }
}

/// The scroll strip drawn on the right edge of a panel that does not fit.
///
/// Real, not decorative: the thumb drags (a grab records a [`MenuDrag`] so the
/// content maps 1:1 to the pointer instead of jumping), the track pages up or
/// down one panel-full, and the wheel handler on the panel scrolls. The drag
/// itself is driven by the *panel's* `onpointermove`, so it keeps working when
/// the pointer outruns the 9 px strip.
/// Geometry of a scrolling panel that needs a drawn scrollbar (logical pixels).
struct ScrollGeom {
    /// The panel top's page coordinates (used to convert pointer events into panel-local
    /// coordinates).
    panel_top: f32,
    panel_h: f32,
    content_h: f32,
    scroll: f32,
    max_s: f32,
}

fn scrollbar(
    mut state: Signal<EditorState>,
    t: &'static Theme,
    g: ScrollGeom,
    in_flyout: bool,
) -> Element {
    let ScrollGeom {
        panel_top,
        panel_h,
        content_h,
        scroll,
        max_s,
    } = g;
    if max_s <= 0.0 {
        return rsx! {};
    }
    let (thumb_top, thumb_h) = thumb_geom(panel_h, content_h, scroll, max_s);
    let track_h = (panel_h - 4.0).max(1.0);
    let travel = (track_h - thumb_h).max(1.0);
    let per_px = max_s / travel;
    let page = (panel_h - ROW_H).max(ROW_H);
    let track_style = format!(
        "position:absolute; right:1px; top:1px; width:{SCROLLBAR_W}px; height:{h}px;
         z-index:2; background:transparent;",
        h = (panel_h - 2.0).max(1.0)
    );
    let thumb_style = format!(
        "position:absolute; left:1px; right:1px; top:{thumb_top}px; height:{thumb_h}px;
         border-radius:3px; background:{bg}; {anim}",
        // The same token as the global scrollbar (it once was two ad hoc rgba values)
        bg = t.sb_thumb,
        anim = motion::fast(motion::SURFACE)
    );

    rsx! {
        div {
            style: "{track_style}",
            // Page up/down on the track, as every scrollbar does. The thumb
            // stops propagation on its own press, so a grab never pages.
            onpointerdown: move |evt| {
                if !matches!(
                    evt.data.trigger_button(),
                    Some(dioxus_elements::input_data::MouseButton::Primary)
                ) {
                    return;
                }
                evt.stop_propagation();
                evt.prevent_default();
                let local_y = evt.data.client_coordinates().y as f32 - panel_top;
                let up = local_y < thumb_top + thumb_h / 2.0;
                let step = if up { -page } else { page };
                let mut st = state.write();
                if in_flyout {
                    st.menu.sub_scroll = (st.menu.sub_scroll + step).clamp(0.0, max_s);
                } else {
                    st.menu.scroll = (st.menu.scroll + step).clamp(0.0, max_s);
                }
            },
            div {
                style: "{thumb_style}",
                onpointerdown: move |evt| {
                    if !matches!(
                        evt.data.trigger_button(),
                        Some(dioxus_elements::input_data::MouseButton::Primary)
                    ) {
                        return;
                    }
                    // a thumb press must not also page the track or reach a row
                    evt.stop_propagation();
                    evt.prevent_default();
                    let start_y = evt.data.client_coordinates().y as f32;
                    let drag = MenuDrag {
                        start_y,
                        start_scroll: scroll,
                        per_px,
                        max: max_s,
                    };
                    let mut st = state.write();
                    if in_flyout {
                        st.menu.sub_drag = Some(drag);
                    } else {
                        st.menu.scroll_drag = Some(drag);
                    }
                },
            }
        }
    }
}

/// Precomputed style and geometry for one rendered row.
#[derive(Clone, PartialEq)]
struct RowGeom {
    label: String,
    accel: String,
    header: bool,
    sep: bool,
    /// A checkable row (even when currently unchecked) — decides whether the whole menu keeps
    /// a check column
    checkable: bool,
    checked: bool,
    y: f32,
    hot: bool,
    cursor: bool,
}

/// Whether this menu needs a check column: any checkable row demands it (even if everything is
/// off right now).
///
/// The criterion is `checkable`, not `checked`: judging by "is anything checked now" would make
/// the column vanish while a run of toggles is all off, then pop the whole column back the
/// moment one is switched on, jumping every label sideways.
fn has_check_column(rows: &[RowGeom]) -> bool {
    rows.iter().any(|r| r.checkable)
}

/// A row label's left padding.
///
/// The only criterion is "does this panel have a check column" — **no hierarchical indent**: a
/// fly-out submenu is its own panel (not a nested row inside the parent), so its first level is
/// its own depth 0. The old code added 14px per tree depth, which left submenus without
/// checkboxes — Recent Files and the like — a trench on the left nobody could account for: from
/// the user's view, "a big blank space where no checkbox ever appears". Rows on the same level
/// should share one left edge no matter which panel draws them.
fn row_indent(show_check: bool) -> f32 {
    if show_check {
        CHECK_W + 6.0
    } else {
        MENU_PAD_L
    }
}

/// Render a list of rows. `in_flyout` selects which click action runs.
fn row_list(state: Signal<EditorState>, rows: Vec<RowGeom>, in_flyout: bool) -> Element {
    let show_check = has_check_column(&rows);
    rsx! {
        for (idx, row) in rows.into_iter().enumerate() {
            MenuRowItem {
                key: "r{idx}",
                state,
                idx,
                row,
                in_flyout,
                show_check,
            }
        }
    }
}

/// One clickable row of a dropdown (or of the fly-out).
///
/// Its own component so `Press` is per-row: hoisting the pressed index into
/// `EditorState` would make every hover and every press a full state write, and
/// the menu re-renders on each pointer move across it.
#[component]
fn MenuRowItem(
    state: Signal<EditorState>,
    idx: usize,
    row: RowGeom,
    in_flyout: bool,
    /// Whether this menu has a check column (see [`has_check_column`])
    show_check: bool,
) -> Element {
    if row.sep {
        // A real `<hr>`: to the accessibility tree the separator means "a break here", not yet
        // another empty div.
        // Geometry is still absolute (all menu rows are absolutely positioned, see the module
        // docs).
        let y = row.y + ROW_H / 2.0;
        let style = format!("position:absolute; left:6px; right:6px; top:{y}px; height:1px;");
        return rsx! { hr { class: "k-sep", style: "{style}" } };
    }

    // Label indent: rows of a menu **with** checkable items start past the
    // check column (like a native Windows menu); a menu where nothing is
    // checkable has no such column and the label sits at a plain padding — no
    // dead gutter. The indent looks only at this panel (see [`row_indent`]): a submenu is its own
    // panel and does not stack levels with the parent's rows. The tri-state lives in the `.mrow` /
    // `.mrow.cur` classes: `cur` = the keyboard cursor's row, beating hover.
    let indent = row_indent(show_check);
    let style = format!(
        "position:absolute; left:0; right:0; top:{y}px; height:{ROW_H}px;
         line-height:{ROW_H}px; font-size:12px;
         padding-left:{indent}px; white-space:nowrap; overflow:hidden;",
        y = row.y,
    );
    // ---- check column -------------------------------------------------------
    //
    // The checkbox is the component library's `ui_kit::CheckMark` (the same part as form
    // checkboxes — identical size/colors/tick, no more maintaining a hand-drawn square plus a
    // `√` glyph).
    let mark_top = (ROW_H - 14.0) / 2.0;

    rsx! {
        div {
            class: if row.cursor { "mrow cur" } else { "mrow" },
            role: "menuitem",
            style: "{style}",
            onmouseenter: {
                let label = row.label.clone();
                let header = row.header;
                move |_| {
                    let mut st = state.write();
                    if in_flyout {
                        st.menu.hover_sub = Some(idx);
                    } else {
                        st.menu.hover = Some(idx);
                        // Keyboard nav continues from wherever the pointer is.
                        st.menu.cursor = idx;
                        if header {
                            // Native menus fly a submenu out on hover, not
                            // only on click; do the same.
                            if st.menu.sub.as_deref() != Some(label.as_str()) {
                                st.menu.sub = Some(label.clone());
                                st.menu.hover_sub = None;
                                st.menu.sub_scroll = 0.0;
                            }
                        } else {
                            // sliding onto a plain item closes the fly-out
                            st.menu.sub = None;
                            st.menu.hover_sub = None;
                        }
                    }
                }
            },
            onpointerdown: move |evt| {
                // A menu row must not reach the app root: its press would blur
                // the composer and kill the keyboard for the rest of the session.
                evt.stop_propagation();
                evt.prevent_default();
            },
            onclick: {
                let label = row.label.clone();
                let header = row.header;
                move |_| {
                    let mut st = state.write();
                    if in_flyout {
                        st.menu_run_sub_row(idx);
                    } else if header {
                        let same = st.menu.sub.as_deref() == Some(label.as_str());
                        st.menu.sub = if same { None } else { Some(label.clone()) };
                        st.menu.hover_sub = None;
                        st.menu.sub_scroll = 0.0;
                        st.menu.cursor = 0;
                    } else {
                        st.menu_run_row(idx);
                    }
                }
            },
            // Check column (only menus like View / Encoding / Language have one): the
            // component-library check part — a box appears only on checkable rows, other rows
            // stay blank (label alignment comes from the indent).
            if show_check && row.checkable {
                span {
                    aria_hidden: "true",
                    style: "position:absolute; left:4px; top:{mark_top}px; \
                            width:14px; height:14px; pointer-events:none;",
                    ui_kit::CheckMark { checked: row.checked }
                }
            }
            // The label must be a **direct** text child of the row element: accesskit only
            // attributes direct text children to their parent, so wrapping it in a span would
            // leave the menu item without a readable name.
            "{row.label}"
            // the fly-out marker is U+203A, present in the UI font;
            // U+25B8 is not and renders as tofu
            span { aria_hidden: "true",
                style: "position:absolute; right:10px; top:0; height:{ROW_H}px;
                        line-height:{ROW_H}px; opacity:0.62; font-size:11px;",
                if row.header { "\u{203A}" } else { "{row.accel}" }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// in-page context menu (the non-Windows fallback for TrackPopupMenu)
// ---------------------------------------------------------------------------

/// The in-page context menu overlay (root level, like [`MenuOverlay`]).
///
/// Windows never renders this: right-click runs the native `TrackPopupMenu`
/// (`ctxmenu::show`) and `EditorState::ctx` stays `None`. Other platforms
/// have no popup that can leave the window, so the same `Item` tree is
/// flattened into an overlay panel (see
/// [`crate::ui::state::EditorState::open_ctx_menu`]).
///
/// A transparent full-window **backdrop** sits under the panel and closes the
/// menu on any press — the row elements stop propagation, so a click on a row
/// runs its command instead of dismissing. Escape and "open another menu"
/// close it through `menu_close`, which folds in `close_ctx_menu`.
#[component]
pub fn CtxMenuOverlay() -> Element {
    let mut state = use_context::<Signal<EditorState>>();

    let (menu, win_w, win_h, t) = {
        let st = state.read();
        (st.ctx.clone(), st.win.w, st.win.h, st.theme())
    };
    let Some(m) = menu else {
        return rsx! {};
    };

    let panel_h = panel_height(m.rows.len(), win_h);
    let max_s = max_scroll(m.rows.len(), win_h);
    let scroll = m.scroll.clamp(0.0, max_s);
    // The panel opens anchored at the pointer but clamps back inside the window (a right-click
    // at the window's right/bottom edge must not spill out).
    let left = m.x.min((win_w - PANEL_W).max(0.0)).max(0.0);
    let top = m.y.min((win_h - panel_h - 2.0).max(0.0)).max(0.0);

    // The check column follows the menubar's convention: keep it when the panel has checkable
    // rows (per this panel, never by "is anything checked now").
    let show_check = m.rows.iter().any(|r| r.checkable);
    let indent = row_indent(show_check);
    let mark_top = (ROW_H - 14.0) / 2.0;

    let style = panel_style(top, left, panel_h, t, PANEL_Z);

    rsx! {
        div {
            // A transparent window-sized backdrop: clicking outside the menu collapses it (row
            // elements stop_propagation, so they never reach here). Equal z-index with the panel
            // suffices — the panel is a later DOM child, and among equals the later one paints on
            // top.
            style: "position:absolute; left:0; top:0; width:{win_w}px; height:{win_h}px;
                    z-index:{PANEL_Z}; background:transparent;",
            onpointerdown: move |evt| {
                evt.stop_propagation();
                evt.prevent_default();
                state.write().close_ctx_menu();
            },
            onwheel: move |evt| {
                // The backdrop swallows the wheel: while a menu is open the wheel must not
                // scroll the editor
                evt.stop_propagation();
            },
            div {
                role: "menu",
                style: "{style}",
                onwheel: move |evt| {
                    evt.stop_propagation();
                    let dy = wheel_delta_px(&evt);
                    let mut st = state.write();
                    if let Some(m) = st.ctx.as_mut() {
                        m.scroll = (m.scroll - wheel_step(dy)).clamp(0.0, max_s);
                    }
                },
                for (idx, row) in m.rows.iter().enumerate() {
                    CtxRowItem {
                        key: "c{idx}",
                        state,
                        idx,
                        row: row.clone(),
                        y: idx as f32 * ROW_H - scroll,
                        indent,
                        mark_top,
                        cur: m.cursor == idx,
                    }
                }
            }
        }
    }
}

/// One row of the in-page context menu.
///
/// Separators are real `<hr>`s, section headers (flattened submenu titles) are
/// muted and inert, command rows reuse the `.mrow` three-state class from the
/// menu bar dropdown (hover/press feedback without any per-row state).
#[component]
fn CtxRowItem(
    state: Signal<EditorState>,
    idx: usize,
    row: crate::ui::state::menu::CtxRow,
    y: f32,
    /// Label left padding (check column + panel decision, see [`row_indent`])
    indent: f32,
    mark_top: f32,
    /// The keyboard cursor rests on this row (`.mrow.cur`, beating hover)
    cur: bool,
) -> Element {
    if row.sep {
        let sep_style = format!(
            "position:absolute; left:6px; right:6px; top:{top}px; height:1px;",
            top = y + ROW_H / 2.0
        );
        return rsx! { hr { class: "k-sep", style: "{sep_style}" } };
    }

    let depth_pad = row.depth as f32 * 14.0;
    let base = format!(
        "position:absolute; left:0; right:0; top:{y}px; height:{ROW_H}px;
         line-height:{ROW_H}px; font-size:12px;
         padding-left:{pad}px; white-space:nowrap; overflow:hidden;",
        pad = indent + depth_pad,
    );

    if row.header {
        // A flattened submenu title: dimmed, not clickable. It gets no `.mrow` (no hover/press
        // feedback needed) but keeps its element and stops the press — otherwise the event falls
        // back to the backdrop and clicking a group header would close the whole menu (in a native
        // menu clicking a group header is harmless).
        let t = state.read().theme();
        let header_style = format!("{base} color:{}; opacity:0.62;", t.menu_fg);
        return rsx! {
            div {
                role: "presentation",
                style: "{header_style}",
                onpointerdown: move |evt| {
                    evt.stop_propagation();
                    evt.prevent_default();
                },
                "{row.label}"
            }
        };
    }

    rsx! {
        div {
            class: if cur { "mrow cur" } else { "mrow" },
            role: "menuitem",
            style: "{base}",
            // The keyboard cursor follows the pointer (the same convention as menubar rows:
            // hovering takes over the cursor)
            onmouseenter: move |_| {
                if let Some(m) = state.write().ctx.as_mut() {
                    m.cursor = idx;
                }
            },
            onpointerdown: move |evt| {
                // Same reason as the menubar rows: the press must not bubble to the root (it
                // would smudge the IME composer)
                evt.stop_propagation();
                evt.prevent_default();
            },
            onclick: move |evt| {
                evt.stop_propagation();
                evt.prevent_default();
                state.write().ctx_menu_activate(idx);
            },
            // Check column (only the window system menu's "Always on Top" has it): the
            // component-library check part
            if row.checkable {
                span {
                    aria_hidden: "true",
                    style: "position:absolute; left:4px; top:{mark_top}px; \
                            width:14px; height:14px; pointer-events:none;",
                    ui_kit::CheckMark { checked: row.checked }
                }
            }
            // The label must be a **direct** text child of the row element (the accesskit
            // naming rule, see MenuRowItem's comment)
            "{row.label}"
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The check column's criterion is `checkable`, not "is anything checked now": while a run
    /// of toggles is all off the column must stay, or checking one would jump every label in the
    /// menu sideways.
    #[test]
    fn check_column_is_reserved_by_checkable_not_by_checked() {
        let row = |checkable: bool, checked: bool| RowGeom {
            label: "x".into(),
            accel: String::new(),
            header: false,
            sep: false,
            checkable,
            checked,
            y: 0.0,
            hot: false,
            cursor: false,
        };
        // all toggles off: keep the column anyway
        assert!(has_check_column(&[row(true, false), row(true, false)]));
        // one checkable row is enough
        assert!(has_check_column(&[row(false, false), row(true, false)]));
        // purely plain items (File/Edit): no column
        assert!(!has_check_column(&[row(false, false), row(false, false)]));
        assert!(!has_check_column(&[]));
    }

    /// Regression: submenus without checkboxes — Recent Files and the like — must put their
    /// labels on the same left edge as the first-level dropdowns. The old implementation added
    /// 14px per tree depth to a fly-out panel, leaving a checkboxless trench on its left — "a big
    /// blank chunk on the left with no checkbox anywhere in it".
    #[test]
    fn a_flyout_without_checkables_starts_where_the_first_level_does() {
        use crate::ui::actions::{self, Caption};
        use crate::ui::Msg;

        let mut st = EditorState::minimal();
        st.recent.push_back("C:/tmp/a.txt".into());
        st.menu_toggle(Caption::File, 0.0);
        let recent = st.tr(Msg::MenuRecentFiles).into_owned();
        st.menu.sub = Some(recent);

        let main = actions::menu_rows(&st);
        let sub = actions::submenu_rows(&st, Caption::File);
        assert!(
            !sub.is_empty(),
            "the Recent Files submenu must have entries"
        );
        assert!(
            !sub.iter().any(|r| r.checkable),
            "Recent Files has no toggles, so no check column should remain"
        );
        assert_eq!(
            row_indent(sub.iter().any(|r| r.checkable)),
            row_indent(main.iter().any(|r| r.checkable)),
            "submenu labels must share the first-level menu's left edge (a panel is a layout unit, levels do not stack)"
        );
        assert_eq!(row_indent(false), MENU_PAD_L);
        // only a panel with checkboxes shifts its labels past the check column
        assert_eq!(row_indent(true), CHECK_W + 6.0);
    }

    /// A panel never grows into the space it does not have, and its height is
    /// exactly the rows — the double-counted padding is what produced the blank
    /// strip under the last item.
    #[test]
    fn panel_height_is_exactly_the_rows_when_they_fit() {
        let rows = 5usize;
        assert_eq!(panel_height(rows, 860.0), rows as f32 * ROW_H);
        assert_eq!(max_scroll(rows, 860.0), 0.0);
    }

    /// A menu taller than the window is clamped to it and scrolls by the
    /// difference.
    #[test]
    fn a_tall_panel_is_clamped_and_scrolls_by_the_excess() {
        let rows = 200usize;
        let natural = rows as f32 * ROW_H;
        let h = panel_height(rows, 400.0);
        assert!(h < natural);
        assert_eq!(h, panel_avail_h(400.0));
        assert_eq!(max_scroll(rows, 400.0), natural - h);
    }

    /// Even a three-row window keeps three rows: a menu that collapses to a
    /// sliver is worse than one that needs scrolling.
    #[test]
    fn the_panel_always_keeps_three_rows_visible() {
        assert_eq!(panel_avail_h(50.0), ROW_H * 3.0);
        assert_eq!(panel_height(3, 50.0), ROW_H * 3.0);
    }

    /// The thumb is proportional, never shorter than the shared minimum, and
    /// its travel ends exactly at the track end.
    #[test]
    fn thumb_geometry_maps_scroll_linearly() {
        let panel_h = 200.0;
        let content_h = 400.0;
        let max_s = content_h - panel_h;
        let track_h = panel_h - 4.0;
        let (top0, h) = thumb_geom(panel_h, content_h, 0.0, max_s);
        assert_eq!(top0, 2.0);
        assert!((h - track_h * panel_h / content_h).abs() < 1e-3);
        let (top1, _) = thumb_geom(panel_h, content_h, max_s, max_s);
        assert!(
            (top1 + h - (2.0 + track_h)).abs() < 1e-3,
            "thumb ends at track end"
        );
    }

    /// An absurd wheel delta (a trackpad fling, a hostile event) is clamped to
    /// a sane number of rows instead of teleporting the panel.
    #[test]
    fn wheel_steps_are_clamped() {
        assert_eq!(wheel_step(1e9), ROW_H * 8.0);
        assert_eq!(wheel_step(-1e9), -ROW_H * 8.0);
        assert_eq!(wheel_step(f32::NAN), 0.0);
    }
}
