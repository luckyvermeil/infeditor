//! The editor viewport's scrollbars.
//!
//! This is **not** "blitz has no scrollbars" — native overlay scrollbars do exist (the `scrollbars`
//! feature, see the root Cargo.toml; the sidebar/outline/dialog-body real scroll containers all use
//! them, colored through `components::style`'s `scrollbar-color`). They are drawn by hand here because **the viewport
//! cannot use a native bar**: the body is virtualized with a manual `scroll_top` (f64, reaching
//! billions of px for huge files), so there is no scrollable box in the DOM to hand to blitz. The
//! find-results panel is the same (line-window virtualized).
//!
//! The style follows the classic Windows form — a track, a proportional thumb (with a minimum
//! size), arrow buttons on both ends — the standard shape of this app's chrome.
//!
//! They take *space* rather than floating over the text: `EditorState::text_w` /
//! `text_h` subtract them, so the last line and the tail of the longest line
//! stay reachable instead of hiding under the bar.
//!
//! Interaction notes:
//!
//! * every handler calls `prevent_default()` — an un-cancelled press makes
//!   blitz blur the composer that is the editor's only keyboard target
//!   (see `viewport.rs`), and `stop_propagation()` so the viewport does not
//!   also run its hit test and move the caret;
//! * the drag is tracked from the **window**'s cursor-move event, not from this
//!   element's `onmousemove`: a drag leaves the 14px strip constantly.
//! * arrow buttons move one line / three columns per press; real Windows
//!   scrollbars auto-repeat while held, which needs a timer this app does not
//!   have — holding the button is therefore a no-op after the first step.

use dioxus_native::prelude::*;

use crate::highlight::Theme;
use crate::ui::motion;
use crate::ui::scrollbar::{SbLive, SbPart, BAR};
use crate::ui::state::{EditorState, SCROLLBAR_W};

/// The drag-time thumb position, provided by `EditorApp` (a dedicated signal:
/// a pointer move during a drag re-renders only the thumb's scope).
fn sb_live_ctx() -> Signal<Option<SbLive>> {
    use_context::<Signal<Option<SbLive>>>()
}

/// Where the thumb sits right now: the **input side** while a drag is live on
/// this bar (glued to the pointer, see ui/scrollbar.rs `SbLive`), the content
/// side (`thumb_geom(scroll_top)`) otherwise.
fn thumb_pos(
    state: &Signal<EditorState>,
    sb_live: &Signal<Option<SbLive>>,
    vertical: bool,
) -> (f32, f64) {
    let st = state.read();
    let live = sb_live.read();
    let g = if vertical { st.v_geom() } else { st.h_geom() };
    let off = match live.as_ref() {
        Some(l) if l.vertical == vertical && st.sb_drag.is_some() => l.off,
        _ => g.off,
    };
    (off as f32, g.len)
}

/// Fill of a part in its three states (rest / hover / pressed).
fn fill(t: &'static Theme, kind: Fill, hot: bool, press: bool) -> &'static str {
    let (rest, hover, active) = match kind {
        Fill::Thumb => (t.sb_thumb, t.sb_thumb_hover, t.sb_thumb_active),
        Fill::Button => (t.sb_track, t.btn_hover, t.btn_active),
    };
    if press && hot {
        active
    } else if hot {
        hover
    } else {
        rest
    }
}

/// Which of the three color families a part uses.
#[derive(Clone, Copy)]
enum Fill {
    /// the draggable thumb
    Thumb,
    /// an arrow button (track-colored at rest, chrome hover/press)
    Button,
}

/// Both bars, drawn over the right and bottom edges of the viewport.
#[component]
pub fn ScrollBars() -> Element {
    let mut state = use_context::<Signal<EditorState>>();

    // v_geom/h_geom are deliberately NOT read here: the thumb computes its
    // own position inside its scope (it also subscribes to the drag-time
    // `sb_live` signal), so a pointer move during a drag re-renders only the
    // thumb, not this parent.
    let (vw, theme, v_on, h_on, text_h, hot, press) = {
        let st = state.read();
        (
            st.view.w,
            st.theme(),
            st.vbar_on(),
            st.hbar_on(),
            st.text_h(),
            st.sb_hot,
            st.sb_press,
        )
    };
    if !v_on && !h_on {
        return rsx! {};
    }

    // ---- vertical: a strip on the right edge, spanning the text area -------
    let v_left = vw - SCROLLBAR_W;
    // ---- horizontal: a strip under it, spanning everything but the corner --
    let h_top = text_h;
    let h_w = vw - if v_on { SCROLLBAR_W } else { 0.0 };

    let corner_style = format!(
        "position:absolute; left:{v_left}px; top:{h_top}px; width:{SCROLLBAR_W}px; \
         height:{SCROLLBAR_W}px; background:{bg}; z-index:21; user-select:none; cursor:default;",
        bg = theme.sb_track
    );

    // The child divs are `position:absolute` inside the bar's own box, so every
    // coordinate below is **relative to the bar**, not to the viewport. Getting
    // this wrong (passing the bar's own left/top again) pushes the thumb and the
    // arrows off the visible area — the track still paints, so the bug looks
    // like "the thumb is missing" rather than "the bar is broken".
    let track_style = |vertical: bool| -> String {
        if vertical {
            format!(
                "position:absolute; left:{v_left}px; top:0px; width:{SCROLLBAR_W}px; \
                 height:{text_h}px; box-sizing:border-box; background:{bg}; user-select:none; \
                 cursor:default; z-index:20; border-left:1px solid {bd};",
                bg = theme.sb_track,
                bd = theme.border
            )
        } else {
            format!(
                "position:absolute; left:0px; top:{h_top}px; width:{h_w}px; \
                 height:{SCROLLBAR_W}px; box-sizing:border-box; background:{bg}; user-select:none; \
                 cursor:default; z-index:20; border-top:1px solid {bd};",
                bg = theme.sb_track,
                bd = theme.border
            )
        }
    };

    rsx! {
        if v_on {
            div {
                style: "{track_style(true)}",
                onmouseleave: move |_| {
                    // Conditional write (update_if): no dirty mark when the hot zone is already cleared
                    crate::ui::state::update_if(
                        &mut state,
                        |st| st.sb_drag.is_none() && st.sb_hot.is_some(),
                        |st| st.sb_hot = None,
                    );
                },
                SbButton { state, which: SbPart::VUp, x: 0.0, y: 0.0, t: theme }
                SbButton { state, which: SbPart::VDown, x: 0.0, y: text_h - BAR, t: theme }
                SbTrack { state, vertical: true, x: 0.0, y: BAR, len: text_h - BAR * 2.0 }
                Thumb { state, vertical: true, t: theme, hot: hot == Some(SbPart::VThumb), press }
            }
        }
        if h_on {
            div {
                style: "{track_style(false)}",
                onmouseleave: move |_| {
                    // Conditional write (update_if): no dirty mark when the hot zone is already cleared
                    crate::ui::state::update_if(
                        &mut state,
                        |st| st.sb_drag.is_none() && st.sb_hot.is_some(),
                        |st| st.sb_hot = None,
                    );
                },
                SbButton { state, which: SbPart::HLeft, x: 0.0, y: 0.0, t: theme }
                SbButton { state, which: SbPart::HRight, x: h_w - BAR, y: 0.0, t: theme }
                SbTrack { state, vertical: false, x: BAR, y: 0.0, len: h_w - BAR * 2.0 }
                Thumb { state, vertical: false, t: theme, hot: hot == Some(SbPart::HThumb), press }
            }
        }
        // the square where the two bars meet; without it the corner shows
        // whatever is underneath
        if v_on && h_on {
            div { style: "{corner_style}" }
        }
    }
}

/// An arrow button: one line / three columns per press.
#[component]
fn SbButton(
    state: Signal<EditorState>,
    which: SbPart,
    x: f32,
    y: f32,
    t: &'static Theme,
) -> Element {
    let (vertical, dir, glyph) = match which {
        SbPart::VUp => (true, -1, "\u{25B2}"),
        SbPart::VDown => (true, 1, "\u{25BC}"),
        SbPart::HLeft => (false, -1, "\u{25C0}"),
        SbPart::HRight => (false, 1, "\u{25B6}"),
        _ => (true, 0, ""),
    };
    let hot = { state.read().sb_hot == Some(which) };
    let press = { state.read().sb_press };
    let bg = fill(t, Fill::Button, hot, press);
    let style = format!(
        "position:absolute; left:{x}px; top:{y}px; width:{BAR}px; height:{BAR}px; \
         line-height:{BAR}px; font-size:8px; text-align:center; background:{bg}; \
         color:{fg}; user-select:none; cursor:default; overflow:hidden; {anim}",
        fg = t.sb_arrow,
        anim = motion::fast(motion::SURFACE)
    );
    rsx! {
        div {
            style: "{style}",
            onmouseenter: move |_| {
                // Conditional write (update_if): no dirty mark when this control is already the hot zone
                crate::ui::state::update_if(
                    &mut state,
                    |st| st.sb_drag.is_none() && st.sb_hot != Some(which),
                    |st| st.sb_hot = Some(which),
                );
            },
            onpointerdown: move |evt| {
                evt.stop_propagation();
                evt.prevent_default();
                let already_down = {
                    let st = state.read();
                    st.sb_hot == Some(which) && st.sb_press
                };
                if already_down {
                    // Already down (auto-repeat path): it still has to step
                    state.write().sb_step(vertical, dir);
                    return;
                }
                let mut st = state.write();
                st.sb_hot = Some(which);
                st.sb_press = true;
                st.sb_step(vertical, dir);
            },
            onpointerup: move |_| {
                // Conditional write (update_if): pointer-up with no active drag/press does not mark dirty
                crate::ui::state::update_if(
                    &mut state,
                    |st| st.sb_drag.is_some() || st.sb_press,
                    |st| {
                        // Land the newest drag sample before teardown, like
                        // the track/thumb release paths: a thumb drag released
                        // over this button would otherwise lose the final
                        // sample to sb_end's discard.
                        if let Some(p) = crate::ui::scroll::sb_take() {
                            st.sb_apply(p);
                        }
                        st.sb_end();
                        st.sb_hot = Some(which);
                    },
                );
            },
            "{glyph}"
        }
    }
}

/// The bare track: a click pages towards the pointer and can turn into a drag.
#[component]
fn SbTrack(state: Signal<EditorState>, vertical: bool, x: f32, y: f32, len: f32) -> Element {
    let style = if vertical {
        format!(
            "position:absolute; left:{x}px; top:{y}px; width:{BAR}px; height:{len}px; \
             user-select:none; cursor:default;"
        )
    } else {
        format!(
            "position:absolute; left:{x}px; top:{y}px; width:{len}px; height:{BAR}px; \
             user-select:none; cursor:default;"
        )
    };
    let (vx, vy) = {
        let st = state.read();
        (st.view.x as f64, st.view.y as f64)
    };
    let mut sb_live = sb_live_ctx();
    rsx! {
        div {
            style: "{style}",
            onpointerdown: move |evt| {
                evt.stop_propagation();
                evt.prevent_default();
                let cp = evt.data.client_coordinates();
                let at = if vertical {
                    cp.y - vy
                } else {
                    cp.x - vx
                };
                let mut st = state.write();
                st.sb_track_click(vertical, at);
                st.sb_begin(vertical, at);
                // The Windows-style jump must be visible immediately: park the
                // input-side thumb at the pointer before any content frame.
                if let Some(live) = st.sb_live_off(vertical, cp.x as f32, cp.y as f32) {
                    sb_live.set(Some(live));
                }
            },
            onpointerup: move |_| {
                // Conditional write: pointer-up with no active drag does not mark dirty (an unconditional
                // write() would mark_dirty and re-render the whole app for the frame)
                if state.read().sb_drag.is_some() || state.read().sb_press {
                    let mut st = state.write();
                    // Land the newest sample so the content matches the thumb
                    // at release; sb_end then discards any residue.
                    if let Some(p) = crate::ui::scroll::sb_take() {
                        st.sb_apply(p);
                    }
                    st.sb_end();
                }
                // Back to the content side. Guarded write: `set` marks the
                // thumb scopes dirty even when the value is unchanged.
                if sb_live.read().is_some() {
                    sb_live.set(None);
                }
            },
        }
    }
}

/// The draggable thumb.
///
/// Geometry and position are computed **inside** this component (not passed as
/// props): it subscribes to both `state` and the drag-time `sb_live` signal,
/// so a pointer move during a drag re-renders only this scope — the thumb
/// tracks the pointer without waiting for the full-app frame.
#[component]
fn Thumb(
    state: Signal<EditorState>,
    vertical: bool,
    t: &'static Theme,
    hot: bool,
    press: bool,
) -> Element {
    let mut sb_live = sb_live_ctx();
    let (x, y, len) = {
        let (off, g_len) = thumb_pos(&state, &sb_live, vertical);
        if vertical {
            (2.0, BAR + off, g_len as f32)
        } else {
            (BAR + off, 2.0, g_len as f32)
        }
    };
    let bg = fill(t, Fill::Thumb, hot, press);
    let style = if vertical {
        format!(
            "position:absolute; left:{x}px; top:{y}px; width:{w}px; height:{len}px; \
             background:{bg}; user-select:none; cursor:default; overflow:hidden; {anim}",
            w = BAR - 4.0,
            anim = motion::fast(motion::SURFACE)
        )
    } else {
        format!(
            "position:absolute; left:{x}px; top:{y}px; width:{len}px; height:{h}px; \
             background:{bg}; user-select:none; cursor:default; overflow:hidden; {anim}",
            h = BAR - 4.0,
            anim = motion::fast(motion::SURFACE)
        )
    };
    let which = if vertical {
        SbPart::VThumb
    } else {
        SbPart::HThumb
    };
    rsx! {
        div {
            style: "{style}",
            onmouseenter: move |_| {
                // Conditional write (update_if): no dirty mark when this control is already the hot zone
                crate::ui::state::update_if(
                    &mut state,
                    |st| st.sb_drag.is_none() && st.sb_hot != Some(which),
                    |st| st.sb_hot = Some(which),
                );
            },
            onpointerdown: move |evt| {
                evt.stop_propagation();
                evt.prevent_default();
                let cp = evt.data.client_coordinates();
                let at = if vertical {
                    cp.y - state.read().view.y as f64
                } else {
                    cp.x - state.read().view.x as f64
                };
                let mut st = state.write();
                st.sb_hot = Some(which);
                st.sb_begin(vertical, at);
                // The input side leads from the first frame: park the thumb
                // where the pointer grabbed it (for a track click this is the
                // Windows-style jump; for a thumb grab it is a no-op).
                // `sb_live_off` takes client px and subtracts the view itself.
                if let Some(live) = st.sb_live_off(vertical, cp.x as f32, cp.y as f32) {
                    sb_live.set(Some(live));
                }
            },
            onpointerup: move |_| {
                if state.read().sb_drag.is_some() || state.read().sb_press {
                    let mut st = state.write();
                    // Land the newest sample so the content matches the thumb
                    // at the moment of release (the frame hook may lag).
                    if let Some(p) = crate::ui::scroll::sb_take() {
                        st.sb_apply(p);
                    }
                    st.sb_end();
                    st.sb_hot = Some(which);
                }
                // Back to the content side. Guarded write: `set` marks the
                // thumb scopes dirty even when the value is unchanged.
                if sb_live.read().is_some() {
                    sb_live.set(None);
                }
            },
        }
    }
}
