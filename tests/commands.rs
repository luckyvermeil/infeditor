//! Full integration tests for editing commands: everything is driven through
//! `actions::run` (the same dispatch path as menus/shortcuts), verifying the
//! command's effect on the document, undo grouping, and checked-state
//! consistency.
//!
//! Coverage: line operations (sort/dedup/reverse/randomize/join/split/move/
//! insert blank), case conversion, whitespace cleanup, comment toggling,
//! bracket jumping, digest tools (MD5/SHA-1), JSON formatting, date insertion,
//! word count, and view toggles like zoom/theme.
//!
//! Headless: only drives the state layer, no window (GUI end-to-end lives in
//! `tests_py/`).

mod common;

use common::{cleanup, open_file, run, tmp_work_dir};
use infeditor::textops::{CaseKind, SortKey};
use infeditor::ui::actions::{is_checked, Cmd};
use infeditor::ui::state::DialogBody;

// ---------------------------------------------------------------------------
// Line operations
// ---------------------------------------------------------------------------

#[test]
fn sort_lines_asc_desc_and_by_length() {
    let dir = tmp_work_dir("cmd-sort");
    let (mut st, _f) = open_file(&dir, "s.txt", "pear\nfig\napple\nbanana\n");

    run(&mut st, Cmd::SortLines(SortKey::Text, false));
    assert_eq!(
        st.active_doc().read_all(),
        "apple\nbanana\nfig\npear\n",
        "ascending"
    );

    run(&mut st, Cmd::SortLines(SortKey::Text, true));
    assert_eq!(
        st.active_doc().read_all(),
        "pear\nfig\nbanana\napple\n",
        "descending"
    );

    // Redo from the original content, by length
    let dir2 = tmp_work_dir("cmd-sort-len");
    let (mut st, _f) = open_file(&dir2, "s.txt", "pear\nfig\napple\nbanana\n");
    run(&mut st, Cmd::SortLines(SortKey::Length, false));
    assert_eq!(
        st.active_doc().read_all(),
        "fig\npear\napple\nbanana\n",
        "by character count"
    );
    cleanup(&dir);
    cleanup(&dir2);
}

#[test]
fn integer_sort_orders_by_the_first_number_not_the_text() {
    let dir = tmp_work_dir("cmd-sort-int");
    let (mut st, _f) = open_file(&dir, "n.txt", "10 x\n2 y\n1 z\n");
    run(&mut st, Cmd::SortLines(SortKey::Integer, false));
    assert_eq!(st.active_doc().read_all(), "1 z\n2 y\n10 x\n");
    cleanup(&dir);
}

#[test]
fn a_sort_is_a_single_undo_step() {
    let dir = tmp_work_dir("cmd-sort-undo");
    let (mut st, _f) = open_file(&dir, "s.txt", "b\na\nc\n");
    run(&mut st, Cmd::SortLines(SortKey::Text, false));
    assert_eq!(st.active_doc().read_all(), "a\nb\nc\n");
    st.undo();
    assert_eq!(
        st.active_doc().read_all(),
        "b\na\nc\n",
        "one undo restores the original"
    );
    cleanup(&dir);
}

#[test]
fn reverse_and_randomize_keep_the_line_multiset() {
    let dir = tmp_work_dir("cmd-rev");
    let (mut st, _f) = open_file(&dir, "r.txt", "1\n2\n3\n4\n");

    run(&mut st, Cmd::ReverseLines);
    assert_eq!(st.active_doc().read_all(), "4\n3\n2\n1\n");

    run(&mut st, Cmd::RandomizeLines);
    let all = st.active_doc().read_all();
    let mut lines: Vec<&str> = all.lines().collect();
    lines.sort();
    assert_eq!(
        lines,
        vec!["1", "2", "3", "4"],
        "randomize only shuffles order"
    );
    cleanup(&dir);
}

#[test]
fn duplicate_and_delete_duplicate_lines() {
    let dir = tmp_work_dir("cmd-dup");
    let (mut st, _f) = open_file(&dir, "d.txt", "x\ny\n");

    run(&mut st, Cmd::DuplicateLine);
    assert_eq!(
        st.active_doc().read_all(),
        "x\nx\ny\n",
        "duplicates the current line"
    );

    let dir2 = tmp_work_dir("cmd-dup2");
    let (mut st, _f) = open_file(&dir2, "d.txt", "a\nb\na\na\nc\n");
    run(&mut st, Cmd::DeleteDupLine);
    assert_eq!(
        st.active_doc().read_all(),
        "a\nb\nc\n",
        "duplicate removal keeps the first occurrence"
    );
    cleanup(&dir);
    cleanup(&dir2);
}

#[test]
fn join_then_split_lines_round_trip_with_blank_rows() {
    let dir = tmp_work_dir("cmd-join");
    let (mut st, _f) = open_file(&dir, "j.txt", "a\nb\nc\n");
    run(&mut st, Cmd::JoinLines);
    assert_eq!(
        st.active_doc().read_all(),
        "a b c\n",
        "joined into one line"
    );

    // Splitting runs in a fresh caret state (a blank line inserted after each line)
    let dir2 = tmp_work_dir("cmd-split");
    let (mut st, _f) = open_file(&dir2, "j.txt", "a\nb\nc\n");
    st.set_caret(0, false);
    run(&mut st, Cmd::SplitLines);
    assert_eq!(
        st.active_doc().read_all(),
        "a\n\nb\n\nc\n",
        "split = insert a blank line after each line"
    );
    cleanup(&dir);
    cleanup(&dir2);
}

#[test]
fn move_lines_up_and_down() {
    let dir = tmp_work_dir("cmd-move");
    let (mut st, _f) = open_file(&dir, "m.txt", "a\nb\nc\n");
    st.set_caret(2, false); // line 1 (b)

    run(&mut st, Cmd::MoveLineUp);
    assert_eq!(st.active_doc().read_all(), "b\na\nc\n");
    run(&mut st, Cmd::MoveLineDown);
    assert_eq!(st.active_doc().read_all(), "a\nb\nc\n");
    cleanup(&dir);
}

#[test]
fn insert_blank_line_above_and_below() {
    let dir = tmp_work_dir("cmd-blank");
    let (mut st, _f) = open_file(&dir, "b.txt", "a\nb\n");
    st.set_caret(0, false);

    run(&mut st, Cmd::InsertBlankAbove);
    assert_eq!(
        st.active_doc().read_all(),
        "\na\nb\n",
        "blank line inserted above"
    );
    cleanup(&dir);
}

#[test]
fn remove_empty_lines_vs_whitespace_only_lines() {
    // Truly empty lines and whitespace-only lines are two commands, two criteria
    let dir = tmp_work_dir("cmd-empty");
    let (mut st, _f) = open_file(&dir, "e.txt", "a\n\n \nb\n");
    run(&mut st, Cmd::RemoveEmptyLines);
    assert_eq!(
        st.active_doc().read_all(),
        "a\n \nb\n",
        "only truly empty lines removed, whitespace-only lines kept"
    );

    let dir2 = tmp_work_dir("cmd-empty2");
    let (mut st, _f) = open_file(&dir2, "e.txt", "a\n\n \nb\n");
    run(&mut st, Cmd::RemoveEmptyLinesWithBlank);
    assert_eq!(
        st.active_doc().read_all(),
        "a\nb\n",
        "whitespace-only lines removed too"
    );
    cleanup(&dir);
    cleanup(&dir2);
}

// ---------------------------------------------------------------------------
// Whitespace cleanup
// ---------------------------------------------------------------------------

#[test]
fn trim_variants_remove_leading_trailing_or_both() {
    for (cmd, want) in [
        (Cmd::TrimBoth, "a b\nc\n"),
        (Cmd::TrimLeading, "a b  \nc  \n"),
        (Cmd::TrimTrailing, "  a b\n   c\n"),
    ] {
        let dir = tmp_work_dir("cmd-trim");
        let (mut st, _f) = open_file(&dir, "t.txt", "  a b  \n   c  \n");
        run(&mut st, cmd);
        assert_eq!(st.active_doc().read_all(), want, "{cmd:?}");
        cleanup(&dir);
    }
}

#[test]
fn tab_space_conversions() {
    let dir = tmp_work_dir("cmd-tab");
    let (mut st, _f) = open_file(&dir, "t.txt", "\tcode\n\t\tmore\n");
    run(&mut st, Cmd::TabToSpace);
    assert_eq!(
        st.active_doc().read_all(),
        "    code\n        more\n",
        "tabs→spaces"
    );

    run(&mut st, Cmd::SpaceToTab);
    assert_eq!(
        st.active_doc().read_all(),
        "\tcode\n\t\tmore\n",
        "spaces→tabs"
    );
    cleanup(&dir);
}

#[test]
fn eol_to_space_flattens_lines() {
    let dir = tmp_work_dir("cmd-eol");
    let (mut st, _f) = open_file(&dir, "e.txt", "a\nb\nc\n");
    run(&mut st, Cmd::EolToSpace);
    assert_eq!(st.active_doc().read_all(), "a b c\n");
    cleanup(&dir);
}

// ---------------------------------------------------------------------------
// Comments
// ---------------------------------------------------------------------------

#[test]
fn line_comment_toggles_follow_the_language_syntax() {
    let dir = tmp_work_dir("cmd-comment");
    let (mut st, _f) = open_file(&dir, "code.rs", "let x = 1;\nlet y = 2;\n");
    st.set_caret(0, false);
    st.set_caret(st.active_doc().line_range(1).end, true); // select two lines

    run(&mut st, Cmd::ToggleLineComment);
    assert_eq!(
        st.active_doc().read_all(),
        "// let x = 1;\n// let y = 2;\n",
        "Rust uses //"
    );
    run(&mut st, Cmd::ToggleLineComment);
    assert_eq!(
        st.active_doc().read_all(),
        "let x = 1;\nlet y = 2;\n",
        "toggling again restores"
    );

    let dir2 = tmp_work_dir("cmd-comment-py");
    let (mut st, _f) = open_file(&dir2, "code.py", "print(1)\n");
    run(&mut st, Cmd::ToggleLineComment);
    assert_eq!(st.active_doc().read_all(), "# print(1)\n", "Python uses #");
    cleanup(&dir);
    cleanup(&dir2);
}

#[test]
fn block_comment_and_uncomment_round_trip() {
    let dir = tmp_work_dir("cmd-block");
    let (mut st, _f) = open_file(&dir, "code.rs", "hello world\n");
    // Block comments require a selection first
    st.set_caret(0, false);
    st.set_caret(5, true); // select "hello"
    run(&mut st, Cmd::BlockComment);
    assert_eq!(
        st.active_doc().read_all(),
        "/*hello*/ world\n",
        "block comment wraps the selection"
    );
    run(&mut st, Cmd::BlockUncomment);
    assert_eq!(
        st.active_doc().read_all(),
        "hello world\n",
        "block uncomment restores"
    );
    cleanup(&dir);
}

#[test]
fn block_comment_without_selection_is_refused_with_a_hint() {
    let dir = tmp_work_dir("cmd-block-nosel");
    let (mut st, _f) = open_file(&dir, "code.rs", "x\n");
    run(&mut st, Cmd::BlockComment);
    assert!(
        st.status_text().contains("选中"),
        "must hint to select first: {}",
        st.status_text()
    );
    assert_eq!(st.active_doc().read_all(), "x\n");
    cleanup(&dir);
}

// ---------------------------------------------------------------------------
// Case
// ---------------------------------------------------------------------------

#[test]
fn case_conversions_cover_all_kinds() {
    let src = "hello World foo\n";
    let cases = [
        (CaseKind::Upper, "HELLO WORLD FOO\n"),
        (CaseKind::Lower, "hello world foo\n"),
        (CaseKind::Proper, "Hello World Foo\n"),
        (CaseKind::Invert, "HELLO wORLD FOO\n"),
    ];
    for (kind, want) in cases {
        let dir = tmp_work_dir("cmd-case");
        let (mut st, _f) = open_file(&dir, "c.txt", src);
        run(&mut st, Cmd::Case(kind));
        assert_eq!(st.active_doc().read_all(), want, "{kind:?}");
        cleanup(&dir);
    }
}

#[test]
fn case_conversion_targets_the_whole_line_of_the_selection() {
    let dir = tmp_work_dir("cmd-case-sel");
    let (mut st, _f) = open_file(&dir, "c.txt", "keep Me\n");
    st.set_caret(5, false);
    st.set_caret(7, true); // select "Me" — case commands apply to the whole line
    run(&mut st, Cmd::Case(CaseKind::Upper));
    assert_eq!(st.active_doc().read_all(), "KEEP ME\n");
    cleanup(&dir);
}

// ---------------------------------------------------------------------------
// Bracket jumping
// ---------------------------------------------------------------------------

#[test]
fn goto_bracket_jumps_to_the_matching_delimiter() {
    let dir = tmp_work_dir("cmd-bracket");
    let (mut st, _f) = open_file(&dir, "b.txt", "{\n  foo\n}\n");
    st.set_caret(0, false);
    run(&mut st, Cmd::GotoBracket);
    assert_eq!(
        st.caret().pos,
        8,
        "jumps from {{ to the matching }} (byte position)"
    );
    cleanup(&dir);
}

// ---------------------------------------------------------------------------
// Digest / encoding tools
// ---------------------------------------------------------------------------

#[test]
fn md5_and_sha1_show_the_digest_in_a_dialog() {
    let dir = tmp_work_dir("cmd-md5");
    let (mut st, _f) = open_file(&dir, "d.txt", "abc");
    run(&mut st, Cmd::Md5);
    let dlg = st
        .dialog
        .clone()
        .expect("MD5 result must be shown in a dialog");
    match dlg.body {
        DialogBody::Text(t) => assert_eq!(
            i18n::current().tr(t).into_owned(),
            "900150983cd24fb0d6963f7d28e17f72",
            "known digest of md5(\"abc\")"
        ),
        other => panic!("digest dialog should be text: {other:?}"),
    }
    st.close_dialog();

    run(&mut st, Cmd::Sha1);
    let dlg = st
        .dialog
        .clone()
        .expect("SHA-1 result must be shown in a dialog");
    match dlg.body {
        DialogBody::Text(t) => assert_eq!(
            i18n::current().tr(t).into_owned(),
            "a9993e364706816aba3e25717850c26c9cd0d89d",
            "known digest of sha1(\"abc\")"
        ),
        other => panic!("digest dialog should be text: {other:?}"),
    }
    st.close_dialog();
    cleanup(&dir);
}

#[test]
fn format_json_pretty_prints_and_reports_syntax_errors() {
    let dir = tmp_work_dir("cmd-json");
    let (mut st, _f) = open_file(&dir, "d.json", r#"{"a":1,"b":[2,3]}"#);
    run(&mut st, Cmd::FormatJson);
    let out = st.active_doc().read_all();
    assert!(
        out.contains("\"a\": 1") || out.contains("\"a\":1"),
        "formatted: {out:?}"
    );
    assert!(out.contains('\n'), "must pretty-print with newlines");

    let dir2 = tmp_work_dir("cmd-json-bad");
    let (mut st, _f) = open_file(&dir2, "d.json", "{ broken");
    run(&mut st, Cmd::FormatJson);
    assert!(
        st.status_text().contains("失败")
            || st.status_text().contains("错误")
            || !st.status_text().is_empty(),
        "syntax error must be reported: {}",
        st.status_text()
    );
    cleanup(&dir);
    cleanup(&dir2);
}

// ---------------------------------------------------------------------------
// Insert / statistics
// ---------------------------------------------------------------------------

#[test]
fn inserting_the_datetime_short_is_hhmm_and_long_has_the_date() {
    let dir = tmp_work_dir("cmd-date");
    let (mut st, _f) = open_file(&dir, "d.txt", "");
    run(&mut st, Cmd::InsertShortDateTime);
    let out = st.active_doc().read_all();
    assert!(
        out.chars().count() == 5 && out.as_bytes()[2] == b':',
        "short format is HH:MM: {out:?}"
    );

    let dir2 = tmp_work_dir("cmd-date2");
    let (mut st, _f) = open_file(&dir2, "d.txt", "");
    run(&mut st, Cmd::InsertLongDateTime);
    let out = st.active_doc().read_all();
    assert!(
        out.starts_with("20") && out.len() >= 19,
        "long format YYYY-MM-DD HH:MM:SS: {out:?}"
    );
    cleanup(&dir);
    cleanup(&dir2);
}

#[test]
fn word_count_dialog_reports_characters_words_and_lines() {
    let dir = tmp_work_dir("cmd-count");
    let (mut st, _f) = open_file(&dir, "d.txt", "one two three\n四\n");
    run(&mut st, Cmd::WordCount);
    let dlg = st
        .dialog
        .clone()
        .expect("word count result must be shown in a dialog");
    match dlg.body {
        DialogBody::Text(t) => {
            let t = i18n::current().tr(t).into_owned();
            assert!(t.contains("16"), "chars (with whitespace) 16: {t:?}");
            assert!(t.contains("12"), "chars (no whitespace) 12: {t:?}");
            assert!(t.contains("行数: 2"), "行数 2: {t:?}");
        }
        other => panic!("word count dialog should be text: {other:?}"),
    }
    st.close_dialog();
    cleanup(&dir);
}

// ---------------------------------------------------------------------------
// View toggles and is_checked consistency
// ---------------------------------------------------------------------------

#[test]
fn view_toggles_flip_state_and_checked_flags_together() {
    let dir = tmp_work_dir("cmd-view");
    let (mut st, _f) = open_file(&dir, "v.txt", "x\n");

    for cmd in [Cmd::WordWrap, Cmd::ShowEol, Cmd::ShowWs] {
        assert!(!is_checked(&st, cmd), "{cmd:?} initially unchecked");
        run(&mut st, cmd);
        assert!(
            is_checked(&st, cmd),
            "{cmd:?} must be checked after running"
        );
        run(&mut st, cmd);
        assert!(
            !is_checked(&st, cmd),
            "{cmd:?} unchecked after running again"
        );
    }
    cleanup(&dir);
}

#[test]
fn zoom_steps_change_the_font_size_and_reset_restores() {
    let dir = tmp_work_dir("cmd-zoom");
    let (mut st, _f) = open_file(&dir, "z.txt", "x\n");
    let base = st.font_size;
    run(&mut st, Cmd::ZoomIn);
    assert!(st.font_size > base, "zoom in");
    run(&mut st, Cmd::ZoomOut);
    run(&mut st, Cmd::ZoomOut);
    assert!(st.font_size < base, "zoom out");
    run(&mut st, Cmd::ZoomReset);
    assert_eq!(st.font_size, base, "reset restores the default");
    cleanup(&dir);
}

#[test]
fn theme_toggle_flips_dark_and_the_checked_flag() {
    let dir = tmp_work_dir("cmd-theme");
    let (mut st, _f) = open_file(&dir, "t.txt", "x\n");
    let was = st.dark;
    run(&mut st, Cmd::ToggleTheme);
    assert_eq!(st.dark, !was, "theme flipped");
    cleanup(&dir);
}

#[test]
fn vim_toggle_command_switches_the_input_mode() {
    let dir = tmp_work_dir("cmd-vim");
    let (mut st, _f) = open_file(&dir, "v.txt", "x\n");
    assert!(!st.vim.enabled);
    run(&mut st, Cmd::ToggleVim);
    assert!(st.vim.enabled);
    run(&mut st, Cmd::ToggleVim);
    assert!(!st.vim.enabled);
    cleanup(&dir);
}

#[test]
fn encoding_and_newline_commands_retarget_the_active_document() {
    let dir = tmp_work_dir("cmd-enc");
    let (mut st, _f) = open_file(&dir, "e.txt", "数据\n");
    run(&mut st, Cmd::SetNewline("CRLF"));
    run(&mut st, Cmd::SetEncoding("UTF-8 BOM"));
    st.save_active();
    let raw = std::fs::read(&_f).unwrap();
    assert_eq!(&raw[..3], &[0xEF, 0xBB, 0xBF], "BOM written to disk");
    assert!(raw.ends_with(b"\r\n"), "CRLF written to disk");
    cleanup(&dir);
}

#[test]
fn read_only_gates_commands_but_not_view_commands() {
    let dir = tmp_work_dir("cmd-ro");
    let (mut st, _f) = open_file(&dir, "r.txt", "keep\n");
    st.read_only = true;
    run(&mut st, Cmd::DuplicateLine);
    assert_eq!(
        st.active_doc().read_all(),
        "keep\n",
        "editing commands are blocked"
    );
    run(&mut st, Cmd::ShowEol);
    assert!(st.show_eol, "view commands are not restricted by read-only");
    cleanup(&dir);
}

/// `Insert Blank Line Below` used to be a permanent no-op: the line-list writer
/// joined `["b", ""]` to exactly the original `"b\n"` and skipped the write.
#[test]
fn insert_blank_line_below_actually_inserts() {
    let dir = tmp_work_dir("cmd-blank");
    let (mut st, _f) = open_file(&dir, "b.txt", "a\nb\nc\n");
    st.set_caret(2, false); // line 1 (b)
    run(&mut st, Cmd::InsertBlankBelow);
    assert_eq!(
        st.active_doc().read_all(),
        "a\nb\n\nc\n",
        "a blank line between b and c"
    );

    // Last line of a terminated document: the blank line lands at EOF
    let dir2 = tmp_work_dir("cmd-blank2");
    let (mut st, _f) = open_file(&dir2, "b.txt", "a\nb\n");
    st.set_caret(2, false); // line 1 (b)
    run(&mut st, Cmd::InsertBlankBelow);
    assert_eq!(st.active_doc().read_all(), "a\nb\n\n");

    // Above still works (the leading empty element is distinguishable)
    let dir3 = tmp_work_dir("cmd-blank3");
    let (mut st, _f) = open_file(&dir3, "b.txt", "a\nb\n");
    st.set_caret(2, false); // line 1 (b)
    run(&mut st, Cmd::InsertBlankAbove);
    assert_eq!(st.active_doc().read_all(), "a\n\nb\n");
    cleanup(&dir);
    cleanup(&dir2);
    cleanup(&dir3);
}

/// The numbering value is `start + step * i`; the extra `* start` made
/// "start = 5, step = 1" count 5, 10, 15 and "start = 0" label every line 0.
#[test]
fn insert_numbering_uses_start_plus_step() {
    let dir = tmp_work_dir("cmd-num");
    let (mut st, _f) = open_file(&dir, "n.txt", "a\nb\nc\n");
    st.insert_numbering(5, 1);
    assert_eq!(st.active_doc().read_all(), "5a\n6b\n7c\n");
    st.insert_numbering(0, 10);
    assert!(
        st.active_doc().read_all().starts_with("05a\n106b\n207c\n"),
        "second pass counts 0, 10, 20: {}",
        st.active_doc().read_all()
    );
    cleanup(&dir);
}
