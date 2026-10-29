//! Headless integration tests for the full tab/document management pipeline.
//!
//! The same path as real interaction: tab switching, the recent-files menu,
//! reload (the background worker is pumped to completion via `poll_opens`),
//! external modification detection, hidden lines, read-only toggling, and the
//! two-stage confirmation of quit/close-all.
//!
//! Throughout, `EditorState::minimal()` plus the `tests/common` temp directory
//! isolation are used.

mod common;

use common::{cleanup, open, run, state, tmp_work_dir, write_file};
use infeditor::ui::actions::Cmd;
use infeditor::ui::state::EditorState;
use std::thread;
use std::time::Duration;

/// Pumps the background open/reload worker to quiescence (the headless
/// equivalent of waiting for UI callbacks).
fn pump(st: &mut EditorState) {
    for _ in 0..200 {
        if !st.poll_opens() {
            break;
        }
    }
}

fn names(st: &EditorState) -> Vec<String> {
    st.tabs
        .iter()
        .map(|t| t.doc.meta.display_name.clone())
        .collect()
}

// ---------------------------------------------------------------------------
// Tab switching
// ---------------------------------------------------------------------------

#[test]
fn tab_activation_cycles_forward_and_backwards_with_wrap() {
    let dir = tmp_work_dir("tabs-cycle");
    let a = write_file(&dir, "a.txt", "A\n");
    let b = write_file(&dir, "b.txt", "B\n");
    let c = write_file(&dir, "c.txt", "C\n");
    let mut st = state();
    for f in [&a, &b, &c] {
        open(&mut st, f);
    }
    assert_eq!(st.active, 2, "the last opened gets focus");

    st.activate_next_tab(1);
    assert_eq!(st.active, 0, "wrap forward: 2+1 → 0");
    st.activate_next_tab(-1);
    assert_eq!(st.active, 2, "wrap backwards: 0-1 → 2");
    st.activate_next_tab(-1);
    assert_eq!(st.active, 1);

    // The Cmd entry shares one path with the menu
    run(&mut st, Cmd::TabNext);
    assert_eq!(st.active, 2);
    run(&mut st, Cmd::TabPrev);
    assert_eq!(st.active, 1);
    cleanup(&dir);
}

#[test]
fn the_active_tab_survives_closing_another_tab() {
    let dir = tmp_work_dir("tabs-close");
    let a = write_file(&dir, "a.txt", "A\n");
    let b = write_file(&dir, "b.txt", "B\n");
    let mut st = state();
    open(&mut st, &a);
    open(&mut st, &b);
    st.activate_next_tab(-1); // activate a (0)
    assert_eq!(st.active, 0);

    // Close b (a clean document doesn't pop a dialog regardless of dirtiness;
    // close it directly)
    assert!(
        st.close_doc(1),
        "a clean document closes directly, no dialog"
    );
    assert_eq!(st.tabs.len(), 1);
    assert_eq!(st.active, 0, "focus stays on a");
    cleanup(&dir);
}

// ---------------------------------------------------------------------------
// Recent files
// ---------------------------------------------------------------------------

#[test]
fn recent_files_dedup_move_to_front_and_reopen() {
    let dir = tmp_work_dir("recent");
    let a = write_file(&dir, "a.txt", "AAA\n");
    let b = write_file(&dir, "b.txt", "BBB\n");
    let mut st = state();
    open(&mut st, &a);
    open(&mut st, &b);
    // open_path pushes to recent internally: the list is "newest at the back"
    assert_eq!(
        st.recent.back().map(String::as_str),
        Some(b.to_string_lossy().as_ref())
    );

    // Manual entry: dedup + move to the newest end
    st.push_recent(a.to_string_lossy().into_owned());
    st.push_recent(b.to_string_lossy().into_owned());
    let recents: Vec<&String> = st.recent.iter().collect();
    assert_eq!(recents.len(), 2, "deduped");
    assert_eq!(
        recents[1],
        &b.to_string_lossy().to_string(),
        "newest at the back"
    );
    assert_eq!(recents[0], &a.to_string_lossy().to_string());

    // Cmd::OpenRecent takes the same path as the menu (index 0 = list head)
    run(&mut st, Cmd::OpenRecent(0));
    pump(&mut st);
    assert_eq!(
        st.active_doc().read_all(),
        "AAA\n",
        "a.txt at the head of the recent list is reopened"
    );
    cleanup(&dir);
}

#[test]
fn opening_a_missing_recent_reports_without_disturbing_tabs() {
    let dir = tmp_work_dir("recent-missing");
    let mut st = state();
    st.recent.push_back("/ surely / missing / x.txt".into());
    let before = st.tabs.len();
    run(&mut st, Cmd::OpenRecent(0));
    pump(&mut st);
    assert_eq!(
        st.tabs.len(),
        before,
        "a failed open must not add or remove tabs"
    );
    cleanup(&dir);
}

// ---------------------------------------------------------------------------
// Reload
// ---------------------------------------------------------------------------

#[test]
fn reload_discards_memory_edits_and_takes_the_disk_version() {
    let dir = tmp_work_dir("reload");
    let f = write_file(&dir, "r.txt", "磁盘 v1\n");
    let mut st = state();
    open(&mut st, &f);
    st.insert_text("内存里的编辑");
    assert!(st.active_doc().is_dirty());

    run(&mut st, Cmd::Reload);
    // Dirty documents confirm first (two-stage: silently dropping in-memory
    // edits would be data loss)
    assert!(
        st.dialog.is_some(),
        "reloading a dirty document must confirm first"
    );
    run(&mut st, Cmd::ReloadConfirm);
    pump(&mut st);
    assert_eq!(
        st.active_doc().read_all(),
        "磁盘 v1\n",
        "back to the disk version after reload"
    );
    assert!(
        !st.active_doc().is_dirty(),
        "document is clean after reload"
    );
    cleanup(&dir);
}

#[test]
fn reload_an_unsaved_document_is_refused_with_a_hint() {
    let mut st = state();
    let before = st.active_doc().read_all();
    run(&mut st, Cmd::Reload);
    assert!(
        st.status_text().contains("尚未保存"),
        "path-less document must hint: {}",
        st.status_text()
    );
    assert_eq!(st.active_doc().read_all(), before, "content untouched");
}

// ---------------------------------------------------------------------------
// External modification detection
// ---------------------------------------------------------------------------

#[test]
fn external_modification_is_detected_and_only_hinted() {
    let dir = tmp_work_dir("external");
    let f = write_file(&dir, "watch.txt", "v1\n");
    let mut st = state();
    open(&mut st, &f);
    st.note_mtime();

    // An external program rewrites the file (mtime granularity is enough to
    // distinguish; the sleep ensures the timestamp changes and bypasses the
    // 1s throttle)
    thread::sleep(Duration::from_millis(1100));
    std::fs::write(&f, "被外部程序改写\n").unwrap();
    st.poll_external_change();

    assert!(
        st.status_text().contains("外部程序修改"),
        "must hint: {}",
        st.status_text()
    );
    assert!(
        st.active_doc().read_all().starts_with("v1"),
        "hint only, no action: in-memory content keeps the version the user sees"
    );
    assert!(!st.active_doc().is_dirty());
    cleanup(&dir);
}

// ---------------------------------------------------------------------------
// Hidden lines
// ---------------------------------------------------------------------------

#[test]
fn hiding_and_showing_lines_round_trips() {
    let dir = tmp_work_dir("hidden");
    let f = write_file(&dir, "h.txt", "l0\nl1\nl2\nl3\n");
    let mut st = state();
    open(&mut st, &f);

    // Select l1..l2 → right-click "hide lines"
    st.set_caret(3, false); // end of l0
    st.set_caret(9, true); // end of l2
    st.hide_lines();
    assert_eq!(st.hidden_count(), 2);
    assert!(st.line_hidden(1) && st.line_hidden(2));
    assert!(!st.line_hidden(0) && !st.line_hidden(3));

    // Right-click "show all hidden lines"
    st.show_hidden_lines();
    assert_eq!(st.hidden_count(), 0);
    assert!(!st.line_hidden(1));
    cleanup(&dir);
}

#[test]
fn hiding_twice_and_showing_without_hidden_reports_politely() {
    let dir = tmp_work_dir("hidden2");
    let f = write_file(&dir, "h.txt", "only\n");
    let mut st = state();
    open(&mut st, &f);

    st.set_caret(0, false);
    st.hide_lines();
    st.hide_lines();
    assert!(
        st.status_text().contains("已经隐藏"),
        "repeated hide must hint: {}",
        st.status_text()
    );

    st.show_hidden_lines();
    st.show_hidden_lines();
    assert!(
        st.status_text().contains("没有隐藏"),
        "nothing to show must hint: {}",
        st.status_text()
    );
    cleanup(&dir);
}

// ---------------------------------------------------------------------------
// Read-only (command-level entry) and quit
// ---------------------------------------------------------------------------

#[test]
fn toggle_read_only_command_gates_all_editing() {
    let dir = tmp_work_dir("ro-cmd");
    let f = write_file(&dir, "ro.txt", "冻结\n");
    let mut st = state();
    open(&mut st, &f);

    run(&mut st, Cmd::ToggleReadOnly);
    assert!(st.read_only, "command turns on read-only");
    st.insert_text("x");
    assert_eq!(st.active_doc().read_all(), "冻结\n");

    run(&mut st, Cmd::ToggleReadOnly);
    assert!(!st.read_only, "toggling again releases read-only");
    st.insert_text("x");
    assert_eq!(st.active_doc().read_all(), "x冻结\n");
    cleanup(&dir);
}

#[test]
fn quit_with_clean_documents_exits_immediately() {
    let mut st = state();
    run(&mut st, Cmd::Quit);
    assert!(st.quit_requested, "no dirty documents: quit immediately");
    assert!(st.dialog.is_none());
}

#[test]
fn quit_with_dirty_documents_asks_first_and_force_quit_is_recorded() {
    let dir = tmp_work_dir("quit-dirty");
    let f = write_file(&dir, "q.txt", "x\n");
    let mut st = state();
    open(&mut st, &f);
    st.insert_text("脏");

    run(&mut st, Cmd::Quit);
    assert!(!st.quit_requested, "dirty documents: ask first");
    assert!(st.dialog.is_some(), "custom quit confirmation dialog pops");
    assert_eq!(
        st.dialog.as_ref().unwrap().title.key(),
        "Plain:DlgQuitTitle",
        "the dialog must state the risk"
    );

    // The user clicks "Quit" (the ForceQuit button)
    st.close_dialog();
    run(&mut st, Cmd::ForceQuit);
    assert!(
        st.quit_requested,
        "force quit recorded, the window layer performs the close"
    );
    cleanup(&dir);
}

// ---------------------------------------------------------------------------
// Close all
// ---------------------------------------------------------------------------

#[test]
fn close_all_with_clean_documents_empties_the_tab_strip() {
    let dir = tmp_work_dir("close-all");
    let a = write_file(&dir, "a.txt", "A\n");
    let b = write_file(&dir, "b.txt", "B\n");
    let mut st = state();
    open(&mut st, &a);
    open(&mut st, &b);

    run(&mut st, Cmd::CloseAll);
    assert_eq!(
        st.tabs.len(),
        1,
        "clean documents all close directly, one blank placeholder remains"
    );
    assert!(
        st.tabs[0].doc.meta.path.is_none(),
        "the placeholder is an unnamed blank"
    );
    assert!(!st.close_all_pending);
    cleanup(&dir);
}

#[test]
fn close_all_with_a_dirty_document_prompts_then_continues_the_sweep() {
    let dir = tmp_work_dir("close-all-dirty");
    let a = write_file(&dir, "a.txt", "A\n");
    let b = write_file(&dir, "b.txt", "B\n");
    let mut st = state();
    open(&mut st, &a);
    open(&mut st, &b);
    st.activate_next_tab(-1); // make a dirty
    st.insert_text("脏");

    run(&mut st, Cmd::CloseAll);
    assert!(
        st.dialog.is_some(),
        "dirty document hit → suspend and await confirmation"
    );
    assert!(
        st.close_all_pending,
        "after confirmation the sweep continues over remaining tabs"
    );

    // Choosing "don't save" → the sweep continues and closes b too
    st.finish_close(Some(false));
    assert!(
        !st.close_all_pending,
        "pending flag cleared once the sweep finishes"
    );
    assert_eq!(
        st.tabs.len(),
        1,
        "a discarded, b closed with the sweep, blank placeholder remains"
    );
    assert!(
        st.tabs[0].doc.meta.path.is_none(),
        "the placeholder is an unnamed blank"
    );
    assert_eq!(
        std::fs::read_to_string(&a).unwrap(),
        "A\n",
        "choosing don't save → a's disk content untouched"
    );
    cleanup(&dir);
}

// ---------------------------------------------------------------------------
// Paths and languages
// ---------------------------------------------------------------------------

/// Windows file systems are case-insensitive (`platform::paths_equal`
/// lowercases and normalizes separators); POSIX is case-sensitive, so opening
/// `CASE.txt` when `Case.txt` exists is a *different* file there. The scenario
/// only exists on Windows.
#[cfg(windows)]
#[test]
fn same_path_is_case_insensitive_on_windows() {
    let dir = tmp_work_dir("samepath");
    let f = write_file(&dir, "Case.txt", "x\n");
    // Windows file systems are case-insensitive: re-opening the uppercase
    // variant must focus instead of duplicating the tab
    let upper = f.with_file_name("CASE.txt");
    let mut st = state();
    open(&mut st, &f);
    let n = st.tabs.len();
    st.open_path(upper);
    assert_eq!(
        st.tabs.len(),
        n,
        "the same path (case variant) must not open a second tab"
    );
    cleanup(&dir);
}

#[test]
fn language_redetection_follows_the_extension_per_tab() {
    let dir = tmp_work_dir("langs");
    let rs = write_file(&dir, "code.rs", "fn main() {}\n");
    let py = write_file(&dir, "script.py", "print('hi')\n");
    let mut st = state();
    open(&mut st, &rs);
    assert!(st.spec().name.to_lowercase().contains("rust"), "rs → Rust");
    open(&mut st, &py);
    assert!(
        st.spec().name.to_lowercase().contains("python"),
        "py → Python"
    );
    // Switch back to the first tab: the language follows the tab
    st.activate_next_tab(-1);
    assert!(st.spec().name.to_lowercase().contains("rust"));
    cleanup(&dir);
}

#[test]
fn tab_strip_names_follow_the_document() {
    let dir = tmp_work_dir("tab-names");
    let a = write_file(&dir, "name-a.txt", "A\n");
    let mut st = state();
    open(&mut st, &a);
    assert_eq!(names(&st), vec!["name-a.txt".to_string()]);
    cleanup(&dir);
}
