//! Popup window state: Preferences (`ui::prefswin`) and Insert Numbers
//! (`ui::numwin`).
//!
//! Both are **Blitz-rendered owned popups** (same shape as the find popup
//! `ui::findwin`, see `ui::shell`'s multi-window routing). Only the
//! main-window-side mirror state lives here:
//!
//! * `open` — whether the popup should be shown (the source of truth is the
//!   shell's window table; this is a projection);
//! * `rect` — popup geometry (screen logical x, y, w, h), written back by the
//!   popup side via a command after dragging/resizing, persisted with the
//!   settings, restored on next open.
//!
//! Insert Numbers additionally carries the text and virtual focus of two
//! input fields: blitz has only one composer DOM focus anchor per window, so
//! text editing happens on the main-window side (same virtual-focus model as
//! the find box).

/// Main-window-side state of the preferences popup.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PrefsWinState {
    /// The popup should be shown
    pub open: bool,
    /// Popup geometry (screen logical x, y, w, h); `None` = default placement
    pub rect: Option<[f32; 4]>,
}

/// Main-window-side state of the About popup (read-only panel: no input, only
/// visibility + geometry).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AboutWinState {
    /// The popup should be shown
    pub open: bool,
    /// Popup geometry (screen logical x, y, w, h); `None` = default placement
    /// (centered)
    pub rect: Option<[f32; 4]>,
}

/// Virtual-focus fields of the Insert Numbers popup.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum NumField {
    /// Start value
    Start,
    /// Step (increment)
    Step,
}

/// Main-window-side state of the Insert Numbers popup.
#[derive(Clone, Debug, PartialEq)]
pub struct NumWinState {
    /// The popup should be shown
    pub open: bool,
    /// Start-value field text (only digits and a minus sign; falls back to 1
    /// if parsing fails on confirm)
    pub start: String,
    /// Step field text
    pub step: String,
    /// Which field currently owns the keyboard (virtual focus)
    pub focus: NumField,
    /// Caret x offset within the focused field (lets the composer anchor to
    /// the insertion point)
    pub caret_dx: f32,
    /// Popup geometry (screen logical x, y, w, h); `None` = default placement
    pub rect: Option<[f32; 4]>,
}

impl Default for NumWinState {
    fn default() -> Self {
        NumWinState {
            open: false,
            start: "1".into(),
            step: "1".into(),
            focus: NumField::Start,
            caret_dx: 5.0,
            rect: None,
        }
    }
}

impl NumWinState {
    /// Text of the focused field.
    pub fn field(&self, f: NumField) -> &str {
        match f {
            NumField::Start => &self.start,
            NumField::Step => &self.step,
        }
    }

    /// Mutable reference to the focused field's text.
    pub fn field_mut(&mut self, f: NumField) -> &mut String {
        match f {
            NumField::Start => &mut self.start,
            NumField::Step => &mut self.step,
        }
    }
}
