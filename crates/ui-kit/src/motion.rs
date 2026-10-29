//! Motion tokens: durations, easing and the three-state tap surface.
//!
//! Blitz implements real CSS transitions (`blitz-dom` maintains an animation
//! set per node, `blitz-shell` keeps calling `request_redraw()` while
//! `Document::is_animating()` is true), so an inline `background-color` change
//! between renders tweens by itself — no per-frame advancement on our side is
//! needed.
//!
//! Two rhythm rules:
//!
//! * hover feedback must be **fast** (see [`FAST`]) — it is the direct response
//!   to the pointer; any slower reads as jank;
//! * everything else (dropdowns, panel expansion) uses [`NORMAL`].
//!
//! `opacity` animation is deliberately avoided: a fading-out element that
//! still receives pointer events is the classic source of "I clicked the menu
//! but nothing seemed to happen", and blitz has no `pointer-events` animation
//! support to fall back on.

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
/// **Prefer CSS pseudo-classes**: blitz's stylo parses `:hover` / `:active` /
/// `:disabled` natively (see the [`crate::style`] module docs), so once the
/// three states are written into the stylesheet no in-component state is
/// needed at all. This function and [`Press`] are left only for **self-drawn
/// surfaces with no stylesheet available**: winshell's popup widgets
/// (`shell::controls`) and this library's menu rows — their colors vary per
/// instance and generating one class per variant is not worth it.
///
/// If you do need it: record on `mousedown`, and clear on `mouseup` **or**
/// `mouseleave` (a press that ends with the pointer having slid off the button
/// is still a finished press; leaving it in the down state reads as "this
/// button is stuck"), and pressed outranks hovered — while held the pointer is
/// still inside the button, so consulting hover first would hide the press.
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
