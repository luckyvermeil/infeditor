//! In-application modal dialogs.
//!
//! Native message boxes (`rfd::MessageDialog`) block the blitz event loop and
//! never became visible on top of the frameless window, so every informational
//! dialog — About, the unsaved-changes confirmation, MD5/SHA-1, the CSV table view,
//! the Markdown preview, Preferences, Shortcuts — is rendered by the app itself: a dimmed
//! backdrop plus a centered panel, stacked above everything else
//! (z-index 1400 > menu 1300).
//!
//! The dialog *content* is pure state ([`crate::ui::state::Dialog`], written by
//! `EditorState` commands); this component renders it, runs its action buttons
//! and closes it. [`DialogBody::Prefs`] is the one body that is **live**: it
//! reads the editor state on every frame instead of holding a snapshot, so a
//! switch inside the panel takes effect immediately.

use dioxus_native::prelude::*;

use crate::highlight::Theme;
use crate::ui::actions::{self, Cmd};
use crate::ui::components::base::{Btn, BtnKind, Field};
use crate::ui::design::FONT_MONO;
use crate::ui::state::{Dialog, DialogBody, EditorState, WINBTN_W};
use crate::ui::Msg;

/// Fluent entrance durations (ms): backdrop fade-in / panel fade-in + scale.
const FADE_MS: u32 = 120;
const PANEL_MS: u32 = 170;
/// The Fluent "decelerate" curve: fast out, slow stop.
const EASE: &str = "cubic-bezier(0.1, 0.9, 0.2, 1)";

/// DOM ids of a dialog's action buttons (`inf-dlg-btn-0/1/2…`; left→right = declaration order).
///
/// One purpose only: GUI automation needs **real geometry**. Self-drawn dialogs have no Win32
/// handle, and tests historically guessed button positions from the accent-colored block — once
/// primary/secondary buttons diverged (primary filled with the accent color, secondary on a
/// neutral surface) that oracle died outright. `ui::shell`'s layout probe uses
/// `get_element_by_id` + `get_client_bounding_rect` to write center points into
/// shell.log; the tests only read that log, so theme/colors never matter.
pub fn action_button_dom_id(idx: usize) -> String {
    format!("inf-dlg-btn-{idx}")
}

/// The dialog panel's DOM id.
///
/// The panel centers with `left/top:50% + transform:translate(-50%,-50%)`, but blitz's
/// `get_client_bounding_rect` only accumulates layout positions and **omits CSS transforms** —
/// the probe's button coordinates are therefore off by half a panel on both axes. With the panel
/// rectangle that known offset can be compensated (see `ui::shell::probe_dialog_buttons`).
/// Changing how the panel is positioned requires updating the probe in lockstep, or GUI
/// automation will click empty space.
pub fn panel_dom_id() -> String {
    "inf-dlg-panel".to_string()
}

/// Root-level modal dialog, mounted once in `editor.rs`.
#[component]
pub fn DialogOverlay() -> Element {
    let mut state: Signal<EditorState> = use_context();
    let Some(dialog) = state.read().dialog.clone() else {
        return rsx! {};
    };

    let t = state.read().theme();
    let bg = t.menu_bg;
    let fg = t.menu_fg;
    let border = t.menu_border;
    let dim = t.dim_backdrop;
    let shadow = t.shadow;

    let body = dialog_body(state, t, &dialog.body);

    let dismissable = dialog.dismissable;

    // ---- text field (Run…, Save Macro…) --------------------------------------
    //
    // The buffer lives in `DialogInput` (state), not in a DOM node: this app
    // has one real focus target and routes keys by state, so the field only
    // has to *show* what the composer is already typing into.
    let input_row: Element = match &dialog.input {
        None => rsx! {},
        Some(inp) => {
            let hint = state.read().tr(inp.hint.clone()).into_owned();
            rsx! {
                div {
                    style: "margin:12px 16px 0; flex:none;",
                    Field { value: inp.value.clone(), hint, focused: true, t }
                }
            }
        }
    };

    let action_row = dialog_action_row(state, t, &dialog);

    rsx! {
        // Backdrop: click anywhere outside the panel closes the dialog (unless
        // it is a confirmation, which the user has to answer explicitly).
        // Fluent-style fade-in (a main-window-only CSS animation — disabled in popup child
        // windows, see controls.rs).
        div {
            style: "position:absolute; left:0; top:0; right:0; bottom:0; z-index:1400; background:{dim}; animation: overlay-fade {FADE_MS}ms {EASE};",
            onpointerdown: move |evt| {
                evt.prevent_default();
                if dismissable {
                    state.write().close_dialog();
                }
            },
        }
        // Panel: swallow the press so it does not fall through to the backdrop
        // (the same trick the window buttons use). Fluent entrance: fade-in plus a slight
        // scale (96% → 100%, decelerate curve).
        div {
            id: "{panel_dom_id()}",
            style: "position:absolute; left:50%; top:50%; transform:translate(-50%,-50%); min-width:380px; max-width:76vw; max-height:78vh; z-index:1401; background:{bg}; border:1px solid {border}; border-radius:6px; box-shadow:0 8px 32px {shadow}; display:flex; flex-direction:column; overflow:hidden; animation: dialog-in {PANEL_MS}ms {EASE};",
            onpointerdown: move |evt| {
                evt.stop_propagation();
                evt.prevent_default();
            },
            // Title bar
            div {
                style: "display:flex; align-items:center; height:34px; padding:0 4px 0 14px; border-bottom:1px solid {border}; flex:none;",
                div {
                    style: "flex:1 1 auto; font-size:13px; font-weight:600; color:{fg};",
                    { state.read().tr(dialog.title.clone()).into_owned() }
                }
                DialogClose { state }
            }
            {input_row}
            // Body
            div {
                style: "padding:14px 16px; overflow:auto; flex:1 1 auto;",
                {body}
            }
            {action_row}
        }
    }
}

/// The dialog's content area: a pure render of [`DialogBody`] (the Prefs
/// variant is the one live body — it reads state on every frame).
fn dialog_body(state: Signal<EditorState>, t: &'static Theme, body: &DialogBody) -> Element {
    let bg = t.menu_bg;
    let fg = t.menu_fg;
    let border = t.menu_border;
    match body {
        DialogBody::Text(text) => {
            let text = state.read().tr(text.clone());
            rsx! {
                div {
                    style: "white-space:pre-wrap; font-family:{FONT_MONO}; font-size:13px; line-height:1.6; color:{fg};",
                    "{text}"
                }
            }
        }
        DialogBody::Table { header, rows } => {
            let cell_css = format!(
                "border:1px solid {border}; padding:3px 10px; font-size:12px; color:{fg}; white-space:nowrap;"
            );
            let head_cell = format!(
                "border:1px solid {border}; padding:3px 10px; font-size:12px; font-weight:600; background:{bg}; color:{fg}; white-space:nowrap;"
            );
            rsx! {
                table {
                    style: "border-collapse:collapse;",
                    thead {
                        tr {
                            for (i, h) in header.iter().enumerate() {
                                th {
                                    key: "{i}",
                                    style: "{head_cell}",
                                    "{h}"
                                }
                            }
                        }
                    }
                    tbody {
                        for (r, row) in rows.iter().enumerate() {
                            tr {
                                key: "{r}",
                                for (c, cell) in row.iter().enumerate() {
                                    td {
                                        key: "{c}",
                                        style: "{cell_css}",
                                        "{cell}"
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        // Fall back to &nbsp; for an empty HTML body: blitz replaces an empty
        // dangerous_inner_html with a zero node and the next template placeholder's
        // load_child panics out of bounds (see the same guard in dialog bodies).
        DialogBody::Html(html) => {
            let body = if html.is_empty() {
                "\u{a0}".to_string()
            } else {
                html.clone()
            };
            rsx! {
                div {
                    style: "font-size:14px; line-height:1.65; color:{fg}; max-width:640px;",
                    dangerous_inner_html: "{body}",
                }
            }
        }
        DialogBody::Prefs => preferences_body(state, t),
    }
}

/// The action row: one button per dialog entry, right-aligned.
///
/// Labels render here, at draw time: the dialog state holds message keys,
/// so an open dialog follows a language switch.
fn dialog_action_row(
    mut state: Signal<EditorState>,
    t: &'static Theme,
    dialog: &Dialog,
) -> Element {
    let border = t.menu_border;
    let buttons: Vec<(String, Option<Cmd>, bool)> = {
        let st = state.read();
        dialog
            .buttons
            .iter()
            .map(|b| (st.tr(b.label.clone()).into_owned(), b.cmd, b.primary))
            .collect()
    };
    let mut button_els: Vec<Element> = Vec::with_capacity(buttons.len());
    // For the probe: the action buttons' captions (order = visual left→right). GUI tests click
    // by caption, never by guessed index — misclicking Cancel or "Confirm discard" means lost
    // data.
    if winshell::shell::ui_probe_on() {
        winshell::shell::set_dialog_labels(buttons.iter().map(|b| b.0.clone()).collect());
    }
    for (bi, (label, cmd, primary)) in buttons.into_iter().enumerate() {
        let kind = if primary {
            BtnKind::Primary
        } else {
            BtnKind::Secondary
        };
        let did = dialog.id;
        button_els.push(rsx! {
            Btn {
                label,
                kind,
                t,
                id: Some(action_button_dom_id(bi)),
                on_click: move |evt: MouseEvent| {
                    // The click must not reach the backdrop (which would close
                    // the dialog) and must not run blitz's focus-losing default
                    // action.
                    evt.stop_propagation();
                    evt.prevent_default();
                    let mut st = state.write();
                    if let Some(cmd) = cmd {
                        // The dialog is closed *after* the command runs, so an
                        // input dialog's submit command can still read the typed
                        // text through `dialog_input()` here.
                        actions::run(&mut st, cmd);
                    }
                    // The button closes the dialog only when the command hung no new one off it:
                    // dialogs a command raised itself (the discard-snapshot double check, the
                    // recovery box you return to after cancel) must stay open.
                    if st.dialog.as_ref().is_some_and(|d| d.id == did) {
                        st.close_dialog();
                    }
                },
            }
        });
    }
    if button_els.is_empty() {
        rsx! {}
    } else {
        rsx! {
            div {
                style: "display:flex; justify-content:flex-end; align-items:center; height:46px;\n                       padding:0 14px; border-top:1px solid {border}; flex:none;",
                { button_els.into_iter() }
            }
        }
    }
}

/// The `×` in a dialog's own title bar.
///
/// It gets the same hover/press treatment as the main window's close button —
/// including turning red — because in a frameless window this is the *only*
/// visible way to dismiss the dialog with the mouse, and a bare glyph with no
/// feedback reads as decoration.
#[component]
fn DialogClose(state: Signal<EditorState>) -> Element {
    // Tri-state colors live in the `.dlg-x` class (components::style, generated per theme)
    let fs_glyph = crate::ui::design::FS_GLYPH_LG;
    let style = format!("width:{WINBTN_W}px; font-size:{fs_glyph}px;");
    rsx! {
        div {
            class: "dlg-x",
            style: "{style}",
            onpointerdown: move |evt| {
                evt.stop_propagation();
                evt.prevent_default();
            },
            onclick: move |evt| {
                evt.stop_propagation();
                evt.prevent_default();
                state.write().close_dialog();
            },
            "\u{00D7}"
        }
    }
}

/// One switchable row of the Preferences panel.
struct PrefRow {
    label: String,
    /// what the setting is set to right now
    value: String,
    /// what pressing the row does
    cmd: Cmd,
}

/// The live Settings → Preferences panel.
///
/// Every row is a real control: pressing the value runs the same [`Cmd`] the
/// menu uses, so the panel can never disagree with the menus about what a
/// setting means.
fn preferences_body(state: Signal<EditorState>, t: &'static Theme) -> Element {
    let rows: Vec<PrefRow> = {
        let st = state.read();
        let tr = |m: Msg| st.tr(m).into_owned();
        vec![
            PrefRow {
                label: tr(Msg::DlgPrefTheme),
                value: tr(if st.dark {
                    Msg::DlgThemeDark
                } else {
                    Msg::DlgThemeLight
                }),
                cmd: Cmd::ToggleTheme,
            },
            PrefRow {
                label: tr(Msg::DlgPrefCaretStyle),
                value: st.caret_style.label().into(),
                cmd: Cmd::CycleCaretStyle,
            },
            PrefRow {
                label: tr(Msg::DlgPrefCaretBlink),
                value: st.tr(on_off(st.caret_blink)).into_owned(),
                cmd: Cmd::ToggleCaretBlink,
            },
            PrefRow {
                label: tr(Msg::DlgPrefHighlight),
                value: st.tr(on_off(st.highlight_on)).into_owned(),
                cmd: Cmd::ToggleHighlight,
            },
            PrefRow {
                label: tr(Msg::DlgPrefHighlightAll),
                value: st.tr(on_off(st.highlight_all)).into_owned(),
                cmd: Cmd::HighlightAll,
            },
            PrefRow {
                label: tr(Msg::DlgPrefWrap),
                value: st.tr(on_off(st.wrap)).into_owned(),
                cmd: Cmd::WordWrap,
            },
            PrefRow {
                label: tr(Msg::DlgPrefShowWs),
                value: st.tr(on_off(st.show_ws)).into_owned(),
                cmd: Cmd::ShowWs,
            },
            PrefRow {
                label: tr(Msg::DlgPrefShowEol),
                value: st.tr(on_off(st.show_eol)).into_owned(),
                cmd: Cmd::ShowEol,
            },
            PrefRow {
                label: tr(Msg::DlgPrefVim),
                value: st.tr(on_off(st.vim.enabled)).into_owned(),
                cmd: Cmd::ToggleVim,
            },
        ]
    };
    let font_size = format!("{} px", state.read().font_size.round() as i32);
    let font_label = state.read().tr(Msg::DlgPrefFontSize).into_owned();
    let note = state.read().tr(Msg::DlgPrefNote).into_owned();

    let label_css = format!("font-size:12px; color:{}; line-height:26px;", t.menu_fg);
    let readout_css = format!(
        "font-size:12px; color:{}; min-width:58px; text-align:center;",
        t.menu_fg
    );
    let fg = t.menu_fg;

    let mut row_els: Vec<Element> = Vec::with_capacity(rows.len());
    for r in rows {
        row_els.push(rsx! {
            div {
                style: "display:flex; align-items:center; justify-content:space-between;
                       height:30px;",
                div { style: "{label_css}", "{r.label}" }
                PrefValue {
                    state,
                    text: r.value,
                    cmd: r.cmd,
                    t,
                }
            }
        });
    }

    rsx! {
        div {
            style: "min-width:340px;",
            { row_els.into_iter() }
            div {
                style: "display:flex; align-items:center; justify-content:space-between;
                       height:30px;",
                div { style: "{label_css}", "{font_label}" }
                div {
                    style: "display:flex; align-items:center;",
                    PrefValue { state, text: "\u{2212}".to_string(), cmd: Cmd::ZoomOut, t }
                    div { style: "{readout_css}", "{font_size}" }
                    PrefValue { state, text: "+".to_string(), cmd: Cmd::ZoomIn, t }
                    PrefValue { state, text: state.read().tr(Msg::PrefsReset).into_owned(), cmd: Cmd::ZoomReset, t }
                }
            }
            div {
                style: "margin-top:10px; font-size:11px; color:{fg}; opacity:0.75;",
                "{note}"
            }
        }
    }
}

/// The clickable value on one Preferences row: the command runs *without* closing
/// the dialog (a preferences panel is meant to be poked at repeatedly).
/// Visually it is [`BtnKind::Secondary`] (the hover border turns theme-colored) — a lightweight
/// secondary button.
#[component]
fn PrefValue(state: Signal<EditorState>, text: String, cmd: Cmd, t: &'static Theme) -> Element {
    rsx! {
        Btn {
            label: text,
            kind: BtnKind::Secondary,
            t,
            on_click: move |evt: MouseEvent| {
                evt.stop_propagation();
                evt.prevent_default();
                // stays open: a preferences panel is meant to be poked at
                actions::run(&mut state.write(), cmd);
            },
        }
    }
}

/// On / Off, the standard wording for toggles.
fn on_off(v: bool) -> Msg {
    if v {
        Msg::CommonOn
    } else {
        Msg::CommonOff
    }
}
