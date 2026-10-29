//! Find / replace bar, the goto-line box and the "find all" results panel.
//!
//! Geometry is explicit pixels (Blitz cannot derive a box height from
//! `top:` + `bottom:` insets). The bar is docked directly under the tab strip
//! and the results panel directly above the status bar; [`EditorState::relayout`]
//! shrinks the viewport accordingly.
//!
//! The boxes are **not** real DOM focus targets — the editor has exactly one
//! focused element (the IME composer), which is parked over whichever box owns
//! [`FindFocus`]. Input is therefore routed by state, not by DOM focus.

use dioxus_native::prelude::*;

use crate::highlight::Theme;
use crate::search::SearchMode;
use crate::ui::state::{EditorState, FindFocus, CHROME_TOP, FIND_PANEL_H, FIND_ROW_H, STATUS_H};
use crate::ui::{Fmt, Msg};

/// vertical padding of a row inside the bar
pub const FIND_PAD: f32 = 4.0;
/// height of a text box inside the bar
pub const FIND_FIELD_H: f32 = 22.0;
/// x / width of the label column
pub const FIND_LABEL_X: f32 = 8.0;
pub const FIND_LABEL_W: f32 = 34.0;
/// x / width of the text boxes
pub const FIND_FIELD_X: f32 = 46.0;
pub const FIND_FIELD_W: f32 = 240.0;
/// height of one results row
pub const RESULT_ROW_H: f32 = 18.0;

/// Where the single IME composer should sit when a find box owns the keyboard.
/// Returns `(x, y, height, width)` in absolute client coordinates, or `None`
/// when the keyboard belongs to the document.
pub fn field_composer_rect(st: &EditorState) -> Option<(f32, f32, f32, f32)> {
    let row = match st.find.focus {
        FindFocus::Doc => return None,
        FindFocus::Pattern => 0.0,
        FindFocus::Replacement => 1.0,
        FindFocus::Goto => 0.0,
    };
    let text = st.find.field().unwrap_or("");
    let w = st.metrics.x_of_offset(text, text.len(), 4);
    let x = FIND_FIELD_X + 5.0 + w.min(FIND_FIELD_W - 40.0);
    let y = CHROME_TOP + row * FIND_ROW_H + FIND_PAD + 3.0;
    Some((x, y, FIND_FIELD_H - 8.0, 80.0))
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum FindAct {
    Prev,
    Next,
    /// Find/replace tab switch (supported by both the popup and the in-page bar)
    Tab(bool),
    FindAll,
    TogglePanel,
    Case,
    Word,
    Mode,
    Replace,
    ReplaceAll,
    Close,
    Focus(FindFocus),
    GotoOk,
    GotoCancel,
    Result(usize),
    // ---- find-popup only (the in-page bar never issues these actions) ----
    /// Count (leaves the selection and results panel alone)
    Count,
    /// Direction radio: true = up
    Dir(bool),
    /// Wrap-around toggle
    Wrap,
    /// Search-within-selection toggle
    InSel,
    /// Search-mode radio
    ModeOf(SearchMode),
    /// Open/close the find-history dropdown (local state inside the popup; the in-page bar
    /// ignores it)
    History,
}

/// One button's geometry and semantics (a layout result shared by the in-page bar and the find
/// child window).
///
/// `label` stores a key, not a rendered string: the text is fetched for the current language at
/// draw time (`FindBtn`).
pub struct BarBtn {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub label: Msg,
    pub act: FindAct,
    pub on: bool,
}

/// Compute the find bar's button layout and message position.
///
/// `vw` is the available width (the main-window width for the in-page bar, the child window's
/// own width for it); the in-page-bar-only goto row opens via `goto_open`. Returns
/// `(buttons, message x, message y, close × x)`.
// The layout parameters are naturally positional and the two callers' fields line up one to one,
// so packing them into a struct would only add boilerplate.
#[allow(clippy::too_many_arguments)]
pub fn layout_bar(
    vw: f32,
    replace: bool,
    goto_open: bool,
    case: bool,
    word: bool,
    mode: SearchMode,
    nres: usize,
    panel: bool,
) -> (Vec<BarBtn>, f32, f32, f32) {
    let close_x = (vw - 24.0).max(0.0);
    let limit = close_x - 6.0;
    let y0 = FIND_PAD;
    let y1 = FIND_ROW_H + FIND_PAD;
    let bx = FIND_FIELD_X + FIND_FIELD_W + 10.0;
    let mut btns: Vec<BarBtn> = Vec::new();
    let mut e0 = bx;
    let mut e1 = bx;
    let push = |btns: &mut Vec<BarBtn>,
                end: &mut f32,
                y: f32,
                w: f32,
                label: Msg,
                act: FindAct,
                on: bool| {
        let x = *end;
        if x + w <= limit {
            btns.push(BarBtn {
                x,
                y,
                w,
                label,
                act,
                on,
            });
        }
        *end = x + w + 6.0;
    };

    if goto_open {
        push(
            &mut btns,
            &mut e0,
            y0,
            56.0,
            Msg::FindBtnGoto,
            FindAct::GotoOk,
            true,
        );
        push(
            &mut btns,
            &mut e0,
            y0,
            56.0,
            Msg::CommonCancel,
            FindAct::GotoCancel,
            false,
        );
    } else {
        push(
            &mut btns,
            &mut e0,
            y0,
            52.0,
            Msg::FindBtnPrev,
            FindAct::Prev,
            false,
        );
        push(
            &mut btns,
            &mut e0,
            y0,
            52.0,
            Msg::FindBtnNext,
            FindAct::Next,
            true,
        );
        push(
            &mut btns,
            &mut e0,
            y0,
            66.0,
            Msg::FindBtnFindAll,
            FindAct::FindAll,
            false,
        );
        push(
            &mut btns,
            &mut e0,
            y0,
            30.0,
            Msg::FindBtnCase,
            FindAct::Case,
            case,
        );
        push(
            &mut btns,
            &mut e0,
            y0,
            40.0,
            Msg::FindBtnWord,
            FindAct::Word,
            word,
        );
        push(
            &mut btns,
            &mut e0,
            y0,
            48.0,
            mode_label(mode),
            FindAct::Mode,
            mode != SearchMode::Plain,
        );
        if nres > 0 {
            push(
                &mut btns,
                &mut e0,
                y0,
                52.0,
                Msg::FindBtnResults,
                FindAct::TogglePanel,
                panel,
            );
        }
        if replace {
            push(
                &mut btns,
                &mut e1,
                y1,
                52.0,
                Msg::FindLabelReplace,
                FindAct::Replace,
                false,
            );
            push(
                &mut btns,
                &mut e1,
                y1,
                66.0,
                Msg::FindBtnReplaceAll,
                FindAct::ReplaceAll,
                false,
            );
        }
    }

    let (msg_x, msg_y) = if replace && !goto_open {
        (e1, y1)
    } else {
        (e0, y0)
    };
    (btns, msg_x, msg_y, close_x)
}

fn run(mut state: Signal<EditorState>, act: FindAct) {
    match act {
        FindAct::Prev => state.write().find_prev(),
        FindAct::Next => state.write().find_next(),
        FindAct::Tab(replace) => {
            state.write().set_find_tab(replace);
            return;
        }
        FindAct::FindAll => state.write().find_all_matches(),
        FindAct::TogglePanel => state.write().toggle_find_panel(),
        FindAct::Case => state.write().toggle_find_case(),
        FindAct::Word => state.write().toggle_find_word(),
        FindAct::Mode => state.write().cycle_find_mode(),
        FindAct::Replace => state.write().replace_one(),
        FindAct::ReplaceAll => state.write().replace_all(),
        FindAct::Close => {
            state.write().close_find();
            return;
        }
        FindAct::Focus(f) => state.write().set_find_focus(f),
        FindAct::GotoOk => {
            state.write().commit_goto();
            return;
        }
        FindAct::GotoCancel => {
            state.write().close_find();
            return;
        }
        FindAct::Result(i) => state.write().goto_result(i),
        // Popup-only action; its semantics still work when the in-page bar receives it
        FindAct::Count => state.write().find_count(),
        FindAct::Dir(up) => state.write().set_find_dir(up),
        FindAct::Wrap => state.write().toggle_find_wrap(),
        FindAct::InSel => state.write().toggle_find_in_sel(),
        FindAct::ModeOf(m) => state.write().set_find_mode(m),
        FindAct::History => {}
    }
    // clicking a button must not hand the keyboard back to the document
    let mut st = state.write();
    if st.find.focus == FindFocus::Doc {
        st.find.focus = match act {
            FindAct::Replace | FindAct::ReplaceAll => FindFocus::Replacement,
            _ => FindFocus::Pattern,
        };
    }
}

/// Short labels for the in-page bar's mode buttons (the popup's full wording lives in
/// FindModePlain / FindModeRegex / FindModeEscExt — the same thing needs different text lengths
/// in the two places, each occupying one key).
fn mode_label(m: SearchMode) -> Msg {
    match m {
        SearchMode::Plain => Msg::FindModeShortPlain,
        SearchMode::Regex => Msg::FindModeShortRegex,
        SearchMode::Esc => Msg::FindModeShortEsc,
    }
}

/// A flat find-bar button with hover / pressed feedback and a short color
/// transition, matching the menu buttons' micro-interactions.
#[component]
fn FindBtn(
    state: Signal<EditorState>,
    x: f32,
    y: f32,
    w: f32,
    label: String,
    act: FindAct,
    on: bool,
    t: &'static Theme,
) -> Element {
    // Tri-state in the `.fbtn` / `.fbtn.on` classes (components::style): `on` is the bar's
    // primary action, parked on the accent background; hovering only swaps the background, never
    // the meaning.
    let style = format!(
        "position:absolute; left:{x}px; top:{y}px; width:{w}px; height:{FIND_FIELD_H}px; \
         line-height:{FIND_FIELD_H}px; font-size:11px; text-align:center; border-radius:2px;
         cursor:default; overflow:hidden; touch-action:none;"
    );
    rsx! {
        div {
            class: if on { "fbtn on" } else { "fbtn" },
            style: "{style}",
            onpointerdown: move |evt| {
                // see `editor.rs`: an un-cancelled click blurs the composer
                evt.stop_propagation();
                evt.prevent_default();
            },
            onclick: move |_| run(state, act),
            "{label}"
        }
    }
}

/// A small square icon button (the bar's `×`), with hover + press feedback.
#[component]
fn CloseBtn(state: Signal<EditorState>, x: f32) -> Element {
    // The same tri-state as the title-bar/dialog close button (`.f-x`: hover/press turns the
    // background close-red)
    let style = format!(
        "position:absolute; left:{x}px; top:{FIND_PAD}px; width:20px; height:{FIND_FIELD_H}px;
         line-height:{FIND_FIELD_H}px; text-align:center; font-size:15px;
         cursor:default; border-radius:2px; touch-action:none;"
    );
    rsx! {
        div {
            class: "f-x",
            style: "{style}",
            onpointerdown: move |evt| {
                evt.stop_propagation();
                evt.prevent_default();
            },
            onclick: move |_| run(state, FindAct::Close),
            "\u{00D7}"
        }
    }
}

/// A text box: label + value + (virtual) caret. `label` / `placeholder` store keys and the text
/// is fetched for the current language at draw time; `placeholder: None` shows no placeholder.
fn field(
    state: Signal<EditorState>,
    label: Msg,
    value: &str,
    placeholder: Option<Msg>,
    row: f32,
    focus: FindFocus,
    t: &'static Theme,
) -> Element {
    let loc = state.read().locale();
    let label_text = label.get(loc);
    let placeholder_text = placeholder.map(|m| m.get(loc)).unwrap_or("");
    let focused = { state.read().find.focus == focus };
    let preedit = {
        let st = state.read();
        if focused {
            st.find.preedit.clone()
        } else {
            String::new()
        }
    };
    let top = row * FIND_ROW_H + FIND_PAD;
    let bg = if focused { t.input_bg } else { t.panel_bg };
    let border = if focused { t.accent } else { t.input_border };
    let box_style = format!(
        "position:absolute; left:{FIND_FIELD_X}px; top:{top}px; width:{FIND_FIELD_W}px; \
         height:{FIND_FIELD_H}px; background:{bg}; border:1px solid {border};
         display:flex; align-items:center; padding:0 4px; overflow:hidden; cursor:text;
         user-select:none; border-radius:2px; {anim}",
        anim = crate::ui::motion::fast("border-color, background-color")
    );
    let label_style = format!(
        "position:absolute; left:{FIND_LABEL_X}px; top:{top}px; width:{FIND_LABEL_W}px; \
         height:{FIND_FIELD_H}px; line-height:{FIND_FIELD_H}px; font-size:12px; color:{fg};
         user-select:none; overflow:hidden;",
        fg = t.menu_fg
    );
    // **flex:0 1 auto** (shrinkable, not greedy): it used to be flex:1 — the text span always
    // filled the remaining width, pushing the caret to the input's right edge instead of hugging
    // the insertion point, which looked like "no caret".
    let text_style = format!(
        "font-size:12px; color:{}; white-space:pre; overflow:hidden;
         flex:0 1 auto; min-width:0;",
        if value.is_empty() && preedit.is_empty() {
            t.gutter_fg
        } else {
            t.menu_fg
        }
    );
    let preedit_style = format!("font-size:12px; color:{}; white-space:pre;", t.accent);
    // Caret blinking rides the state phase (`caret_phase`, flipped by the BlinkTick thread every
    // 265ms and written back by the frame hook), the same source as the main editor. A CSS
    // `caret-blink` infinite animation is **not** an option: blitz keeps self-renewing
    // request_redraw every frame for windows with running animations, pinning the whole app at
    // 40–60fps (measured in round seven, 2026-10; see the winshell bridge module comment).
    let (fb_blink, fb_phase) = {
        let st = state.read();
        (st.caret_blink, st.caret_phase)
    };
    let caret_op = if fb_blink && !fb_phase { 0.0 } else { 1.0 };
    let caret_style = format!(
        "width:1px; height:14px; background:{}; opacity:{caret_op};",
        t.caret
    );
    let shown = if value.is_empty() {
        placeholder_text
    } else {
        value
    };
    rsx! {
        div { style: "{label_style}", "{label_text}" }
        div {
            style: "{box_style}",
            onclick: move |_| run(state, FindAct::Focus(focus)),
            span { style: "{text_style}", "{shown}" }
            if focused && !preedit.is_empty() {
                span { style: "{preedit_style}", "{preedit}" }
            }
            if focused {
                div { style: "{caret_style}" }
            }
        }
    }
}

#[component]
pub fn FindBar() -> Element {
    let state = use_context::<Signal<EditorState>>();
    let (open, replace, pattern, replacement, mode, case, word, msg, nres, panel, goto, vw, t) = {
        let st = state.read();
        (
            st.find.open,
            st.find.replace,
            st.find.pattern.clone(),
            st.find.replacement.clone(),
            st.find.mode,
            st.find.case,
            st.find.word,
            st.find
                .message
                .as_ref()
                .map(|m| st.tr(m.clone()).into_owned())
                .unwrap_or_default(),
            st.find.results.len(),
            st.find.panel,
            st.find.goto.clone(),
            st.win.w,
            st.theme(),
        )
    };
    // The in-page bar only draws in embedded mode: in child-window mode the find bar lives in
    // its own window.
    if !open || state.read().find.child_win {
        return rsx! {};
    }

    let bar_h = if replace {
        FIND_ROW_H * 2.0
    } else {
        FIND_ROW_H
    };
    let bar_style = format!(
        "position:absolute; left:0; top:{CHROME_TOP}px; width:{vw}px; height:{bar_h}px;
         background:{}; border-bottom:1px solid {}; z-index:60; user-select:none;",
        t.menu_bg, t.menu_border
    );

    // ---- buttons -----------------------------------------------------------
    let (btns, msg_x, msg_y, close_x) =
        layout_bar(vw, replace, goto.is_some(), case, word, mode, nres, panel);
    let limit = close_x - 6.0;
    let msg_w = (limit - msg_x).max(60.0);
    let msg_style = format!(
        "position:absolute; left:{msg_x}px; top:{msg_y}px; width:{msg_w}px;
         height:{FIND_FIELD_H}px; line-height:{FIND_FIELD_H}px; font-size:11px; color:{fg};
         overflow:hidden; white-space:nowrap;",
        fg = t.menu_fg
    );

    let goto_field: Element = match goto.as_deref() {
        Some(g) => field(state, Msg::FindLblLine, g, None, 0.0, FindFocus::Goto, t),
        None => rsx! {},
    };
    let pattern_field: Element = if goto.is_some() {
        rsx! {}
    } else {
        field(
            state,
            Msg::FindLabelFind,
            &pattern,
            Some(Msg::FindPhPattern),
            0.0,
            FindFocus::Pattern,
            t,
        )
    };
    let repl_field: Element = if replace && goto.is_none() {
        field(
            state,
            Msg::FindLabelReplace,
            &replacement,
            Some(Msg::FindPhReplace),
            1.0,
            FindFocus::Replacement,
            t,
        )
    } else {
        rsx! {}
    };
    let msg_text = if goto.is_some() { String::new() } else { msg };

    rsx! {
        div {
            style: "{bar_style}",
            {goto_field}
            {pattern_field}
            {repl_field}
            for (i, b) in btns.iter().enumerate() {
                FindBtn {
                    key: "b{i}",
                    state,
                    x: b.x,
                    y: b.y,
                    w: b.w,
                    // The label stores a key: text is fetched during this component's render
                    // (state is subscribed here), while `FindBtn` itself reads no state — so each
                    // button does not end up with its own state subscription.
                    label: state.read().tr(b.label).into_owned(),
                    act: b.act,
                    on: b.on,
                    t,
                }
            }
            div { style: "{msg_style}", "{msg_text}" }
            CloseBtn { state, x: close_x }
        }
    }
}

#[component]
pub fn FindPanel() -> Element {
    let mut state = use_context::<Signal<EditorState>>();
    let (open, nres, panel, cur, top, vw, t, top_px, fresh) = {
        let st = state.read();
        let (doc_id, gen) = st
            .tabs
            .get(st.active)
            .map(|t| (t.doc.id, t.doc.gen))
            .unwrap_or((0, 0));
        (
            st.find.open,
            st.find.results.len(),
            st.find.panel,
            st.find.cur,
            st.find.panel_top,
            st.win.w,
            st.theme(),
            st.win.h - STATUS_H - FIND_PANEL_H,
            st.find.valid_for(doc_id, gen),
        )
    };
    if !open || !panel || nres == 0 {
        return rsx! {};
    }

    let panel_style = format!(
        "position:absolute; left:0; top:{top_px}px; width:{vw}px; height:{FIND_PANEL_H}px;
         background:{}; border-top:1px solid {}; z-index:60; overflow:hidden;
         user-select:none;",
        t.panel_bg, t.menu_border
    );
    let head_style = format!(
        "position:absolute; left:0; top:0; width:{vw}px; height:22px; line-height:22px;
         font-size:11px; color:{fg}; padding:0 10px; background:{bg};
         display:flex; align-items:center;",
        fg = t.menu_fg,
        bg = t.menu_bg
    );
    let body_h = FIND_PANEL_H - 24.0;
    let body_style = format!(
        "position:absolute; left:0; top:24px; width:{vw}px; height:{body_h}px; overflow:hidden;"
    );

    // only the visible slice is materialised — reading 50k lines per frame
    // would be far too slow
    let rows: Vec<(String, String, String, String, usize)> = {
        let st = state.read();
        let doc = st.active_doc();
        let n = st.find.rows_per_page();
        st.find
            .results
            .iter()
            .skip(top)
            .take(n)
            .enumerate()
            .map(|(k, m)| {
                let i = top + k;
                // offsets may point past the end once the document changed
                let start = m.start.min(doc.table.len());
                let line = doc.line_of_pos(start);
                let ls = doc.pos_of_line(line);
                let col = start.saturating_sub(ls) + 1;
                let mut text = doc.line_text(line).replace('\t', "    ");
                if text.len() > 300 {
                    // String::truncate panics unless the cut lands on a char
                    // boundary; step back to one (long lines routinely contain
                    // multi-byte CJK characters).
                    let mut end = 300;
                    while !text.is_char_boundary(end) {
                        end -= 1;
                    }
                    text.truncate(end);
                }
                let y = k as f32 * RESULT_ROW_H;
                // Rows are their own component (`ResultRow`) for the press
                // state; this only supplies the geometry-independent text.
                let style = format!(
                    "position:absolute; left:0; top:{y}px; width:{vw}px; height:{RESULT_ROW_H}px;",
                    y = y
                );
                let loc_style = format!("color:{};", t.accent);
                (
                    style,
                    loc_style,
                    format!("{}:{}  {}", line + 1, col, text),
                    t.menu_fg.to_string(),
                    i,
                )
            })
            .collect()
    };

    // The header text stores a key: the result count and freshness are data; the text is
    // fetched at draw time.
    let head_text = {
        let msg: i18n::Message = if fresh {
            Fmt::FindPanelTitle { n: nres }.into()
        } else {
            Fmt::FindPanelTitleStale { n: nres }.into()
        };
        state.read().tr(msg).into_owned()
    };

    rsx! {
        div {
            style: "{panel_style}",
            onpointerdown: move |evt| {
                // Finger-down accounting: a slide = scroll the result list (window-level deltas
                // route into `ui::touch`, per-line conversion in
                // `EditorState::apply_pending_scroll`); a lift with no slide is still a tap (the
                // row's onclick still fires).
                if evt.data.pointer_type() == "touch" {
                    let c = evt.data.client_coordinates();
                    crate::ui::touch::begin(
                        crate::ui::touch::PanTarget::Find,
                        evt.data.pointer_id(),
                        c.x,
                        c.y,
                    );
                }
            },
            onwheel: move |evt| {
                let dy = match evt.data.delta() {
                    dioxus_elements::geometry::WheelDelta::Pixels(v) => v.y as f32,
                    dioxus_elements::geometry::WheelDelta::Lines(v) => v.y as f32 * RESULT_ROW_H * 3.0,
                    dioxus_elements::geometry::WheelDelta::Pages(v) => v.y as f32 * body_h,
                };
                // Conditional write: scrolling while already at a bound (0/max) does not mark
                // dirty — viewport/csvtable wheel handling is the same two-phase shape;
                // this was the last one still missing it
                let next = {
                    let st = state.read();
                    let max = st.find.results.len().saturating_sub(st.find.rows_per_page());
                    (st.find.panel_top as i64 + (dy / RESULT_ROW_H) as i64)
                        .clamp(0, max as i64) as usize
                };
                if next != state.read().find.panel_top {
                    state.write().find.panel_top = next;
                }
            },
            div {
                style: "{head_style}",
                span {
                    style: "flex:1;",
                    "{head_text}"
                }
                PanelClose { state }
            }
            div {
                style: "{body_style}",
                for (style, loc_style, text, _fg, i) in rows {
                    ResultRow {
                        key: "r{i}",
                        state,
                        style,
                        loc_style,
                        text,
                        index: i,
                        active: cur == Some(i),
                    }
                }
            }
        }
    }
}

/// One row of the "find all" results, with hover + press feedback.
#[component]
fn ResultRow(
    state: Signal<EditorState>,
    style: String,
    loc_style: String,
    text: String,
    index: usize,
    active: bool,
) -> Element {
    // Tri-state in the `.rrow` / `.rrow.on` classes (components::style)
    let full = format!(
        "{style} line-height:{RESULT_ROW_H}px; font-size:11px; padding:0 10px;
         overflow:hidden; white-space:pre;"
    );
    rsx! {
        div {
            class: if active { "rrow on" } else { "rrow" },
            style: "{full}",
            onpointerdown: move |evt| {
                // No stop_propagation: the panel needs finger-drag scrolling of the result list
                // (see the panel root's `touch::begin`)
                evt.prevent_default();
            },
            onclick: move |_| run(state, FindAct::Result(index)),
            span { style: "{loc_style}", "{text}" }
        }
    }
}

/// The `×` that dismisses the results panel.
#[component]
fn PanelClose(state: Signal<EditorState>) -> Element {
    let style = "padding:0 6px; cursor:default;";
    rsx! {
        div {
            class: "f-x",
            style: "{style}",
            onpointerdown: move |evt| {
                evt.stop_propagation();
                evt.prevent_default();
            },
            onclick: move |_| run(state, FindAct::Close),
            "\u{00D7}"
        }
    }
}
