//! About popup: an Blitz-rendered owned window outside the main window
//! (replacing the former in-page dialog).
//!
//! The window shape is exactly the same as the find popup (`findwin`), with
//! the skeleton in winshell (`winshell::shell::bridge::Bridge`) +
//! `winshell::shell` (multi-window routing) + `winshell::shell::controls`
//! (the widget library): a system title bar (draggable, `×` close, Alt+F4),
//! owner attaching, kept out of the taskbar.
//!
//! The About popup is a **read-only information panel** (version, build,
//! current document stats) with no input boxes: the composer still needs a
//! focus anchor to receive Esc/Enter (close), which is pinned inside the
//! "Close" button. The document stats are snapshotted into the mirror when it
//! opens ([`EditorState::about_text`]); document changes during the popup do
//! not flow back — that is dialog semantics, not live monitoring.

use dioxus_native::prelude::*;
use dioxus_native::use_window;
use dioxus_native::winit::event::WindowEvent;

use crate::highlight::Theme;
use crate::ui::design::FONT_UI;
use crate::ui::state::EditorState;
use crate::ui::{Fmt, Msg};
use i18n::Locale;
use winshell::shell::bridge::{bump_focus_epoch, focus_epoch, Bridge};
use winshell::shell::controls::{CompKey, CtlBtn, CtlComposer};
use winshell::shell::{send, use_window_event, ShellEvt};

// ---------------------------------------------------------------------------
// Layout constants
// ---------------------------------------------------------------------------

/// Default/min popup size (logical pixels). About six lines of info text plus
/// one button.
pub const POPUP_SIZE: (f32, f32) = (360.0, 200.0);

const PAD: f32 = 14.0;
const LINE_H: f32 = 20.0;
const BTN_W: f32 = 76.0;
const BTN_H: f32 = 24.0;

// ---------------------------------------------------------------------------
// Main window → popup: display mirror
// ---------------------------------------------------------------------------

/// All display state the popup needs (snapshotted at open, unchanged while
/// the popup is up).
#[derive(Clone, PartialEq)]
pub struct AboutSnap {
    pub open: bool,
    pub dark: bool,
    pub theme: &'static Theme,
    /// About text lines (one per line; blank lines kept). Snapshotted at open
    /// — dialog semantics.
    pub lines: Vec<String>,
    /// UI language: the popup is a separate vdom that cannot read the main
    /// state, so button labels render in the language from the snapshot.
    pub locale: Locale,
    /// Composer remount generation (maintained on the shell side, see
    /// `winshell::shell::bridge::focus_epoch`)
    pub kb_epoch: u32,
}

impl Default for AboutSnap {
    fn default() -> Self {
        AboutSnap {
            open: false,
            dark: false,
            theme: Theme::of(false),
            lines: Vec::new(),
            locale: Locale::DEFAULT,
            kb_epoch: 0,
        }
    }
}

thread_local! {
    static BRIDGE: Bridge<AboutSnap, AboutCmd> = Bridge::new();
}

// ---------------------------------------------------------------------------
// Popup → main window: commands
// ---------------------------------------------------------------------------

/// User actions of the About popup (read-only panel: only close and geometry
/// write-back).
#[derive(Debug, Clone, PartialEq)]
pub enum AboutCmd {
    /// Esc / Enter / "Close" / the system `×`
    Close,
    /// Popup geometry write-back (screen logical coordinates; the main side
    /// stores them in settings)
    Geometry { x: f32, y: f32, w: f32, h: f32 },
}

/// Shell side: called when the popup receives OS focus (rebuild the DOM
/// focus chain via `autofocus`).
pub fn on_child_focused() {
    bump_focus_epoch();
}

/// The main window projects the About text into the shared mirror.
pub fn sync_shared(st: &EditorState) {
    let lines = st.about_text();
    BRIDGE.with(|b| {
        b.set_shared(AboutSnap {
            open: st.about.open,
            dark: st.dark,
            theme: st.theme(),
            lines,
            locale: st.locale(),
            kb_epoch: 0,
        });
    });
}

/// Shell side: flush the shared mirror into the popup Signal (set only on
/// change).
pub fn flush_snap() {
    BRIDGE.with(|b| b.flush(|s| s.kb_epoch = focus_epoch()));
}

/// The popup's current theme darkness (the shell uses it to pick the system
/// title bar's dark/light look).
pub fn snap_dark() -> bool {
    BRIDGE.with(|b| b.shared().dark)
}

/// Popup side: send a command to the main window.
fn send_cmd(c: AboutCmd) {
    BRIDGE.with(|b| b.send(c));
}

/// Main-window side: request the popup to close (shared by the system `×`
/// and Esc).
pub fn request_close() {
    winshell::win::frameless::focus_main();
    send_cmd(AboutCmd::Close);
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
            AboutCmd::Close => {
                st.close_about_window();
                changed = true;
            }
            AboutCmd::Geometry { x, y, w, h } => {
                st.about.rect = Some([x, y, w, h]);
                changed = true;
            }
        }
    }
    changed
}

// ---------------------------------------------------------------------------
// Main-window side: open/close
// ---------------------------------------------------------------------------

impl EditorState {
    /// Show the About popup.
    /// Returns false when the platform has no owned popups or the shell bridge
    /// is unavailable (the caller falls back to an in-page dialog).
    pub fn open_about_window(&mut self) -> bool {
        use crate::ui::shell::ShellEvt;
        if !winshell::owned_popups() || !crate::ui::shell::bridge_ready() {
            return false;
        }
        self.about.open = true;
        self.doc_focused = false;
        let (dw, dh) = POPUP_SIZE;
        let rect = self.about.rect.unwrap_or_else(|| {
            let (mx, my) = winshell::win::frameless::main_window_pos();
            let x = mx + ((self.win.w - dw) / 2.0).max(0.0);
            let y = my + ((self.win.h - dh) / 2.0).max(0.0);
            [x, y, dw, dh]
        });
        self.about.rect = Some(rect);
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

    /// Hide the About popup and give the keyboard back to the document.
    pub fn close_about_window(&mut self) {
        if !self.about.open {
            return;
        }
        self.about.open = false;
        self.doc_focused = true;
        send(ShellEvt::PopupHide { id: ID });
        winshell::win::frameless::activate();
    }

    /// The text lines the About popup shows (snapshotted at open; blank lines
    /// are kept as separators).
    pub fn about_text(&self) -> Vec<String> {
        let doc = self.active_doc();
        vec![
            format!("InfEditor v{}", env!("CARGO_PKG_VERSION")),
            self.tr(Msg::AboutTagline).into_owned(),
            String::new(),
            self.tr(Fmt::AboutDoc {
                name: doc.meta.display_name.clone(),
            })
            .into_owned(),
            self.tr(Fmt::AboutLines {
                lines: doc.line_count() as usize,
            })
            .into_owned(),
            self.tr(Fmt::AboutBytes {
                bytes: doc.table.len() as usize,
            })
            .into_owned(),
            self.tr(Fmt::AboutSyntax {
                syntax: self.spec().name.to_string(),
            })
            .into_owned(),
        ]
    }
}

// ---------------------------------------------------------------------------
// Popup component
// ---------------------------------------------------------------------------

/// The About popup's content (the whole DOM of the second window).
///
/// Read-only information panel: no input boxes; the composer is pinned inside
/// the "Close" button and only receives Esc/Enter.
#[component]
pub fn AboutWindow() -> Element {
    let window = use_window();
    {
        let window = std::sync::Arc::clone(&window);
        use_window_event(move |evt, _| match evt {
            WindowEvent::Moved(_) | WindowEvent::SurfaceResized(_) => {
                let scale = window.scale_factor() as f32;
                if let Ok(p) = window.outer_position() {
                    let size = window.surface_size();
                    send_cmd(AboutCmd::Geometry {
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

    let root_style = format!(
        "position:absolute; inset:0; background:{bg}; overflow:hidden; user-select:none;
         font-family:{FONT_UI};",
        bg = theme.menu_bg
    );
    let text_style = format!(
        "position:absolute; left:{PAD}px; top:{PAD}px; right:{PAD}px;
         color:{fg}; font-size:12px; line-height:{LINE_H}px; white-space:pre-wrap;",
        fg = theme.menu_fg
    );
    let btn_x = POPUP_SIZE.0 - PAD - BTN_W;
    let btn_y = POPUP_SIZE.1 - PAD - BTN_H;

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
            div {
                style: "{text_style}",
                for (i, line) in s.lines.iter().enumerate() {
                    div { key: "l{i}", "{line}" }
                }
            }
            // The composer is pinned inside the close button: Esc/Enter closes
            // (dialog convention)
            CtlComposer {
                key: "cmp",
                x: btn_x + 4.0,
                y: btn_y + 4.0,
                w: BTN_W - 8.0,
                h: BTN_H - 8.0,
                font_px: 11.0,
                ck: "about".to_string(),
                onkey: move |k: CompKey| match k {
                    CompKey::Escape | CompKey::Enter { .. } => {
                        winshell::win::frameless::focus_main();
                        send_cmd(AboutCmd::Close);
                    }
                    _ => {}
                },
            }
            CtlBtn {
                key: "close",
                x: btn_x,
                y: btn_y,
                w: BTN_W,
                h: BTN_H,
                label: s.locale.tr(Msg::CommonClose).into_owned(),
                on: false,
                primary: true,
                theme,
                onclick: move |_| {
                    winshell::win::frameless::focus_main();
                    send_cmd(AboutCmd::Close);
                },
            }
        }
    }
}

// ---------------------------------------------------------------------------
// winshell registration
// ---------------------------------------------------------------------------

/// The stable id of this popup (the routing key of
/// `ShellEvt::PopupShow/Hide`).
pub const ID: winshell::PopupId = winshell::PopupId("about");

/// Zero-sized registration instance (the [`winshell::Popup`] implementation,
/// entering `register_popups` at startup).
pub struct AboutPopup;

impl winshell::Popup for AboutPopup {
    fn id(&self) -> winshell::PopupId {
        ID
    }
    fn title(&self) -> &'static str {
        // The system title bar reads this once at window creation (see the
        // known limitation in README §6): a language switch takes effect on
        // the next open.
        Msg::DlgAboutTitle.get(i18n::current())
    }
    fn min_size(&self) -> (f32, f32) {
        POPUP_SIZE
    }
    fn dark(&self) -> bool {
        snap_dark()
    }
    fn root(&self) -> fn() -> Element {
        AboutWindow
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
