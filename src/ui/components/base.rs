//! Base widgets — **implementation moved to the shared [`ui_kit`] library**;
//! this is the app adapter layer.
//!
//! The library only knows its own solid-color palette `ui_kit::Theme`; this
//! layer converts the app theme ([`crate::highlight::Theme`]) into it at the
//! render site (value copy, zero allocation) and delegates. Component names
//! and props match the original implementation (`t: &'static Theme`), so no
//! call site anywhere in the repo needed to change.
//!
//! - [`Btn`] → [`ui_kit::Button`] (real `<button>`, primary/secondary/subtle/danger)
//! - [`Check`] → [`ui_kit::Checkbox`] (semantic `<input type=checkbox>` + drawn box)
//! - [`Field`] → [`ui_kit::TextInput`] (display buffer only; keyboard routed by composer)
//! - [`KitStyle`] → [`ui_kit::KitStyle`] (class stylesheet; mounted once per window)

use dioxus_native::prelude::*;

use crate::highlight::Theme;
use ui_kit::{Button as KitButton, Checkbox as KitCheckbox, TextInput as KitTextInput};

/// Semantic button tier (Fluent Design's button hierarchy) — i.e. the library's
/// [`ui_kit::ButtonKind`].
pub use ui_kit::ButtonKind as BtnKind;

/// The library's class stylesheet (mounted once at the root of every window
/// that uses library components).
///
/// `dark` goes into the `<style>` key: theme switch → node rebuild → blitz
/// re-parses the CSS (editing the `<style>` text in place does not trigger a
/// re-parse; see the `ui_kit::style` module docs).
#[component]
pub fn KitStyle(t: &'static Theme) -> Element {
    rsx! {
        ui_kit::KitStyle { theme: ui_kit::Theme::from(t), dark: t.dark }
    }
}

/// A button. `on_click` receives the raw mouse event (original semantics kept).
#[component]
pub fn Btn(
    label: String,
    #[props(default)] kind: BtnKind,
    t: &'static Theme,
    on_click: EventHandler<MouseEvent>,
    /// Primary buttons get extra horizontal padding (Fluent's action-row visual weight).
    #[props(default)]
    wide: bool,
    /// DOM id (used by layout probes; see `components::dialog::action_button_dom_id`).
    #[props(default)]
    id: Option<String>,
) -> Element {
    rsx! {
        KitButton {
            label,
            kind,
            theme: ui_kit::Theme::from(t),
            on_click,
            wide,
            id,
        }
    }
}

/// A checkbox (optional label). `on` is the controlled state; the click
/// callback `on_toggle` flips it.
#[component]
pub fn Check(
    on: bool,
    t: &'static Theme,
    on_toggle: EventHandler<MouseEvent>,
    #[props(default)] label: String,
) -> Element {
    rsx! {
        KitCheckbox {
            checked: on,
            theme: ui_kit::Theme::from(t),
            on_toggle,
            label,
        }
    }
}

/// A single-line text box (display buffer only; input routed by composer, see
/// components::composer).
#[component]
pub fn Field(
    value: String,
    #[props(default)] hint: String,
    focused: bool,
    t: &'static Theme,
) -> Element {
    rsx! {
        KitTextInput {
            value,
            hint,
            focused,
            theme: ui_kit::Theme::from(t),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Primary buttons always carry the theme accent border (visual hierarchy
    /// guarantee: the recommended action stands out at a glance in an action row).
    #[test]
    fn btn_kinds_cover_all_four() {
        assert!(!BtnKind::Primary.is_destructive());
        assert!(!BtnKind::Secondary.is_destructive());
        assert!(!BtnKind::Subtle.is_destructive());
        assert!(BtnKind::Danger.is_destructive());
        assert_eq!(BtnKind::default(), BtnKind::Primary);
    }
}
