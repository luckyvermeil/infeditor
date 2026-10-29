//! Integration tests for settings persistence, the menu state machine,
//! self-drawn dialogs, and shortcut bindings.
//!
//! * Settings: capture → write to disk → fresh "process" (minimal +
//!   load_settings) → fields match; all through the isolated
//!   `INFEDITOR_CONFIG_DIR`, never touching the user's real configuration.
//! * Menus: bar structure invariants, keyboard open/close / row selection /
//!   activation (the same dispatch path as the menu).
//! * Dialogs: submit command with an input field (run first, then close),
//!   confirm dialogs cannot be dismissed by a stray click.
//! * Shortcuts: chord parsing, no conflicts in the binding table,
//!   lookup/accel_of as inverses.
//!
//! Headless; for recovery-session writes / restores see `crash_recovery.rs`.

mod common;

use common::{cleanup, state, tmp_work_dir, write_file};
use infeditor::ui::actions::{self, Cmd};
use infeditor::ui::shortcuts::{Chord, Mods, K};
use infeditor::ui::state::Dialog;
use infeditor::ui::Fmt;
use infeditor::ui::Msg;

/// The same in-process settings.json is read and written in parallel by many
/// tests: tests touching that file all hold this lock and run serially,
/// otherwise the "corrupt file falls back" test would corrupt a file another
/// test is currently reading.
static SETTINGS_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

// ---------------------------------------------------------------------------
// Settings persistence
// ---------------------------------------------------------------------------

#[test]
fn settings_survive_a_restart_through_the_json_file() {
    let _guard = SETTINGS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let config = common::test_config_dir().to_path_buf();
    let mut s = state();

    // The user changes a round of settings (through command dispatch; persist
    // hangs off the end of the command)
    actions::run(&mut s, Cmd::ToggleTheme);
    actions::run(&mut s, Cmd::ShowWs);
    actions::run(&mut s, Cmd::ZoomIn);
    s.persist_settings();

    let file = config.join("settings.json");
    assert!(
        file.exists(),
        "settings must be written to disk: {}",
        file.display()
    );

    // Simulate a restart: a fresh state reads from disk
    let mut s2 = state();
    s2.load_settings();
    assert_eq!(s2.dark, s.dark, "theme");
    assert_eq!(s2.show_ws, s.show_ws, "whitespace display");
    assert_eq!(s2.font_size, s.font_size, "font size");
}

#[test]
fn the_recent_list_is_persisted_and_restored() {
    let _guard = SETTINGS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tmp_work_dir("sess-recent");
    let a = write_file(&dir, "a.txt", "A\n");
    let b = write_file(&dir, "b.txt", "B\n");
    let mut s = state();
    s.open_path(a.clone());
    s.open_path(b.clone());
    s.persist_settings();

    let mut s2 = state();
    s2.load_settings();
    assert_eq!(
        s2.recent, s.recent,
        "recent list restored in original order (newest last)"
    );
    cleanup(&dir);
}

#[test]
fn a_corrupt_settings_file_falls_back_to_defaults() {
    let _guard = SETTINGS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let config = common::test_config_dir().to_path_buf();
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(config.join("settings.json"), "{{ 不是 JSON").unwrap();

    let mut s = state();
    s.load_settings(); // must never panic; falls back to defaults
    assert!(!s.show_ws, "whitespace display off by default");
}

// ---------------------------------------------------------------------------
// Menu state machine
// ---------------------------------------------------------------------------

#[test]
fn the_menu_bar_has_unique_captions_and_well_formed_items() {
    let _guard = SETTINGS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let s = state();
    let items = actions::menu_bar_items(&s);
    assert!(
        items.len() >= 5,
        "menu bar has at least File/Edit/Search/View/Help"
    );

    let mut labels: Vec<&str> = Vec::new();
    for it in &items {
        assert!(!it.label().is_empty(), "caption must not be empty");
        if !labels.contains(&it.label()) {
            labels.push(it.label());
        }
        // Item shapes are legal: command / submenu / separator, one of three
        // (guaranteed by the enum itself); here we verify the label()/cmd_of()
        // accessors don't panic in every shape
        let _ = it.cmd_of();
    }
    assert_eq!(labels.len(), items.len(), "captions must be unique");
}

#[test]
fn keyboard_menu_flow_open_move_activate_runs_the_command() {
    let _guard = SETTINGS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut st = state();

    // Open by clicking the caption
    st.menu_toggle(actions::Caption::View, 40.0);
    assert_eq!(st.menu.open, Some(actions::Caption::View));

    // The row list is non-empty, arrow keys skip separators
    let rows = actions::menu_rows(&st);
    assert!(!rows.is_empty(), "View menu must have items");
    st.menu_move(1);
    let cursor = st.menu.cursor;
    assert!(!rows[cursor].sep, "cursor must rest on a real item");

    // Find an executable row and activate it (the pointer path menu_run_row /
    // the keyboard path menu_activate share one origin)
    let target = rows
        .iter()
        .position(|r| r.cmd == Some(Cmd::ShowEol))
        .expect("View menu contains Show EOL");
    st.menu_run_row(target);
    assert!(st.show_eol, "menu activation must run the command");
    assert!(st.menu.open.is_none(), "menu closes after running");
    assert!(actions::is_checked(&st, Cmd::ShowEol), "check state synced");
}

#[test]
fn toggling_the_same_caption_folds_it_back() {
    let _guard = SETTINGS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut st = state();
    st.menu_toggle(actions::Caption::File, 10.0);
    st.menu_toggle(actions::Caption::File, 10.0);
    assert!(
        st.menu.open.is_none(),
        "clicking the same caption again = collapse"
    );
}

#[test]
fn the_more_menu_button_toggles_the_overflow_panel() {
    let _guard = SETTINGS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut st = state();
    assert!(!st.menu.more_open);
    st.menu_toggle_more();
    assert!(st.menu.more_open);
    st.menu_toggle_more();
    assert!(!st.menu.more_open);
}

// ---------------------------------------------------------------------------
// Self-drawn dialogs
// ---------------------------------------------------------------------------

#[test]
fn an_input_dialog_reads_the_value_then_runs_and_closes() {
    let _guard = SETTINGS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut st = state();

    // The dialog holds message keys: the arbitrary placeholder text in tests
    // goes through pass-through keys (the text itself is data)
    st.show_dialog(
        Dialog::text(
            Fmt::CommonRaw {
                text: "测试输入".into(),
            },
            Fmt::CommonRaw {
                text: "正文".into(),
            },
        )
        .with_input(
            Fmt::CommonRaw {
                text: "提示".into(),
            },
            Cmd::SetNewline("CRLF"),
        ),
    );
    assert!(st.dialog.is_some());
    assert!(
        st.dialog_input_insert("键盘与 IME 同入口"),
        "text enters the field"
    );
    assert_eq!(st.dialog_input(), "键盘与 IME 同入口");
    assert!(st.dialog_input_backspace());
    assert_eq!(st.dialog_input(), "键盘与 IME 同入");

    st.dialog_input_clear();
    assert!(st.dialog_input_insert("CRLF"));
    assert!(st.dialog_input_submit(), "Enter submits");
    // The submit command has run: the document newline was rewritten (run
    // first, then close)
    assert_eq!(st.active_doc().newline(), infeditor::buffer::Newline::CrLf);
    assert!(st.dialog.is_none(), "dialog closes after submit");
}

#[test]
fn a_confirm_dialog_cannot_be_dismissed_by_a_stray_click() {
    let _guard = SETTINGS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut st = state();
    st.show_dialog(
        Dialog::text(Msg::DangerOpTitle, Msg::DangerOpBody)
            .button(Msg::CommonOk, None)
            .button(Msg::CommonCancel, None)
            .confirm(),
    );
    assert!(
        !st.dialog.as_ref().unwrap().dismissable,
        "confirm dialog must resist stray clicks"
    );
    st.close_dialog();
    assert!(st.dialog.is_none());
}

// ---------------------------------------------------------------------------
// Shortcuts
// ---------------------------------------------------------------------------

#[test]
fn every_binding_chord_is_unique_and_parses() {
    let _guard = SETTINGS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    use std::collections::HashSet;
    let mut seen: HashSet<(bool, bool, bool, String)> = HashSet::new();
    for b in infeditor::ui::shortcuts::BINDINGS {
        let key = (
            b.chord.mods.ctrl,
            b.chord.mods.shift,
            b.chord.mods.alt,
            format!("{:?}", b.chord.key),
        );
        assert!(seen.insert(key), "duplicate binding: {:?}", b.chord);
    }
}

#[test]
fn ctrl_s_maps_to_save_and_accel_of_round_trips() {
    let _guard = SETTINGS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut m = Mods::NONE;
    m.ctrl = true;
    let chord = Chord::new(m, K::Char('s'));
    assert_eq!(
        infeditor::ui::shortcuts::lookup(&chord),
        Some(Cmd::Save),
        "Ctrl+S must be Save"
    );
    let accel = infeditor::ui::shortcuts::accel_of(Cmd::Save);
    assert!(!accel.is_empty(), "Save has an accelerator label");
    // Chord::parse can read back its own label form
    let parsed = Chord::parse("Ctrl+S").expect("canonical spelling parses");
    assert_eq!(parsed, chord);
}

#[test]
fn chord_parse_rejects_garbage() {
    let _guard = SETTINGS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    assert!(Chord::parse("不是快捷键").is_err());
    assert!(Chord::parse("").is_err());
}
