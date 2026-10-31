//! Button primitive: a real `<button>` element.
//!
//! [`Button`] is a set of four semantic button kinds (primary/secondary/subtle/
//! danger), matching the button hierarchy of Fluent / Ant Design: the primary
//! button uses accent-color fill for the "recommended action", the secondary
//! button a neutral surface, the subtle button has no background and surfaces
//! on hover, and the danger button turns red on hover (destructive actions
//! must be "clearly seen before you dare click").
//!
//! All state goes through standard CSS: `:hover` / `:active` are maintained
//! natively by blitz (stylo pseudo-class parsing + the document's hover/active
//! state bits), and `:disabled` follows the `disabled` attribute — see
//! [`crate::stylesheet`] for the three-state rules; there is no state signal
//! left inside the component.
//!
//! Event contract (preserving the old semantics): `onpointerdown` does stop +
//! prevent (stopping bubbling to the scrim of a dismissable dialog, and
//! stopping the press-drag text selection), `onclick` does stop + prevent and
//! then reports upward (blitz's default click action clears focus / walks the
//! hit chain; buttons intercept it all). Hosts need not repeat this.
//!
//! **Note that `disabled` must be written as
//! `disabled: disabled.then_some(true)`**: blitz judges disabled by attribute
//! presence, so `disabled: false` writes a `disabled="false"` attribute and
//! disables the button (see the `crate::style` module docs).

use dioxus_native::prelude::*;

use crate::Theme;

/// The semantic kind of a button.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum ButtonKind {
    /// Primary: accent-color fill + white text (OK / recommended / submit).
    #[default]
    Primary,
    /// Secondary: neutral surface + border (Cancel / other non-emphasized
    /// actions).
    Secondary,
    /// Subtle: no background, surfaces on hover (value controls that can be
    /// clicked repeatedly).
    Subtle,
    /// Danger: turns red on hover.
    Danger,
}

impl ButtonKind {
    /// The danger kind (destructive action) — rendering uses the red three-
    /// state family accordingly.
    pub fn is_destructive(self) -> bool {
        matches!(self, ButtonKind::Danger)
    }
    /// The class of this kind (the rule group of the same name in
    /// [`crate::stylesheet`]).
    pub fn class(self) -> &'static str {
        match self {
            ButtonKind::Primary => "k-btn k-btn-primary",
            ButtonKind::Secondary => "k-btn k-btn-secondary",
            ButtonKind::Subtle => "k-btn k-btn-subtle",
            ButtonKind::Danger => "k-btn k-btn-danger",
        }
    }
}

/// A button. `on_click` receives the raw mouse event (this component already
/// `stop`s/`prevent`s inside the event, hosts need not repeat it; in the popup
/// scrim scenario bubbling is already handled by this component).
#[component]
pub fn Button(
    label: String,
    #[props(default)] kind: ButtonKind,
    theme: Theme,
    on_click: EventHandler<MouseEvent>,
    /// The primary button gets extra horizontal padding (visual weight in an
    /// action row).
    #[props(default)]
    wide: bool,
    #[props(default)] disabled: bool,
    /// DOM id. Hosts use it as a layout probe (the real-machine geometry oracle
    /// for dialog buttons, see `ui::shell::probe_dialog_buttons`); no id is
    /// set when omitted.
    #[props(default)]
    id: Option<String>,
    /// Extra inline declarations (caller-owned; appended after the class).
    ///
    /// Only needed when a colour must follow a **theme switch made while the
    /// button is on screen**: a class-only colour is restyled, but blitz keeps
    /// painting the cached glyph run until the node itself is mutated (see the
    /// menu captions' colour in `ui/components/titlebar.rs`). The preferences
    /// popup carries the theme toggle, so its footer button pins the secondary
    /// foreground inline.
    #[props(default)]
    style: Option<String>,
) -> Element {
    let _ = theme; // all colors come from the class stylesheet; the theme
                   // argument is kept to stabilize the API
    let mut class = kind.class().to_string();
    if wide {
        class.push_str(" k-btn-wide");
    }
    rsx! {
        button {
            class: "{class}",
            id: id,
            style: style.unwrap_or_default(),
            disabled: disabled.then_some(true),
            onpointerdown: move |evt| {
                // Must stop_propagation + prevent_default: the scrim of a
                // dismissable dialog closes the dialog in its own
                // onpointerdown — a button press bubbling there would remove the
                // dialog before its click ever ran the command; prevent_default
                // also stops press-drag selection and focus clearing.
                evt.stop_propagation();
                evt.prevent_default();
            },
            onclick: move |evt| {
                evt.stop_propagation();
                evt.prevent_default();
                if !disabled {
                    on_click.call(evt);
                }
            },
            "{label}"
        }
    }
}
