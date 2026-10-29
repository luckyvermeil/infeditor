//! The merged title bar + menu bar.
//!
//! # Why this exists
//!
//! The window is created with `decorations(false)`
//! (see `main.rs`), so there is **no** OS title bar. What used to be a native
//! strip plus a 24px menu bar is now a single custom row that carries, in one
//! line: the window controls, the app icon, the document title, and the whole
//! menu tree. Dragging the empty part of the bar moves the window;
//! the edges resize it (see [`winshell::win::frameless`]).
//!
//! # Layout
//!
//! ```text
//! ┌──────────────────────────────────────────────────────────────┐
//! │ [icon][File][Edit]…      <title>            ─  □  ✕ │  │
//! └──────────────────────────────────────────────────────────────┘
//! ```
//!
//! The window buttons sit on the **right**. The menus follow the icon, exactly where a menu
//! bar used to start, so muscle memory still lines up.
//!
//! Only the *empty* background of the bar drags the window. The captions and
//! the buttons stop propagation, so clicking a menu does not also start a
//! window drag.

use dioxus_native::prelude::*;

use crate::highlight::Theme;
use crate::ui::actions;
use crate::ui::components::appicon;
use crate::ui::state::{EditorState, TITLEBAR_H, WINBTN_W};
use crate::ui::Msg;

/// Glyphs for the window buttons.
///
/// These are all U+0020-range or box-drawing characters that exist in Consolas
/// and in the CJK fallback font; astral-plane symbols (U+1Fxxx) would render as
/// tofu, which is exactly the bug the menu check-mark already had to avoid.
mod glyph {
    /// minimize
    pub const MIN: &str = "\u{2500}"; // ─
    /// maximize
    pub const MAX: &str = "\u{25A1}"; // □
    /// restore (window is currently maximized)
    pub const RESTORE: &str = "\u{2750}"; // ❐
    /// close
    pub const CLOSE: &str = "\u{00D7}"; // ×
}

/// Which window-control button was pressed.
#[derive(Clone, Copy, PartialEq, Eq)]
enum WB {
    Min,
    Max,
    Close,
}

/// Press on the title bar's background: hand the gesture to the OS.
///
/// Exactly what a real caption does: the system's own modal move loop follows
/// the cursor (snapping, drag-to-top maximize, the right cursor) instead of the
/// app repositioning the window one frame at a time. See
/// [`winshell::win::frameless::start_move`].
///
/// Every non-interactive part of the bar uses this — the two flexible gaps
/// *and* the document title between them. The title used to be inert, which
/// left a dead band in the middle of the bar: pressing it felt like the window
/// refused to move. The menu captions and the window buttons stop propagation,
/// so they never reach here.
///
/// # Why the pointer type is dispatched here
///
/// The system loop is entered by posting `WM_NCLBUTTONDOWN`, and it only ends
/// on the matching `WM_NCLBUTTONUP`. winit consumes touch as *pointer* input
/// and suppresses the mouse messages the OS would otherwise synthesize, so with
/// a finger the loop never sees its release: the gesture hangs and then dies
/// the moment another window takes focus. Touch therefore runs a custom loop
/// driven by window-level pointer events (see
/// [`winshell::win::frameless::begin_touch_move`]). Pens get the same treatment —
/// nothing guarantees mouse emulation for them either.
fn start_drag(mut state: Signal<EditorState>, evt: PointerEvent) {
    evt.stop_propagation();
    let c = evt.data.client_coordinates();
    crate::ui::shell::debug_log(&format!("titlebar start_drag @ ({},{})", c.x, c.y));
    // Skip blitz's pointer-down default action: it has nothing to focus here,
    // and running it would only disturb the editor's own selection state while
    // the OS takes over the gesture.
    evt.prevent_default();
    // Two quick presses on the title bar maximize/restore, like every window
    // frame. The pair is detected here (see `win32::is_double_press`), before
    // the gesture is armed, so the second press becomes a maximize instead of
    // a move.
    if winshell::win::frameless::is_double_press(c.x as i32, c.y as i32) {
        // Sync the flag here rather than waiting for the `SurfaceResized` that
        // follows: the resize strips and the restore glyph both read it, and a
        // frame of disagreement shows a resize cursor on a maximized window.
        //
        // `toggle_maximize` asks for the state the window is in *right now*
        // (true → restore, false → maximize), so the pre-flip value is what it
        // gets; the mirrored flag is what flips. Passing the flipped value ran
        // the gesture backwards and the double click did nothing at all.
        let zoomed = {
            let mut st = state.write();
            let was = st.win.maximized;
            st.win.maximized = !was;
            was
        };
        toggle_maximize(zoomed);
        return;
    }
    if evt.data.pointer_type() == "mouse" || !winshell::win::frameless::touch_move_loop_supported()
    {
        // Mouse, or a platform whose generic `drag_window()` already covers
        // touch (X11/Wayland/macOS hand the gesture to the window manager):
        // one path for every pointer type.
        winshell::win::frameless::start_move();
    } else {
        // Armed even while maximized: the custom loop carries drag-to-restore (the restore
        // branch of `update_touch_gesture`, with placement rules matching the system loop);
        // drag back to the top snap band, lift, and it maximizes again.
        // This spot used to drop the gesture outright when maximized, so on touch a maximized
        // window could never be dragged down.
        winshell::win::frameless::begin_touch_move(c.x, c.y, evt.data.pointer_id());
    }
}

/// Transparent drag strip: the title bar's background, minus its children.
///
/// A `div` with `flex:1` and no content occupies the gap between the menus and
/// the window buttons, balancing the centered title.
fn drag_region(state: Signal<EditorState>, t: &'static Theme) -> Element {
    let style = format!(
        "flex:1 1 auto; align-self:stretch; background:{bg};",
        bg = t.menu_bg
    );
    rsx! {
        div {
            style: "{style}",
            onpointerdown: move |evt| start_drag(state, evt),
        }
    }
}

/// Double-click on the caption: maximize, or restore when already maximized.
fn toggle_maximize(zoomed: bool) {
    winshell::win::frameless::toggle_maximize(zoomed);
}

/// One top-level menu caption, with hover, press and the Alt access key.
///
/// The caption is its own component because all three of those are *local* state:
/// hoisting them into `EditorState` would mean a state write (and a full
/// re-render of the editor) on every pixel the mouse crosses the bar.
///
/// # The access key
///
/// Windows 11 style: the caption is just `文件`, and the parenthesised key
/// (`文件(F)`, key underlined) appears **only** while the bar is active — a bare
/// <kbd>Alt</kbd> press, carried in through `hot` from `EditorState::menu_alt`.
/// An open dropdown does not count: clicking a caption with the mouse ends the
/// Alt session, so the keys disappear again as the menu opens.
///
/// # The DOM id
///
/// The caption box carries `inf-cap-{idx}` (`idx` = its position in
/// [`actions::menu_bar_items`]). That id is how [`crate::ui::shell`] finds the
/// node in blitz's layout tree to measure where it actually ended up — the
/// anchor an <kbd>Alt</kbd>-opened dropdown needs. See [`caption_x`].
#[derive(Clone, PartialEq)]
struct CaptionView {
    /// Caption identity (menu state is keyed by it, see [`actions::Caption`]).
    cap: actions::Caption,
    /// Caption text in the current language (the rendered `cap.msg()`).
    label: String,
    is_open: bool,
    /// The access key, rendered as `(F)` **and** underlined — `Some` only while
    /// the bar is active (see [`CaptionView::hot`]'s producer).
    hot: Option<char>,
}

#[component]
fn MenuCaption(
    state: Signal<EditorState>,
    view: CaptionView,
    idx: usize,
    t: &'static Theme,
) -> Element {
    let CaptionView {
        cap,
        label,
        is_open,
        hot,
    } = view;
    // Tri-state in the `.mi` / `.mi.open` classes (components::style): an open caption stays
    // highlighted even with the pointer away — the `:open` class is exactly this "keep it on
    // while the pointer is elsewhere" state.
    // Geometry (padding/flex/nowrap/user-select) lives in the `.mi` class
    let style = "";
    let underline_style = format!(
        "position:absolute; left:0; bottom:0; width:100%; height:2px; background:{};",
        t.accent
    );
    // Windows 11 menu bar: the caption reads `文件` on its own; holding
    // <kbd>Alt</kbd> appends the dimmed, underlined access key `(F)`. Only that
    // session shows it — an always-on `(F)` made every caption ~25% wider and
    // the underline read as decoration rather than a keyboard hint.
    let key_text = hot.map(|c| c.to_ascii_uppercase().to_string());
    const KEY_STYLE: &str = "opacity:0.72; margin-left:3px; text-decoration:underline;";

    rsx! {
        div {
            // The stable id used for measured anchors — the shell side finds this box in
            // blitz's layout tree by it (see `caption_dom_id` / `CAPTION_X`).
            id: caption_dom_id(idx),
            style: "position:relative; height:100%; display:flex; align-items:center;",
            div {
                class: if is_open { "mi open" } else { "mi" },
                role: "menuitem",
                style: "{style}",
                onmouseenter: move |evt: MouseEvent| {
                    let c = evt.data.client_coordinates();
                    let e = evt.data.element_coordinates();
                    let anchor = (c.x - e.x) as f32;
                    // Conditional write: this is a no-op while no menu is open
                    // or this caption already is, and an unconditional write()
                    // would re-render the whole app as the pointer crosses the bar.
                    crate::ui::state::update_if(
                        &mut state,
                        |st| st.menu.open.is_some() && st.menu.open != Some(cap),
                        |st| st.menu_focus_caption(cap, anchor),
                    );
                },
            onpointerdown: move |evt| {
                // A caption click opens a menu; it must never also start a
                // window drag.
                evt.stop_propagation();
                // Stopping propagation also hides the click from the root's
                // "keep the IME composer focused" handler, so cancel the
                // default action here.
                evt.prevent_default();
            },
            onclick: move |evt: MouseEvent| {
                // see `onmousedown`
                evt.stop_propagation();
                evt.prevent_default();
                let c = evt.data.client_coordinates();
                let e = evt.data.element_coordinates();
                state.write().menu_toggle(cap, (c.x - e.x) as f32);
            },
                "{label}"
                if let Some(k) = key_text {
                    span { style: "{KEY_STYLE}", "({k})" }
                }
            }
            if is_open {
                div { style: "{underline_style}" }
            }
        }
    }
}

/// The current window handle as an `isize`.
pub fn hwnd() -> isize {
    winshell::win::frameless::current_hwnd()
}

/// The menubar overflow button (`››`): captions that no longer fit the window enter through it
/// as a vertical list.
///
/// The overflow list itself is a root-level [`crate::ui::components::menu::MoreMenu`] (at the
/// same level as an ordinary dropdown, dodging the title bar's stacking context).
#[component]
fn MoreBtn(state: Signal<EditorState>) -> Element {
    // Colors live in the `.mi` / `.mi.open` classes (components::style)
    let is_open = state.read().menu.more_open;
    let style =
        format!("width:{MORE_W}px; justify-content:center; font-size:13px; letter-spacing:-2px;");
    rsx! {
        div {
            class: if is_open { "mi open" } else { "mi" },
            role: "button",
            style: "{style}",
            onpointerdown: move |evt| {
                evt.stop_propagation();
                evt.prevent_default();
            },
            onclick: move |evt| {
                evt.stop_propagation();
                evt.prevent_default();
                state.write().menu_toggle_more();
            },
            span { aria_hidden: "true", "\u{203A}\u{203A}" }
            span { style: "font-size:0; line-height:0;",
                "{state.read().tr(crate::ui::Msg::StatusMoreMenus).into_owned()}"
            }
        }
    }
}

/// Left edge of the caption `label`, in CSS px.
///
/// The dropdown anchors itself to this, so it has to be computable **without**
/// the event that a mouse click would have provided. Two sources, best first:
///
/// 1. the **real** laid-out rectangle. [`crate::ui::shell`] measures it from
///    blitz's layout tree every idle turn and parks it in this module's slot
///    (`set_measured_caption_x`); this is the very number the click path
///    derives from the event (`client_x - element_x`), and it follows window
///    size, DPI and font changes for free;
/// 2. `estimate_caption_x`, a pure-function mirror of the bar's flex layout,
///    for the frames before the first measurement lands.
///
/// The estimate matters because <kbd>Alt</kbd>+letter can be the first thing a
/// user ever does with the menu bar: there is no pointer event to measure from,
/// and a dropdown anchored hundreds of px away from its own caption is worse
/// than one anchored a pixel off. (It used to be the *only* source, and its
/// accel-key term alone was ~31px too wide per caption — by Help, the last
/// caption, the panel opened ~370px to the right of the caption it belonged to.)
pub fn caption_x(st: &EditorState, cap: actions::Caption) -> f32 {
    let idx = cap.index();
    measured_caption_x(idx).unwrap_or_else(|| estimate_caption_x(st, idx))
}

/// The measured left edge of the `idx`-th caption (CSS px, root-element coordinates); `None`
/// when nothing was measured.
fn measured_caption_x(idx: usize) -> Option<f32> {
    CAPTION_X.lock().ok()?.get(idx).copied().flatten()
}

/// Estimate the left edge of caption `idx` by mirroring the bar's flex layout.
///
/// Fallback only — see [`caption_x`]. It walks the same boxes the browser lays
/// out: the app icon, then one [`caption_box_w`] per preceding caption.
fn estimate_caption_x(st: &EditorState, idx: usize) -> f32 {
    // The icon is `margin:10px 8px 10px 12px` around a 16px box, i.e. 12px of
    // left margin plus the box plus its 8px right margin — see `TitleBar`'s
    // `icon_style`. All three are part of the advance to the first caption.
    let mut x = ICON_LEFT + ICON_BOX + ICON_GAP;
    for (i, it) in actions::menu_bar_items(st).iter().enumerate() {
        if i == idx {
            return x;
        }
        x += caption_box_w(i, it.label());
    }
    0.0
}

/// One caption's box width: `padding:0 9px` on both sides, the label, and the
/// access key drawn after it (`File(F)`).
///
/// The key is part of the estimate because this is only a **fallback** for a
/// caption nobody measured yet, and the case that needs it is exactly an
/// <kbd>Alt</kbd>-opened dropdown — the Alt session is what draws the key, so
/// its width is the one to mirror. (Measured geometry wins whenever it exists:
/// with the keys hidden a bar of bare captions is ~25% narrower.)
fn caption_box_w(idx: usize, label: &str) -> f32 {
    let mut w = caption_text_w(label);
    if let Some(accel) = actions::caption_accel(idx) {
        // ` (X)` — space, parenthesis, letter, parenthesis.
        w += caption_text_w(&format!(" ({})", accel.to_ascii_uppercase()));
    }
    18.0 + w
}

/// Estimated rendered width of caption text (px) in the UI font.
///
/// The captions are drawn at 12px in `Microsoft YaHei UI` / `Segoe UI`
/// (`EditorApp`'s root `font-family`); the editor's `Metrics` measures Consolas
/// at the caret's size and says nothing about this font. Advances are taken per
/// character class, which is what the font actually does: a CJK glyph is a full
/// 1em, uppercase Latin ≈ 0.6em, lowercase ≈ 0.52em, digits ≈ 0.55em, brackets
/// and other ASCII punctuation ≈ 0.33em, a space ≈ 0.28em.
fn caption_text_w(text: &str) -> f32 {
    text.chars().map(|c| EM_12 * char_em(c)).sum()
}

/// Advance of one character in em units (see [`caption_text_w`]).
fn char_em(c: char) -> f32 {
    if c == ' ' {
        0.28
    } else if unicode_width::UnicodeWidthChar::width(c).unwrap_or(0) >= 2 {
        1.0
    } else if c.is_ascii_uppercase() {
        0.60
    } else if c.is_ascii_lowercase() {
        0.52
    } else if c.is_ascii_digit() {
        0.55
    } else {
        0.33
    }
}

/// Caption font size (px) — matches `TitleBar`'s `bar_style`.
const EM_12: f32 = 12.0;

/// Width of the menubar overflow "more" button (px).
pub const MORE_W: f32 = 30.0;

/// Left margin + box width + right margin of the app icon in the title bar (px).
///
/// The icon is `margin:10px 8px 10px 12px` around a 16px box — see `TitleBar`'s
/// `icon_style`. The right margin is part of the advance to the first caption;
/// omitting it put every estimated caption 8px left of where it is drawn.
const ICON_LEFT: f32 = 12.0;
const ICON_BOX: f32 = 16.0;
const ICON_GAP: f32 = 8.0;

/// Measured caption left edges (CSS px, root-element coordinates); the index matches the
/// `menu_bar_items` order.
///
/// `None` = that entry has not been measured yet (no matching node in the DOM yet). Written by
/// [`crate::ui::shell`] every idle turn; [`caption_x`] reads it to anchor the <kbd>Alt</kbd>+letter
/// dropdown. Why it is not on `EditorState`: the measurement happens on the shell side (it has to
/// reach blitz's layout tree) while reading happens on the component side — the two share only
/// this one slot.
static CAPTION_X: std::sync::Mutex<Vec<Option<f32>>> = std::sync::Mutex::new(Vec::new());

/// The `idx`-th caption's DOM id — `MenuCaption` stamps it in when rendering, and the
/// measurement finds the node by id.
pub(crate) fn caption_dom_id(idx: usize) -> String {
    format!("inf-cap-{idx}")
}

/// Record one round of layout measurement (called by [`crate::ui::shell`] every idle turn). The
/// return value says whether the value really changed, so the caller can decide whether to log.
pub(crate) fn set_measured_caption_x(xs: Vec<Option<f32>>) -> bool {
    let Ok(mut g) = CAPTION_X.lock() else {
        return false;
    };
    if *g == xs {
        return false;
    }
    *g = xs;
    true
}

/// The merged title + menu bar.
#[component]
pub fn TitleBar() -> Element {
    let mut state = use_context::<Signal<EditorState>>();

    let (caps, open, title, dirty, maximized, alt_hot, t) = {
        let st = state.read();
        let name = st
            .tabs
            .get(st.active)
            .map(|t| t.doc.meta.display_name.clone());
        let dirty = st
            .tabs
            .get(st.active)
            .map(|t| t.doc.is_dirty())
            .unwrap_or(false);
        (
            actions::menu_bar_items(&st),
            st.menu.open,
            name.unwrap_or_else(|| st.tr(crate::ui::Msg::DocUntitled).into_owned()),
            dirty,
            st.win.maximized,
            st.menu.alt,
            st.theme(),
        )
    };

    let bar_style = format!(
        "position:absolute; top:0; left:0; right:0; height:{TITLEBAR_H}px; z-index:50;
         background:{bg}; border-bottom:1px solid {border}; display:flex; align-items:stretch;
         font-size:12px; color:{fg}; user-select:none; touch-action:none; cursor:default;",
        bg = t.menu_bg,
        border = t.menu_border,
        fg = t.menu_fg
    );
    // `touch-action:none`: blitz only dispatches `click` if the gesture never
    // entered its pan-drag mode, and a finger tap always jitters more than the
    // 2px threshold — so without this, every touch tap on a caption or a window
    // button is swallowed as a (barren) pan and the menu never opens. The bar
    // has nothing to scroll, so forbidding pans costs nothing. Note this must
    // NOT go on the app root: the editor viewport *needs* touch panning for
    // single-finger scrolling. blitz resolves the effective value by walking up
    // from the touch target, so one declaration here covers every caption and
    // window button; see blitz-dom `events/pointer.rs::touch_action_pan_axes`.

    // ---- app icon ----------------------------------------------------------
    // The mark itself (see `appicon`): the accent tile, the page and the
    // caret are all inside the SVG, so this box is transparent padding only.
    // A CSS background here would sit *behind* the mark's own rounded tile and
    // the two radii would not line up at non-integer DPI scales.
    let icon_style = "width:16px; height:16px; margin:10px 8px 10px 12px;
         align-self:center; flex:0 0 auto; overflow:hidden;";
    let mark = appicon::svg();

    // ---- captions ----------------------------------------------------------
    // Each caption is its own component (see `MenuCaption`) so hover/press are
    // tracked locally; only the geometry is computed here.
    //
    // A caption is rendered as `文件(F)` — the label plus its access key in
    // parentheses — because the label is Chinese and the key is Latin, so there
    // is no character in the label to underline; the parenthesised key is what
    // makes <kbd>Alt</kbd> discoverable at all.
    //
    // `hot` is whether the bar is active — <kbd>Alt</kbd> held. An open menu
    // deliberately does *not* count: opening one by mouse ends the Alt session
    // (`MenuUi::menu_toggle`), so clicking a caption must not leave the access
    // keys on screen.
    let caps_v: Vec<CaptionView> = caps
        .iter()
        .zip(actions::Caption::ALL)
        .map(|(it, cap)| {
            let label = it.label().to_string();
            let is_open = open == Some(cap);
            let hot = alt_hot.then(|| cap.accel());
            CaptionView {
                cap,
                label,
                is_open,
                hot,
            }
        })
        .collect();

    // ---- overflow (the more button) ------------------------------------------
    // Captions that no longer fit the window are not rendered; a more button lists all
    // hidden captions vertically.
    // Available width = window width − app icon − 3 window buttons − more-button reserve − margins.
    let win_w = { state.read().win.w };
    let icons_w = ICON_LEFT + ICON_BOX + ICON_GAP;
    let winbtns_w = WINBTN_W * 3.0;
    let avail = (win_w - icons_w - winbtns_w - MORE_W - 12.0).max(0.0);
    let mut used = 0.0f32;
    let mut visible = 0usize;
    for (idx, cap) in caps_v.iter().enumerate() {
        let w = caption_box_w(idx, &cap.label);
        // `visible > 0`: keep at least the first caption however narrow the window (the old
        // condition was always true, so the comment's "keep at least one" never actually held)
        if used + w > avail && visible > 0 {
            break;
        }
        used += w;
        visible += 1;
    }
    let hidden_from = visible; // index of the first hidden caption
    let has_more = hidden_from < caps_v.len();
    // Write the truncation result back to state for the more list and keyboard navigation.
    // Conditional write: an unconditional write() during render would mark_dirty and make this
    // component re-render itself every frame.
    if state.read().menu.more_from != hidden_from {
        state.write().menu.more_from = hidden_from;
    }

    // ---- centered document title ------------------------------------------
    let title_style = format!(
        "flex:0 1 auto; align-self:center; max-width:40%; padding:0 12px; overflow:hidden;
         text-overflow:ellipsis; white-space:nowrap; font-size:12px; color:{fg}; cursor:default;",
        fg = t.menu_fg
    );
    let dot_style = format!("color:{c}; margin-right:5px;", c = t.accent);

    // ---- window buttons ----------------------------------------------------
    // Rendered as their own component so each can track hover locally; the
    // close button turns red on hover, exactly like every native frame.
    let max_glyph = if maximized {
        glyph::RESTORE
    } else {
        glyph::MAX
    }
    .to_string();
    // The state-dependent maximize/restore name has to change together with the glyph.
    let max_name = if maximized {
        Msg::WinBtnRestore
    } else {
        Msg::WinBtnMaximize
    };
    let w = WINBTN_W;

    rsx! {
        div {
            style: "{bar_style}",
            div { style: "{icon_style}", dangerous_inner_html: "{mark}" }
            for (idx, cap) in caps_v.iter().enumerate().take(visible) {
                MenuCaption { key: "cap{idx}", state, view: cap.clone(), idx, t }
            }
            if has_more {
                MoreBtn { state }
            }
            { drag_region(state, t) }
            // The title itself drags the window (see
            // `start_drag`); a double click still maximizes.
            div {
                style: "{title_style}",
                onpointerdown: move |evt| start_drag(state, evt),
                if dirty { span { style: "{dot_style}", "\u{25CF}" } }
                "{title}"
            }
            { drag_region(state, t) }
            WinBtn { state, w, glyph: glyph::MIN.to_string(), kind: WB::Min, name: Msg::WinBtnMinimize }
            WinBtn { state, w, glyph: max_glyph, kind: WB::Max, name: max_name }
            WinBtn { state, w, glyph: glyph::CLOSE.to_string(), kind: WB::Close, name: Msg::WinBtnClose }
        }
    }
}

/// One window-control button, with the hover highlight every native frame has.
///
/// Tri-state (hover / pressed / close-red) lives in the `.wb` / `.wb.close` classes, see
/// `components::style`.
#[component]
fn WinBtn(
    state: Signal<EditorState>,
    w: f32,
    glyph: String,
    kind: WB,
    /// Accessibility name: the accesskit tree only names nodes from text children (blitz does not
    /// support `aria-label` yet), the glyphs themselves (─ □ ✕) are not readable names, so a
    /// zero-size, layout-free text child is added here; the glyph span is marked `aria-hidden`
    /// so the name never mixes into the symbol.
    name: Msg,
) -> Element {
    // Tri-state in the `.wb` (min/max) / `.wb.close` (close red) classes.
    let mut tip: Signal<crate::ui::components::Tip> = use_context();
    let style = format!("width:{w}px;");
    let name_text = state.read().tr(name).into_owned();
    rsx! {
        div {
            class: if kind == WB::Close { "wb close" } else { "wb" },
            role: "button",
            style: "{style}",
            onmouseenter: move |evt| {
                let c = evt.data.client_coordinates();
                tip.write().show(name_text.clone(), c.x, c.y);
            },
            onmouseleave: move |_| tip.write().hide(),
            onpointerdown: move |evt| {
                tip.write().hide();
                evt.stop_propagation();
                // see the caption handler: the root cannot cancel this click for
                // us, and an un-cancelled click would blur the IME composer
                evt.prevent_default();
            },
            onclick: move |evt| {
                evt.stop_propagation();
                // see the caption handler: the root cannot cancel this click for
                // us, and an un-cancelled click would blur the IME composer
                evt.prevent_default();
                act_on_window_button(kind, state);
            },
            span { aria_hidden: "true", "{glyph}" }
            span { style: "font-size:0; line-height:0;", "{name_text}" }
        }
    }
}

/// Act on a window-control button.
fn act_on_window_button(wb: WB, mut state: Signal<EditorState>) {
    use winshell::win::frameless as win32;
    match wb {
        WB::Min => win32::minimize(),
        WB::Max => {
            let maximized = { state.read().win.maximized };
            win32::toggle_maximize(maximized);
            // The OS is the source of truth for the maximized state; reflect it
            // so the button swaps to the restore glyph immediately.
            state.write().win.maximized = !maximized;
        }
        WB::Close => win32::close(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Captions must advance left to right and never overlap.
    ///
    /// `caption_x` is what places an <kbd>Alt</kbd>-opened dropdown, so a
    /// caption that measured narrower than it draws would put its panel under
    /// the *previous* caption — a bug that is invisible until you use the
    /// keyboard.
    #[test]
    fn caption_positions_are_increasing_and_non_overlapping() {
        let st = EditorState::minimal();
        let caps = actions::menu_bar_items(&st);
        let mut prev_end = 0.0f32;
        for (idx, (item, cap)) in caps.iter().zip(actions::Caption::ALL).enumerate() {
            let x = caption_x(&st, cap);
            assert!(
                x >= prev_end,
                "caption {:?} overlaps the previous one",
                item.label()
            );
            // Same box the estimate advances by — the widths must agree or the
            // test would assert against a layout that does not exist.
            prev_end = x + caption_box_w(idx, item.label());
        }
    }

    /// The first caption starts after the app icon, and every caption has a
    /// positive position.
    ///
    /// Captions are located by identity, so "no such caption" cannot happen
    /// (`Caption` is exhaustive); this pins that every caption has an anchor.
    #[test]
    fn caption_x_finds_every_caption() {
        let st = EditorState::minimal();
        for cap in actions::Caption::ALL {
            assert!(caption_x(&st, cap) > 0.0, "{cap:?} has no anchor position");
        }
    }

    /// A real measurement (parked by shell from blitz's layout tree) beats the
    /// flex-layout estimate, and a missing entry falls back per-caption rather
    /// than disabling the slot as a whole — early frames and mid-layout turns
    /// have only *some* rects available.
    ///
    /// The injected measurement targets the *last* caption: tests run in
    /// parallel and share this module's static slot, and shifting the last
    /// caption 5px right cannot break any other test's assertions (no caption
    /// sits after it), whereas shifting the first would overlap the second in
    /// the monotonicity test.
    #[test]
    fn measured_anchor_wins_and_missing_entries_fall_back() {
        let st = EditorState::minimal();
        let caps = actions::menu_bar_items(&st);
        let last_idx = caps.len() - 1;
        let last = actions::Caption::ALL[last_idx];

        // Slot empty: pure estimate.
        let est_last = caption_x(&st, last);

        // Park a real measurement for the last caption only.
        let mut xs: Vec<Option<f32>> = vec![None; caps.len()];
        xs[last_idx] = Some(est_last + 5.0);
        assert!(set_measured_caption_x(xs));
        assert_eq!(
            caption_x(&st, last),
            est_last + 5.0,
            "measured value must win"
        );

        // A `None` entry falls back to the estimate for that caption.
        let mut xs: Vec<Option<f32>> = vec![None; caps.len()];
        xs[last_idx] = None;
        set_measured_caption_x(xs);
        assert_eq!(caption_x(&st, last), est_last);

        // Clearing the slot returns to pure estimation (restore for other tests).
        set_measured_caption_x(Vec::new());
        assert_eq!(caption_x(&st, last), est_last);
    }
}
