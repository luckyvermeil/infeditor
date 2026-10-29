//! End-to-end integration tests for open → edit → save → close, plus the
//! safety boundaries of every step.
//!
//! # Coverage
//!
//! * Open/focus: opening the same path again focuses instead of duplicating
//!   the tab
//! * Edit: insertion, undo/redo, dirty flag propagation
//! * Save: content round trip, newlines (LF/CRLF/CR), encodings
//!   (UTF-8/BOM/GBK/UTF-16LE), lossy encoding reporting, tmp+rename atomicity,
//!   failures leave no debris and never damage the original file
//! * Read-only mode: insertions blocked, file untouched
//! * Close: the dirty-document three-way question (save/discard/cancel), every
//!   exit safe
//! * Command dispatch: `Cmd::Save` / `Cmd::SaveAll` via the same path as menus
//!
//! Headless: only drives the state layer, no window (GUI end-to-end lives in
//! `tests_py/`).
//!
//! # Safety conventions
//!
//! All reads and writes happen inside a test-specific directory under
//! `std::env::temp_dir()`; [`common::test_config_dir`] isolates
//! `INFEDITOR_CONFIG_DIR` into a temp directory, never touching the user's
//! real files.

mod common;

use common::{cleanup, open, run, state, tmp_work_dir, write_file};
use infeditor::ui::actions::Cmd;
use infeditor::ui::commands::READONLY_MSG;
use std::path::Path;

fn bytes_of(path: &Path) -> Vec<u8> {
    std::fs::read(path).unwrap_or_else(|e| panic!("failed to read back {}: {e}", path.display()))
}

fn text_of(path: &Path) -> String {
    String::from_utf8_lossy(&bytes_of(path)).into_owned()
}

// ---------------------------------------------------------------------------
// Open / focus
// ---------------------------------------------------------------------------

#[test]
fn opening_the_same_path_twice_focuses_instead_of_duplicating() {
    let dir = tmp_work_dir("focus");
    let f = write_file(&dir, "a.txt", "内容\n");
    let mut st = state();
    open(&mut st, &f);
    let tabs_after_first = st.tabs.len();
    open(&mut st, &f);
    assert_eq!(
        st.tabs.len(),
        tabs_after_first,
        "reopening must focus the existing tab"
    );
    assert_eq!(st.active_doc().read_all(), "内容\n");
    cleanup(&dir);
}

#[test]
fn opening_a_missing_file_reports_and_keeps_state_intact() {
    let dir = tmp_work_dir("missing");
    let mut st = state();
    let before = st.tabs.len();
    st.open_path(dir.join("不存在.txt"));
    assert!(
        st.status_text().contains("打开失败"),
        "must report: {}",
        st.status_text()
    );
    assert_eq!(
        st.tabs.len(),
        before,
        "a failed open must not add or remove tabs"
    );
    cleanup(&dir);
}

// ---------------------------------------------------------------------------
// Edit → save round trip
// ---------------------------------------------------------------------------

#[test]
fn edit_then_save_round_trips_content_and_clears_dirty() {
    let dir = tmp_work_dir("roundtrip");
    let f = write_file(&dir, "note.txt", "第一行\n第二行\n");
    let mut st = state();
    open(&mut st, &f);

    st.insert_text("插在开头");
    assert!(st.active_doc().is_dirty(), "must be dirty after insertion");

    st.save_active();
    assert!(
        !st.active_doc().is_dirty(),
        "dirty flag must clear after a successful save"
    );
    assert!(
        st.status_text().contains("已保存"),
        "status bar: {}",
        st.status_text()
    );
    assert_eq!(text_of(&f), "插在开头第一行\n第二行\n");
    assert_eq!(st.active_doc().read_all(), "插在开头第一行\n第二行\n");
    cleanup(&dir);
}

#[test]
fn undo_after_save_marks_dirty_but_leaves_the_file_alone() {
    let dir = tmp_work_dir("undo");
    let f = write_file(&dir, "u.txt", "基线\n");
    let mut st = state();
    open(&mut st, &f);
    st.insert_text("追加"); // caret at 0 → prepends
    st.save_active();
    assert_eq!(text_of(&f), "追加基线\n");

    st.undo();
    assert_eq!(
        st.active_doc().read_all(),
        "基线\n",
        "undo only affects the in-memory buffer"
    );
    assert!(
        st.active_doc().is_dirty(),
        "differs from disk after undo → dirty"
    );
    assert_eq!(
        text_of(&f),
        "追加基线\n",
        "the on-disk file must not be rewritten by undo"
    );

    st.redo();
    assert_eq!(st.active_doc().read_all(), "追加基线\n");
    cleanup(&dir);
}

#[test]
fn save_converts_line_endings_for_every_flavor() {
    for (name, nl_name, sep) in [
        ("lf.txt", "LF", "\n"),
        ("crlf.txt", "CRLF", "\r\n"),
        ("cr.txt", "CR", "\r"),
    ] {
        let dir = tmp_work_dir("newline");
        let f = write_file(&dir, name, "a\nb\n");
        let mut st = state();
        open(&mut st, &f);
        st.set_newline(nl_name);
        st.save_active();
        let raw = bytes_of(&f);
        assert_eq!(
            String::from_utf8_lossy(&raw),
            format!("a{sep}b{sep}"),
            "{name} line endings must be {sep:?}"
        );
        cleanup(&dir);
    }
}

#[test]
fn save_writes_bom_and_multi_byte_encodings_correctly() {
    let dir = tmp_work_dir("encoding");
    let content = "汉字与 ascii\n";

    // UTF-8 BOM
    let f = write_file(&dir, "bom.txt", content);
    let mut st = state();
    open(&mut st, &f);
    st.set_encoding("UTF-8 BOM");
    st.save_active();
    let raw = bytes_of(&f);
    assert_eq!(&raw[..3], &[0xEF, 0xBB, 0xBF], "UTF-8 BOM must reach disk");
    assert_eq!(String::from_utf8_lossy(&raw[3..]), content);

    // UTF-16LE (with BOM).
    let f16 = write_file(&dir, "u16.txt", content);
    let mut st_enc = state();
    open(&mut st_enc, &f16);
    st_enc.set_encoding("UTF-16LE");
    st_enc.save_active();
    let raw = bytes_of(&f16);
    assert_eq!(&raw[..2], &[0xFF, 0xFE], "UTF-16LE BOM");
    let (decoded, _, had_errors) = encoding_rs::UTF_16LE.decode(&raw);
    assert!(!had_errors);
    assert_eq!(decoded, content);
    cleanup(&dir);
}

#[test]
fn gbk_save_reports_lossy_characters_instead_of_silently_clobbering() {
    let dir = tmp_work_dir("gbk");
    let f = write_file(&dir, "g.txt", "");
    let mut st = state();
    open(&mut st, &f);
    st.set_encoding("GBK");

    // Content representable in GBK: lossless round trip
    st.active_doc_mut().insert_text(0, "中文没问题\n");
    st.save_active();
    let raw = bytes_of(&f);
    let (decoded, _, _) = encoding_rs::GBK.decode(&raw);
    assert_eq!(decoded, "中文没问题\n");

    // An emoji cannot be represented in GBK: the loss must be reported, never
    // leave the user in the dark
    let len = st.active_doc().total_bytes();
    st.active_doc_mut().insert_text(len, "😀");
    st.save_active();
    assert!(
        st.status_text().contains("无法表示的字符"),
        "lossy save must report: {}",
        st.status_text()
    );
    cleanup(&dir);
}

// ---------------------------------------------------------------------------
// Save safety boundaries
// ---------------------------------------------------------------------------

#[test]
fn save_is_atomic_and_leaves_no_temp_files_behind() {
    let dir = tmp_work_dir("atomic");
    let f = write_file(&dir, "a.txt", "v1\n");
    let mut st = state();
    open(&mut st, &f);
    let len = st.active_doc().total_bytes();
    st.active_doc_mut().insert_text(len, "v2\n");
    st.save_active();
    assert_eq!(text_of(&f), "v1\nv2\n");
    let leftovers: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.contains(".infeditor-") || n.ends_with(".tmp"))
        .collect();
    assert!(
        leftovers.is_empty(),
        "no temp files may remain after a successful save: {leftovers:?}"
    );
    cleanup(&dir);
}

#[test]
fn a_failed_save_reports_and_neither_damages_the_target_nor_litters() {
    let dir = tmp_work_dir("fail");
    let f = write_file(&dir, "target.txt", "原始内容\n");
    let mut st = state();
    open(&mut st, &f);
    st.active_doc_mut().insert_text(0, "新内容");

    // The parent directory of the target path doesn't exist: File::create(tmp)
    // is guaranteed to fail
    st.active_doc_mut().meta.path = Some(dir.join("不存在的目录").join("blocked.txt"));
    st.save_active();
    assert!(
        st.status_text().contains("保存失败"),
        "failure must be reported: {}",
        st.status_text()
    );

    // The original file is untouched
    assert_eq!(text_of(&f), "原始内容\n");
    // A failed tmp+rename leaves no garbage either
    let leftovers: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.contains(".infeditor-") || n.ends_with(".tmp"))
        .collect();
    assert!(
        leftovers.is_empty(),
        "a failed save must not leave temp files: {leftovers:?}"
    );
    assert!(
        st.active_doc().is_dirty(),
        "a document that failed to save stays dirty"
    );
    cleanup(&dir);
}

#[test]
fn read_only_mode_blocks_every_editing_path() {
    let dir = tmp_work_dir("ro");
    let f = write_file(&dir, "ro.txt", "不许动\n");
    let mut st = state();
    open(&mut st, &f);
    st.read_only = true;

    st.insert_text("x");
    assert_eq!(st.active_doc().read_all(), "不许动\n");
    assert_eq!(st.status_text(), st.tr(READONLY_MSG).into_owned());

    st.undo();
    assert_eq!(
        st.active_doc().read_all(),
        "不许动\n",
        "nothing to undo: no edit was applied in read-only mode"
    );
    assert!(
        !st.active_doc().is_dirty(),
        "the document must not become dirty in read-only mode"
    );
    assert_eq!(text_of(&f), "不许动\n");
    cleanup(&dir);
}

// ---------------------------------------------------------------------------
// The three exits of closing a dirty document
// ---------------------------------------------------------------------------

#[test]
fn closing_a_dirty_doc_saving_keeps_the_edit_on_disk() {
    let dir = tmp_work_dir("close-save");
    let f = write_file(&dir, "cs.txt", "old\n");
    let mut st = state();
    open(&mut st, &f);
    st.insert_text("new");
    assert!(
        !st.close_doc(st.active),
        "closing a dirty document must ask first"
    );
    assert!(
        st.dialog.is_some(),
        "must pop the custom confirmation dialog"
    );
    st.finish_close(Some(true));
    assert_eq!(
        st.tabs.len(),
        1,
        "a blank document fills in after the tab is removed"
    );
    assert!(st.dialog.is_none());
    assert_eq!(
        text_of(&f),
        "newold\n",
        "choosing save → the edit must reach disk"
    );
    cleanup(&dir);
}

#[test]
fn closing_a_dirty_doc_discarding_leaves_the_file_untouched() {
    let dir = tmp_work_dir("close-discard");
    let f = write_file(&dir, "cd.txt", "磁盘原样\n");
    let mut st = state();
    open(&mut st, &f);
    st.insert_text("内存里的编辑");
    st.close_doc(st.active);
    st.finish_close(Some(false));
    assert_eq!(text_of(&f), "磁盘原样\n", "discard → disk untouched");
    assert_eq!(st.tabs.len(), 1);
    cleanup(&dir);
}

#[test]
fn cancelling_a_close_keeps_the_tab_and_the_edit() {
    let dir = tmp_work_dir("close-cancel");
    let f = write_file(&dir, "cc.txt", "还在\n");
    let mut st = state();
    open(&mut st, &f);
    st.insert_text("别丢");
    st.close_doc(st.active);
    st.finish_close(None);
    // minimal()'s blank placeholder tab has been removed by drop_scratch_tab;
    // only the opened document remains here
    assert_eq!(st.tabs.len(), 1, "cancel → tab kept");
    assert!(st.active_doc().is_dirty(), "edit still present");
    assert!(st.dialog.is_none());
    let _ = f;
    cleanup(&dir);
}

// ---------------------------------------------------------------------------
// Command dispatch (the same path as menus/shortcuts)
// ---------------------------------------------------------------------------

#[test]
fn cmd_save_and_save_all_dispatch_like_the_menu_does() {
    let dir = tmp_work_dir("dispatch");
    let a = write_file(&dir, "1.txt", "A0\n");
    let b = write_file(&dir, "2.txt", "B0\n");
    let mut st = state();
    open(&mut st, &a);
    open(&mut st, &b);
    // Make both documents dirty: after opening the caret is at 0, so inserts
    // prepend. Edit the active tab b first (writes A1), then switch back to a
    // (writes B1).
    st.insert_text("A1");
    st.activate_next_tab(-1);
    st.insert_text("B1");

    run(&mut st, Cmd::SaveAll);
    assert_eq!(text_of(&a), "B1A0\n", "a's edit (B1) should reach disk");
    assert_eq!(text_of(&b), "A1B0\n", "b's edit (A1) should reach disk");
    for t in &st.tabs {
        assert!(!t.doc.is_dirty(), "all clean after SaveAll");
    }
    cleanup(&dir);
}

#[test]
fn save_all_skips_clean_documents_without_touching_them() {
    let dir = tmp_work_dir("dispatch-clean");
    let a = write_file(&dir, "1.txt", "A\n");
    let mut st = state();
    open(&mut st, &a);
    // Clean documents must be skipped by SaveAll (save_doc's dirty check
    // gates it) — this also guarantees headless tests never reach the save
    // branch that would pop an rfd dialog.
    run(&mut st, Cmd::SaveAll);
    assert_eq!(text_of(&a), "A\n");
    assert!(!st.active_doc().is_dirty());
    cleanup(&dir);
}

// ---------------------------------------------------------------------------
// The encoding probe window (64KB) boundary
// ---------------------------------------------------------------------------

/// When the probe window cuts exactly through a multi-byte character, a
/// **valid UTF-8** file must not be misidentified as another encoding (that
/// would turn the whole text into mojibake). Here the 64KB boundary is placed
/// precisely on the first byte of "中".
#[test]
fn utf8_file_cut_mid_character_at_the_probe_boundary_still_opens_as_utf8() {
    let dir = tmp_work_dir("probe-boundary");
    let f = write_file(&dir, "boundary.txt", "");
    let mut s = String::new();
    // 65535 bytes of ASCII + "中" (3 bytes, straddling the 65536 boundary) +
    // some more Chinese
    s.push_str(&"a".repeat(65535));
    s.push_str("中文测试行\n");
    while s.len() < 80 * 1024 {
        s.push_str("再补一行中文，确保整档超过探测窗口\n");
    }
    std::fs::write(&f, s.as_bytes()).unwrap();

    let mut st = state();
    open(&mut st, &f);
    let got = st.active_doc().read_all();
    if got != s {
        let at = got
            .as_bytes()
            .iter()
            .zip(s.as_bytes())
            .position(|(a, b)| a != b)
            .unwrap_or(got.len().min(s.len()));
        let lo = at.saturating_sub(20);
        panic!(
            "encoding misdetected: len got={} want={} first diff @{at}\n\
             got={:?}\nwant={:?}\nstatus: {}",
            got.len(),
            s.len(),
            &got[lo..(at + 40).min(got.len())],
            &s[lo..(at + 40).min(s.len())],
            st.status_text()
        );
    }
    cleanup(&dir);
}

// ---------------------------------------------------------------------------
// Lenient decoding of invalid UTF-8
// ---------------------------------------------------------------------------

/// A file whose prefix is valid UTF-8 but whose tail contains invalid bytes:
/// opening goes through lenient decoding (U+FFFD placeholders), the flag must
/// land in `meta.lossy_utf8`, and on save the status bar must **explicitly**
/// say that replacement characters are written back — the original bytes are
/// no longer in the buffer, and silently reporting "saved" is data-corruption
/// level misleading.
#[test]
fn lossily_decoded_file_is_flagged_and_reported_on_save() {
    let dir = tmp_work_dir("lossy-utf8");
    let f = dir.join("mixed.txt");
    // Trigger condition: the **first 64KB** (probe window) is valid UTF-8 →
    // decided as utf8_fast; only the whole-file validation then finds the
    // invalid bytes. Had the invalid bytes been inside the window, encoding
    // probing would kick in (a different path: decoded as GBK/1252, no
    // U+FFFD produced).
    let mut raw = String::new();
    while raw.len() < 70 * 1024 {
        raw.push_str("前段是合法 UTF-8，这一行用来把非法字节挤出探测窗口\n");
    }
    let mut raw = raw.into_bytes();
    raw.extend_from_slice(&[0xE4, 0xB8]); // truncated "中": invalid tail
    raw.extend_from_slice(b"\n");
    std::fs::write(&f, &raw).unwrap();

    let mut st = state();
    open(&mut st, &f);
    assert!(
        st.active_doc().meta.lossy_utf8,
        "lenient decoding must set the flag, or the save path can't warn"
    );
    assert!(
        st.active_doc().read_all().contains('\u{FFFD}'),
        "invalid bytes should show as replacement characters"
    );

    // Edit and save: replacement characters are written back, and the status
    // bar must say so clearly
    let len = st.active_doc().total_bytes();
    st.active_doc_mut().insert_text(len, "尾\n");
    st.save_active();
    assert!(
        st.status_text().contains("替换字符"),
        "saving a lossily-decoded document must warn that original bytes were replaced: {}",
        st.status_text()
    );
    assert!(!st.active_doc().is_dirty());
    // What's written back is itself valid UTF-8 (an editor invariant)
    assert!(std::str::from_utf8(&bytes_of(&f)).is_ok());
    cleanup(&dir);
}
