//! Common theme palette.
//!
//! The component library knows nothing about app theme types — all paintable
//! elements take colors from this [`Theme`] (a pure palette whose fields are
//! all `&'static str`). Apps use
//! `impl From<&AppTheme> for ui_kit::Theme` to do a **value-copy conversion**
//! at the render site (zero allocation) — the same decoupling pattern as
//! `winshell::PopTheme`.
//!
//! This module ships [`LIGHT`] / [`DARK`], two neutral palettes ready to use
//! out of the box for standalone consumers; apps that already have their own
//! theme can simply ignore them during conversion.

/// Component library theme palette (pure colors, no editor/business semantics).
///
/// Fields are grouped by purpose: surfaces / interactive triad / input /
/// accent / misc chrome. Field names align with the many app themes (e.g.
/// `highlight::Theme`) so `From` can map them directly.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Theme {
    /// Theme name (for display/debug only).
    pub name: &'static str,
    /// Whether dark (title bar light/dark, icon tinting, etc. may use this).
    pub dark: bool,

    // ---- Surfaces ----
    /// Window/app background.
    pub bg: &'static str,
    /// Body foreground.
    pub fg: &'static str,
    /// Panel/dialog background.
    pub panel_bg: &'static str,
    /// Menu/toolbar background.
    pub menu_bg: &'static str,
    /// Menu/toolbar foreground (button labels, tab labels).
    pub menu_fg: &'static str,
    /// Menu/panel border color.
    pub menu_border: &'static str,
    /// Menu item "expanded/current" background.
    pub menu_open: &'static str,

    // ---- Interactive triad ----
    /// Hover background (menu rows / list rows).
    pub hover_bg: &'static str,
    /// Hover foreground.
    pub hover_fg: &'static str,
    /// Keyboard cursor / pressed background (one notch stronger than hover).
    pub active_bg: &'static str,
    /// Keyboard cursor / pressed foreground.
    pub active_fg: &'static str,
    /// Flat button hover background.
    pub btn_hover: &'static str,
    /// Flat button pressed background.
    pub btn_active: &'static str,

    // ---- Input ----
    /// Input background.
    pub input_bg: &'static str,
    /// Input border color at rest.
    pub input_border: &'static str,
    /// Focus border ring color.
    pub focus_ring: &'static str,

    // ---- Accent ----
    /// Accent color (primary button fill, checkmarks, focus borders).
    pub accent: &'static str,

    // ---- Misc chrome ----
    /// Secondary background (gutter / group headers) — also used as the
    /// "de-emphasized surface".
    pub gutter_bg: &'static str,
    /// Secondary foreground (placeholder / dimmed text).
    pub gutter_fg: &'static str,
    /// Dropdown/panel shadow color.
    pub shadow: &'static str,
    /// Modal backdrop color.
    pub dim_backdrop: &'static str,
    /// Close button hover background.
    pub close_hot: &'static str,
    /// Close button pressed background.
    pub close_down: &'static str,
    /// Virtual caret color.
    pub caret: &'static str,
    /// Scrollbar thumb color (for self-drawn scrollbars in dropdown panels etc.).
    pub sb_thumb: &'static str,
    /// Text selection background.
    pub selection: &'static str,
    /// Text selection foreground.
    pub selection_fg: &'static str,
}

/// Light neutral palette (Fluent style).
pub const LIGHT: Theme = Theme {
    // The theme name is not supplied by the component library itself
    // (i18n: the host passes in a localized name at the From conversion
    // point); the built-in neutral palettes only keep English debug names.
    name: "Light",
    dark: false,
    bg: "#ffffff",
    fg: "#1a1a1a",
    panel_bg: "#ffffff",
    menu_bg: "#f9f9f9",
    menu_fg: "#1a1a1a",
    menu_border: "#cfcfcf",
    menu_open: "#e4e4e4",
    hover_bg: "#eaeaea",
    hover_fg: "#1a1a1a",
    active_bg: "#dce8fb",
    active_fg: "#1a1a1a",
    btn_hover: "#e5e5e5",
    btn_active: "#d8d8d8",
    input_bg: "#ffffff",
    input_border: "#c8c8c8",
    focus_ring: "#4a7dff",
    accent: "#4a7dff",
    gutter_bg: "#f3f3f3",
    gutter_fg: "#6a6a6a",
    shadow: "rgba(0,0,0,0.18)",
    dim_backdrop: "rgba(0,0,0,0.25)",
    close_hot: "#e81123",
    close_down: "#c50f1f",
    caret: "#4a7dff",
    sb_thumb: "#c0c0c0",
    selection: "#b4d5ff",
    selection_fg: "#1a1a1a",
};

/// Dark neutral palette.
pub const DARK: Theme = Theme {
    name: "Dark",
    dark: true,
    bg: "#1e1e1e",
    fg: "#d4d4d4",
    panel_bg: "#252526",
    menu_bg: "#2b2b2b",
    menu_fg: "#d4d4d4",
    menu_border: "#3c3c3c",
    menu_open: "#3a3d41",
    hover_bg: "#2a2d2e",
    hover_fg: "#ffffff",
    active_bg: "#094771",
    active_fg: "#ffffff",
    btn_hover: "#3a3d41",
    btn_active: "#45494e",
    input_bg: "#3c3c3c",
    // The border must be one notch brighter than the input background: the
    // old value was the same #3c3c3c, and in dark mode the input had neither
    // a border nor visible placeholder text (placeholder coloring read this
    // field too) — an entirely "invisible input".
    input_border: "#5a5a5a",
    focus_ring: "#4a7dff",
    accent: "#4a7dff",
    gutter_bg: "#252526",
    gutter_fg: "#858585",
    shadow: "rgba(0,0,0,0.36)",
    dim_backdrop: "rgba(0,0,0,0.45)",
    close_hot: "#e81123",
    close_down: "#c50f1f",
    caret: "#4a7dff",
    sb_thumb: "#4a4a4a",
    selection: "#264f78",
    selection_fg: "#ffffff",
};

impl Default for Theme {
    fn default() -> Self {
        LIGHT
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_light_and_dark_flag_matches() {
        assert!(!Theme::default().dark);
        // Const assertion (clippy assertions_on_constants): DARK is a
        // constant palette; this guards against someone mistakenly changing
        // the `dark` field — should be caught at compile time
        const { assert!(DARK.dark) }
        assert_eq!(Theme::default().name, LIGHT.name);
    }

    /// The input border must be distinguishable from the input background.
    ///
    /// In the old DARK palette both fields were `#3c3c3c`: in dark mode the
    /// input had no border and its placeholder text (which was colored from
    /// `input_border` at the time) was entirely invisible — an input that
    /// "did not exist".
    #[test]
    fn input_border_is_visible_against_its_background() {
        for t in [LIGHT, DARK] {
            assert_ne!(
                t.input_border, t.input_bg,
                "{}: input border and background are the same value",
                t.name
            );
        }
    }

    /// Both palettes are "pure palettes": all color values are non-empty
    /// (a missing value would render as transparent/invalid).
    #[test]
    fn palettes_are_fully_populated() {
        for t in [LIGHT, DARK] {
            for (k, v) in [
                ("bg", t.bg),
                ("fg", t.fg),
                ("panel_bg", t.panel_bg),
                ("menu_bg", t.menu_bg),
                ("menu_fg", t.menu_fg),
                ("menu_border", t.menu_border),
                ("accent", t.accent),
                ("input_bg", t.input_bg),
                ("input_border", t.input_border),
                ("btn_hover", t.btn_hover),
                ("btn_active", t.btn_active),
                ("shadow", t.shadow),
                ("dim_backdrop", t.dim_backdrop),
            ] {
                assert!(!v.is_empty(), "{}: {k} is empty", t.name);
            }
        }
    }
}
