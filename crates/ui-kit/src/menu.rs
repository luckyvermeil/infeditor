//! Generic dropdown menu panel.
//!
//! Decoupled from any application state: the panel only knows [`MenuRow`]
//! data + geometry + a set of callbacks — row clicks / hovers / wheel /
//! scrollbar dragging are all reported upward via [`EventHandler`] and
//! interpreted by the host (e.g. mapping a row index to its own command).
//! That way other Blitz applications can use the very same menu panel.
//!
//! # Why the panel is a root-level overlay
//!
//! The intuitive approach is to nest the panel inside the title bar
//! (`position:relative` + a child panel) — that is broken in this renderer:
//! an ancestor with a `z-index` creates a **stacking context**, and
//! descendants are pressed under other overlays no matter how large their
//! `z-index` is. So the panel should render as a root-level overlay (high
//! `z-index`), with the anchor given by the caller.
//!
//! # Height and scrolling
//!
//! Rows are **absolutely positioned** and do not stretch the panel — the
//! panel height must be given explicitly, must equal `rows × ROW_H`, and must
//! have **no padding** (absolutely positioned children lay out against the
//! padding box; `padding:5px 0` does not offset the first row, it only adds a
//! dead white strip below the last one).
//!
//! A menu can be taller than the window: the panel is clamped to
//! [`panel_avail_h`], rows are painted at `i·ROW_H − scroll`, and when
//! [`max_scroll`] is non-zero a real scrollbar appears on the right edge
//! (wheel scrolling, thumb dragging, clicking the track to page).

use dioxus_native::prelude::*;

use crate::motion::{self, Press};
use crate::Theme;

/// Fixed height of a row (px) — the fixed value lets a flyout's y be
/// computed from the row number without measuring.
pub const ROW_H: f32 = 26.0;
/// Panel width (px).
pub const PANEL_W: f32 = 252.0;
/// Check-column width (px).
pub const CHECK_W: f32 = 22.0;
/// Left padding when the whole menu has no checkable rows (no empty check
/// column left behind).
pub const MENU_PAD_L: f32 = 12.0;
/// Scrollbar width on the right edge of a tall panel (px).
pub const SCROLLBAR_W: f32 = 9.0;
/// Shortest scrollbar thumb (px) — a 3px thumb cannot be grabbed and looks
/// like a rendering defect.
pub const THUMB_MIN: f32 = 20.0;
/// Default panel `z-index` (above ordinary overlays).
pub const PANEL_Z: i32 = 900;

/// A menu row (data). Rendering see [`MenuRowView`].
#[derive(Clone, PartialEq, Default, Debug)]
pub struct MenuRow {
    pub label: String,
    pub accel: String,
    /// Submenu header (flies out on click/hover).
    pub header: bool,
    /// Separator row (label ignored).
    pub sep: bool,
    /// Whether this row **has** a check box (even when currently unchecked).
    /// When no row in the whole menu is `checkable` no check column is left
    /// and the label sits flush left.
    pub checkable: bool,
    /// Checked state (the check column fills with the accent color).
    pub checked: bool,
}

/// Left padding of a row label.
///
/// Only looks at "does this panel have a check column", **not at depth**: a
/// flyout submenu is its own panel, and its first level is its own depth 0.
/// Indenting level by level along the tree depth would leave an empty gutter
/// on the left of submenus without checkboxes and misalign them with the
/// parent panel's labels.
pub fn row_indent(show_check: bool) -> f32 {
    if show_check {
        CHECK_W + 6.0
    } else {
        MENU_PAD_L
    }
}

/// The three-state background of a clickable menu row (hover / keyboard
/// cursor / pressed). Pressed or keyboard cursor uses the `active` notch,
/// hover uses `hover`, otherwise the quiet `menu`.
pub fn row_colors(
    hovered: bool,
    keyboard: bool,
    pressed: bool,
    t: Theme,
) -> (&'static str, &'static str) {
    if pressed || keyboard {
        (t.active_bg, t.active_fg)
    } else if hovered {
        (t.hover_bg, t.hover_fg)
    } else {
        (t.menu_bg, t.menu_fg)
    }
}

/// The panel's maximum: the space from below the title bar to the window
/// bottom, and never less than three rows (a two-row panel is unusable).
/// `top` is the distance from the panel top to the window top (each row's
/// anchor height).
pub fn panel_avail_h(win_h: f32, top: f32) -> f32 {
    (win_h - top - 6.0).max(ROW_H * 3.0)
}

/// How many whole rows fit in a panel — the step PageUp/PageDown moves the
/// keyboard cursor by.
pub fn visible_rows(win_h: f32, top: f32) -> usize {
    (panel_avail_h(win_h, top) / ROW_H).floor().max(1.0) as usize
}

/// The panel's actual painted height: the natural height clamped to the
/// available height.
pub fn panel_height(rows: usize, win_h: f32, top: f32) -> f32 {
    (rows as f32 * ROW_H).min(panel_avail_h(win_h, top))
}

/// The maximum scroll of a `rows`-row panel (0 when it fits).
pub fn max_scroll(rows: usize, win_h: f32, top: f32) -> f32 {
    ((rows as f32 * ROW_H) - panel_height(rows, win_h, top)).max(0.0)
}

/// The thumb `(top, height)` inside the panel track: by visible proportion,
/// floored at `THUMB_MIN`; scroll ↔ displacement maps linearly over the
/// remaining track.
pub fn thumb_geom(panel_h: f32, content_h: f32, scroll: f32, max_s: f32) -> (f32, f32) {
    let track_h = (panel_h - 4.0).max(1.0);
    // `clamp` panics when `min > max`: a panel shorter than the minimum thumb
    // (a docked/embedded host may pass any height) must degrade to a
    // track-sized thumb, not bring the window down.
    let thumb_h = (track_h * panel_h / content_h.max(1.0)).clamp(THUMB_MIN.min(track_h), track_h);
    let travel = (track_h - thumb_h).max(0.0);
    let top = 2.0
        + if max_s > 0.0 {
            travel * (scroll / max_s)
        } else {
            0.0
        };
    (top, thumb_h)
}

/// Wheel delta (CSS px, positive = up) → scroll step. One wheel notch moves
/// one row.
pub fn wheel_step(dy: f32) -> f32 {
    let dy = if dy.is_finite() { dy } else { 0.0 };
    dy.clamp(-ROW_H * 8.0, ROW_H * 8.0)
}

/// Panel shell style (shared by dropdown and flyout). `h` is the **painted**
/// height (already clamped to the window).
pub fn panel_style(top: f32, left: f32, h: f32, t: Theme, z: i32) -> String {
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

/// A menu row (rendering). A separator row only draws a 1px line.
///
/// `hot` = pointer hover, `cursor` = keyboard cursor, `pressed` = held down
/// (the three states are decided by [`row_colors`]). `on_hover`/`on_click`
/// pass the row index back.
#[component]
pub fn MenuRowView(
    row: MenuRow,
    y: f32,
    theme: Theme,
    #[props(default)] hot: bool,
    #[props(default)] cursor: bool,
    /// Hover enter/leave: `Some(idx)` = enter, `None` = leave.
    on_hover: EventHandler<Option<usize>>,
    on_click: EventHandler<usize>,
    /// This row's index in the host's row list (passed back).
    idx: usize,
    /// Whether this menu has a check column (computed by [`MenuPanel`] over
    /// all rows).
    #[props(default)]
    show_check: bool,
) -> Element {
    let t = theme;
    let mut press = use_signal(Press::default);
    if row.sep {
        let yy = y + ROW_H / 2.0;
        let style = format!(
            "position:absolute; left:6px; right:6px; top:{yy}px; height:1px;
             background:{border};",
            border = t.menu_border
        );
        return rsx! { div { style: "{style}" } };
    }
    let (bg, fg) = row_colors(hot, cursor, press().down, t);
    // Label indent: with a check column it starts after the column (same as
    // Windows native menus), without one it uses the ordinary left padding —
    // no whitespace for a checkbox that does not exist. The panel is a layout
    // unit; submenus do not stack levels with the parent panel (see
    // [`row_indent`]).
    let indent = row_indent(show_check);
    let style = format!(
        "position:absolute; left:0; right:0; top:{y}px; height:{ROW_H}px;
         line-height:{ROW_H}px; font-size:12px; cursor:default;
         padding-left:{indent}px; white-space:nowrap; overflow:hidden;
         background:{bg}; color:{fg}; user-select:none; {anim}",
        anim = motion::fast(motion::SURFACE)
    );
    // Check column: reuses the component library's [`crate::CheckMark`] (the
    // same widget as the form checkbox) instead of hand-drawing a box + a `√`
    // glyph. Only checkable rows draw the box; other rows leave it blank.
    let mark_top = (ROW_H - 14.0) / 2.0;
    let label = row.label.clone();
    let accel = row.accel.clone();
    let header = row.header;
    rsx! {
        div {
            role: "menuitem",
            style: "{style}",
            onmouseenter: move |_| {
                press.write().hover = true;
                on_hover.call(Some(idx));
            },
            onmouseleave: move |_| {
                *press.write() = Press::default();
                on_hover.call(None);
            },
            onpointerdown: move |evt| {
                // A menu row press must not bubble to the application root: it
                // would make the host focus lose focus and kill the keyboard for
                // the whole session.
                evt.stop_propagation();
                evt.prevent_default();
                press.write().down = true;
            },
            onpointerup: move |_| press.write().down = false,
            onclick: move |evt| {
                evt.stop_propagation();
                evt.prevent_default();
                press.write().down = false;
                on_click.call(idx);
            },
            if show_check && row.checkable {
                span {
                    aria_hidden: "true",
                    style: "position:absolute; left:4px; top:{mark_top}px; \
                            width:14px; height:14px; pointer-events:none;",
                    crate::CheckMark { checked: row.checked }
                }
            }
            // The label must be a **direct** text child of the row element:
            // accesskit only annotates direct text children to the parent, and a
            // menu item wrapped in a span would have no accessible name.
            "{label}"
            // The flyout marker uses U+203A (present in UI fonts); U+25B8 is
            // not, and would render as a tofu box.
            span { aria_hidden: "true",
                style: "position:absolute; right:10px; top:0; height:{ROW_H}px;
                        line-height:{ROW_H}px; opacity:0.62; font-size:11px;",
                if header { "\u{203A}" } else { "{accel}" }
            }
        }
    }
}

/// Panel geometry (scrollbar painting + drag conversion).
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct PanelGeom {
    /// Page coordinates of the panel top (for converting pointer events into
    /// panel-local coordinates).
    pub top: f32,
    pub height: f32,
    pub content_h: f32,
    pub scroll: f32,
    pub max_s: f32,
}

/// A dropdown menu panel: shell + row list + optional scrollbar. All
/// interaction is reported upward through callbacks.
///
/// Rows are addressed by **index**: `on_click(idx)` / `on_hover(idx)`.
/// Scrollbar dragging keeps its start state in the host (this component only
/// forwards the down/move/up events).
#[component]
pub fn MenuPanel(
    rows: Vec<MenuRow>,
    /// Panel top-left corner (page coordinates).
    top: f32,
    left: f32,
    win_h: f32,
    theme: Theme,
    #[props(default = PANEL_Z)] z: i32,
    #[props(default)] cursor: Option<usize>,
    #[props(default)] hover: Option<usize>,
    #[props(default)] scroll: f32,
    on_click: EventHandler<usize>,
    /// Hover enter/leave a row (leave is `None`).
    on_hover: EventHandler<Option<usize>>,
    /// Pointer left the whole panel (clear highlight / end drag).
    on_leave: EventHandler<()>,
    /// Wheel (delta already converted to px, positive = up).
    on_wheel: EventHandler<f32>,
    /// Click on the scrollbar track (argument = client y).
    #[props(default)]
    on_scroll_track: Option<EventHandler<f32>>,
    /// Press on the scrollbar thumb (argument = client y).
    #[props(default)]
    on_scroll_drag_start: Option<EventHandler<f32>>,
    /// While dragging (argument = client y; forwarded from the panel's
    /// `pointermove`).
    #[props(default)]
    on_scroll_drag_move: Option<EventHandler<f32>>,
    /// Release (end of drag).
    #[props(default)]
    on_scroll_drag_end: Option<EventHandler<()>>,
) -> Element {
    if rows.is_empty() {
        return rsx! {};
    }
    let t = theme;
    // Keep the check column only when some row has a check box (the test is
    // checkable, not checked — otherwise a menu with "all toggles off" would
    // first have no column and then grow the whole column when one opens).
    let show_check = rows.iter().any(|r| r.checkable);
    let content_h = rows.len() as f32 * ROW_H;
    let panel_h = panel_height(rows.len(), win_h, top);
    let max_s = max_scroll(rows.len(), win_h, top);
    let scroll = scroll.clamp(0.0, max_s);
    let style = panel_style(top, left, panel_h, t, z);

    let sb = if max_s > 0.0 {
        Some(scrollbar(
            t,
            PanelGeom {
                top,
                height: panel_h,
                content_h,
                scroll,
                max_s,
            },
            on_wheel,
            on_scroll_track,
            on_scroll_drag_start,
        ))
    } else {
        None
    };

    rsx! {
        div {
            role: "menu",
            style: "{style}",
            onmouseleave: move |_| on_leave.call(()),
            onwheel: move |evt| {
                evt.stop_propagation();
                let dy = match evt.data.delta() {
                    dioxus_elements::geometry::WheelDelta::Pixels(v) => v.y as f32,
                    dioxus_elements::geometry::WheelDelta::Lines(v) => v.y as f32 * ROW_H,
                    dioxus_elements::geometry::WheelDelta::Pages(v) => v.y as f32 * ROW_H * 8.0,
                };
                on_wheel.call(dy);
            },
            onpointermove: move |evt| {
                if let Some(cb) = on_scroll_drag_move {
                    cb.call(evt.data.client_coordinates().y as f32);
                }
            },
            onpointerup: move |_| {
                if let Some(cb) = on_scroll_drag_end {
                    cb.call(());
                }
            },
            { sb }
            for (i, row) in rows.into_iter().enumerate() {
                MenuRowView {
                    key: "r{i}",
                    row,
                    y: i as f32 * ROW_H - scroll,
                    theme: t,
                    hot: hover == Some(i),
                    cursor: cursor == Some(i),
                    on_hover,
                    on_click,
                    idx: i,
                    show_check,
                }
            }
        }
    }
}

/// The scrollbar on the panel's right edge (drawn only when
/// [`PanelGeom::max_s`] is non-zero).
fn scrollbar(
    t: Theme,
    g: PanelGeom,
    on_wheel: EventHandler<f32>,
    on_scroll_track: Option<EventHandler<f32>>,
    on_scroll_drag_start: Option<EventHandler<f32>>,
) -> Element {
    let PanelGeom {
        top: panel_top,
        height: panel_h,
        content_h,
        scroll,
        max_s,
    } = g;
    let (thumb_top, thumb_h) = thumb_geom(panel_h, content_h, scroll, max_s);
    let track_style = format!(
        "position:absolute; right:1px; top:1px; width:{SCROLLBAR_W}px; height:{h}px;
         z-index:2; background:transparent;",
        h = (panel_h - 2.0).max(1.0)
    );
    let thumb_style = format!(
        "position:absolute; left:1px; right:1px; top:{thumb_top}px; height:{thumb_h}px;
         border-radius:3px; background:{bg}; {anim}",
        bg = t.sb_thumb,
        anim = motion::fast(motion::SURFACE)
    );
    rsx! {
        div {
            style: "{track_style}",
            // Track paging (same as native scrollbars); the thumb's own press
            // stops propagation, so it does not page.
            onpointerdown: move |evt| {
                if !matches!(
                    evt.data.trigger_button(),
                    Some(dioxus_elements::input_data::MouseButton::Primary)
                ) {
                    return;
                }
                evt.stop_propagation();
                evt.prevent_default();
                if let Some(cb) = on_scroll_track {
                    cb.call(evt.data.client_coordinates().y as f32);
                }
            },
            onwheel: move |evt| {
                evt.stop_propagation();
                let dy = match evt.data.delta() {
                    dioxus_elements::geometry::WheelDelta::Pixels(v) => v.y as f32,
                    dioxus_elements::geometry::WheelDelta::Lines(v) => v.y as f32 * ROW_H,
                    dioxus_elements::geometry::WheelDelta::Pages(v) => v.y as f32 * ROW_H * 8.0,
                };
                on_wheel.call(dy);
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
                    evt.stop_propagation();
                    evt.prevent_default();
                    let _ = panel_top; // panel_top is reserved for the host's
                                       // local-coordinate conversion
                    if let Some(cb) = on_scroll_drag_start {
                        cb.call(evt.data.client_coordinates().y as f32);
                    }
                },
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn panel_height_is_exactly_the_rows_when_they_fit() {
        let rows = 5usize;
        assert_eq!(panel_height(rows, 860.0, 40.0), rows as f32 * ROW_H);
        assert_eq!(max_scroll(rows, 860.0, 40.0), 0.0);
    }

    #[test]
    fn panel_clamps_and_scrolls_when_it_does_not_fit() {
        // 40 rows × 26 = 1040 > available height → clamped to the available
        // height, max_scroll positive.
        let avail = panel_avail_h(300.0, 40.0);
        assert_eq!(panel_height(40, 300.0, 40.0), avail);
        assert!(max_scroll(40, 300.0, 40.0) > 0.0);
        // Never shorter than three rows.
        assert_eq!(panel_avail_h(10.0, 40.0), ROW_H * 3.0);
    }

    #[test]
    fn thumb_is_proportional_and_never_thinner_than_the_floor() {
        let (top, h) = thumb_geom(100.0, 100.0, 0.0, 0.0);
        assert_eq!(top, 2.0);
        assert!(h > 0.0);
        // Extremely tall content → the thumb is held up by the floor.
        let (_, h2) = thumb_geom(100.0, 10_000.0, 0.0, 500.0);
        assert_eq!(h2, THUMB_MIN);
    }

    /// A track shorter than the minimum thumb must not panic in `clamp`
    /// (`min > max` is a panic, not a saturating operation).
    #[test]
    fn a_tiny_panel_yields_a_track_sized_thumb() {
        let (_, h) = thumb_geom(10.0, 100.0, 0.0, 0.0);
        assert_eq!(h, 6.0);
    }

    #[test]
    fn wheel_step_is_clamped_to_a_sane_range() {
        assert_eq!(wheel_step(1.0), 1.0);
        assert_eq!(wheel_step(1e9), ROW_H * 8.0);
        assert_eq!(wheel_step(-1e9), -ROW_H * 8.0);
        assert_eq!(wheel_step(f32::NAN), 0.0);
    }

    #[test]
    fn row_colors_pressed_and_keyboard_outrank_hover() {
        let t = Theme::default();
        assert_eq!(row_colors(false, false, false, t), (t.menu_bg, t.menu_fg));
        assert_eq!(row_colors(true, false, false, t), (t.hover_bg, t.hover_fg));
        assert_eq!(row_colors(true, false, true, t), (t.active_bg, t.active_fg));
        assert_eq!(row_colors(true, true, false, t), (t.active_bg, t.active_fg));
    }

    /// A panel without checkable rows leaves no check column: the label just
    /// uses the ordinary left padding (not accumulated by level).
    #[test]
    fn a_panel_without_checkables_has_no_check_column() {
        assert_eq!(row_indent(false), MENU_PAD_L);
        assert_eq!(row_indent(true), CHECK_W + 6.0);
    }
}
