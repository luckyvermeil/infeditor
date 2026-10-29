//! Insert-numbering popup: an Blitz-rendered owned window outside the main
//! window (replacing the former Win32 native dialog).
//!
//! The window shape is exactly the same as the find popup (`findwin`), with
//! the skeleton in winshell (`winshell::shell::bridge::Bridge`) +
//! `winshell::shell` (multi-window routing) + `winshell::shell::controls`
//! (the widget library).
//!
//! The two input boxes (start value / step) reuse the find popup's
//! **virtual-focus model**: text editing happens on the main-window side in
//! [`NumWinState`](crate::ui::state::NumWinState), and the popup only shows
//! the mirror; the composer (a transparent textarea) is the only DOM focus
//! anchor, pinned inside the currently focused input box. Enter = confirm,
//! Esc = close, Tab = switch boxes. Confirming goes through
//! [`EditorState::insert_numbering`], the same document-writing path as the
//! menu command.

use dioxus_native::prelude::*;
use dioxus_native::use_window;
use dioxus_native::winit::event::WindowEvent;

use crate::highlight::Theme;
use crate::ui::design::FONT_UI;
use crate::ui::state::{EditorState, NumField};
use crate::ui::Msg;
use i18n::Locale;
use winshell::shell::bridge::{bump_focus_epoch, focus_epoch, Bridge};
use winshell::shell::controls::{CompKey, CtlBtn, CtlComposer, CtlField};
use winshell::shell::{send, use_window_event, ShellEvt};

// ---------------------------------------------------------------------------
// Layout constants
// ---------------------------------------------------------------------------

/// Default/min popup size (logical pixels).
pub const POPUP_SIZE: (f32, f32) = (260.0, 150.0);

const PAD: f32 = 12.0;
const LABEL_W: f32 = 52.0;
const FIELD_X: f32 = PAD + LABEL_W + 6.0;
const FIELD_W: f32 = 120.0;
const ROW_STEP: f32 = 30.0;
const BTN_W: f32 = 64.0;
const BTN_H: f32 = 24.0;

// ---- GUI smoke -----------------------------------------------------------
// With `INF_NUM_SMOKE=1` at application startup, the insert-numbering popup
// opens automatically after ~1.5s (for screenshot verification).
// The general implementation lives in [`crate::ui::smoke`].

static SMOKE: crate::ui::smoke::Smoke = crate::ui::smoke::Smoke::new();

pub fn arm_smoke_from_env() {
    SMOKE.arm_from_env("INF_NUM_SMOKE", 1500);
}

pub fn smoke_due() -> bool {
    SMOKE.due()
}

/// Non-destructive probe (for `needs_idle_poll` only, see
/// [`crate::ui::smoke::Smoke::pending`]).
pub fn smoke_pending() -> bool {
    SMOKE.pending()
}

// ---------------------------------------------------------------------------
// Main window → popup: display mirror
// ---------------------------------------------------------------------------

/// All display state the popup needs.
#[derive(Clone, PartialEq)]
pub struct NumSnap {
    /// Composer remount generation (maintained on the shell side, see
    /// `winshell::shell::bridge::focus_epoch`)
    pub kb_epoch: u32,
    pub dark: bool,
    pub theme: &'static Theme,
    pub start: String,
    pub step: String,
    /// 0 = start value / 1 = step
    pub focus: usize,
    /// Caret x offset inside the focused input box
    pub caret_dx: f32,
    /// Input-box caret blink phase (flipped by the shell's timer thread, see
    /// the winshell bridge comments)
    pub blink: bool,
    /// UI language: the popup is a separate vdom that cannot read the main
    /// state, so widget labels render in the language from the snapshot.
    pub locale: Locale,
}

impl Default for NumSnap {
    fn default() -> Self {
        NumSnap {
            kb_epoch: 0,
            dark: false,
            theme: Theme::of(false),
            start: String::new(),
            step: String::new(),
            focus: 0,
            caret_dx: 5.0,
            blink: false,
            locale: Locale::DEFAULT,
        }
    }
}

thread_local! {
    static BRIDGE: Bridge<NumSnap, NumCmd> = Bridge::new();
}

// ---------------------------------------------------------------------------
// Popup → main window: commands
// ---------------------------------------------------------------------------

/// User actions of the insert-numbering popup.
#[derive(Debug, Clone, PartialEq)]
pub enum NumCmd {
    /// Insert text (shared by the keydown's `WM_CHAR` and IME commit)
    Insert(String),
    Backspace,
    /// Tab / clicking an input box: switch the focused box
    Focus(usize),
    /// Enter or "OK"
    Confirm,
    /// Esc / "Cancel" / the system `×`
    Close,
    /// Popup geometry write-back (screen logical coordinates; the main side
    /// stores them in settings)
    Geometry {
        x: f32,
        y: f32,
        w: f32,
        h: f32,
    },
}

/// Shell side: called when the popup receives OS focus (rebuild the DOM
/// focus chain via `autofocus`).
pub fn on_child_focused() {
    bump_focus_epoch();
}

/// The main window projects the numbering state into the shared mirror.
pub fn sync_shared(st: &EditorState) {
    let n = &st.numbering;
    let text = n.field(match n.focus {
        NumField::Step => NumField::Step,
        _ => NumField::Start,
    });
    let w = st.metrics.x_of_offset(text, text.len(), 4);
    let caret_dx = 5.0 + w.min(FIELD_W - 24.0);
    BRIDGE.with(|b| {
        b.set_shared(NumSnap {
            kb_epoch: 0,
            dark: st.dark,
            theme: st.theme(),
            start: n.start.clone(),
            step: n.step.clone(),
            focus: match n.focus {
                NumField::Step => 1,
                _ => 0,
            },
            caret_dx,
            blink: false,
            locale: st.locale(),
        });
    });
}

/// Shell side: flush the shared mirror into the popup Signal (set only on
/// change).
pub fn flush_snap() {
    BRIDGE.with(|b| {
        b.flush(|s| {
            s.kb_epoch = focus_epoch();
            s.blink = winshell::shell::bridge::blink_on();
        })
    });
}

/// The popup's current theme darkness (the shell uses it to pick the system
/// title bar's dark/light look).
pub fn snap_dark() -> bool {
    BRIDGE.with(|b| b.shared().dark)
}

/// Popup side: send a command to the main window.
fn send_cmd(c: NumCmd) {
    BRIDGE.with(|b| b.send(c));
}

/// Main-window side: request the popup to close (shared by the system `×`
/// and Esc).
pub fn request_close() {
    winshell::win::frameless::focus_main();
    send_cmd(NumCmd::Close);
}

/// Main-window side: apply popup commands (called in `poll_dialogs`).
/// Returns whether anything changed.
pub fn apply_cmds(st: &mut EditorState) -> bool {
    let cmds = BRIDGE.with(|b| b.drain());
    if cmds.is_empty() {
        return false;
    }
    let mut changed = false;
    for c in cmds {
        match c {
            NumCmd::Insert(text) => {
                st.numbering_insert(&text);
                changed = true;
            }
            NumCmd::Backspace => {
                st.numbering_backspace();
                changed = true;
            }
            NumCmd::Focus(i) => {
                st.numbering.focus = if i == 1 {
                    NumField::Step
                } else {
                    NumField::Start
                };
                changed = true;
            }
            NumCmd::Confirm => {
                let start: i64 = st.numbering.start.trim().parse().unwrap_or(1);
                let step: i64 = st.numbering.step.trim().parse().unwrap_or(1);
                st.insert_numbering(start, step);
                st.close_numbering_window();
                changed = true;
            }
            NumCmd::Close => {
                st.close_numbering_window();
                changed = true;
            }
            NumCmd::Geometry { x, y, w, h } => {
                st.numbering.rect = Some([x, y, w, h]);
                changed = true;
            }
        }
    }
    changed
}

// ---------------------------------------------------------------------------
// Main-window side: open/close + text editing
// ---------------------------------------------------------------------------

impl EditorState {
    /// Show the insert-numbering popup (the start/step dialog).
    /// Returns false when the platform has no owned popups or the shell bridge
    /// is unavailable (there is no equivalent in-page surface; the caller puts
    /// a hint in the status bar).
    pub fn open_numbering_window(&mut self) -> bool {
        use crate::ui::shell::ShellEvt;
        if !winshell::owned_popups() || !crate::ui::shell::bridge_ready() {
            return false;
        }
        // Every open starts from the default values (same as the old native
        // dialog)
        self.numbering = crate::ui::state::NumWinState::default();
        self.numbering.open = true;
        self.doc_focused = false;
        let (dw, dh) = POPUP_SIZE;
        let rect = self.numbering.rect.unwrap_or_else(|| {
            let (mx, my) = winshell::win::frameless::main_window_pos();
            let x = mx + ((self.win.w - dw) / 2.0).max(0.0);
            let y = my + (self.win.h * 0.3).max(0.0);
            [x, y, dw, dh]
        });
        self.numbering.rect = Some(rect);
        sync_shared(self);
        send(ShellEvt::PopupShow {
            id: ID,
            parent: winshell::win::frameless::current_hwnd(),
            x: rect[0],
            y: rect[1],
            w: rect[2],
            h: rect[3],
        });
        true
    }

    /// Hide the insert-numbering popup and give the keyboard back to the
    /// document.
    pub fn close_numbering_window(&mut self) {
        if !self.numbering.open {
            return;
        }
        self.numbering.open = false;
        self.doc_focused = true;
        send(ShellEvt::PopupHide { id: ID });
        winshell::win::frameless::activate();
    }

    /// Popup input: only digits and the minus sign are accepted (the number
    /// is an i64 prefix; all other characters are dropped outright, aligned
    /// with Win32's `ES_NUMBER` behavior).
    pub fn numbering_insert(&mut self, text: &str) {
        let f = self.numbering.focus;
        let field = self.numbering.field_mut(f);
        for ch in text.chars() {
            if ch.is_ascii_digit() || (ch == '-' && field.is_empty()) {
                field.push(ch);
            }
        }
    }

    /// Popup input: backspace.
    pub fn numbering_backspace(&mut self) {
        let f = self.numbering.focus;
        self.numbering.field_mut(f).pop();
    }
}

// ---------------------------------------------------------------------------
// Popup component
// ---------------------------------------------------------------------------

/// The insert-numbering popup's content (the whole DOM of the second
/// window).
#[component]
pub fn NumberingWindow() -> Element {
    let window = use_window();
    {
        let window = std::sync::Arc::clone(&window);
        use_window_event(move |evt, _| match evt {
            // IME commits do not come through here: vendor patch seven
            // rewrites Commit into a synthetic keydown, and CtlComposer's
            // onkeydown (Character → CompKey::Insert) is the only insertion
            // channel — inserting again in the hook would write every commit
            // twice (hit for real in the main window).
            WindowEvent::Moved(_) | WindowEvent::SurfaceResized(_) => {
                let scale = window.scale_factor() as f32;
                if let Ok(p) = window.outer_position() {
                    let size = window.surface_size();
                    send_cmd(NumCmd::Geometry {
                        x: p.x as f32 / scale,
                        y: p.y as f32 / scale,
                        w: size.width as f32 / scale,
                        h: size.height as f32 / scale,
                    });
                }
            }
            _ => {}
        });
    }

    let snap = use_signal(|| BRIDGE.with(|b| b.shared()));
    use_hook({
        let handle = snap;
        move || {
            BRIDGE.with(|b| b.register(handle));
        }
    });

    let s = snap.read();
    let theme = winshell::PopTheme::from(s.theme);
    let focus = s.focus;
    let start_focused = focus == 0;
    let step_focused = focus == 1;

    let root_style = format!(
        "position:absolute; inset:0; background:{bg}; overflow:hidden; user-select:none;
         font-family:{FONT_UI};",
        bg = theme.menu_bg
    );
    let row0_y = PAD + 4.0;
    let row1_y = row0_y + ROW_STEP;
    let btn_y = row1_y + ROW_STEP + 6.0;

    rsx! {
        div {
            style: "{root_style}",
            onpointerdown: move |evt| {
                evt.stop_propagation();
                evt.prevent_default();
            },
            onclick: move |evt| {
                evt.prevent_default();
            },
            // The composer is pinned inside the focused input box: keyboard
            // input and IME both come through here
            CtlComposer {
                key: "cmp-{focus}",
                x: FIELD_X + s.caret_dx,
                y: (if focus == 1 { row1_y } else { row0_y }) + 3.0,
                w: 80.0,
                h: 16.0,
                font_px: 12.0,
                ck: "{focus}".to_string(),
                onkey: move |k: CompKey| match k {
                    CompKey::Insert(t) => send_cmd(NumCmd::Insert(t)),
                    CompKey::Backspace => send_cmd(NumCmd::Backspace),
                    CompKey::Enter { .. } => send_cmd(NumCmd::Confirm),
                    CompKey::Escape => {
                        winshell::win::frameless::focus_main();
                        send_cmd(NumCmd::Close);
                    }
                    CompKey::Tab { .. } => send_cmd(NumCmd::Focus(1 - focus)),
                    CompKey::Paste | CompKey::Copy | CompKey::Cut | CompKey::Up
                    | CompKey::Down | CompKey::Other => {}
                },
            }
            CtlField {
                key: "f0",
                label: s.locale.tr(Msg::NumWinStart).into_owned(),
                value: s.start.clone(),
                placeholder: "1".to_string(),
                x: FIELD_X,
                y: row0_y,
                label_w: LABEL_W,
                field_w: FIELD_W,
                focused: start_focused,
                blink: s.blink,
                theme,
                onfocus: move |_| send_cmd(NumCmd::Focus(0)),
            }
            CtlField {
                key: "f1",
                label: s.locale.tr(Msg::NumWinStep).into_owned(),
                value: s.step.clone(),
                placeholder: "1".to_string(),
                x: FIELD_X,
                y: row1_y,
                label_w: LABEL_W,
                field_w: FIELD_W,
                focused: step_focused,
                blink: s.blink,
                theme,
                onfocus: move |_| send_cmd(NumCmd::Focus(1)),
            }
            CtlBtn {
                key: "ok",
                x: FIELD_X,
                y: btn_y,
                w: BTN_W,
                h: BTN_H,
                label: s.locale.tr(Msg::CommonOk).into_owned(),
                on: false,
                primary: true,
                theme,
                onclick: move |_| send_cmd(NumCmd::Confirm),
            }
            CtlBtn {
                key: "cancel",
                x: FIELD_X + BTN_W + 8.0,
                y: btn_y,
                w: BTN_W,
                h: BTN_H,
                label: s.locale.tr(Msg::CommonCancel).into_owned(),
                on: false,
                primary: false,
                theme,
                onclick: move |_| {
                    winshell::win::frameless::focus_main();
                    send_cmd(NumCmd::Close);
                },
            }
        }
    }
}

// ---------------------------------------------------------------------------
// winshell registration
// ---------------------------------------------------------------------------

/// This popup's stable identity (the routing key for
/// `ShellEvt::PopupShow/Hide`).
pub const ID: winshell::PopupId = winshell::PopupId("numbering");

/// Zero-size registration instance ([`winshell::Popup`] impl; enters
/// `register_popups` at startup).
pub struct NumPopup;

impl winshell::Popup for NumPopup {
    fn id(&self) -> winshell::PopupId {
        ID
    }
    fn title(&self) -> &'static str {
        // The system title bar is read once at window creation (see README §6
        // known limitations): a language switch takes effect on the next open.
        Msg::NumWinTitle.get(i18n::current())
    }
    fn min_size(&self) -> (f32, f32) {
        POPUP_SIZE
    }
    fn dark(&self) -> bool {
        snap_dark()
    }
    fn root(&self) -> fn() -> Element {
        NumberingWindow
    }
    fn flush(&self) {
        flush_snap()
    }
    fn on_focused(&self) {
        on_child_focused()
    }
    fn request_close(&self) {
        request_close()
    }
}
