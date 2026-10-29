//! Class stylesheet of the component library: class + standard CSS
//! pseudo-classes express every interactive state.
//!
//! blitz's stylo parses `:hover` / `:active` / `:focus` / `:disabled` /
//! `:checked` natively (`match_non_ts_pseudo_class` in
//! `blitz-dom/src/stylo.rs`; the document side's `set_hover_to` /
//! `active_node` maintain the state bits), so hover and press feedback
//! **do not need** a Press signal machine inside the component — that was a
//! product of the "stylo has no pseudo-classes" era and has been retired
//! along with pseudo-class support. `motion::Press` is still exported for
//! self-drawn components with no standard HTML counterpart (menu rows etc.)
//! to keep using.
//!
//! Colors come from the runtime [`Theme`]: the stylesheet is **generated**
//! per theme, and the host mounts one [`KitStyle`] at the root of every
//! window that uses this library. On theme switch the key changes → the
//! `<style>` node is rebuilt → blitz re-parses (editing the text in place
//! does not trigger a re-parse, see `style_nodes` in `mutator.rs`).
//!
//! # Touch (`touch-action`)
//!
//! Two lines of defense, neither enough alone:
//!
//! 1. **slop** (blitz patches eleven/thirteen/fourteen): finger displacement
//!    within 12px counts as a tap, only beyond it does `DragMode::Panning`
//!    engage; and lifting the finger still dispatches a Click when the pan
//!    **did not actually scroll any content**. This layer guarantees that
//!    "a widget that forgot to declare `touch-action` does not become a dead
//!    key".
//! 2. **`touch-action:none`**: once an interactive element declares it,
//!    blitz never enters Panning (`touch_action_pan_axes`), taps are immune
//!    to displacement — the most stable.
//!
//! But **rows inside a scrollable container must not write `none`**:
//! `touch_action_pan_axes` only looks at the target chain, and a `none` on
//! the row vetoes the whole gesture, effectively turning off the parent
//! container's native scrolling — in scrollable lists like the sidebar /
//! menu write `pan-y` instead (see `.ws-row` in
//! `src/ui/components/style.rs`). Rule of thumb: does the area this row
//! lives in **need finger scrolling**?
//!
//! One more hard constraint: `prevent_default()` / `stop_propagation()` in
//! `onpointerdown` cancel blitz's **default actions** (focusing, placing the
//! caret, selection), so do not write them casually; gesture bookkeeping was
//! moved out of the default actions by patch fourteen and is unaffected.
//!
//! # The boolean-attribute trap (hosts must know this)
//!
//! dioxus-native writes `AttributeValue::Bool(false)` as a literal `"false"`
//! attribute (`mutation_writer.rs::set_attribute_inner`), while blitz judges
//! `disabled` by **attribute presence** (`mutator.rs`) — `disabled: false`
//! disables the button! You must write
//! `disabled: disabled.then_some(true)` (None → clears the attribute).
//! `checked` has no such problem (dioxus-native specially clears a falsy
//! `checked`).

use dioxus_native::prelude::*;

use crate::motion;
use crate::Theme;

/// Generate the component library's class stylesheet (colored by theme).
///
/// All rules are scoped to `k-`-prefixed classes, never element selectors —
/// to avoid leaking into the host's self-drawn components (an application
/// has plenty of div buttons where a global `button {…}` would be
/// irrelevant). Visual baseline: VS Code's flat widgets — 2px radii, a 1px-
/// border secondary button, an accent-fill primary button, restrained hover
/// backgrounds.
pub fn stylesheet(t: &Theme) -> String {
    let trans = motion::fast(motion::SURFACE);
    // Button three-state (faithful port: color-for-color aligned with the old
    // inline Press three-state).
    format!(
        r#"
.k-btn {{
    box-sizing:border-box;
    display:inline-flex; align-items:center; justify-content:center;
    height:24px; padding:0 14px; font-size:12px; text-align:center;
    white-space:nowrap; flex:none; cursor:default; border-radius:2px;
    background:transparent; color:{fg}; border:1px solid transparent;
    font-family:inherit; user-select:none; touch-action:none; {trans}
}}
.k-btn-wide {{ padding:0 18px; }}
.k-btn-primary {{ background:{accent}; border-color:{accent}; color:#ffffff; }}
.k-btn-primary:hover {{ background:{btn_hover}; border-color:{btn_hover}; }}
.k-btn-primary:active {{ background:{btn_active}; border-color:{btn_active}; color:{fg}; }}
.k-btn-secondary {{ background:{input_bg}; border-color:{menu_border}; color:{fg}; }}
.k-btn-secondary:hover {{ background:{btn_hover}; border-color:{accent}; }}
.k-btn-secondary:active {{ background:{btn_active}; border-color:{accent}; }}
.k-btn-subtle:hover {{ background:{btn_hover}; }}
.k-btn-subtle:active {{ background:{btn_active}; }}
.k-btn-danger:hover {{ background:{close_hot}; border-color:{close_hot}; color:#ffffff; }}
.k-btn-danger:active {{ background:{close_down}; border-color:{close_down}; color:#ffffff; }}
/* The :disabled rule goes after the variants: with equal specificity the
   later source order wins, so disabled covers the variants (no !important
   needed). */
.k-btn:disabled {{
    background:transparent; border-color:{menu_border}; color:{gutter_fg};
    opacity:0.5;
}}

.k-check {{
    display:flex; align-items:center; cursor:default; flex:none; user-select:none;
    touch-action:none;
}}
/* Semantic widget: takes no space, paints nothing. 0×0 keeps it out of the
   inline flow; `opacity:0` is mandatory — blitz's native widget painting
   does not honor "zero size" (measured: it still draws a small color block,
   most obvious once focused), while opacity silences it completely while
   keeping focus and accessibility semantics (display:none /
   visibility:hidden would remove the input from the accessibility tree). */
.k-cb {{
    width:0px; height:0px; margin:0px; padding:0px; border:none; flex:none;
    opacity:0;
}}
.k-cb:focus + .k-check-box {{ border-color:{focus_ring}; }}
/* The unchecked state **must have a solid background** (input_bg);
   background:transparent does not work: measured on the preferences popup,
   blitz skips drawing empty elements with "transparent background + a class
   border" entirely (it does not even draw the border, though the layout
   space remains) — yet the same DOM/stylesheet works fine in the find
   popup, and the window-level trigger condition is unknown (unrelated to
   composer / first frame / redraw, see the 2026-10-21 log). With a solid
   fill both windows paint stably, which is also exactly VS Code's native
   checkbox style. */
.k-check-box {{
    box-sizing:border-box;
    width:14px; height:14px; flex:none; border-radius:3px;
    border:1px solid {ctl_border}; background:{input_bg};
    display:flex; align-items:center; justify-content:center;
    pointer-events:none; {trans}
}}
.k-check:hover .k-check-box {{ border-color:{accent}; }}
.k-check-box-on {{ background:{accent}; border-color:{accent}; }}
.k-check-tick {{
    width:10px; height:10px; flex:none; pointer-events:none;
    display:flex; align-items:center; justify-content:center;
}}
.k-check-label {{ font-size:12px; color:{fg}; margin-left:7px; }}

/* Radio: also self-drawn (the native painting hard-codes the ring gap to
   pure white — a white dot in a white ring under a dark theme). */
.k-radio-row {{
    display:flex; align-items:center; height:22px; font-size:12px; color:{fg};
    cursor:default; user-select:none; touch-action:none; flex:none;
}}
.k-radio {{
    width:0px; height:0px; margin:0px; padding:0px; border:none; flex:none;
    opacity:0;
}}
.k-radio:focus + .k-radio-ring {{ border-color:{focus_ring}; }}
/* The solid-background rationale is the same as .k-check-box (transparent
   background + a class border gets skipped entirely). */
.k-radio-ring {{
    box-sizing:border-box;
    width:14px; height:14px; flex:none; border-radius:7px;
    border:1px solid {ctl_border}; background:{input_bg};
    display:flex; align-items:center; justify-content:center; {trans}
}}
.k-radio-row:hover .k-radio-ring {{ border-color:{accent}; }}
.k-radio-on {{ border-color:{accent}; }}
.k-radio-dot {{
    width:6px; height:6px; flex:none; border-radius:3px; background:{accent};
}}

.k-ibtn {{
    box-sizing:border-box;
    display:flex; align-items:center; justify-content:center;
    border:none; border-radius:2px; cursor:default; flex:0 0 auto; line-height:1;
    padding:0; background:transparent; color:{fg}; touch-action:none; {trans}
}}
.k-ibtn:hover {{ background:{btn_hover}; }}
.k-ibtn:active {{ background:{btn_active}; }}
.k-ibtn:disabled {{ color:{gutter_fg}; opacity:0.5; }}

.k-close {{
    box-sizing:border-box;
    display:flex; align-items:center; justify-content:center;
    width:32px; height:26px; border:none; border-radius:2px; font-size:14px;
    cursor:default; flex:none; padding:0; background:transparent; color:{fg};
    touch-action:none; {trans}
}}
.k-close:hover {{ background:{close_hot}; color:#ffffff; }}
.k-close:active {{ background:{close_down}; color:#ffffff; }}

.k-sep {{
    border:none; margin:0; overflow:hidden; flex:none; pointer-events:none;
    background:{menu_border};
}}

/* Tab (the find popup's Find/Replace): VS Code-style underline for active. */
.k-tab {{
    box-sizing:border-box;
    display:inline-flex; align-items:center; height:24px; padding:0 14px;
    font-size:12px; background:transparent; color:{gutter_fg};
    border:none; border-bottom:2px solid transparent; border-radius:2px 2px 0 0;
    font-family:inherit; user-select:none; touch-action:none; cursor:default; {trans}
}}
.k-tab:hover {{ background:{btn_hover}; color:{fg}; }}
.k-tab:active {{ background:{btn_active}; color:{fg}; }}
.k-tab-on {{ color:{fg}; border-bottom-color:{accent}; }}
.k-tab-on:hover {{ border-bottom-color:{accent}; }}

/* Nav row (the preferences sidebar categories): the same left accent bar as
   VS Code's settings page. */
.k-nav {{
    display:block; width:100%; height:26px; padding:0 10px; font-size:12px;
    line-height:26px; text-align:left; background:transparent; color:{fg};
    border:none; border-left:2px solid transparent; border-radius:0 2px 2px 0;
    font-family:inherit; user-select:none; touch-action:none; cursor:default;
    white-space:pre; overflow:hidden; {trans}
}}
.k-nav:hover {{ background:{btn_hover}; }}
.k-nav:active {{ background:{btn_active}; }}
.k-nav-on {{ background:{menu_open}; border-left-color:{accent}; }}
.k-nav-on:hover {{ background:{menu_open}; }}

/* Group box (the preferences right-hand sections): flow layout, children
   stack naturally. */
.k-group {{
    display:flex; flex-direction:column; gap:2px; padding:8px 12px 10px;
    border:1px solid {menu_border}; border-radius:3px; background:{panel_bg};
    flex:none;
}}
.k-group-title {{ font-size:11px; color:{fg}; font-weight:600; margin-bottom:2px; }}

/* Static explanatory text. */
.k-note {{ font-size:11px; color:{gutter_fg}; line-height:16px; overflow:hidden; }}
.k-label {{ font-size:12px; color:{fg}; }}

/* List row (an option row in a scrollable list): hover highlight goes
   through pseudo-classes.
   `touch-action:pan-y` (not none): the rows lie in a scrollable container;
   writing none would let `touch_action_pan_axes` veto the whole gesture at
   the target node, effectively turning off the parent container's native
   scrolling (the finger cannot move it). With pan-y: displacement within
   slop is a tap, beyond it native panning; a pan that did not scroll any
   content still counts as a click on lift (blitz patch thirteen). */
.k-list-row {{
    height:20px; line-height:20px; padding:0 8px; font-size:12px; color:{fg};
    white-space:pre; overflow:hidden; cursor:default; user-select:none;
    touch-action:pan-y; background:transparent; {trans}
}}
.k-list-row:hover {{ background:{btn_hover}; }}
.k-list-row:active {{ background:{btn_active}; }}
.k-list-row-on {{ background:{menu_open}; }}
.k-list-row-on:hover {{ background:{menu_open}; }}
"#,
        fg = t.menu_fg,
        accent = t.accent,
        // Widget stroke (unchecked checkbox / radio): the secondary foreground
        // is a medium gray in both the light and dark themes — enough contrast
        // without "glowing" in the dark like pure white.
        ctl_border = t.gutter_fg,
        focus_ring = t.focus_ring,
        btn_hover = t.btn_hover,
        btn_active = t.btn_active,
        input_bg = t.input_bg,
        menu_border = t.menu_border,
        menu_open = t.menu_open,
        panel_bg = t.panel_bg,
        gutter_fg = t.gutter_fg,
        close_hot = t.close_hot,
        close_down = t.close_down,
        trans = trans,
    )
}

/// The component library's class stylesheet (mounted once per window root).
///
/// `dark` is part of the key: light/dark theme switch → node rebuilt → blitz
/// re-parses the CSS.
#[component]
pub fn KitStyle(theme: Theme, dark: bool) -> Element {
    rsx! {
        style { key: "{dark}", "{stylesheet(&theme)}" }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{DARK, LIGHT};

    /// Fetch a rule's declaration block. The selector must **own the start of
    /// its line** — otherwise a search for `.k-check-box {` would also hit
    /// `.k-cb:focus + .k-check-box {`.
    fn rule<'a>(css: &'a str, selector: &str) -> &'a str {
        let sel = selector.trim_end_matches(" {");
        let pat = format!("\n{sel} {{");
        let at = css
            .find(&pat)
            .unwrap_or_else(|| panic!("missing rule {selector}"));
        let rest = &css[at + pat.len()..];
        let close = rest
            .find('}')
            .unwrap_or_else(|| panic!("{selector} has no terminator"));
        &rest[..close]
    }

    /// The light and dark palettes each generate their own stylesheet, and the
    /// widget colors really come from the theme (not hard-coded colors).
    #[test]
    fn stylesheet_follows_the_theme() {
        let dark = stylesheet(&DARK);
        let light = stylesheet(&LIGHT);
        assert_ne!(
            dark, light,
            "different themes must yield different stylesheets"
        );
        for (t, css) in [(DARK, &dark), (LIGHT, &light)] {
            assert!(
                rule(css, ".k-check-box {").contains(t.gutter_fg),
                "checkbox stroke"
            );
            assert!(
                rule(css, ".k-radio-ring {").contains(t.gutter_fg),
                "radio stroke"
            );
            assert!(
                rule(css, ".k-check-box-on {").contains(t.accent),
                "checked fill"
            );
            assert!(
                rule(css, ".k-radio-dot {").contains(t.accent),
                "checked dot"
            );
            assert!(
                rule(css, ".k-radio-on {").contains(t.accent),
                "checked stroke"
            );
        }
    }

    /// The native `<input>` must take no space and paint nothing: 0×0 keeps it
    /// out of the inline flow, and `opacity:0` silences blitz's native widget
    /// painting (it still draws a small color block at zero size — measured as
    /// a pale-blue square once focused). The white box / white ring in the dark
    /// is exactly what must be eliminated.
    #[test]
    fn native_inputs_are_collapsed_to_nothing() {
        for t in [LIGHT, DARK] {
            let css = stylesheet(&t);
            for sel in [".k-cb {", ".k-radio {"] {
                let body = rule(&css, sel);
                assert!(body.contains("width:0px"), "{sel} must be zero wide");
                assert!(body.contains("height:0px"), "{sel} must be zero high");
                assert!(body.contains("opacity:0"), "{sel} must not paint");
            }
        }
    }

    /// Hover / focus feedback goes through standard pseudo-classes (parsed
    /// natively by blitz's stylo): an ancestor `:hover` + an adjacent sibling
    /// of an input `:focus`, with no JS state machine involved.
    #[test]
    fn interaction_states_are_plain_css() {
        for t in [LIGHT, DARK] {
            let css = stylesheet(&t);
            assert!(rule(&css, ".k-check:hover .k-check-box {").contains(t.accent));
            assert!(rule(&css, ".k-radio-row:hover .k-radio-ring {").contains(t.accent));
            assert!(rule(&css, ".k-cb:focus + .k-check-box {").contains(t.focus_ring));
            assert!(rule(&css, ".k-radio:focus + .k-radio-ring {").contains(t.focus_ring));
        }
    }
}
