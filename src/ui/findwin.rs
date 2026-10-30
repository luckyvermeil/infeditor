//! Find/replace popup: a second Blitz-rendered window outside the main window (owned popup).
//!
//! Window shape see `shell.rs`: an owned window in the **system dialog shape** — an ordinary
//! top-level window gets an owner attached after creation via
//! [`winshell::win::frameless::attach_owner`], and therefore has a **system title bar**
//! (OS-drawn, draggable, `×` close, Alt+F4 and the system menu; the shell intercepts
//! CloseRequested and turns it into [`Cmd::Close`]), a system border that allows resizing,
//! auto-hide when the owner is minimized, and no taskbar entry. The title bar has only one
//! button, `×` (dialog convention), and follows the app's light/dark theme.
//!
//! The state bridge is one-way with two lanes:
//!
//! * Main window → popup: the [`Snap`] mirror (shared via `OnceLock`/thread_local); the main
//!   side calls [`sync_shared`] after every change, and the shell flushes it into the popup
//!   vdom's Signal on the next event pass (see `shell::ShellApp` + `flush_snap`);
//! * Popup → main window: user actions produce [`Cmd`]s, queued through an mpsc channel and
//!   applied in one place by the main window in `poll_opens` (`opens.rs::poll_dialogs`).
//!
//! Keyboard/IME belong to the popup itself: the two input fields are **standard HTML
//! `<input>`s**, natively hosted by blitz's TextInput — caret, selection, arrow keys /
//! Home/End/Delete, Ctrl+A/C/X/V and IME composition/commit are all built in
//! (`blitz-dom::node::text::TextInputData`). The Dioxus side does only three things: the
//! `value` attribute does controlled echo (two-way synced with the main window's `FindState`),
//! `oninput` sends the whole new value back to the main window
//! (`Cmd::SetPattern`/`SetReplacement`), and `onkeydown` intercepts Enter/Esc/↑/↓ and turns
//! them into commands — **all other keys are never `prevent_default`-ed** (once blitz's
//! default action is blocked, nothing gets typed into the input; it goes deaf). Focus is held
//! natively by the DOM (click-to-focus + native Tab cycling via `focus_next_node`), and
//! `onfocus` mirrors focus changes back to the main window's virtual focus (`FindFocus`) for
//! the in-page bar and highlighting to share.
//!
//! All controls sit on ui-kit / native HTML: action buttons are [`ui_kit::Button`], toggles
//! are [`ui_kit::Checkbox`], direction/search mode are [`ui_kit::Radio`] (blitz-native group
//! exclusivity), tabs are real `.k-tab` buttons. The layout is **flex flow** (left main
//! column + right button column), reflowing naturally when the window resizes, with no more
//! per-pixel hand placement. Touch reliability comes from `touch-action:none` in the class
//! stylesheet (finger jitter >2px is no longer swallowed by blitz as a pan, killing the
//! click; see the `ui_kit::style` module docs).

use dioxus_native::prelude::*;
use dioxus_native::use_window;
use dioxus_native::winit::event::WindowEvent;

use crate::highlight::Theme;
use crate::search::SearchMode;
use crate::ui::components::findbar::FindAct;
use crate::ui::design::FONT_UI;
use crate::ui::shell::use_window_event;
use crate::ui::state::{EditorState, FindFocus};
use crate::ui::Msg;
use winshell::shell::bridge::Bridge;
use winshell::Popup;

// ---------------------------------------------------------------------------
// Popup geometry (the height accounting of the flex flow layout)
// ---------------------------------------------------------------------------

/// Popup content padding.
const PAD: f32 = 8.0;
/// Tab row height (one dialog, two tabs).
const TAB_H: f32 = 24.0;
const TAB_MB: f32 = 8.0;
/// Height of one input/checkbox/radio row and the row gap.
const FIELD_H: f32 = 22.0;
const ROW_H: f32 = 22.0;
const ROW_GAP: f32 = 6.0;
/// Bottom message row height (inline font size 11px).
const MSG_H: f32 = 16.0;
/// Right column buttons.
const BTN_H: f32 = 24.0;
const BTN_GAP: f32 = 3.0;
const BTN_W: f32 = 92.0;
/// Left column label width / history button width.
const LABEL_W: f32 = 36.0;
const HIST_W: f32 = 20.0;

/// Default popup size (shared by main-side placement and the popup's minimum content).
///
/// Height = the larger of the left main column and the right button column: the left column is
/// tabs + n input rows + toggle row + mode row + message/direction row; the right column is the
/// action button stack. Width is derived backwards from "input field ≥266px".
pub fn popup_size(replace: bool) -> (f32, f32) {
    const FIELD_W: f32 = 266.0;
    let rows = if replace { 2.0 } else { 1.0 };
    let left_h = PAD
        + TAB_H
        + TAB_MB
        + rows * (FIELD_H + ROW_GAP)
        + ROW_H
        + ROW_GAP
        + ROW_H
        + ROW_GAP
        + ROW_H
        + PAD
        + PAD;
    let n = if replace { 7.0 } else { 5.0 };
    let btn_h = PAD + n * BTN_H + (n - 1.0) * BTN_GAP + PAD;
    let w = PAD + LABEL_W + FIELD_W + HIST_W + 4.0 + 8.0 + BTN_W + 10.0 + PAD;
    (w, left_h.max(btn_h))
}

/// Minimum size of the popup content (the shell uses it to set the window's minimum size,
/// so the layout cannot be crushed).
///
/// `replace` is taken from the shared mirror (`sync_shared` has already refreshed it before display).
pub fn min_size() -> (f32, f32) {
    let replace = BRIDGE.with(|b| b.shared().replace);
    popup_size(replace)
}

/// The popup's current light/dark theme (the shell uses it to pick the system title bar's
/// dark/light variant).
pub fn snap_dark() -> bool {
    BRIDGE.with(|b| b.shared().dark)
}

// ---------------------------------------------------------------------------
// winshell registration (the shell routes via the registry, no more matching on business enums)
// ---------------------------------------------------------------------------

/// This popup's stable identity (routing key of `ShellEvt::PopupShow/Hide`).
pub const ID: winshell::PopupId = winshell::PopupId("find");

/// Zero-size registered instance ([`winshell::Popup`] impl; enters `register_popups` at startup).
pub struct FindPopup;

impl Popup for FindPopup {
    fn id(&self) -> winshell::PopupId {
        ID
    }
    fn title(&self) -> &'static str {
        // The system title is set once at window creation, so a language switch
        // takes effect on the next open (`Popup::title` is read only then).
        Msg::FindLabelFind.get(i18n::current())
    }
    fn min_size(&self) -> (f32, f32) {
        min_size()
    }
    fn dark(&self) -> bool {
        snap_dark()
    }
    fn root(&self) -> fn() -> Element {
        FindChildWindow
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

// ---------------------------------------------------------------------------
// Main window → popup: display mirror
// ---------------------------------------------------------------------------

/// All display state the popup needs to render (a projection of the main window's `FindState`).
///
/// `message` stores the key ([`i18n::Message`]) not the rendered string: the popup cannot get
/// an `EditorState`, so it looks the text up with [`i18n::current`] at draw time and a language
/// switch takes effect immediately.
#[derive(Clone)]
pub struct Snap {
    pub open: bool,
    pub replace: bool,
    pub focus: FindFocus,
    pub pattern: String,
    pub replacement: String,
    pub message: Option<i18n::Message>,
    pub case: bool,
    pub word: bool,
    pub mode: SearchMode,
    pub nres: usize,
    pub panel: bool,
    pub up: bool,
    pub wrap: bool,
    pub in_sel: bool,
    /// Find history (most recent first, ≤30)
    pub history: Vec<String>,
    pub dark: bool,
    /// Caret blink phase (`bridge::blink_on()`) at flush time.
    ///
    /// The dialog's input is a real `<input>`: its caret is painted by blitz,
    /// which keeps no blink timer of its own. The phase therefore has to reach
    /// the popup as data — each flip changes the snapshot, which re-pushes it
    /// and repaints the caret (`caret-color` follows this flag).
    pub blink: bool,
    pub theme: &'static Theme,
}

impl Default for Snap {
    fn default() -> Self {
        Snap {
            open: false,
            replace: false,
            focus: FindFocus::Doc,
            pattern: String::new(),
            replacement: String::new(),
            message: None,
            case: false,
            word: false,
            mode: SearchMode::Plain,
            nres: 0,
            panel: false,
            up: false,
            wrap: true,
            in_sel: false,
            history: Vec::new(),
            dark: false,
            blink: true,
            theme: Theme::of(false),
        }
    }
}

impl PartialEq for Snap {
    fn eq(&self, other: &Self) -> bool {
        // compare theme by dark (the &'static pointers are identical, but the comparison semantics are more stable)
        self.open == other.open
            && self.replace == other.replace
            && self.focus == other.focus
            && self.pattern == other.pattern
            && self.replacement == other.replacement
            && self.message == other.message
            && self.case == other.case
            && self.word == other.word
            && self.mode == other.mode
            && self.nres == other.nres
            && self.panel == other.panel
            && self.up == other.up
            && self.wrap == other.wrap
            && self.in_sel == other.in_sel
            && self.history == other.history
            && self.dark == other.dark
            && self.blink == other.blink
    }
}

// The popup skeleton ([`Bridge`]) instance: the main-window → popup display mirror plus the
// popup → main-window command channel; one thread_local is all the cross-window state
// (see the winshell bridge).
thread_local! {
    static BRIDGE: Bridge<Snap, Cmd> = Bridge::new();
}

/// Shell side: called when the child window gains OS focus.
///
/// A no-op in the real-`<input>` era: DOM focus is natively held by the input element and
/// survives show/hide; the composer-era "rebuild the focus chain by re-mounting via kb_epoch"
/// mechanism was removed together with the virtual focus.
pub fn on_child_focused() {}

/// Main window: project its find state into the shared mirror. The shell then flushes it into the popup vdom.
pub fn sync_shared(st: &EditorState) {
    BRIDGE.with(|b| {
        b.set_shared(Snap {
            open: st.find.open,
            replace: st.find.replace,
            focus: st.find.focus,
            pattern: st.find.pattern.clone(),
            replacement: st.find.replacement.clone(),
            message: st.find.message.clone(),
            case: st.find.case,
            word: st.find.word,
            mode: st.find.mode,
            nres: st.find.results.len(),
            panel: st.find.panel,
            up: st.find.up,
            wrap: st.find.wrap,
            in_sel: st.find.in_sel,
            history: st.find.history.clone(),
            dark: st.dark,
            blink: winshell::shell::bridge::blink_on(),
            theme: st.theme(),
        });
    });
}

/// Shell side: flush the shared mirror into the popup Signal (only sets it when changed).
///
/// **Only a real Signal write is accounted for** (`Bridge` deduplicates internally): the popup
/// vdom is created lazily, so the main side may flush after `FindShow` but before the window is
/// built (the Signal is not registered yet). If such failed pushes were also accounted for, the
/// first flush after registration would be swallowed by dedup and the popup would stay stuck at
/// [`Snap::default`] (`focus=Doc` → no autofocus on the input → cannot type).
pub fn flush_snap() {
    // The blink phase is owned by the shell's timer, not by the main window's
    // state: inject it here so every tick changes the snapshot (and therefore
    // repaints the caret). Without it the snapshot compare would swallow the
    // tick and the caret would freeze.
    BRIDGE.with(|b| {
        b.flush(|s| s.blink = winshell::shell::bridge::blink_on());
    });
}

// ---------------------------------------------------------------------------
// Popup → main window: commands
// ---------------------------------------------------------------------------

/// Popup user actions.
#[derive(Debug, Clone, PartialEq)]
pub enum Cmd {
    Close,
    /// Switch the Find/Replace tab (the two tabs of one dialog): `true` = replace
    Tab(bool),
    Next,
    Prev,
    FindAll,
    TogglePanel,
    Case,
    Word,
    Mode(SearchMode),
    Replace,
    ReplaceAll,
    Focus(FindFocus),
    Count,
    /// Direction radio: true = up
    Dir(bool),
    /// Wrap-around search toggle
    Wrap,
    /// In-selection toggle
    InSel,
    /// Whole new value of the find box (`oninput` passthrough; blitz natively handles per-character/selection/clipboard)
    SetPattern(String),
    /// Whole new value of the replace box (`oninput` passthrough)
    SetReplacement(String),
    /// Pick history entry i (0 = most recent)
    HistoryPick(usize),
    /// Popup geometry write-back (screen logical coordinates; the main side stores it in settings)
    Geometry {
        x: f32,
        y: f32,
        w: f32,
        h: f32,
    },
}

fn send_cmd(c: Cmd) {
    crate::ui::shell::debug_log(&format!("findwin cmd sent: {c:?}"));
    BRIDGE.with(|b| b.send(c));
}

/// Main window: request closing the popup (shared by the system `×` and Esc; hides + returns focus to the document).
pub fn request_close() {
    winshell::win::frameless::focus_main();
    send_cmd(Cmd::Close);
}

/// Main window: take all queued commands.
pub fn drain_cmds() -> Vec<Cmd> {
    BRIDGE.with(|b| b.drain())
}

/// Main window: apply popup commands (called in `poll_dialogs`).
pub fn apply_cmds(st: &mut EditorState) {
    let cmds = drain_cmds();
    if cmds.is_empty() {
        return;
    }
    for c in cmds {
        match c {
            Cmd::Close => st.close_find(),
            Cmd::Tab(replace) => st.set_find_tab(replace),
            Cmd::Next => st.find_next(),
            Cmd::Prev => st.find_prev(),
            Cmd::FindAll => st.find_all_matches(),
            Cmd::TogglePanel => st.toggle_find_panel(),
            Cmd::Case => st.toggle_find_case(),
            Cmd::Word => st.toggle_find_word(),
            Cmd::Mode(m) => st.set_find_mode(m),
            Cmd::Replace => st.replace_one(),
            Cmd::ReplaceAll => st.replace_all(),
            Cmd::Focus(f) => st.set_find_focus(f),
            Cmd::Count => st.find_count(),
            Cmd::Dir(up) => st.set_find_dir(up),
            Cmd::Wrap => st.toggle_find_wrap(),
            Cmd::InSel => st.toggle_find_in_sel(),
            Cmd::SetPattern(v) => {
                // Single-line box semantics: strip newlines pasted in (same as the in-page bar's find_insert)
                st.find.pattern = v.replace(['\n', '\r'], "");
                st.invalidate_find();
                st.find_incremental();
            }
            Cmd::SetReplacement(v) => {
                st.find.replacement = v.replace(['\n', '\r'], "");
            }
            Cmd::HistoryPick(i) => {
                if let Some(p) = st.find.history.get(i).cloned() {
                    st.find.pattern = p;
                    st.set_find_focus(FindFocus::Pattern);
                    st.invalidate_find();
                    st.find_incremental();
                }
            }
            Cmd::Geometry { x, y, w, h } => {
                st.find.popup_rect = Some([x, y, w, h]);
            }
        }
    }
    if st.find.open {
        sync_shared(st);
    }
}

// ---------------------------------------------------------------------------
// Popup components
// ---------------------------------------------------------------------------

fn act_cmd(a: FindAct) -> Option<Cmd> {
    Some(match a {
        FindAct::Prev => Cmd::Prev,
        FindAct::Tab(replace) => Cmd::Tab(replace),
        FindAct::Next => Cmd::Next,
        FindAct::FindAll => Cmd::FindAll,
        FindAct::TogglePanel => Cmd::TogglePanel,
        FindAct::Case => Cmd::Case,
        FindAct::Word => Cmd::Word,
        FindAct::Mode => return None, // the in-page bar's cycling mode; the popup uses ModeOf
        FindAct::Replace => Cmd::Replace,
        FindAct::ReplaceAll => Cmd::ReplaceAll,
        FindAct::Close => Cmd::Close,
        FindAct::Focus(f) => Cmd::Focus(f),
        FindAct::Count => Cmd::Count,
        FindAct::Dir(up) => Cmd::Dir(up),
        FindAct::Wrap => Cmd::Wrap,
        FindAct::InSel => Cmd::InSel,
        FindAct::ModeOf(m) => Cmd::Mode(m),
        // goto and the result row only appear in the main window's in-page bar
        FindAct::GotoOk | FindAct::GotoCancel | FindAct::Result(_) | FindAct::History => {
            return None
        }
    })
}

/// The find popup's content (the entire DOM of the second window).
///
/// The layout is a flex two-column arrangement: the left main column (tabs + input rows +
/// toggles/radios + message) reflows as the window resizes, the right column is the action
/// button stack; the height is computed by [`popup_size`] from the same constants.
#[component]
pub fn FindChildWindow() -> Element {
    // Initial value comes from the shared mirror, not `Snap::default`: the first frame renders
    // with the real focus/pattern, the find box mounts on the first frame and grabs DOM focus
    // via `autofocus` — otherwise the first frame's `focus=Doc` would skip autofocus and all
    // keystrokes would land on `<html>`.
    let snap = use_signal(|| BRIDGE.with(|b| b.shared()));
    // Hand the Signal handle to the shell (flush_snap pushes updates through it).
    use_hook({
        let handle = snap;
        move || {
            BRIDGE.with(|b| b.register(handle));
        }
    });

    let window = use_window();
    // This window's own events: move/resize → geometry write-back. IME is fed natively by blitz
    // to the focused `<input>` (window-level always-on, see winshell `enable_ime`); it no longer
    // participates in text input here. The shell's `use_window_event` only calls back for this
    // window id's events, so there is no cross-talk with the main window.
    // Note: writing a Signal inside the callback does not trigger a re-render (the vdom never
    // gets woken); all of this component's updates go through the Bridge's two lanes
    // (sync_shared → flush_snap / send_cmd).
    {
        let window = std::sync::Arc::clone(&window);
        use_window_event(move |evt, _| match evt {
            WindowEvent::Moved(_) | WindowEvent::SurfaceResized(_) => {
                let scale = window.scale_factor() as f32;
                if let Ok(p) = window.outer_position() {
                    let size = window.surface_size();
                    send_cmd(Cmd::Geometry {
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

    let (
        replace,
        focus,
        pattern,
        replacement,
        message,
        case,
        word,
        mode,
        panel,
        up,
        wrap,
        in_sel,
        blink,
        theme,
    ) = {
        let s = snap.read();
        (
            s.replace,
            s.focus,
            s.pattern.clone(),
            s.replacement.clone(),
            s.message.clone(),
            s.case,
            s.word,
            s.mode,
            s.panel,
            s.up,
            s.wrap,
            s.in_sel,
            s.blink,
            s.theme,
        )
    };
    // Window show/hide is controlled by the shell (PopupShow/PopupHide); the DOM always renders
    // the full content — if this returned an empty DOM when !open, then once the update chain
    // broke after the first frame you would get a "transparent empty window" (blitz draws no
    // background for an empty DOM).
    crate::ui::shell::debug_log(&format!(
        "child render: msg={:?} pattern={:?} replace={replace}",
        message, pattern
    ));

    // The popup's actual height (user-resizable; the history dropdown's available height follows it).
    let scale = window.scale_factor() as f32;
    let vh = (window.surface_size().height as f32 / scale).max(popup_size(replace).1);

    // The title is pinned to "Find": Find/Replace are two tabs of the same dialog,
    // switching tabs does not change the window title. The popup cannot get an EditorState,
    // so it goes through the process-level language mirror.
    let loc = i18n::current();
    window.set_title(Msg::FindLabelFind.get(loc));
    // Message row: Snap stores keys; the text is looked up only here.
    let message = message.map(|m| loc.tr(m).into_owned()).unwrap_or_default();

    let hist_open = use_signal(|| false);
    let kit_theme = ui_kit::Theme::from(theme);
    let gutter_fg = theme.gutter_fg;

    let pattern_focused = focus == FindFocus::Pattern;
    let repl_focused = focus == FindFocus::Replacement;

    // ---- Right column action buttons (ui-kit Button; the results button is a toggle state) ----
    // Buttons store keys; text is looked up at render time (reusing the Find Next/Find Previous
    // wording that matches the menus).
    let mut btns: Vec<(Msg, FindAct, bool, ui_kit::ButtonKind)> = vec![
        (
            Msg::MenuFindNext,
            FindAct::Next,
            true,
            ui_kit::ButtonKind::Primary,
        ),
        (
            Msg::MenuFindPrev,
            FindAct::Prev,
            false,
            ui_kit::ButtonKind::Secondary,
        ),
        (
            Msg::FindBtnCount,
            FindAct::Count,
            false,
            ui_kit::ButtonKind::Secondary,
        ),
        (
            Msg::FindBtnFindAll,
            FindAct::FindAll,
            false,
            ui_kit::ButtonKind::Secondary,
        ),
    ];
    if replace {
        btns.push((
            Msg::FindLabelReplace,
            FindAct::Replace,
            false,
            ui_kit::ButtonKind::Secondary,
        ));
        btns.push((
            Msg::FindBtnReplaceAll,
            FindAct::ReplaceAll,
            false,
            ui_kit::ButtonKind::Secondary,
        ));
    }
    let panel_kind = if panel {
        ui_kit::ButtonKind::Primary
    } else {
        ui_kit::ButtonKind::Secondary
    };
    btns.push((Msg::FindBtnResults, FindAct::TogglePanel, panel, panel_kind));

    let root_style = format!(
        "position:absolute; inset:0; background:{bg}; overflow:hidden; user-select:none;
         font-family:{FONT_UI}; display:flex; padding:{PAD}px; gap:8px;",
        bg = theme.menu_bg
    );

    rsx! {
        div {
            style: "{root_style}",
            // Cancel the default action of **Click** events: blitz's `handle_click` calls
            // `clear_focus()` for clicks that miss a focusable element (blitz-dom
            // `events/pointer.rs`), and once DOM focus is cleared the input's keyboard input
            // dies entirely (vendor patch five only exempts when focus is a text input).
            // Bubbling to the root covers every control inside the popup.
            //
            // Note **onpointerdown must not cancel the default action**: pointer-down's default
            // action *is* the input's click-to-focus + caret placement (the
            // `ClickTarget::TextInput` branch of `handle_pointerdown`); cancelling it would
            // disable all mouse interaction with the real input. A click on blank space only
            // defaults to `clear_text_selection`, with no focus side effects — let it be.
            onclick: move |evt| {
                evt.prevent_default();
            },
            ui_kit::KitStyle { theme: kit_theme, dark: theme.dark }
            // ---- Left main column: reflows as the window resizes ----
            div {
                style: "flex:1 1 auto; min-width:0; display:flex; flex-direction:column;",
                // Tabs: Find / Replace, switching does not destroy the dialog
                div {
                    style: "display:flex; gap:2px; margin-bottom:{TAB_MB}px; height:{TAB_H}px;",
                    for (i, (label, active)) in [
                        (Msg::FindLabelFind, !replace),
                        (Msg::FindLabelReplace, replace),
                    ].iter().enumerate() {
                        button {
                            key: "tab{i}",
                            class: if *active { "k-tab k-tab-on" } else { "k-tab" },
                            onclick: move |evt| {
                                evt.stop_propagation();
                                evt.prevent_default();
                                send_cmd(Cmd::Tab(i == 1));
                            },
                            { label.get(loc) }
                        }
                    }
                }
                // Find box: the key includes replace — when switching tabs the previously focused
                // box may be unmounted (the replace box holds DOM focus and the user switches back
                // to "Find"), and the pattern input regains focus via remount + `autofocus`
                // (executed uniformly at the end of the mutator flush, so it does not race with
                // the unmount path). Known small edge case: pressing Ctrl+H again while already on
                // the replace tab with focus in the replace box leaves the key unchanged → no
                // remount → DOM focus stays in the replace box, briefly mismatching the main
                // window's virtual focus; clicking the input or pressing Tab fixes it.
                PopField {
                    key: "f0-{replace}",
                    field: FindFocus::Pattern,
                    label: Msg::FindLabelFind,
                    value: pattern,
                    placeholder: Msg::FindPhPattern,
                    focused: pattern_focused,
                    blink,
                    hist: true,
                    hist_open,
                    vh,
                    kit_theme,
                    theme,
                }
                if replace {
                    PopField {
                        key: "f1-{replace}",
                        field: FindFocus::Replacement,
                        label: Msg::FindLabelReplace,
                        value: replacement,
                        placeholder: Msg::FindPhReplace,
                        focused: repl_focused,
                        blink,
                        hist: false,
                        hist_open,
                        vh,
                        kit_theme,
                        theme,
                    }
                }
                // Toggle row
                div {
                    style: "display:flex; gap:12px; margin-top:{ROW_GAP}px; height:{ROW_H}px;",
                    for (label, act, on) in [
                        (Msg::FindOptCase, FindAct::Case, case),
                        (Msg::FindOptWord, FindAct::Word, word),
                        (Msg::FindOptWrap, FindAct::Wrap, wrap),
                        (Msg::FindOptInSel, FindAct::InSel, in_sel),
                    ] {
                        ui_kit::Checkbox {
                            key: "{label:?}",
                            checked: on,
                            theme: kit_theme,
                            label: loc.tr(label).into_owned(),
                            on_toggle: move |_| {
                                if let Some(c) = act_cmd(act) {
                                    send_cmd(c);
                                }
                            },
                        }
                    }
                }
                // Search mode (native radios; blitz enforces exclusivity by name natively)
                div {
                    style: "display:flex; align-items:center; gap:8px; margin-top:{ROW_GAP}px; height:{ROW_H}px;",
                    span { style: "font-size:12px; color:{gutter_fg};", { Msg::FindModeLabel.get(loc) } }
                    for (m, label) in [
                        (SearchMode::Plain, Msg::FindModePlain),
                        (SearchMode::Esc, Msg::FindModeEscExt),
                        (SearchMode::Regex, Msg::FindModeRegex),
                    ] {
                        ui_kit::Radio {
                            key: "{label:?}",
                            name: "findmode".to_string(),
                            checked: mode == m,
                            theme: kit_theme,
                            label: loc.tr(label).into_owned(),
                            on_pick: move |_| send_cmd(Cmd::Mode(m)),
                        }
                    }
                }
                // Message row: margin-top:auto pins it to the bottom of the left column; the
                // search direction radios sit on the right (the direction decides where
                // "Find Next" goes, sharing the row with the results message)
                div {
                    style: "margin-top:auto; height:{ROW_H}px; display:flex;
                            align-items:center; gap:8px;",
                    span {
                        style: "flex:1 1 auto; min-width:0; font-size:11px;
                                color:{gutter_fg}; overflow:hidden; white-space:nowrap;
                                line-height:{MSG_H}px;",
                        { message }
                    }
                    for (label, is_up, on) in [
                        (Msg::FindDirUp, true, up),
                        (Msg::FindDirDown, false, !up),
                    ] {
                        ui_kit::Radio {
                            key: "{label:?}",
                            name: "finddir".to_string(),
                            checked: on,
                            theme: kit_theme,
                            label: loc.tr(label).into_owned(),
                            on_pick: move |_| send_cmd(Cmd::Dir(is_up)),
                        }
                    }
                }
            }
            // ---- Right column: action button stack (flex stretch, uniform width) ----
            div {
                style: "flex:none; width:{BTN_W}px; display:flex; flex-direction:column;
                        gap:{BTN_GAP}px;",
                for (label, act, _on, kind) in btns {
                    ui_kit::Button {
                        key: "{label:?}",
                        label: loc.tr(label).into_owned(),
                        kind: kind,
                        theme: kit_theme,
                        on_click: move |evt: MouseEvent| {
                            evt.stop_propagation();
                            evt.prevent_default();
                            if let Some(c) = act_cmd(act) {
                                send_cmd(c);
                            }
                        },
                    }
                }
            }
        }
    }
}

/// An input row inside the popup: label + **standard HTML `<input>`** + placeholder text when
/// empty + optional history dropdown. The find box carries a history button (`hist`).
///
/// Structure matches a web page: the row container is `position:relative` (anchor of the history
/// dropdown), the input itself is transparently filled, and the placeholder span sits *under*
/// the input (earlier in DOM order = drawn below; clicks always hit the input, no
/// pointer-events support needed).
///
/// All input behavior is blitz-native: characters/backspace/arrows/Home/End/Delete/Ctrl+A/C/X/V
/// and IME composition commit (`TextInputData`). Dioxus intercepts only Enter (find next/prev),
/// Esc (close) and ↑/↓ (results navigation); **all other keys are not `prevent_default`-ed** —
/// blocking blitz's default action means no characters get written and the input goes deaf.
///
/// `value` is a controlled property: every edit sends the whole new value back to the main
/// window via `oninput`; when the value synced back matches the input's buffer, blitz's
/// `set_text` skips it as-is and the caret stays put.
#[component]
fn PopField(
    field: FindFocus,
    label: Msg,
    value: String,
    placeholder: Msg,
    focused: bool,
    /// Caret blink phase (see `Snap::blink`)
    blink: bool,
    hist: bool,
    hist_open: Signal<bool>,
    /// Popup content height (the history dropdown clamps its visible rows by it)
    vh: f32,
    kit_theme: ui_kit::Theme,
    theme: &'static Theme,
) -> Element {
    let border = if focused {
        theme.focus_ring
    } else {
        theme.input_border
    };
    // The popup cannot get an EditorState: label and placeholder text are looked up via the process-level language mirror.
    let loc = i18n::current();
    let label_text = label.get(loc);
    let placeholder_text = placeholder.get(loc);
    let label_style = format!(
        "width:{LABEL_W}px; flex:none; font-size:12px; color:{fg}; overflow:hidden;",
        fg = theme.menu_fg
    );
    // Placeholder text: same font as the input, absolutely positioned above the input but
    // earlier in DOM order — hit testing always favors the input, so the placeholder layer
    // needs no pointer-events support.
    let placeholder_style = format!(
        "position:absolute; left:{left}px; top:1px; height:20px; line-height:20px;
         font-size:12px; color:{}; white-space:pre; overflow:hidden;
         pointer-events:none;
         font-family:Consolas,'Cascadia Mono',monospace;",
        theme.gutter_fg,
        left = LABEL_W + 7.0,
    );
    // The input itself: fills the whole row to the right of the label (the history button takes
    // a bit); the caret is drawn by blitz when focused, `caret-color` follows the theme.
    let input_style = format!(
        "flex:1 1 auto; min-width:0; height:{FIELD_H}px; box-sizing:border-box;
         background:{bg}; border:1px solid {border}; border-radius:2px;
         padding:0 6px; margin:0; line-height:{lh}px;
         font-size:12px; color:{fg}; caret-color:{caret}; outline:none;
         font-family:Consolas,'Cascadia Mono',monospace;",
        bg = theme.input_bg,
        fg = theme.menu_fg,
        caret = if blink { theme.caret } else { "transparent" },
        border = border,
        lh = FIELD_H - 2.0,
    );
    let is_pattern = field == FindFocus::Pattern;
    let hist_left = LABEL_W + 4.0;
    let hist_right = HIST_W + 6.0;
    rsx! {
        div {
            style: "position:relative; display:flex; align-items:center;
                    margin-bottom:{ROW_GAP}px; height:{FIELD_H}px;",
            span { style: "{label_style}", "{label_text}" }
            if value.is_empty() {
                span { style: "{placeholder_style}", "{placeholder_text}" }
            }
            input {
                style: "{input_style}",
                value: "{value}",
                spellcheck: "false",
                autocomplete: "off",
                autocorrect: "off",
                autofocus: focused,
                onfocus: move |_| {
                    // The mirror point from DOM focus to the main window's virtual focus. Skip
                    // when it is already this box: every Cmd::Focus triggers a sync re-render,
                    // and a re-render that changes the key would also remount the input (caret
                    // thrown to the end). Re-focusing the same box (clicking to place the
                    // caret) must be zero-cost.
                    if !focused {
                        send_cmd(Cmd::Focus(field));
                    }
                },
                oninput: move |evt| {
                    // Whole new value passed straight to the main window (the controlled input's single source of truth lives in FindState)
                    let v = evt.value().replace(['\n', '\r'], "");
                    if v != value {
                        send_cmd(if is_pattern {
                            Cmd::SetPattern(v)
                        } else {
                            Cmd::SetReplacement(v)
                        });
                    }
                },
                onkeydown: move |evt| {
                    let d = evt.data();
                    let shift = d.modifiers().shift();
                    match d.key() {
                        // Enter: find next (Shift = previous).
                        // Enter that confirms an IME composition string (is_composing) is not intercepted.
                        Key::Enter if !d.is_composing() => {
                            evt.prevent_default();
                            send_cmd(if shift { Cmd::Prev } else { Cmd::Next });
                        }
                        Key::Escape => {
                            evt.prevent_default();
                            winshell::win::frameless::focus_main();
                            send_cmd(Cmd::Close);
                        }
                        // ↑/↓: results navigation (a single-line box has no vertical caret semantics anyway)
                        Key::ArrowUp => {
                            evt.prevent_default();
                            send_cmd(Cmd::Prev);
                        }
                        Key::ArrowDown => {
                            evt.prevent_default();
                            send_cmd(Cmd::Next);
                        }
                        // Everything else passes through: blitz TextInput natively handles
                        // characters/backspace/arrows/Home/End/Delete/Ctrl+A/C/X/V; Tab goes
                        // through `focus_next_node` native cycling (input→replace box→wrap),
                        // and blur/focus events are mirrored back to the main window via onfocus.
                        _ => {}
                    }
                },
            }
            if hist {
                button {
                    class: "k-ibtn",
                    style: "width:{HIST_W}px; height:{FIELD_H}px; margin-left:4px;
                            font-size:9px;",
                    // Out of Tab rotation: the history button is an auxiliary entry; Tab should go
                    // straight from the find box to the replace box; -1 = clickable but not
                    // Tab-reachable (blitz's focus_next_node skips tabindex<0).
                    tabindex: "-1",
                    onclick: move |evt| {
                        evt.stop_propagation();
                        evt.prevent_default();
                        let cur = *hist_open.read();
                        hist_open.set(!cur);
                    },
                    "▼"
                }
                if *hist_open.read() && is_pattern {
                    HistoryList {
                        left: hist_left,
                        right: hist_right,
                        vh,
                        pattern_focused: focused,
                        hist_open,
                        theme,
                    }
                }
            }
        }
    }
}

/// Find history dropdown (an absolutely positioned layer anchored under the find input row's
/// bottom edge; clicking an item backfills the pattern).
///
/// Collapses when focus moves away; row count is clamped by the popup's remaining height
/// (the `overflow:hidden` root clips anything beyond the window, so the list must fit
/// entirely inside it).
#[component]
fn HistoryList(
    left: f32,
    right: f32,
    vh: f32,
    pattern_focused: bool,
    hist_open: Signal<bool>,
    theme: &'static Theme,
) -> Element {
    let open = *hist_open.read();
    let history = BRIDGE.with(|b| b.shared().history);
    if !open || history.is_empty() {
        return rsx! {};
    }
    if !pattern_focused {
        // Collapse when focus moved away (read before write: an unconditional set used to be a
        // render-time write — costing an extra full-popup re-render; when the value is already
        // false it skips at zero cost)
        if *hist_open.read() {
            hist_open.set(false);
        }
        return rsx! {};
    }
    // Available height: from the find row's bottom edge (PAD+tabs+row gap+one input row+row gap)
    // to 6px above the window bottom. Each row is 18px.
    let avail = (vh - PAD - TAB_H - TAB_MB - FIELD_H - ROW_GAP - 6.0).max(36.0);
    let max_rows = ((avail / 18.0).floor() as usize).clamp(1, 8);
    let list_style = format!(
        "position:absolute; left:{left}px; right:{right}px; top:100%; margin-top:2px;
         max-height:{}px; overflow:hidden; background:{bg}; border:1px solid {border};
         border-radius:3px; z-index:50; box-shadow:0 4px 12px {shadow};
         touch-action:none;",
        max_rows as f32 * 18.0,
        bg = theme.panel_bg,
        border = theme.menu_border,
        shadow = theme.shadow
    );
    rsx! {
        div {
            style: "{list_style}",
            for (i, item) in history.iter().take(max_rows).enumerate() {
                HistRow {
                    key: "h{i}",
                    index: i,
                    text: item.clone(),
                    hist_open,
                    theme,
                }
            }
        }
    }
}

#[component]
fn HistRow(
    index: usize,
    text: String,
    mut hist_open: Signal<bool>,
    theme: &'static Theme,
) -> Element {
    let style = format!(
        "height:18px; line-height:18px; padding:0 6px; font-size:12px; color:{fg};
         white-space:pre; overflow:hidden; background:transparent;
         font-family:Consolas,'Cascadia Mono',monospace; cursor:default;
         touch-action:none;",
        fg = theme.menu_fg
    );
    rsx! {
        div {
            style: "{style}",
            onpointerdown: move |evt| {
                evt.stop_propagation();
                evt.prevent_default();
            },
            onclick: move |evt| {
                evt.stop_propagation();
                evt.prevent_default();
                send_cmd(Cmd::HistoryPick(index));
                hist_open.set(false);
            },
            "{text}"
        }
    }
}
