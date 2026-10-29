//! Headless integration tests for the full find/replace pipeline.
//!
//! The same path as real interaction: open the find bar → type (find_insert,
//! same entry as keyboard/IME) → incremental locating → option toggles →
//! navigate one by one → count → replace one/all → goto line → history.
//!
//! Without a bridge, `open_find` falls back to the inline bar
//! (`open_find_inline`) — no real window is opened; the background search path
//! for large documents is not covered here (unit tests and benches already
//! cover the search core).

mod common;

use common::{cleanup, open_file, tmp_work_dir};
use infeditor::search::SearchMode;
use infeditor::ui::state::EditorState;
use infeditor::ui::state::FindFocus;

/// Opens a temporary document and returns `(state, file path, its directory)`.
fn st_with(content: &str) -> (EditorState, std::path::PathBuf, std::path::PathBuf) {
    let dir = tmp_work_dir("find");
    let (st, f) = open_file(&dir, "doc.txt", content);
    (st, f, dir)
}

/// The current selection (find hits are presented as selections).
fn sel(st: &EditorState) -> Option<(u64, u64)> {
    st.sel_range().map(|r| (r.start, r.end))
}

fn text_at(st: &EditorState, span: (u64, u64)) -> String {
    st.active_doc().text_range(span.0..span.1)
}

// ---------------------------------------------------------------------------
// Open / type
// ---------------------------------------------------------------------------

#[test]
fn opening_find_picks_up_a_short_selection_as_the_pattern() {
    let (mut st, _f, dir) = st_with("needle in a haystack, needle again\n");
    // Select the second "needle" (a short selection becomes the find pattern directly)
    st.set_caret(22, false);
    st.set_caret(28, true);
    st.open_find(false, false);
    assert!(st.find.open, "find bar must open");
    assert_eq!(
        st.find.pattern, "needle",
        "short selection should prefill the find box"
    );
    assert_eq!(
        st.find.focus,
        FindFocus::Pattern,
        "focus defaults to the pattern field"
    );
    cleanup(&dir);
}

#[test]
fn typing_into_the_pattern_box_reveals_matches_incrementally() {
    let (mut st, _f, dir) = st_with("alpha beta gamma beta\n");
    st.open_find(false, false);
    assert!(st.find_insert("beta"), "typing must go into the find box");
    // Incremental find: the first beta is already selected
    let span = sel(&st).expect("first match should be selected after typing");
    assert_eq!(text_at(&st, span), "beta");
    assert_eq!(span.0, 6, "should hit the first beta after the caret (0)");
    cleanup(&dir);
}

#[test]
fn find_insert_ignores_newlines_and_routes_to_the_focused_field() {
    let (mut st, _f, dir) = st_with("x\n");
    st.open_find(true, false);
    st.set_find_focus(FindFocus::Replacement);
    assert!(st.find_insert("re\nplace"));
    assert_eq!(
        st.find.replacement, "replace",
        "the find box is single-line: newlines filtered"
    );
    cleanup(&dir);
}

// ---------------------------------------------------------------------------
// Navigation: next / prev / wrap
// ---------------------------------------------------------------------------

#[test]
fn find_next_walks_matches_forward_then_wraps() {
    let (mut st, _f, dir) = st_with("aXbXc\n");
    st.find.pattern = "X".into();
    st.open_find(false, false);

    st.find_next();
    assert_eq!(sel(&st), Some((1, 2)));
    st.find_next();
    assert_eq!(sel(&st), Some((3, 4)));
    st.find_next();
    assert_eq!(
        sel(&st),
        Some((1, 2)),
        "wrapped search: past the end back to the first"
    );
    cleanup(&dir);
}

#[test]
fn find_prev_walks_backwards_and_reports_the_top_without_wrap() {
    let (mut st, _f, dir) = st_with("aXbXc\n");
    st.find.pattern = "X".into();
    st.open_find(false, false);
    if !st.find.wrap {
        st.toggle_find_wrap(); // ensure wrap mode, walk two steps first
    }
    st.find_next();
    st.find_next(); // stops on the second X
    assert_eq!(sel(&st), Some((3, 4)));

    st.find_prev();
    assert_eq!(sel(&st), Some((1, 2)));

    // Turn wrapping off: hitting the top must report instead of wrapping around
    if st.find.wrap {
        st.toggle_find_wrap();
    }
    st.find_prev();
    assert_eq!(
        st.find_message_text(),
        "已到开头",
        "non-wrapped hit-the-top must state it clearly"
    );
    cleanup(&dir);
}

#[test]
fn find_direction_up_makes_find_next_search_backwards() {
    let (mut st, _f, dir) = st_with("aXbXc\n");
    st.set_caret(4, false); // start near the end
    st.find.pattern = "X".into();
    st.set_find_dir(true);
    st.find_next();
    let span = sel(&st).expect("upward search should hit");
    assert_eq!(text_at(&st, span), "X");
    assert_eq!(span.0, 3, "upward direction should hit the previous match");
    cleanup(&dir);
}

#[test]
fn find_count_reports_occurrences() {
    let (mut st, _f, dir) = st_with("one two one three one\n");
    st.find.pattern = "one".into();
    st.find_count();
    assert_eq!(
        st.find_message_text(),
        "3 处",
        "count: {}",
        st.find_message_text()
    );
    assert!(
        st.status_text().contains("3"),
        "status bar should also give the count: {}",
        st.status_text()
    );
    cleanup(&dir);
}

// ---------------------------------------------------------------------------
// Options: case / whole word / mode / selection
// ---------------------------------------------------------------------------

#[test]
fn case_sensitivity_toggle_changes_the_hit_set() {
    let (mut st, _f, dir) = st_with("Foo foo FOO fOo\n");
    st.find.pattern = "foo".into();
    st.find_count();
    assert_eq!(
        st.find_message_text(),
        "4 处",
        "case-insensitive by default"
    );

    if !st.find.case {
        st.toggle_find_case();
    }
    st.find_count();
    assert_eq!(
        st.find_message_text(),
        "1 处",
        "case-sensitive leaves only lowercase foo"
    );
    cleanup(&dir);
}

#[test]
fn whole_word_toggle_avoids_substring_hits() {
    let (mut st, _f, dir) = st_with("win window win\n");
    st.find.pattern = "win".into();
    st.find_count();
    assert_eq!(st.find_message_text(), "3 处", "substrings count too");
    if !st.find.word {
        st.toggle_find_word();
    }
    st.find_count();
    assert_eq!(
        st.find_message_text(),
        "2 处",
        "whole-word excludes the one inside window"
    );
    cleanup(&dir);
}

#[test]
fn regex_and_escape_modes_change_the_pattern_semantics() {
    // Regex: \d+ matches digit runs
    let (mut st, _f, dir) = st_with("id=42 id=7\n");
    st.set_find_mode(SearchMode::Regex);
    st.find.pattern = r"id=\d+".into();
    st.find_count();
    assert_eq!(st.find_message_text(), "2 处", "regex mode");
    cleanup(&dir);

    // Escapes: `\t` is interpreted as a literal tab
    let (mut st, _f, dir) = st_with("a\tb c\n");
    st.set_find_mode(SearchMode::Esc);
    st.find.pattern = r"a\tb".into();
    st.find_count();
    assert_eq!(
        st.find_message_text(),
        "1 处",
        "escape mode treats \\t as a tab"
    );
    cleanup(&dir);
}

#[test]
fn an_invalid_regex_is_reported_in_the_bar_not_panicked() {
    let (mut st, _f, dir) = st_with("text\n");
    st.set_find_mode(SearchMode::Regex);
    st.find.pattern = "([unclosed".into();
    st.find_count();
    assert!(
        st.find_message_text().starts_with("正则错误"),
        "error shown in the find bar: {}",
        st.find_message_text()
    );
    st.find_next();
    assert!(
        st.find_message_text().starts_with("正则错误"),
        "find_next uses the same error path"
    );
    cleanup(&dir);
}

#[test]
fn find_in_selection_limits_the_search_range() {
    let (mut st, _f, dir) = st_with("hit hit hit\n");
    // Select only [4,7) containing the second hit
    st.set_caret(4, false);
    st.set_caret(7, true);
    if !st.find.in_sel {
        st.toggle_find_in_sel();
    }
    st.find.pattern = "hit".into();
    st.find_count();
    assert_eq!(
        st.find_message_text(),
        "1 处",
        "find in selection only looks within the selection"
    );
    cleanup(&dir);
}

// ---------------------------------------------------------------------------
// Replace
// ---------------------------------------------------------------------------

#[test]
fn replace_one_replaces_only_the_current_match() {
    let (mut st, _f, dir) = st_with("cat and cat\n");
    st.find.pattern = "cat".into();
    st.find.replacement = "dog".into();
    st.find_next(); // select the first cat
    st.replace_one();
    assert_eq!(
        st.active_doc().read_all(),
        "dog and cat\n",
        "only the current match replaced"
    );
    cleanup(&dir);
}

#[test]
fn replace_all_replaces_everything_in_one_undo_step() {
    let (mut st, _f, dir) = st_with("ab ab ab\n");
    st.find.pattern = "ab".into();
    st.find.replacement = "xyz".into();
    st.replace_all();
    assert_eq!(st.active_doc().read_all(), "xyz xyz xyz\n");
    assert_eq!(st.find_message_text(), "已替换 3 处");

    st.undo();
    assert_eq!(
        st.active_doc().read_all(),
        "ab ab ab\n",
        "replace all must be a single undo step"
    );
    cleanup(&dir);
}

#[test]
fn replace_all_in_selection_leaves_the_rest_alone() {
    let (mut st, _f, dir) = st_with("e e e\n");
    // There is no notion of "second line" selection — select [2,3) (the second e)
    st.set_caret(2, false);
    st.set_caret(3, true);
    if !st.find.in_sel {
        st.toggle_find_in_sel();
    }
    st.find.pattern = "e".into();
    st.find.replacement = "EE".into();
    st.replace_all();
    assert_eq!(
        st.active_doc().read_all(),
        "e EE e\n",
        "the two e's outside the selection must stay intact"
    );
    cleanup(&dir);
}

#[test]
fn regex_replacement_expands_capture_groups() {
    let (mut st, _f, dir) = st_with("John Smith\n");
    st.set_find_mode(SearchMode::Regex);
    st.find.pattern = r"(\w+) (\w+)".into();
    st.find.replacement = "$2, $1".into();
    st.replace_all();
    assert_eq!(
        st.active_doc().read_all(),
        "Smith, John\n",
        "capture groups expand in order"
    );
    cleanup(&dir);
}

#[test]
fn replace_with_no_match_reports_instead_of_touching_the_document() {
    let (mut st, _f, dir) = st_with("untouched\n");
    st.find.pattern = "不存在的内容".into();
    st.find.replacement = "x".into();
    st.replace_all();
    assert_eq!(st.active_doc().read_all(), "untouched\n");
    assert_eq!(st.find_message_text(), "找不到匹配项");
    cleanup(&dir);
}

// ---------------------------------------------------------------------------
// Goto line
// ---------------------------------------------------------------------------

#[test]
fn goto_line_jumps_and_clamps_out_of_range_values() {
    let (mut st, _f, dir) = st_with("l1\nl2\nl3\n");
    st.goto_line(2);
    let col0 = st.active_doc().line_range(1).start;
    assert_eq!(st.caret().pos, col0, "jumps to the start of line 2");

    st.goto_line(99);
    let last = st.active_doc().line_range(2).start;
    assert_eq!(st.caret().pos, last, "out-of-range clamps to the last line");
    cleanup(&dir);
}

#[test]
fn the_goto_box_prefills_the_current_line_and_commits_on_enter() {
    let (mut st, _f, dir) = st_with("l1\nl2\nl3\n");
    st.goto_line(2); // the caret is now on line 2
    st.open_goto();
    assert!(st.find.open, "goto box opens");
    assert_eq!(st.dialog_input(), "", "goto does not use the dialog input");
    assert_eq!(
        st.find.goto.as_deref(),
        Some("2"),
        "prefills the current line number"
    );

    st.find_clear_field();
    assert!(st.find_insert("3"));
    assert!(st.commit_goto(), "Enter commits");
    let col0 = st.active_doc().line_range(2).start;
    assert_eq!(st.caret().pos, col0);
    assert!(!st.find.open, "goto box closes after commit");
    cleanup(&dir);
}

// ---------------------------------------------------------------------------
// Result invalidation and history
// ---------------------------------------------------------------------------

#[test]
fn editing_the_document_invalidates_previous_results() {
    let (mut st, _f, dir) = st_with("aa\n");
    st.find.pattern = "aa".into();
    st.find_count();
    assert_eq!(st.find_message_text(), "1 处");

    let before = st.active_doc().gen;
    // Move the caret to the end and append an "aa": "aaaa" has exactly two
    // non-overlapping "aa" matches
    let len = st.active_doc().total_bytes();
    st.set_caret(len, false);
    st.insert_text("aa");
    assert!(
        !st.find.valid_for(st.active_doc().id, before),
        "document generation changed: old results must be invalid"
    );
    st.find_count();
    assert_eq!(
        st.find_message_text(),
        "2 处",
        "recount reflects the new content"
    );
    cleanup(&dir);
}

#[test]
fn successful_searches_record_history_newest_first_with_dedup() {
    let (mut st, _f, dir) = st_with("alpha beta gamma\n");
    st.open_find(false, false);
    for pat in ["alpha", "beta", "alpha"] {
        st.find_clear_field();
        st.find_insert(pat);
        st.find_next();
    }
    assert_eq!(
        st.find.history.first().map(String::as_str),
        Some("alpha"),
        "most recent first"
    );
    assert_eq!(st.find.history.len(), 2, "deduped");
    cleanup(&dir);
}

#[test]
fn closing_find_returns_focus_to_the_document() {
    let (mut st, _f, dir) = st_with("x\n");
    st.open_find(true, true);
    assert!(st.find.open && st.find.replace && st.find.panel);
    st.close_find();
    assert!(!st.find.open);
    assert!(!st.find.panel, "result panel collapses along with it");
    cleanup(&dir);
}
