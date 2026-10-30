//! Status bar: position, selection, encoding, newline style, file type, messages.
//!
//! Vim badge: parked on the right of the status bar while Vim mode is on (accent background,
//! impossible to miss). Clicking it pops a small menu offering "Turn off Vim mode" — rather than
//! toggling silently on one click: a stray tap must not disable an editing mode the user chose.

use dioxus_native::prelude::*;

use crate::io::encoding;
use crate::ui::actions::Cmd;
use crate::ui::state::{EditorState, STATUS_H};
use crate::ui::{Fmt, Msg};

#[component]
pub fn StatusBar() -> Element {
    let mut state = use_context::<Signal<EditorState>>();
    let mut vim_menu = use_signal(|| false);
    let (
        sel,
        pos,
        total_bytes,
        enc,
        nl,
        ftype,
        name,
        dirty,
        msg,
        top,
        theme,
        recording,
        read_only,
        loading,
        vim_on,
        indexing,
        lines_estimated,
        loc,
        line_col,
        sel_text,
        lines_text,
        recording_text,
    ) = {
        let st = state.read();
        let (line, col, sel) = st.position_info();
        let doc = st.active_doc();
        let enc = encoding::display_name(doc.meta.encoding.unwrap_or(encoding_rs::UTF_8));
        let nl = doc
            .meta
            .user_newline
            .unwrap_or(doc.meta.detected_newline)
            .display()
            .to_string();
        let pos = st.caret().pos;
        // Rendered once here: reading the state again inside rsx takes another
        // guard and puts calls in the per-frame redraw path. `into_owned` because
        // dioxus text nodes take `String`, not `Cow` — a dozen strings per frame
        // is nothing. The zero-allocation claim is about the menu tree, which
        // keeps `Cow` inside `Item`.
        let loc = st.locale();
        let total_lines = doc.line_count();
        let n_lines = total_lines as usize;
        let line_col = loc
            .tr(Fmt::StatusLineCol {
                line: line as usize,
                col: col as usize,
            })
            .into_owned();
        let sel_text = loc.tr(Fmt::StatusSelectedBytes { bytes: sel }).into_owned();
        let lines_text = if !doc.index_complete() {
            if st.is_indexing() {
                loc.tr(Fmt::StatusLinesCounting { lines: n_lines })
            } else {
                loc.tr(Fmt::StatusLinesEstimate { lines: n_lines })
            }
        } else {
            loc.tr(Fmt::StatusLines { lines: n_lines })
        }
        .into_owned();
        let steps = st.macro_len();
        let recording_text = loc.tr(Fmt::StatusRecording { steps }).into_owned();
        (
            sel,
            pos,
            doc.table.len(),
            enc.to_string(),
            nl,
            crate::ui::lang_label(loc, &doc.meta.file_type).into_owned(),
            doc.meta.display_name.clone(),
            doc.is_dirty(),
            st.status
                .as_ref()
                .map(|m| st.tr(m.clone()).into_owned())
                .unwrap_or_default(),
            (st.win.h - STATUS_H).max(0.0),
            st.theme(),
            st.is_recording(),
            st.read_only,
            st.is_loading(),
            st.vim.enabled,
            st.is_indexing(),
            !doc.index_complete(),
            loc,
            line_col,
            sel_text,
            lines_text,
            recording_text,
        )
    };

    let pct = pos
        .saturating_mul(100)
        .checked_div(total_bytes)
        .unwrap_or(0);

    // State snapshot of the sidebar toggles (icons appear only when usable, the current panel highlighted):
    // Explorer needs a workspace; Outline opens only for Markdown (see `outline_supported`).
    let (has_ws, md, ws_on, outline_on) = {
        let st = state.read();
        let md = st.outline_supported();
        (
            st.ws_root.is_some(),
            md,
            st.ws_show && st.side_panel == crate::ui::state::SidePanel::Workspace,
            st.ws_show && st.side_panel == crate::ui::state::SidePanel::Outline && md,
        )
    };
    let ws_class = if ws_on { "sb-btn on" } else { "sb-btn" };
    let outline_class = if outline_on { "sb-btn on" } else { "sb-btn" };

    let bar_style = format!(
        "position:absolute; top:{top}px; left:0; right:0; height:{STATUS_H}px; z-index:50;
         background:{}; color:{}; display:flex;
         align-items:center; padding:0 10px; font-size:11px; gap:16px;
         user-select:none; overflow:hidden; touch-action:none;",
        theme.status_bg, theme.status_fg
    );
    // `touch-action:none`: same touch-tap-vs-pan trade as the title bar — the
    // status bar's clickable segments fire on `click`, and blitz suppresses
    // `click` once a touch drifts past 2px into its pan mode. Nothing here
    // scrolls, so forbidding pans is free.

    rsx! {
        div {
            role: "status",
            style: "{bar_style}",
            // Sidebar toggles (VS Code's Ctrl+B): the icons sit together at the status bar's
            // lower left. Explorer is always there (with no workspace, clicking it = open
            // folder); Outline only appears for Markdown documents. Icons are drawn with boxes
            // (no font-codepoint gambling); color follows `.sb-btn`'s color (:hover / .on).
            div {
                class: "{ws_class}",
                role: "button",
                title: if has_ws {
                    loc.tr(Msg::StatusExplorerTip).into_owned()
                } else {
                    loc.tr(Msg::StatusOpenFolderTip).into_owned()
                },
                onpointerdown: move |evt| {
                    // Like every other clickable segment of the status bar: stop the default action so the document does not lose focus
                    evt.stop_propagation();
                    evt.prevent_default();
                },
                onclick: move |evt| {
                    evt.stop_propagation();
                    evt.prevent_default();
                    // With no workspace, lighting it up = open folder (the status bar is the
                    // UI's edge, so native pickers pop there; `state` runs no blocking dialogs)
                    let toggled = state.write().sidebar_toggle();
                    if !toggled {
                        state.write().ws_open_folder_dialog();
                    }
                },
                span { class: "sb-folder",
                    span { class: "sb-folder-tab" }
                    span { class: "sb-folder-lid" }
                    span { class: "sb-folder-body" }
                }
            }
            if md {
                div {
                    class: "{outline_class}",
                    role: "button",
                    title: loc.tr(Msg::StatusOutlineTip).into_owned(),
                    onpointerdown: move |evt| {
                        evt.stop_propagation();
                        evt.prevent_default();
                    },
                    onclick: move |evt| {
                        evt.stop_propagation();
                        evt.prevent_default();
                        state.write().toggle_outline();
                    },
                    span { class: "sb-bars",
                        span { class: "sb-bar sb-bar1" }
                        span { class: "sb-bar sb-bar2" }
                        span { class: "sb-bar sb-bar3" }
                    }
                }
            }
            // human_size is defined below (outside the component, so unit tests can reach it)
            // Macro recording indicator (the permanent red dot in the status bar).
            // Placed first: every click during recording may be recorded into the macro, so the
            // indicator must be visible at a glance.
            if recording {
                span { style: "color:{theme.status_danger}; font-weight:600;", "\u{25CF}" }
                span { "{recording_text}" }
            }
            if read_only {
                // The read-only indicator is permanent: in read-only mode every stray touch must show its reason at once.
                span { style: "font-weight:600;", "{loc.tr(Msg::StatusReadOnly).into_owned()}" }
            }
            if loading {
                span { "{loc.tr(Msg::StatusOpening).into_owned()}" }
            }
            span { "{line_col}" }
            if sel > 0 {
                span { "{sel_text}" }
            }
            span { "{enc}" }
            span { "{nl}" }
            span { "{ftype}" }
            // Size in human units: the status bar is for readers, not for stat calls.
            span { "{human_size(total_bytes)}" }
            // While progressive indexing runs the line count is an estimate — the rule is "no
            // line number until the position lookup finishes", so show a ≈ prefix here and allow
            // a click to stop the counting.
            if lines_estimated {
                if indexing {
                    span {
                        style: "cursor:default; opacity:0.85;",
                        title: loc.tr(Msg::StatusStopCountingTip).into_owned(),
                        onclick: move |_| {
                            let _ = state.write().cancel_indexing();
                        },
                        "{lines_text}"
                    }
                } else {
                    span { "{lines_text}" }
                }
            } else {
                span { "{lines_text}" }
            }
            span { "{pct}%" }
            if dirty {
                // Same token as the tab/title-bar dirty dot (it once was a third color)
                span { style: "color:{theme.dirty_dot};", "{loc.tr(Msg::StatusModified).into_owned()}" }
            }
            span { style: "flex:1; overflow:hidden; text-align:right; white-space:nowrap;
                           text-overflow:ellipsis;",
                if msg.is_empty() { "{name}" } else { "{msg}" }
            }
            if vim_on {
                VimBadge { state, open: vim_menu, theme }
            }
        }
        if vim_on && vim_menu() {
            // Click any blank space to close the menu (same click-away scheme as the menubar dropdowns).
            div {
                style: "position:absolute; left:0; right:0; top:0; bottom:0; z-index:80;
                        cursor:default;",
                onpointerdown: move |evt| {
                    evt.stop_propagation();
                    evt.prevent_default();
                    vim_menu.set(false);
                },
            }
            VimMenu { state, open: vim_menu, theme }
        }
    }
}

/// The Vim badge on the right of the status bar: accent background + white text, always visible;
/// clicking pops the menu that turns it off.
#[component]
fn VimBadge(
    state: Signal<EditorState>,
    mut open: Signal<bool>,
    theme: &'static crate::highlight::Theme,
) -> Element {
    // Tri-state in the `.vbadge` / `.vbadge.open` classes (components::style)
    let style = format!(
        "font-weight:600; padding:1px 8px;
         border-radius:3px; letter-spacing:0.5px; cursor:default;
         border:1px solid {border};",
        border = if theme.dark {
            "rgba(255,255,255,0.35)"
        } else {
            theme.input_border
        }
    );
    rsx! {
        span {
            class: if open() { "vbadge open" } else { "vbadge" },
            style: "{style}",
            onpointerdown: move |evt| {
                // A status-bar click would otherwise take the document-blur path; the badge takes over.
                evt.stop_propagation();
                evt.prevent_default();
            },
            onclick: move |_| open.set(!open()),
            "\u{2328} VIM"
        }
    }
}

/// The menu popped by the badge: its one and most important entry is "Turn off Vim mode".
#[component]
fn VimMenu(
    state: Signal<EditorState>,
    mut open: Signal<bool>,
    theme: &'static crate::highlight::Theme,
) -> Element {
    let style = format!(
        "position:absolute; right:8px; bottom:{}px; z-index:90; min-width:150px;
         background:{}; color:{}; border:1px solid {}; padding:4px 0;
         font-size:12px; cursor:default; user-select:none;",
        STATUS_H + 4.0,
        theme.menu_bg,
        theme.menu_fg,
        theme.menu_border
    );
    rsx! {
        div {
            style: "{style}",
            onpointerdown: move |evt| {
                evt.stop_propagation();
                evt.prevent_default();
            },
            div {
                class: "mrow",
                style: "padding:5px 14px;",
                onclick: move |_| {
                    open.set(false);
                    crate::ui::actions::run(&mut state.write(), Cmd::ToggleVim);
                },
                "{state.read().tr(Msg::ViewVimOff).into_owned()}"
            }
            div { style: "padding:3px 14px 1px; font-size:10px; opacity:0.6;",
                "{state.read().tr(Msg::VimModesHint).into_owned()}"
            }
        }
    }
}

/// Human-readable byte size (for the status bar).
///
/// Binary units (KB=1024), following editor-world convention; below 1 KiB give the byte count
/// directly — "512 B" reads better than "0.5 KB".
pub fn human_size(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;
    let b = bytes as f64;
    if b < KB {
        format!("{bytes} B")
    } else if b < MB {
        format!("{:.1} KB", b / KB)
    } else if b < GB {
        format!("{:.1} MB", b / MB)
    } else {
        format!("{:.2} GB", b / GB)
    }
}

#[cfg(test)]
mod status_tests {
    use super::human_size;
    #[test]
    fn human_size_uses_binary_units() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(1024), "1.0 KB");
        assert_eq!(human_size(256 * 1024 * 1024), "256.0 MB");
        assert_eq!(human_size(3 * 1024 * 1024 * 1024), "3.00 GB");
    }
}
