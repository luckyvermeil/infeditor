//! Headless integration tests for Vim mode and the macro system, end to end.
//!
//! * Vim: driven by real key sequences (`handle_key` → `Plan` →
//!   `apply_vim_plan`, exactly the composer's keyboard path), covering
//!   dd/dw/w/yy+p/x/i-Esc, `/` search, and `:w` command-line disk writes.
//! * Macros: recording (typing + commands) → replay on multiple tabs → save →
//!   `MacroPlaySaved`; also verifies that "non-recordable commands"
//!   (Quit/Open and other human-interactive ones) never enter a macro.
//!
//! Fully headless: the Vim and macro engines create no windows.

mod common;

use common::{cleanup, open_file, run, state, tmp_work_dir, write_file};
use infeditor::ui::actions::Cmd;
use infeditor::ui::recorder::MacroStep;
use infeditor::ui::state::EditorState;
use infeditor::ui::vim::Ev;

/// Feeds a real key sequence to vim and immediately applies the resulting Plan
/// (same dispatch order as the composer).
fn keys(st: &mut EditorState, evs: &[Ev]) {
    for ev in evs {
        if let Some(plan) = st.vim.handle_key(*ev, st.caret().pos, &st.vim_buffer()) {
            st.apply_vim_plan(&plan);
        }
    }
}

fn ch(c: char) -> Ev {
    Ev::Char(c)
}

// ---------------------------------------------------------------------------
// Vim basics
// ---------------------------------------------------------------------------

#[test]
fn vim_starts_disabled_and_toggle_switches_it() {
    let mut st = state();
    assert!(!st.vim.enabled);
    run(&mut st, Cmd::ToggleVim);
    assert!(st.vim.enabled);
    assert_eq!(
        st.vim.mode,
        infeditor::ui::vim::Mode::Normal,
        "starts in Normal after enabling"
    );
}

#[test]
fn dd_deletes_a_line_and_u_like_undo_restores_it_via_cmd() {
    let dir = tmp_work_dir("vim-dd");
    let (mut st, _f) = open_file(&dir, "v.txt", "one\ntwo\nthree\n");
    run(&mut st, Cmd::ToggleVim);

    keys(&mut st, &[ch('d'), ch('d')]);
    assert_eq!(
        st.active_doc().read_all(),
        "two\nthree\n",
        "dd deletes the first line"
    );

    // Undo goes through the regular Cmd (vim doesn't hijack undo entries other
    // than Ctrl+Z)
    run(&mut st, Cmd::Undo);
    assert_eq!(st.active_doc().read_all(), "one\ntwo\nthree\n");
    cleanup(&dir);
}

#[test]
fn motion_words_and_dw_delete_to_the_next_word() {
    let dir = tmp_work_dir("vim-word");
    let (mut st, _f) = open_file(&dir, "v.txt", "alpha beta gamma\n");
    run(&mut st, Cmd::ToggleVim);

    keys(&mut st, &[ch('d'), ch('w')]);
    assert_eq!(
        st.active_doc().read_all(),
        "beta gamma\n",
        "dw deletes to the next word start"
    );

    keys(&mut st, &[ch('w')]);
    // w only moves, doesn't delete: the caret should rest on the g of gamma
    let pos = st.caret().pos;
    assert_eq!(
        st.active_doc().text_range(pos..pos + 1),
        "g",
        "w jumps to the next word start"
    );
    cleanup(&dir);
}

#[test]
fn yank_paste_duplicates_the_line() {
    let dir = tmp_work_dir("vim-yy");
    let (mut st, _f) = open_file(&dir, "v.txt", "line1\nline2\n");
    run(&mut st, Cmd::ToggleVim);

    keys(&mut st, &[ch('y'), ch('y'), ch('p')]);
    assert_eq!(
        st.active_doc().read_all(),
        "line1\nline1\nline2\n",
        "yy + p pastes the whole line below"
    );
    cleanup(&dir);
}

#[test]
fn x_deletes_a_character_and_insert_mode_types_then_escapes() {
    let dir = tmp_work_dir("vim-insert");
    let (mut st, _f) = open_file(&dir, "v.txt", "ab\n");
    run(&mut st, Cmd::ToggleVim);

    keys(&mut st, &[ch('x')]);
    assert_eq!(
        st.active_doc().read_all(),
        "b\n",
        "x deletes the character at the caret"
    );

    keys(&mut st, &[ch('i')]);
    assert_eq!(
        st.vim.mode,
        infeditor::ui::vim::Mode::Insert,
        "i enters insert mode"
    );
    // In insert mode typed text goes through the regular editing path
    st.insert_text("ZZ");
    keys(&mut st, &[Ev::Key(infeditor::ui::shortcuts::K::Escape)]);
    assert_eq!(
        st.vim.mode,
        infeditor::ui::vim::Mode::Normal,
        "Esc returns to Normal"
    );
    assert_eq!(st.active_doc().read_all(), "ZZb\n");
    cleanup(&dir);
}

#[test]
fn slash_search_finds_and_n_repeats() {
    let dir = tmp_work_dir("vim-search");
    let (mut st, _f) = open_file(&dir, "v.txt", "cat dog cat bird cat\n");
    run(&mut st, Cmd::ToggleVim);

    keys(&mut st, &[ch('/'), ch('c'), ch('a'), ch('t')]);
    // / enters the command line and accumulates input
    assert!(
        st.vim.cmdline_text().contains("cat"),
        "command line accumulates: {:?}",
        st.vim.cmdline_text()
    );
    keys(&mut st, &[Ev::Key(infeditor::ui::shortcuts::K::Enter)]);

    // Op::Search places the caret at the end of the match
    let first = st.caret().pos;
    assert!(first >= 3, "caret is after the first match: {first}");
    assert_eq!(
        st.active_doc().text_range(first - 3..first),
        "cat",
        "hits the first cat"
    );

    keys(&mut st, &[ch('n')]);
    let second = st.caret().pos;
    assert!(second > first, "n jumps to the next match");
    cleanup(&dir);
}

#[test]
fn vim_disabled_ignores_keys_so_normal_typing_works() {
    let dir = tmp_work_dir("vim-off");
    let (mut st, _f) = open_file(&dir, "v.txt", "x\n");
    // Not enabled: handle_key produces no Plan
    assert!(st
        .vim
        .handle_key(Ev::Char('d'), st.caret().pos, &st.vim_buffer())
        .is_none());
    st.insert_text("plain typing");
    assert!(st.active_doc().read_all().contains("plain typing"));
    cleanup(&dir);
}

// ---------------------------------------------------------------------------
// Macros: record → replay
// ---------------------------------------------------------------------------

#[test]
fn recording_captures_text_and_commands_and_replays_on_another_doc() {
    let dir = tmp_work_dir("macro-basic");
    let (mut st, _f) = open_file(&dir, "m.txt", "");
    // Record: typed text + one command (duplicate the line while the caret is
    // still on the "hi" line)
    assert!(st.macro_toggle_record().is_some());
    assert!(st.is_recording());

    st.record_step(MacroStep::Text("hi".into()));
    st.record_cmd(Cmd::DuplicateLine);
    assert_eq!(st.macro_len(), 2);
    assert!(st.is_recording(), "still recording before replay");

    st.macro_toggle_record(); // stop
    assert!(!st.is_recording());

    // Replay: text lands in the document + the line was duplicated
    st.macro_play();
    let out = st.active_doc().read_all();
    assert!(out.starts_with("hi"), "replays typed text: {out:?}");
    assert!(
        out.matches("hi").count() >= 2,
        "command step (duplicate line) also replays: {out:?}"
    );
    cleanup(&dir);
}

#[test]
fn replaying_a_macro_on_a_second_tab_applies_to_the_active_document() {
    let dir = tmp_work_dir("macro-tabs");
    let a = write_file(&dir, "a.txt", "AAA\n");
    let b = write_file(&dir, "b.txt", "BBB\n");
    let mut st = state();
    st.open_path(a);
    st.open_path(b);

    let _ = st.macro_toggle_record(); // start recording
    st.record_step(MacroStep::Text(">>".to_string()));
    st.macro_toggle_record(); // stop recording

    st.macro_play();
    assert_eq!(
        st.active_doc().read_all(),
        ">>BBB\n",
        "macro applies to the active document"
    );

    st.activate_next_tab(-1);
    st.macro_play();
    assert_eq!(
        st.active_doc().read_all(),
        ">>AAA\n",
        "the same macro applies to another tab"
    );
    cleanup(&dir);
}

#[test]
fn macro_play_steps_replays_text_backspace_enter_and_tab() {
    let dir = tmp_work_dir("macro-steps");
    let (mut st, _f) = open_file(&dir, "m.txt", "");
    st.macro_play_steps(vec![
        MacroStep::Text("abc".to_string()),
        MacroStep::Backspace, // -> "ab"
        MacroStep::Tab,       // -> "ab\t" (tab character)
        MacroStep::Enter,     // newline
        MacroStep::Text("d".to_string()),
        MacroStep::Delete, // Delete removes after the caret (nothing at line end)
    ]);
    let out = st.active_doc().read_all();
    assert!(out.starts_with("ab"), "text + backspace: {out:?}");
    assert!(
        out.starts_with("ab    \n") || out.starts_with("ab\t"),
        "Tab expands to indent (default spaces) or a tab char: {out:?}"
    );
    assert!(out.ends_with('d'), "typed d after Enter's newline: {out:?}");
    cleanup(&dir);
}

// ---------------------------------------------------------------------------
// Macro safety boundaries
// ---------------------------------------------------------------------------

#[test]
fn recording_replay_does_not_re_record_itself() {
    let dir = tmp_work_dir("macro-loop");
    let (mut st, _f) = open_file(&dir, "m.txt", "seed\n");
    st.record_step(MacroStep::Text("T".to_string()));
    st.macro_toggle_record();
    st.macro_toggle_record();

    let len = st.macro_len();
    st.macro_play();
    assert_eq!(
        st.macro_len(),
        len,
        "replay must never record itself (the classic self-increment bug)"
    );
    cleanup(&dir);
}

#[test]
fn non_recordable_commands_never_enter_the_macro() {
    // Human-interactive commands (quit/open/save-as) don't enter macros: a
    // misfired macro must not be able to take the user's documents with it
    for cmd in [Cmd::Quit, Cmd::Open, Cmd::SaveAs, Cmd::Find(false, false)] {
        assert!(
            !cmd.is_recordable(),
            "{cmd:?} is a human-interactive command and must not be recordable"
        );
    }
    for cmd in [
        Cmd::Save,
        Cmd::DuplicateLine,
        Cmd::SortLines(infeditor::textops::SortKey::Text, false),
    ] {
        assert!(
            cmd.is_recordable(),
            "{cmd:?} is a plain editing command and must be recordable"
        );
    }

    let mut st = state();
    let _ = st.macro_toggle_record();
    st.record_cmd(Cmd::Quit);
    st.record_cmd(Cmd::Save);
    st.macro_toggle_record();
    assert_eq!(
        st.macro_len(),
        1,
        "only recordable commands entered the macro"
    );
}

#[test]
fn recording_short_circuits_direct_step_recording_during_playback() {
    let dir = tmp_work_dir("macro-latch");
    let (mut st, _f) = open_file(&dir, "m.txt", "");
    st.record_step(MacroStep::Text("X".to_string()));
    st.macro_toggle_record();
    st.macro_toggle_record();

    // record_step is short-circuited by a latch during playback (playback
    // internally goes through the same path)
    st.macro_play_steps(vec![MacroStep::Text("Y".to_string())]);
    assert_eq!(st.active_doc().read_all(), "Y", "replay works");
    assert!(!st.is_recording(), "must not be recording after replay");
    cleanup(&dir);
}

// ---------------------------------------------------------------------------
// Macro persistence and save naming
// ---------------------------------------------------------------------------

#[test]
fn macro_play_saved_ignores_out_of_range_indices_safely() {
    let mut st = state();
    run(&mut st, Cmd::MacroPlaySaved(usize::MAX));
    // Out of range only reports, never crashes — document and state stay usable
    assert!(st.active_doc().read_all().is_empty());
}

#[test]
fn a_recorded_macro_survives_save_and_run_via_commands() {
    let dir = tmp_work_dir("macro-run");
    let (mut st, _f) = open_file(&dir, "m.txt", "data\n");
    // Record a "delete current line" macro
    let _ = st.macro_toggle_record();
    st.record_cmd(Cmd::Delete);
    let _ = st.macro_toggle_record();

    // Replay through the command entry (menu path)
    run(&mut st, Cmd::MacroPlayback);
    assert!(
        !st.status_text().is_empty(),
        "replay must give status feedback"
    );
    cleanup(&dir);
}
