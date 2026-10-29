//! The popup control library: controls shared by Blitz popups (insert number,
//! about…).
//!
//! Theme decoupling: the controls only know this module's [`PopTheme`] (a
//! pure palette) and not the editor's `highlight::Theme` — this is the
//! precondition for extracting into `winshell` (the app side feeds it in via
//! `PopTheme`'s `From` conversion).
//!
//! All are pure display + event-bubbling components: they hold no business
//! state; clicks/keys go through [`EventHandler`] into each popup's own
//! command channel.
//!
//! Note: find/replace (`findwin`) and preferences (`prefswin`) have already
//! moved to `ui-kit` components + standard HTML (real `<button>`/`<input>`,
//! three-state CSS pseudo-classes; see the `ui_kit::style` module doc); this
//! module only keeps what the **virtual focus model** still needs —
//! [`CtlComposer`] (the transparent textarea focus anchor) and [`CtlField`]
//! (the buffer-only input row). When migrating the remaining popups
//! (numwin/aboutwin), prefer ui-kit, and this module shrinks until deletion.
//!
//! # Iron rule (violating it means "cannot type in the popup")
//!
//! **Keyboard only goes into the composer**: the transparent textarea is the
//! only DOM focus anchor (blitz's `MountedData::set_focus` panics inside event
//! handlers, so focus can only be taken via `autofocus` when a node mounts).
//! The `key` parameter is used to swap the node and re-grab focus.

use dioxus_native::prelude::*;

use crate::shell::motion::Press;

// ---------------------------------------------------------------------------
// Theme (pure palette)
// ---------------------------------------------------------------------------

/// The popup controls' theme palette: only the dozen or so colors the controls
/// actually read, carrying no editor semantics.
///
/// The decoupling point for extracting the crate: the control library no
/// longer knows `highlight::Theme`; the app side converts with
/// `impl From<&'static Theme> for PopTheme` at the render site (value copy,
/// zero allocation).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PopTheme {
    /// Accent color (checked / primary button / focus border)
    pub accent: &'static str,
    /// Input background
    pub input_bg: &'static str,
    /// Input border color
    pub input_border: &'static str,
    /// Body foreground (menu/label/button text)
    pub menu_fg: &'static str,
    /// Menu/title-bar background
    pub menu_bg: &'static str,
    /// Group box/dropdown border color
    pub menu_border: &'static str,
    /// Selected/expanded item background
    pub menu_open: &'static str,
    /// Hover background
    pub btn_hover: &'static str,
    /// Pressed background
    pub btn_active: &'static str,
    /// Panel background
    pub panel_bg: &'static str,
    /// Secondary text (placeholder/disabled)
    pub gutter_fg: &'static str,
    /// Virtual caret color
    pub caret: &'static str,
}

/// Input field height
pub const FIELD_H: f32 = 22.0;

// ---------------------------------------------------------------------------
// Keyboard events
// ---------------------------------------------------------------------------

/// A key press reported by the composer (interpreted by the popup itself).
#[derive(Debug, Clone, PartialEq)]
pub enum CompKey {
    /// Plain character input (including IME-committed text)
    Insert(String),
    Backspace,
    /// Enter (shift=true means Shift+Enter)
    Enter {
        shift: bool,
    },
    Escape,
    /// Tab (shift=true means Shift+Tab)
    Tab {
        shift: bool,
    },
    Paste,
    Copy,
    Cut,
    Up,
    Down,
    /// Unconsumed (arrow keys/delete etc.); the default action continues
    Other,
}

// ---------------------------------------------------------------------------
// composer
// ---------------------------------------------------------------------------

/// The popup's IME composer: a transparent textarea focus anchor.
///
/// The model matches the main window's composer (`components/composer.rs`):
/// value is always empty, character input goes through `onkeydown` (`WM_CHAR`
/// straight in, bubbled up as [`CompKey::Insert`]), and IME commits go through
/// `WindowEvent::Ime` (registered by each popup's `use_window_event` and
/// converted to [`CompKey::Insert`]). A `key` change = swap the node and
/// re-mount, rebuilding the DOM focus chain via `autofocus` (blitz's
/// `set_focus` panics inside event handlers, so this is the only safe way to
/// take it back).
#[component]
pub fn CtlComposer(
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    font_px: f32,
    /// Re-mount key (changing it replaces the node)
    ck: String,
    onkey: EventHandler<CompKey>,
) -> Element {
    let comp_x = x as i32;
    let comp_y = y as i32;
    let comp_w = w as i32;
    let comp_h = h as i32;
    // Blur re-mount epoch: once DOM focus is cleared by an external path
    // (some blitz paths clear_focus after a popup loses focus), take it back
    // on the re-mounted node via `autofocus` — blitz's `set_focus` panics
    // inside event handlers, so this is the only safe way to take it back.
    let mut epoch = use_signal(|| 0u32);
    let e = epoch();
    let ck = format!("{ck}-{e}");
    let style = format!(
        "position:absolute; left:{comp_x}px; top:{comp_y}px; width:{comp_w}px; height:{comp_h}px;
         background:transparent; color:transparent; caret-color:transparent;
         border:none; outline:none; padding:0; margin:0; resize:none;
         font-family:Consolas,'Cascadia Mono',monospace; font-size:{font_px}px;
         line-height:{comp_h}px; white-space:pre; overflow:hidden; z-index:200;"
    );

    rsx! {
        textarea {
            key: "{ck}",
            style: "{style}",
            value: "",
            autocorrect: "off",
            autocomplete: "off",
            spellcheck: "false",
            rows: "1",
            autofocus: "true",
            onblur: move |_| {
                let next = *epoch.peek() + 1;
                epoch.set(next);
            },
            onkeydown: move |evt| {
                let d = &evt.data;
                let ctrl = d.modifiers().ctrl();
                let shift = d.modifiers().shift();
                let msg = match d.key() {
                    Key::Enter => Some(CompKey::Enter { shift }),
                    Key::Escape => Some(CompKey::Escape),
                    Key::Tab => Some(CompKey::Tab { shift }),
                    Key::Backspace => Some(CompKey::Backspace),
                    Key::ArrowUp => Some(CompKey::Up),
                    Key::ArrowDown => Some(CompKey::Down),
                    Key::ArrowLeft | Key::ArrowRight | Key::Home | Key::End | Key::Delete => {
                        Some(CompKey::Other)
                    }
                    Key::Character(s) if ctrl => match s.to_ascii_lowercase().as_str() {
                        "v" => Some(CompKey::Paste),
                        "c" => Some(CompKey::Copy),
                        "x" => Some(CompKey::Cut),
                        _ => None,
                    },
                    Key::Character(s) => Some(CompKey::Insert(s)),
                    _ => None,
                };
                if let Some(m) = msg {
                    let consumed = m != CompKey::Other;
                    onkey.call(m);
                    if consumed {
                        evt.prevent_default();
                    }
                }
            },
            onpointerdown: move |evt| {
                // Clicking into the textarea already lands on the input (focus
                // is managed by the keyboard model)
                evt.stop_propagation();
                evt.prevent_default();
            },
            onclick: move |evt| evt.prevent_default(),
        }
    }
}

// ---------------------------------------------------------------------------
// Buttons
// ---------------------------------------------------------------------------

/// Standard button (used by numwin/aboutwin; same visual language as ui-kit's
/// `Button`).
///
/// `on` = pressed/toggled state (accent fill); `primary` = primary button
/// (default button; accent fill always on).
#[component]
pub fn CtlBtn(
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    label: String,
    on: bool,
    primary: bool,
    theme: PopTheme,
    onclick: EventHandler<()>,
) -> Element {
    let mut press = use_signal(Press::default);
    let p = press();
    let (rest_bg, rest_fg, rest_border) = if on || primary {
        (theme.accent, "#ffffff", theme.accent)
    } else {
        (theme.input_bg, theme.menu_fg, theme.menu_border)
    };
    let bg = p.color(rest_bg, theme.btn_hover, theme.btn_active);
    let fg = p.color(rest_fg, theme.menu_fg, theme.menu_fg);
    let border = p.color(rest_border, theme.btn_hover, theme.btn_active);
    let style = format!(
        "position:absolute; left:{x}px; top:{y}px; width:{w}px; height:{h}px; \
         line-height:{h}px; font-size:11px; text-align:center; border-radius:2px;
         cursor:default; overflow:hidden; background:{bg}; color:{fg}; border:1px solid {border};
         {anim}",
        anim = crate::shell::motion::fast(crate::shell::motion::SURFACE)
    );
    rsx! {
        div {
            style: "{style}",
            onmouseenter: move |_| press.write().hover = true,
            onmouseleave: move |_| *press.write() = Press::default(),
            onpointerdown: move |evt| {
                evt.stop_propagation();
                evt.prevent_default();
                press.write().down = true;
            },
            onpointerup: move |_| press.write().down = false,
            onclick: move |evt| {
                evt.stop_propagation();
                evt.prevent_default();
                onclick.call(());
            },
            "{label}"
        }
    }
}

// ---------------------------------------------------------------------------
// Input rows
// ---------------------------------------------------------------------------

/// An input field row in a popup: label + value + virtual caret (clicking
/// fires a focus event).
///
/// Additional controls can hang on the right (children, like the find box's
/// history dropdown button). Clicking the text inside the field only bubbles
/// up a focus request (the virtual focus is managed by the popup's state
/// machine; DOM focus is always on the composer).
#[component]
pub fn CtlField(
    label: String,
    value: String,
    placeholder: String,
    x: f32,
    y: f32,
    label_w: f32,
    field_w: f32,
    focused: bool,
    blink: bool,
    theme: PopTheme,
    onfocus: EventHandler<()>,
    children: Element,
) -> Element {
    let bg = if focused {
        theme.input_bg
    } else {
        theme.panel_bg
    };
    let border = if focused {
        theme.accent
    } else {
        theme.input_border
    };
    let box_style = format!(
        "position:absolute; left:{x}px; top:{y}px; width:{field_w}px; height:{FIELD_H}px;
         background:{bg}; border:1px solid {border}; display:flex; align-items:center;
         padding:0 4px; overflow:hidden; cursor:text; border-radius:2px;"
    );
    let label_style = format!(
        "position:absolute; left:{}px; top:{y}px; width:{label_w}px; height:{FIELD_H}px;
         line-height:{FIELD_H}px; font-size:12px; color:{}; overflow:hidden; white-space:nowrap;",
        x - label_w - 2.0,
        theme.menu_fg
    );
    let text_style = format!(
        "font-size:12px; color:{}; white-space:pre; overflow:hidden; flex:1;
         font-family:Consolas,'Cascadia Mono',monospace;",
        if value.is_empty() {
            theme.gutter_fg
        } else {
            theme.menu_fg
        }
    );
    let caret_style = format!(
        "width:1px; height:14px; background:{}; opacity:{};",
        theme.caret,
        if blink { 1 } else { 0 }
    );
    let shown = if value.is_empty() {
        placeholder
    } else {
        value.clone()
    };
    rsx! {
        div { style: "{label_style}", "{label}" }
        div {
            style: "{box_style}",
            onpointerdown: move |evt| {
                evt.stop_propagation();
                evt.prevent_default();
                onfocus.call(());
            },
            onclick: move |evt| evt.prevent_default(),
            span { style: "{text_style}", "{shown}" }
            if focused {
                div { style: "{caret_style}" }
            }
        }
        {children}
    }
}
