//! The wys selection floating toolbar.
//!
//! When the selection is non-empty (drag-select / double-click word / Shift-extend), a small
//! toolbar floats **above the selection's first rectangle**: Bold B / Italic I / Strikethrough S /
//! inline code <> / link 🔗 / headings H1–H3 (the heading dropdown simplified to three buttons).
//! Clicking runs the existing `WysTool` commands — the same entry as the fixed toolbar
//! (`actions::run(Cmd::WysTool(..))` → `mdws::toolbar_apply`); the inline "on" states reuse the
//! `WysFocus::inline` statistics.
//!
//! Show/hide and positioning are a **pure state machine** ([`step`] / [`bar_pos`], see the unit
//! tests at the end of the file); the mouse drag-select/double-click happens inside the wys kernel
//! component where the host has no event to hook, so the state is advanced by
//! `mdws::overlays_frame` in the `RedrawRequested` frame hook (event-phase geometry queries
//! allowed; visible on the next frame). It hides when: the selection collapses / the IME is
//! composing / **scrolling** (a scroll collapses it; only a selection change re-pops) / focus is
//! lost (leaving wys mode = the component unmounts).
//!
//! Positioning is approximate: the kernel `Geom` only has the single-point `caret_rect` and no
//! selection-rectangle API (`sel_rects` was removed earlier as dead code) — the caret rectangle at
//! the **selection start** stands in for the "first rectangle" (the start's x/y is the top-left of
//! the first line's selected range), placed above with a gap. When a multi-block selection starts
//! low the buttons sit a bit lower; that is an accepted approximation.

use std::time::{Duration, Instant};

use dioxus_native::prelude::*;
use wysiwyg::{Sel, WrapKind};

use crate::ui::actions::{Cmd, WysTool};
use crate::ui::state::EditorState;

/// Height of the floating bar (px).
pub const BAR_H: f32 = 30.0;
/// Gap above the selection's first line (px).
pub const BAR_GAP: f32 = 6.0;
/// How long a selection must stay stable before the bar pops. **Must be greater than the system
/// double/triple-click interval**: a bar popping right after a drag-select covers the area above
/// the selection, so the third click of a burst (triple-click block select) lands on the bar and
/// the block-select never fires (seen in practice: after a triple-click the caret stopped at the
/// block's end). 250ms separates the cases cleanly.
pub const BAR_SHOW_DELAY: Duration = Duration::from_millis(250);

/// Floating-bar state (pure data; transitions in [`step`]).
#[derive(Default, Clone, Copy, PartialEq, Debug)]
pub struct SelBar {
    pub show: bool,
    /// Position (viewport coordinates, relative to the wys-root border box, same as
    /// `mdws::caret_view`).
    pub x: f32,
    pub y: f32,
    /// Scroll offset at show time (any scroll collapses the bar).
    pub scroll: (f32, f32),
    /// The selection at the moment of collapsing: after a scroll collapse it **stays collapsed**
    /// until the selection changes, then re-pops (otherwise the next frame would see "there is a
    /// selection" and pop right back, defeating the scroll hide).
    pub at_sel: Option<Sel>,
    /// When the selection first appeared (the delayed pop's timer start; `None` = not timing).
    pub since: Option<Instant>,
    /// **Sticky collapse** caused by scrolling: the same selection stays hidden until it changes.
    pub sticky: bool,
}

/// Positioning (pure function): the selection's first-rect top-left → the bar's top-left (a gap
/// above; if it would cross the viewport top it clamps to 0 instead of covering the selection
/// below).
pub fn bar_pos(first: (f32, f32)) -> (f32, f32) {
    (first.0.max(0.0), (first.1 - BAR_H - BAR_GAP).max(0.0))
}

/// One show/hide transition step (pure function, unit-testable). `pos` = the viewport
/// coordinates of the selection's first-rect top-left (`None` when geometry is unavailable →
/// hide).
pub fn step(
    prev: &SelBar,
    sel: Sel,
    composing: bool,
    scroll: (f32, f32),
    pos: Option<(f32, f32)>,
    now: Instant,
) -> SelBar {
    // No selection / IME composing / no geometry: hide (focus loss is covered by the component
    // unmounting)
    if sel.collapsed() || composing {
        return SelBar::default();
    }
    let Some((fx, fy)) = pos else {
        return SelBar::default();
    };
    let (x, y) = bar_pos((fx, fy));
    if prev.show {
        if prev.scroll != scroll {
            // Collapse on scroll and remember the selection (see [`SelBar::at_sel`])
            return SelBar {
                show: false,
                x: 0.0,
                y: 0.0,
                scroll,
                at_sel: Some(sel),
                since: prev.since,
                sticky: true,
            };
        }
        return SelBar {
            show: true,
            x,
            y,
            scroll,
            at_sel: Some(sel),
            since: prev.since,
            sticky: false,
        };
    }
    // Sticky collapse after a scroll: the same selection stays hidden; only a change restarts
    // the timer
    if prev.sticky && prev.at_sel == Some(sel) {
        return *prev;
    }
    // New selection: start the timer first, pop only after BAR_SHOW_DELAY of stability (clear of
    // the double/triple-click window)
    if prev.at_sel != Some(sel) || prev.since.is_none() {
        return SelBar {
            show: false,
            x,
            y,
            scroll,
            at_sel: Some(sel),
            since: Some(now),
            sticky: false,
        };
    }
    if now.duration_since(prev.since.unwrap_or(now)) < BAR_SHOW_DELAY {
        return SelBar {
            show: false,
            x,
            y,
            scroll,
            at_sel: Some(sel),
            since: prev.since,
            sticky: false,
        };
    }
    SelBar {
        show: true,
        x,
        y,
        scroll,
        at_sel: Some(sel),
        since: prev.since,
        sticky: false,
    }
}

// ---- global state (written by the host's frame hook, read-only during render) ------------

static SELBAR: GlobalSignal<SelBar> = Signal::global(SelBar::default);

/// Read the current state (inside a component the name `state` is taken by the `use_context`
/// signal, so the accessor is named `current`).
pub fn current() -> SelBar {
    *SELBAR.read()
}

/// Conditional write (the iron rule: write only on change, never a self-perpetuating redraw).
pub fn set_state(next: SelBar) {
    if *SELBAR.read() != next {
        *SELBAR.signal().write() = next;
    }
}

/// Reset to hidden (when seeding / leaving wys mode).
pub fn reset() {
    set_state(SelBar::default());
}

/// `#rrggbb` → `rgba(r,g,b,0.92)` (the bar's translucent background); returns the input
/// unchanged when parsing fails (an opaque fallback so the text is at least legible).
fn translucent(color: &str) -> String {
    let hex = color.strip_prefix('#').unwrap_or("");
    if hex.len() != 6 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return color.to_string();
    }
    let n = |a: &str| u8::from_str_radix(a, 16).unwrap_or(0);
    format!(
        "rgba({},{},{},0.92)",
        n(&hex[0..2]),
        n(&hex[2..4]),
        n(&hex[4..6])
    )
}

// ---- view (not covered by headless render tests — manual verification in the component's comment) ----

/// The selection floating toolbar.
///
/// Beyond what headless render tests cover — verify manually: drag-select some text in wys
/// mode and a translucent rounded bar (B/I/S/<>/🔗/H1–H3) floats above the selection, with the
/// format buttons' on-states lit by the selection's content; click B to bold it and the bar
/// follows the selection; scrolling the page makes it vanish, drag-selecting again brings it
/// back; double-click word selection pops it too.
#[component]
pub fn WysSelBar() -> Element {
    let mut state = use_context::<Signal<EditorState>>();
    let bar = current();
    if !bar.show {
        return rsx! {};
    }
    // The on states reuse the focus statistics (reading the global wys — read-only during render)
    let f = super::mdws::focus_state();
    let m = f.inline;
    let (bg, fg, border, on_bg) = {
        let st = state.read();
        let t = st.theme();
        (
            translucent(t.menu_bg),
            t.menu_fg,
            t.menu_border,
            t.menu_open,
        )
    };
    let panel = format!(
        "position:absolute; left:{:.1}px; top:{:.1}px; height:{BAR_H:.0}px; \
         display:flex; align-items:center; gap:2px; padding:0 6px; \
         background:{bg}; border:1px solid {border}; border-radius:6px; \
         box-shadow:0 2px 8px rgba(0,0,0,0.35); z-index:5; font-size:12px; color:{fg}; \
         touch-action:none;",
        bar.x, bar.y
    );
    // (label, on state, command) — the same command entry as the fixed toolbar; styles are
    // computed into values before rendering (rsx attributes have no inline branches).
    let items: Vec<(&str, bool, Cmd)> = vec![
        ("B", m.bold, Cmd::WysTool(WysTool::Wrap(WrapKind::Bold))),
        ("I", m.italic, Cmd::WysTool(WysTool::Wrap(WrapKind::Italic))),
        ("S", m.strike, Cmd::WysTool(WysTool::Wrap(WrapKind::Del))),
        ("<>", m.code, Cmd::WysTool(WysTool::Wrap(WrapKind::Code))),
        ("🔗", m.link, Cmd::WysTool(WysTool::Link)),
        ("H1", f.head == Some(1), Cmd::WysTool(WysTool::SetHead(1))),
        ("H2", f.head == Some(2), Cmd::WysTool(WysTool::SetHead(2))),
        ("H3", f.head == Some(3), Cmd::WysTool(WysTool::SetHead(3))),
    ];
    let rows: Vec<(&str, String, Cmd)> = items
        .into_iter()
        .map(|(label, on, cmd)| {
            let bgc = if on { on_bg } else { "transparent" };
            (
                label,
                format!(
                    "min-width:26px; height:22px; line-height:22px; text-align:center; \
                     padding:0 4px; border-radius:4px; background:{bgc}"
                ),
                cmd,
            )
        })
        .collect();
    rsx! {
        div { style: "{panel}",
            for (n, (label, btn_style, cmd)) in rows.into_iter().enumerate() {
                div {
                    key: "{n}",
                    style: "{btn_style}",
                    onclick: move |_| {
                        crate::ui::actions::run(&mut state.write(), cmd);
                    },
                    "{label}"
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wysiwyg::Pos;

    fn sel(a: usize, b: usize) -> Sel {
        Sel {
            anchor: Pos::new(0, a),
            focus: Pos::new(0, b),
        }
    }

    /// The instant used to unwrap `step`'s success branch once the delay has elapsed.
    fn due(t0: Instant) -> Instant {
        t0 + BAR_SHOW_DELAY + Duration::from_millis(1)
    }

    #[test]
    fn shows_on_selection_and_hides_on_collapse() {
        let pos = Some((100.0, 80.0));
        let t0 = Instant::now();
        // no selection → hidden
        assert_eq!(
            step(&SelBar::default(), sel(3, 3), false, (0.0, 0.0), pos, t0),
            SelBar::default()
        );
        // New selection: the first frame only arms (the delayed pop); it shows once due
        let armed = step(&SelBar::default(), sel(0, 5), false, (0.0, 0.0), pos, t0);
        assert!(
            !armed.show,
            "a fresh selection must not pop immediately (it would block the triple-click)"
        );
        let s = step(&armed, sel(0, 5), false, (0.0, 0.0), pos, due(t0)).unwrap_show();
        assert_eq!((s.x, s.y), bar_pos((100.0, 80.0)));
        // selection collapsed → hidden
        assert_eq!(
            step(&s, sel(5, 5), false, (0.0, 0.0), pos, due(t0)),
            SelBar::default()
        );
        // IME composing / no geometry → hidden
        assert_eq!(
            step(&s, sel(0, 5), true, (0.0, 0.0), pos, due(t0)),
            SelBar::default()
        );
        assert_eq!(
            step(&s, sel(0, 5), false, (0.0, 0.0), None, due(t0)),
            SelBar::default()
        );
    }

    #[test]
    fn scroll_hides_and_selection_change_reopens() {
        let pos = Some((10.0, 200.0));
        let t0 = Instant::now();
        let armed = step(&SelBar::default(), sel(0, 5), false, (0.0, 0.0), pos, t0);
        let s = step(&armed, sel(0, 5), false, (0.0, 0.0), pos, due(t0)).unwrap_show();
        assert!(s.show);
        // scroll → collapse
        let t1 = due(t0);
        let hidden = step(&s, sel(0, 5), false, (0.0, 30.0), pos, t1);
        assert!(!hidden.show, "must collapse after scrolling");
        // Selection unchanged, still scrolling or idle: stay collapsed (it must not be popped
        // back by "there is a selection")
        let again = step(&hidden, sel(0, 5), false, (0.0, 60.0), pos, t1);
        assert!(!again.show);
        let still = step(&again, sel(0, 5), false, (0.0, 60.0), pos, due(t1));
        assert!(!still.show);
        // Selection changed: restart the timer, pop only once due
        let re_armed = step(&still, sel(0, 6), false, (0.0, 60.0), pos, due(t1));
        assert!(!re_armed.show);
        let re = step(&re_armed, sel(0, 6), false, (0.0, 60.0), pos, due(due(t1)));
        assert!(re.show, "a changed selection should pop again");
    }

    #[test]
    fn bar_pos_sits_above_first_rect_and_clamps_at_top() {
        // normal: leave BAR_H + BAR_GAP above
        assert_eq!(bar_pos((120.0, 300.0)), (120.0, 300.0 - BAR_H - BAR_GAP));
        // no room at the viewport top → clamp to 0 (never cover the selection below)
        assert_eq!(bar_pos((5.0, 4.0)), (5.0, 0.0));
        // x never goes negative
        assert_eq!(bar_pos((-3.0, 100.0)).0, 0.0);
    }

    #[test]
    fn translucent_parses_hex_only() {
        assert_eq!(translucent("#112233"), "rgba(17,34,51,0.92)");
        assert_eq!(
            translucent("red"),
            "red",
            "returns unparsed input unchanged (an opaque fallback)"
        );
        assert_eq!(translucent("#12345"), "#12345");
    }

    /// Test helper: unwrap `step`'s success branch (a failure blows up the test — clearer than
    /// an unwrap).
    trait UnwrapShow {
        fn unwrap_show(self) -> SelBar;
    }
    impl UnwrapShow for SelBar {
        fn unwrap_show(self) -> SelBar {
            assert!(self.show, "expected popped, got hidden");
            self
        }
    }
}
