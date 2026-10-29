//! Preferences popup: a Blitz-rendered owned window outside the main window
//! (replacing the original Win32 native dialog).
//!
//! The window shape matches the find popup (`findwin`) exactly; the skeleton
//! lives in winshell (`winshell::shell::bridge::Bridge`) + `winshell::shell`
//! (multi-window routing). This module does two things:
//!
//! * **Projection**: the main window's `EditorState` setting switches → the
//!   [`PrefsSnap`] display mirror;
//! * **Commands**: a user click → [`PrefsCmd`] (wrapping [`Cmd`] or a
//!   popup-local action), applied uniformly on the main-window side in
//!   `poll_dialogs`.
//!
//! Layout: a category list on the left (Common / Editing / Highlight / Vim /
//! Association), grouped pages on the right, "Close" at the bottom right.
//! Every switch takes effect immediately and shares the same
//! [`crate::ui::actions::run`] path as the in-page settings commands.
//!
//! Controls come from ui-kit / native HTML: switches are
//! [`ui_kit::Checkbox`] (natively drawn), buttons are [`ui_kit::Button`],
//! category navigation is real `.k-nav` buttons, and group boxes are flow
//! `.k-group` (content stacks naturally, no more per-pixel placement). Touch
//! reliability matches the find popup: interactive classes all carry
//! `touch-action:none` (see the `ui_kit::style` module docs). The 1px
//! transparent composer remains the only DOM focus anchor (catches Esc to
//! close; the clear_focus on empty-space clicks is exempted by vendor patch
//! five, with a composer refocus-rehook self-healing fallback).

use dioxus_native::prelude::*;
use dioxus_native::use_window;
use dioxus_native::winit::event::WindowEvent;

use crate::highlight::Theme;
use crate::ui::actions::{self, Cmd};
use crate::ui::design::FONT_UI;
use crate::ui::state::{EditorState, PrefsWinState};
use crate::ui::{Fmt, Locale, Msg};
use winshell::shell::bridge::{bump_focus_epoch, focus_epoch, Bridge};
use winshell::shell::controls::{CompKey, CtlComposer};
use winshell::shell::{send, use_window_event, ShellEvt};

// ---------------------------------------------------------------------------
// Layout constants
// ---------------------------------------------------------------------------

/// Popup default/minimum size (logical pixels).
///
/// Must leave enough room for the "Association" page: three lists stacked
/// vertically (file types / supported extensions / registered), plus two
/// switches and a note line below.
pub const POPUP_SIZE: (f32, f32) = (580.0, 400.0);

const PAD: f32 = 10.0;
/// Width of the left category nav (row height lives in the `.k-nav` class).
const NAV_W: f32 = 120.0;
/// Bottom button row height.
const BOTTOM_H: f32 = 44.0;

/// Category list (this app's preferences category tabs).
///
/// Preferences categories. Holds [`Msg`], not strings: the tab text follows the
/// language while "which page" is the array index (`PageBody`'s `match page`).
const CATEGORIES: &[Msg] = &[
    Msg::PrefsCatCommon,
    Msg::PrefsCatEdit,
    Msg::PrefsCatHighlight,
    Msg::PrefsCatVim,
    Msg::PrefsCatLanguage,
    Msg::PrefsCatAssoc,
];

// ---- GUI smoke -------------------------------------------------------------
// When the app starts with `INF_PREFS_SMOKE=1`, the preferences popup opens
// automatically after ~1.5s (screenshot verification).
// The generic implementation lives in [`crate::ui::smoke`].

static SMOKE: crate::ui::smoke::Smoke = crate::ui::smoke::Smoke::new();

pub fn arm_smoke_from_env() {
    SMOKE.arm_from_env("INF_PREFS_SMOKE", 1500);
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

/// All display state the popup needs to render (a projection of the
/// `EditorState` settings).
#[derive(Clone)]
pub struct PrefsSnap {
    /// composer re-mount epoch (maintained on the shell side, see
    /// `winshell::shell::bridge::focus_epoch`)
    pub kb_epoch: u32,
    pub dark: bool,
    pub theme: &'static Theme,
    pub highlight_on: bool,
    pub highlight_all: bool,
    pub wrap: bool,
    pub show_eol: bool,
    pub show_ws: bool,
    pub caret_blink: bool,
    /// 0 line / 1 block / 2 underline
    pub caret_style: usize,
    pub vim: bool,
    pub font_size: f32,
    pub auto_close: bool,
    pub topmost: bool,
    /// UI language. This window is a separate vdom and cannot read the main state.
    pub locale: Locale,
    /// Extensions already associated (written to `OpenWithProgids`); the
    /// truth lives in the registry, this is the mirror read at open time,
    /// see [`crate::ui::assoc`].
    pub assoc_registered: Vec<String>,
    /// Whether Explorer's right-click "Open with InfEditor" is registered.
    pub assoc_ctx_menu: bool,
}

impl Default for PrefsSnap {
    fn default() -> Self {
        PrefsSnap {
            kb_epoch: 0,
            dark: false,
            theme: Theme::of(false),
            highlight_on: true,
            highlight_all: false,
            wrap: false,
            show_eol: false,
            show_ws: false,
            caret_blink: true,
            caret_style: 0,
            vim: false,
            font_size: crate::ui::state::DEFAULT_FONT_SIZE,
            auto_close: true,
            topmost: false,
            locale: Locale::DEFAULT,
            assoc_registered: Vec::new(),
            assoc_ctx_menu: false,
        }
    }
}

impl PartialEq for PrefsSnap {
    fn eq(&self, other: &Self) -> bool {
        // compare theme via dark (`&'static` pointer semantics are more robust)
        self.kb_epoch == other.kb_epoch
            && self.dark == other.dark
            && self.highlight_on == other.highlight_on
            && self.highlight_all == other.highlight_all
            && self.wrap == other.wrap
            && self.show_eol == other.show_eol
            && self.show_ws == other.show_ws
            && self.caret_blink == other.caret_blink
            && self.caret_style == other.caret_style
            && self.vim == other.vim
            && self.font_size == other.font_size
            && self.auto_close == other.auto_close
            && self.topmost == other.topmost
            && self.locale == other.locale
            && self.assoc_registered == other.assoc_registered
            && self.assoc_ctx_menu == other.assoc_ctx_menu
    }
}

thread_local! {
    static BRIDGE: Bridge<PrefsSnap, PrefsCmd> = Bridge::new();
}

// ---------------------------------------------------------------------------
// Popup → main window: commands
// ---------------------------------------------------------------------------

/// User actions of the preferences popup.
#[derive(Debug, Clone, PartialEq)]
pub enum PrefsCmd {
    /// A settings switch/action (reusing the main window's settings-menu commands)
    Toggle(Cmd),
    /// Close the popup (content `×`, Esc, and system `×` share one path)
    Close,
    /// File association: registry writes (see [`crate::ui::assoc`]); not
    /// carried by a plain `Cmd` because it has no menu entry and no
    /// "settings item" semantics.
    Assoc(crate::ui::assoc::AssocCmd),
    /// Popup geometry write-back (screen logical coords; stored in settings
    /// on the main side)
    Geometry { x: f32, y: f32, w: f32, h: f32 },
}

/// Shell side: called when the popup gains OS focus (rebuilds the DOM focus
/// chain via `autofocus`).
pub fn on_child_focused() {
    bump_focus_epoch();
}

/// Cache of registry-derived state. Every call to `registered_exts()` walks
/// dozens of registry keys (an OpenKey+QueryValue each), and `poll_dialogs`
/// calls `sync_shared` **every frame** while the popup is open — hammering
/// the registry at ~60 calls/sec. The cache refreshes only when the popup
/// opens (first use) and after [`PrefsCmd::Assoc`] runs (a registry write).
struct AssocSnap {
    registered: Vec<String>,
    ctx_menu: bool,
}

thread_local! {
    static ASSOC_CACHE: std::cell::RefCell<Option<AssocSnap>> =
        const { std::cell::RefCell::new(None) };
}

fn assoc_snapshot() -> (Vec<String>, bool) {
    ASSOC_CACHE.with(|c| {
        let mut c = c.borrow_mut();
        let snap = c.get_or_insert_with(|| AssocSnap {
            registered: crate::ui::assoc::registered_exts(),
            ctx_menu: crate::ui::assoc::context_menu_installed(),
        });
        (snap.registered.clone(), snap.ctx_menu)
    })
}

fn invalidate_assoc_cache() {
    ASSOC_CACHE.with(|c| *c.borrow_mut() = None);
}

/// The main window projects its settings into the shared mirror.
pub fn sync_shared(st: &EditorState) {
    let (assoc_registered, assoc_ctx_menu) = assoc_snapshot();
    BRIDGE.with(|b| {
        b.set_shared(PrefsSnap {
            kb_epoch: 0,
            dark: st.dark,
            theme: st.theme(),
            highlight_on: st.highlight_on,
            highlight_all: st.highlight_all,
            wrap: st.wrap,
            show_eol: st.show_eol,
            show_ws: st.show_ws,
            caret_blink: st.caret_blink,
            caret_style: match st.caret_style {
                crate::ui::state::CaretStyle::Line => 0,
                crate::ui::state::CaretStyle::Block => 1,
                crate::ui::state::CaretStyle::Underline => 2,
            },
            vim: st.vim.enabled,
            font_size: st.font_size,
            auto_close: st.auto_close,
            topmost: st.win.topmost,
            locale: st.locale,
            assoc_registered,
            assoc_ctx_menu,
        });
    });
}

/// Shell side: flush the shared mirror into the popup's Signal (sets only on
/// change).
pub fn flush_snap() {
    BRIDGE.with(|b| b.flush(|s| s.kb_epoch = focus_epoch()));
}

/// The popup's current theme darkness (the shell uses it to pick a dark/light
/// system title bar).
pub fn snap_dark() -> bool {
    BRIDGE.with(|b| b.shared().dark)
}

/// Popup side: send a command to the main window.
fn send_cmd(c: PrefsCmd) {
    BRIDGE.with(|b| b.send(c));
}

/// Main-window side: request closing the popup (shared by the system `×` and
/// Esc).
pub fn request_close() {
    winshell::win::frameless::focus_main();
    send_cmd(PrefsCmd::Close);
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
            PrefsCmd::Toggle(cmd) => {
                actions::run(st, cmd);
                changed = true;
            }
            PrefsCmd::Close => {
                st.close_prefs_window();
                changed = true;
            }
            // Registry IO: invalidate the cache and re-project the mirror
            // right after the write, so the right-hand list refreshes in the
            // same interaction.
            PrefsCmd::Assoc(cmd) => {
                st.status = crate::ui::assoc::apply(cmd);
                invalidate_assoc_cache();
                sync_shared(st);
                changed = true;
            }
            PrefsCmd::Geometry { x, y, w, h } => {
                st.prefs.rect = Some([x, y, w, h]);
                changed = true;
            }
        }
    }
    changed
}

// ---------------------------------------------------------------------------
// Main-window side: open/close (routed via `shell::ShellEvt::PopupShow`)
// ---------------------------------------------------------------------------

impl EditorState {
    /// Place the preferences popup at its remembered position (default:
    /// centered over the main window's upper area when nothing is remembered)
    /// and show it. Returns false when the platform has no owned popups or
    /// the shell bridge is unavailable; the caller falls back to the in-page
    /// dialog.
    pub fn open_prefs_window(&mut self) -> bool {
        use crate::ui::shell::ShellEvt;
        if !winshell::owned_popups() || !crate::ui::shell::bridge_ready() {
            return false;
        }
        let (dw, dh) = POPUP_SIZE;
        let rect = self
            .prefs
            .rect
            .filter(|r| r[2] >= dw * 0.6 && r[3] >= dh * 0.6)
            .unwrap_or_else(|| {
                let (mx, my) = winshell::win::frameless::main_window_pos();
                let x = mx + ((self.win.w - dw) / 2.0).max(0.0);
                let y = my + (self.win.h * 0.15).max(0.0);
                [x, y, dw, dh]
            });
        self.prefs.open = true;
        self.doc_focused = false;
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

    /// Hide the preferences popup and give the keyboard back to the document.
    pub fn close_prefs_window(&mut self) {
        if !self.prefs.open {
            return;
        }
        self.prefs.open = false;
        self.doc_focused = true;
        send(ShellEvt::PopupHide { id: ID });
        winshell::win::frameless::activate();
        // Persist settings such as geometry once on close (the unit tests'
        // minimal() has no shell bridge).
        if crate::ui::shell::bridge_ready() {
            crate::ui::settings::save(&crate::ui::settings::AppSettings::capture(self));
        }
    }

    /// Main-window-side state (for tests/callers).
    pub fn prefs_window(&self) -> &PrefsWinState {
        &self.prefs
    }
}

// ---------------------------------------------------------------------------
// Popup component
// ---------------------------------------------------------------------------

/// The preferences popup's content (the entire DOM of the second window).
///
/// The layout is flex: the content row (left nav + right page) takes all
/// height except the bottom button row; page content is flow `.k-group`
/// boxes that grow naturally with their content.
#[component]
pub fn PrefsWindow() -> Element {
    let window = use_window();
    // This window's own events: move/resize → geometry write-back (same path
    // as findwin).
    {
        let window = std::sync::Arc::clone(&window);
        use_window_event(move |evt, _| match evt {
            WindowEvent::Moved(_) | WindowEvent::SurfaceResized(_) => {
                let scale = window.scale_factor() as f32;
                if let Ok(p) = window.outer_position() {
                    let size = window.surface_size();
                    send_cmd(PrefsCmd::Geometry {
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
    let kit_theme = ui_kit::Theme::from(s.theme);
    let (vw, vh) = {
        let scale = window.scale_factor() as f32;
        let size = window.surface_size();
        let vw = (size.width as f32 / scale).max(POPUP_SIZE.0);
        let vh = (size.height as f32 / scale).max(POPUP_SIZE.1);
        (vw, vh)
    };
    // Available size for the right-hand page: content row = window − bottom
    // button row − vertical padding.
    let page_w = vw - 2.0 * PAD - NAV_W - 10.0;
    let page_h = vh - BOTTOM_H - 2.0 * PAD;

    // Category selection is the popup's own UI state (no need to report back
    // to the main window)
    let mut page = use_signal(|| 0usize);
    let cur = *page.read();

    let root_style = format!(
        "position:absolute; inset:0; background:{bg}; overflow:hidden; user-select:none;
         font-family:{FONT_UI}; display:flex; flex-direction:column;",
        bg = s.theme.menu_bg
    );
    let border = s.theme.menu_border;
    let bottom_style = format!(
        "flex:none; height:{BOTTOM_H}px; display:flex; align-items:center;
         justify-content:flex-end; padding:0 {PAD}px; border-top:1px solid {border};"
    );

    // 1px transparent composer: the only DOM focus anchor, just to catch Esc
    // to close.
    let esc_anchor: Element = {
        rsx! {
            CtlComposer {
                x: 0.0,
                y: 0.0,
                w: 1.0,
                h: 1.0,
                font_px: 10.0,
                ck: "0".to_string(),
                onkey: move |k: CompKey| {
                    if k == CompKey::Escape {
                        send_cmd(PrefsCmd::Close);
                    }
                },
            }
        }
    };

    rsx! {
        div {
            style: "{root_style}",
            ui_kit::KitStyle { theme: kit_theme, dark: s.dark }
            {esc_anchor}
            // ---- Content row: left nav + right page ----
            div {
                style: "flex:1 1 auto; min-height:0; display:flex; padding:{PAD}px;
                        gap:10px;",
                // Left category nav (real .k-nav buttons, same left accent bar
                // as the VS Code settings page)
                nav {
                    style: "flex:none; width:{NAV_W}px; display:flex; flex-direction:column;
                            gap:2px;",
                    for (i, name) in CATEGORIES.iter().enumerate() {
                        button {
                            key: "nav{i}",
                            class: if cur == i { "k-nav k-nav-on" } else { "k-nav" },
                            onclick: move |evt| {
                                evt.stop_propagation();
                                evt.prevent_default();
                                page.set(i);
                            },
                            "{s.locale.tr(*name).into_owned()}"
                        }
                    }
                }
                // Right-hand page
                div {
                    style: "flex:1 1 auto; min-width:0;",
                    PageBody { page: cur, w: page_w, h: page_h, snap: s.clone(), kit_theme, theme: s.theme }
                }
            }
            // ---- Bottom button row ----
            div {
                style: "{bottom_style}",
                ui_kit::Button {
                    label: s.locale.tr(Msg::CommonClose).into_owned(),
                    kind: ui_kit::ButtonKind::Secondary,
                    theme: kit_theme,
                    on_click: move |_| send_cmd(PrefsCmd::Close),
                }
            }
        }
    }
}

/// One page of settings content: flow `.k-group` boxes that grow naturally
/// with their content.
///
/// `w`/`h` are only needed by the "Association" page's custom scroll lists
/// (list row counts are computed from height); ordinary pages are pure flex
/// flow with no per-pixel placement.
#[component]
fn PageBody(
    page: usize,
    w: f32,
    h: f32,
    snap: PrefsSnap,
    kit_theme: ui_kit::Theme,
    theme: &'static Theme,
) -> Element {
    let s = snap;
    let toggle = |cmd: Cmd| move |_| send_cmd(PrefsCmd::Toggle(cmd));

    let loc = s.locale;
    // Group box (title + flow children)
    let group = |title: Msg, body: Element| {
        rsx! {
            div {
                class: "k-group",
                div { class: "k-group-title", "{loc.tr(title).into_owned()}" }
                { body }
            }
        }
    };
    let checks = |rows: Vec<(Msg, bool, Cmd)>| {
        rsx! {
            for (label, on, cmd) in rows {
                ui_kit::Checkbox {
                    key: "{label.key()}",
                    checked: on,
                    theme: kit_theme,
                    label: loc.tr(label).into_owned(),
                    on_toggle: toggle(cmd),
                }
            }
        }
    };

    match page {
        // ---- Common: appearance + font ------------------------------------------
        0 => {
            let g1 = group(
                Msg::PrefsAppearance,
                checks(vec![
                    (Msg::PrefsDarkTheme, s.dark, Cmd::ToggleTheme),
                    (Msg::PrefsTopmost, s.topmost, Cmd::AlwaysOnTop),
                ]),
            );
            let sz = s.font_size.round() as i32;
            // An rsx format string cannot nest a `{ px: sz }` expression, so
            // parameterized messages are rendered into a local first.
            let font_label = loc.tr(Fmt::PrefsFontSize { px: sz }).into_owned();
            let g2 = group(
                Msg::PrefsFont,
                rsx! {
                    div {
                        style: "display:flex; align-items:center; gap:8px;",
                        span { class: "k-label", "{font_label}" }
                        ui_kit::Button { label: "\u{2212}".to_string(), theme: kit_theme, on_click: toggle(Cmd::ZoomOut) }
                        ui_kit::Button { label: loc.tr(Msg::PrefsReset).into_owned(), theme: kit_theme, on_click: toggle(Cmd::ZoomReset) }
                        ui_kit::Button { label: "+".to_string(), theme: kit_theme, on_click: toggle(Cmd::ZoomIn) }
                    }
                },
            );
            rsx! {
                div {
                    style: "display:flex; flex-direction:column; gap:10px;",
                    {g1}
                    {g2}
                }
            }
        }
        // ---- Editing: caret + input ------------------------------------------
        1 => {
            let style_label = match s.caret_style {
                1 => Msg::PrefsCaretStyleBlock,
                2 => Msg::PrefsCaretStyleUnderline,
                _ => Msg::PrefsCaretStyleLine,
            };
            let g1 = group(
                Msg::PrefsCaret,
                rsx! {
                    ui_kit::Checkbox {
                        checked: s.caret_blink,
                        theme: kit_theme,
                        label: loc.tr(Msg::PrefsCaretBlink).into_owned(),
                        on_toggle: toggle(Cmd::ToggleCaretBlink),
                    }
                    ui_kit::Button {
                        label: loc.tr(style_label).into_owned(),
                        theme: kit_theme,
                        on_click: toggle(Cmd::CycleCaretStyle),
                    }
                },
            );
            let g2 = group(
                Msg::PrefsInput,
                checks(vec![
                    (Msg::PrefsAutoClose, s.auto_close, Cmd::ToggleAutoClose),
                    (Msg::PrefsWordWrap, s.wrap, Cmd::WordWrap),
                    (Msg::PrefsShowEol, s.show_eol, Cmd::ShowEol),
                    (Msg::PrefsShowWs, s.show_ws, Cmd::ShowWs),
                ]),
            );
            rsx! {
                div {
                    style: "display:flex; flex-direction:column; gap:10px;",
                    {g1}
                    {g2}
                }
            }
        }
        // ---- Highlight --------------------------------------------------------
        2 => {
            let g = group(
                Msg::PrefsHighlight,
                checks(vec![
                    (Msg::PrefsHighlightOn, s.highlight_on, Cmd::ToggleHighlight),
                    (Msg::PrefsHighlightAll, s.highlight_all, Cmd::HighlightAll),
                ]),
            );
            rsx! {
                div {
                    style: "display:flex; flex-direction:column; gap:10px;",
                    {g}
                }
            }
        }
        // ---- Vim ---------------------------------------------------------
        3 => {
            let g = group(
                Msg::PrefsVim,
                rsx! {
                    ui_kit::Checkbox {
                        checked: s.vim,
                        theme: kit_theme,
                        label: loc.tr(Msg::PrefsVimEnable).into_owned(),
                        on_toggle: toggle(Cmd::ToggleVim),
                    }
                    div {
                        class: "k-note",
                        style: "margin-top:4px;",
                        "{loc.tr(Msg::PrefsLiveNote).into_owned()}"
                    }
                },
            );
            rsx! {
                div {
                    style: "display:flex; flex-direction:column; gap:10px;",
                    {g}
                }
            }
        }
        // ---- Language: the only setting that changes this window's wording ----
        4 => {
            let g = group(
                Msg::PrefsLanguageGroup,
                rsx! {
                    div {
                        style: "display:flex; flex-direction:column; gap:2px;",
                        for lang in Locale::ALL {
                            ui_kit::Radio {
                                key: "{lang.tag()}",
                                name: "uilan".to_string(),
                                checked: s.locale == lang,
                                theme: kit_theme,
                                // Language names are self-names, never translated:
                                // An English UI must still show Simplified Chinese.
                                label: lang.native_name().to_string(),
                                on_pick: move |_| send_cmd(PrefsCmd::Toggle(Cmd::SetLocale(lang))),
                            }
                        }
                    }
                    div { class: "k-note", style: "margin-top:6px;", "{loc.tr(Msg::PrefsLanguageNote).into_owned()}" }
                },
            );
            rsx! {
                div {
                    style: "display:flex; flex-direction:column; gap:10px;",
                    {g}
                }
            }
        }
        // ---- Association: file types ↔ supported extensions ↔ registered --------------------------
        _ => rsx! {
            AssocPage { w, h, snap: s.clone(), kit_theme, theme }
        },
    }
}

// ---------------------------------------------------------------------------
// Association page (file type → supported extensions → registered)
// ---------------------------------------------------------------------------

/// A scrollable selection list (shared by the Association page's three
/// columns).
///
/// Scrolling is delegated to blitz's **native** `overflow-y:auto` (with its
/// overlay thumb, hover feedback and idle fade-out, see
/// `components/style.rs`'s `scrollbar-color`): these columns are small
/// collections, so rows are materialized in full instead of hand-writing a
/// "top row index" row window + wheel conversion
/// — This is a product of the `overflow:hidden` era (the comment "overflow:hidden of
/// — the concept "no scrollbar in box" has become obsolete with native scrollbars. Rows are `.k-list-row`
/// (hover/selected via pseudo-classes).
#[component]
fn AssocList(
    w: f32,
    h: f32,
    items: Vec<String>,
    /// Index of the selected row (guaranteed < `items.len()`)
    selected: usize,
    theme: &'static Theme,
    onpick: EventHandler<usize>,
) -> Element {
    let style = format!(
        "flex:none; width:{w}px; height:{h}px; box-sizing:border-box;
         background:{bg}; border:1px solid {bd}; overflow-y:auto; overflow-x:hidden;",
        bg = theme.input_bg,
        bd = theme.input_border
    );
    rsx! {
        div {
            style: "{style}",
            onpointerdown: move |evt| {
                evt.stop_propagation();
                evt.prevent_default();
            },
            onclick: move |evt| evt.prevent_default(),
            for (idx, name) in items.iter().enumerate() {
                {
                    let name = name.clone();
                    rsx! {
                        div {
                            key: "{name}",
                            class: if idx == selected {
                                "k-list-row k-list-row-on"
                            } else {
                                "k-list-row"
                            },
                            style: "width:100%; box-sizing:border-box;",
                            onclick: move |evt| {
                                evt.stop_propagation();
                                evt.prevent_default();
                                onpick.call(idx);
                            },
                            "{name}"
                        }
                    }
                }
            }
        }
    }
}

/// File association page: Settings → Preferences → File association.
///
/// Three columns (file types / extensions each type supports / registered
/// extensions) + two transfer buttons in between; the two switches below
/// decide "take over the default" and "install the context-menu entry". The
/// actual registry write happens on the main-window side
/// ([`PrefsCmd::Assoc`] → [`crate::ui::assoc::apply`]); this page only picks.
#[component]
fn AssocPage(
    w: f32,
    h: f32,
    snap: PrefsSnap,
    kit_theme: ui_kit::Theme,
    theme: &'static Theme,
) -> Element {
    let groups = use_hook(crate::ui::assoc::supported_groups);
    let registered = snap.assoc_registered.clone();
    let ctx_menu = snap.assoc_ctx_menu;
    let writable = crate::ui::assoc::supported();
    let group_names: Vec<String> = groups.iter().map(|g| g.name.to_string()).collect();
    // The separate vdom cannot read the main state: text renders in the
    // snapshot's language (same as the pages above).
    let loc = snap.locale;

    // ---- Geometry: two lists flanking a button column, two switch rows below ----
    let head_h = 18.0;
    let opt_h = 2.0 * 22.0 + 26.0;
    let list_h = (h - head_h - 10.0 - opt_h).max(64.0);
    let mid_w = 34.0;
    let gap = 6.0;
    let avail = w - mid_w - gap * 2.0;
    let gw = avail * 0.30;
    let ew = avail * 0.32;
    let rw = avail - gw - ew;

    // ---- Selection: pure UI state, not reported to the main window (scrolling is the list's own overflow) ----
    let mut gi = use_signal(|| 0usize);
    let mut sel_ext = use_signal(|| 0usize);
    let mut sel_reg = use_signal(|| 0usize);
    let mut take_default = use_signal(|| false);

    let cur_group = (*gi.read()).min(groups.len().saturating_sub(1));
    let exts: Vec<String> = groups
        .get(cur_group)
        .map(|g| g.exts.clone())
        .unwrap_or_default();
    // After switching file types the old middle-column index may be out of
    // bounds — clamp to the last row instead of leaving it empty.
    let ext_idx = (*sel_ext.read()).min(exts.len().saturating_sub(1));
    let reg_idx = (*sel_reg.read()).min(registered.len().saturating_sub(1));
    let picked_ext = exts.get(ext_idx).cloned().unwrap_or_default();
    let picked_reg = registered.get(reg_idx).cloned().unwrap_or_default();

    let note = if writable {
        loc.tr(Msg::PrefsAssocNote).into_owned()
    } else {
        loc.tr(Msg::PrefsAssocWindowsOnly).into_owned()
    };
    let reg_n = registered.len();
    let assoc_note = loc.tr(Fmt::PrefsAssocRegistered { n: reg_n }).into_owned();

    let reg_click = move |_| {
        if picked_ext.is_empty() {
            return;
        }
        send_cmd(PrefsCmd::Assoc(crate::ui::assoc::AssocCmd::Register {
            exts: vec![picked_ext.clone()],
            take_default: *take_default.peek(),
        }));
    };
    let unreg_click = move |_| {
        if picked_reg.is_empty() {
            return;
        }
        send_cmd(PrefsCmd::Assoc(crate::ui::assoc::AssocCmd::Unregister {
            exts: vec![picked_reg.clone()],
        }));
    };

    rsx! {
        div {
            style: "display:flex; flex-direction:column; gap:10px; height:{h}px;",
            // ---- Three columns' headers + lists + transfer buttons ----
            div {
                style: "display:flex; gap:{gap}px; height:{list_h}px;",
                div {
                    style: "flex:none; width:{gw}px; display:flex; flex-direction:column;",
                    span {
                        class: "k-note",
                        style: "height:{head_h}px;",
                        "{loc.tr(Msg::PrefsAssocFileType).into_owned()}"
                    }
                    AssocList {
                        w: gw,
                        h: list_h,
                        items: group_names,
                        selected: cur_group,
                        theme,
                        onpick: move |i| {
                            gi.set(i);
                            sel_ext.set(0);
                        },
                    }
                }
                div {
                    style: "flex:none; width:{mid_w}px; display:flex; flex-direction:column;
                            justify-content:center; gap:6px;",
                    ui_kit::Button {
                        label: "\u{2192}".to_string(),
                        kind: ui_kit::ButtonKind::Secondary,
                        theme: kit_theme,
                        on_click: reg_click,
                    }
                    ui_kit::Button {
                        label: "\u{2190}".to_string(),
                        kind: ui_kit::ButtonKind::Secondary,
                        theme: kit_theme,
                        on_click: unreg_click,
                    }
                }
                div {
                    style: "flex:none; width:{ew}px; display:flex; flex-direction:column;",
                    span {
                        class: "k-note",
                        style: "height:{head_h}px;",
                        "{loc.tr(Msg::PrefsAssocExts).into_owned()}"
                    }
                    AssocList {
                        w: ew,
                        h: list_h,
                        items: exts,
                        selected: ext_idx,
                        theme,
                        onpick: move |i| sel_ext.set(i),
                    }
                }
                div {
                    style: "flex:1 1 auto; min-width:0; display:flex; flex-direction:column;",
                    span {
                        class: "k-note",
                        style: "height:{head_h}px;",
                        "{assoc_note}"
                    }
                    AssocList {
                        w: rw,
                        h: list_h,
                        items: registered,
                        selected: reg_idx,
                        theme,
                        onpick: move |i| sel_reg.set(i),
                    }
                }
            }
            // ---- Two switches + note ----
            div {
                style: "display:flex; flex-direction:column; gap:2px;",
                ui_kit::Checkbox {
                    checked: *take_default.read(),
                    theme: kit_theme,
                    label: loc.tr(Msg::PrefsAssocTakeDefault).into_owned(),
                    disabled: !writable,
                    on_toggle: move |_| {
                        let v = !*take_default.peek();
                        take_default.set(v);
                    },
                }
                ui_kit::Checkbox {
                    checked: ctx_menu,
                    theme: kit_theme,
                    label: loc.tr(Msg::PrefsAssocCtx).into_owned(),
                    disabled: !writable,
                    on_toggle: move |_| {
                        send_cmd(PrefsCmd::Assoc(crate::ui::assoc::AssocCmd::SetContextMenu(!ctx_menu)));
                    },
                }
                div {
                    class: "k-note",
                    "{note}"
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// winshell registration
// ---------------------------------------------------------------------------

/// This popup's stable identity (routing key for `ShellEvt::PopupShow/Hide`).
pub const ID: winshell::PopupId = winshell::PopupId("prefs");

/// Zero-sized registration instance ([`winshell::Popup`] impl; goes into
/// `register_popups` at startup).
pub struct PrefsPopup;

impl winshell::Popup for PrefsPopup {
    fn id(&self) -> winshell::PopupId {
        ID
    }
    fn title(&self) -> &'static str {
        // The system title is set once at window creation, so a language switch
        // takes effect on the next open (`Popup::title` is read only then).
        Msg::PrefsTitle.get(i18n::current())
    }
    fn min_size(&self) -> (f32, f32) {
        POPUP_SIZE
    }
    fn dark(&self) -> bool {
        snap_dark()
    }
    fn root(&self) -> fn() -> Element {
        PrefsWindow
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
