//! Motion tokens.
//!
//! Blitz implements real CSS transitions (`blitz-dom/src/stylo.rs` keeps an
//! animation set per node and `blitz-shell/src/window.rs` keeps calling
//! `request_redraw()` while `Document::is_animating()` is true), so an inline
//! `background-color` that changes between renders *does* animate without any
//! per-frame work from us.
//!
//! Two rules keep the UI from feeling sluggish:
//!
//! * hover feedback is **fast** (see [`FAST`]) — it is a direct response to the
//!   pointer and anything slower reads as lag;
//! * everything else is [`NORMAL`] — dropdowns, panel reveals.
//!
//! `OPACITY` animations are deliberately avoided: a fading element that still
//! receives pointer events is a classic source of "clicked the menu but nothing
//! happened", and blitz has no `pointer-events` animation support to fix it.

/// Hover / press feedback, in milliseconds.
pub const FAST: u32 = 90;
/// Dropdown and panel transitions, in milliseconds.
pub const NORMAL: u32 = 150;
/// Shared easing. `cubic-bezier(0.2, 0, 0, 1)` is the Material "decelerate"
/// curve: fast out of the gate, gentle landing.
pub const EASE: &str = "cubic-bezier(0.2, 0, 0, 1)";

/// `transition: <prop> <FAST>ms <EASE>` for a single property.
pub fn fast(prop: &str) -> String {
    format!("transition: {prop} {FAST}ms {EASE};")
}

/// `transition: <prop> <NORMAL>ms <EASE>` for a single property.
pub fn normal(prop: &str) -> String {
    format!("transition: {prop} {NORMAL}ms {EASE};")
}

/// The standard "this surface reacts to the pointer" bundle: background, text
/// color and the outline used for the focus ring.
pub const SURFACE: &str = "background-color, color, border-color, outline-color, box-shadow";

/// Pick between the three visual states of a clickable surface.
///
/// Blitz has **no** `:active` pseudo-class — stylo resolves no pseudo-classes at
/// all — so "the button looks pressed" has to be real state rather than a
/// stylesheet rule. A component records `mousedown` and clears it on `mouseup`
/// *or* `mouseleave` (a press that is released outside the button must not leave
/// it stuck down) and calls this to pick the color for the frame.
///
/// Pressed wins over hovered: the pointer is still inside the button when it is
/// held down, so checking hover first would make the press invisible.
pub fn pick<T: Copy>(rest: T, hover: T, down: T, is_hover: bool, is_down: bool) -> T {
    if is_down {
        down
    } else if is_hover {
        hover
    } else {
        rest
    }
}

/// The state of one clickable surface, driven by its pointer handlers.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct Press {
    pub hover: bool,
    pub down: bool,
}

impl Press {
    /// Currently held down (a press beats a hover — the pointer is inside the
    /// surface either way).
    pub fn down(&self) -> bool {
        self.down
    }

    /// The colour to paint, given the three candidates.
    pub fn color<T: Copy>(&self, rest: T, hover: T, down: T) -> T {
        pick(rest, hover, down, self.hover, self.down)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_are_emitted_in_milliseconds() {
        assert_eq!(
            fast("background-color"),
            "transition: background-color 90ms cubic-bezier(0.2, 0, 0, 1);"
        );
        assert_eq!(
            normal("box-shadow"),
            "transition: box-shadow 150ms cubic-bezier(0.2, 0, 0, 1);"
        );
    }

    /// A press must be visible even though the pointer is still hovering the
    /// button — otherwise the button lights up on hover and then nothing at all
    /// happens while it is held.
    #[test]
    fn a_press_outranks_the_hover() {
        assert_eq!(pick("rest", "hot", "down", true, true), "down");
        assert_eq!(pick("rest", "hot", "down", true, false), "hot");
        assert_eq!(pick("rest", "hot", "down", false, true), "down");
        assert_eq!(pick("rest", "hot", "down", false, false), "rest");
    }

    #[test]
    fn press_color_matches_pick() {
        let p = Press {
            hover: true,
            down: true,
        };
        assert!(p.down());
        assert_eq!(p.color("rest", "hot", "down"), "down");
        assert_eq!(Press::default().color("rest", "hot", "down"), "rest");
    }
}
