//! End-to-end tests for crash recovery: journal written to disk → simulated
//! crash → a new process discovers the remains → restore/discard → remains
//! cleared.
//!
//! # Coverage
//!
//! * Full round trip of a dirty session: content, caret, dirty flag, newline,
//!   and encoding are all restored together
//! * Clean documents restore from disk (no content snapshot written)
//! * Missing (deleted/corrupt) snapshot falls back to the disk file, never panics
//! * Corrupt manifest / foreign format / newer version: safely skipped as
//!   "no session"
//! * Journal throttling: no rewrite while the signature is unchanged; written
//!   only when due
//! * Not a single byte written outside the recovery directory (manifest escape
//!   defense has its own unit tests in the library)
//!
//! All cases use explicit directory parameters (`*_in` variants) and stay
//! isolated from the user's real config directory.

mod common;

use common::{cleanup, tmp_work_dir, write_file};
use infeditor::buffer::Newline;
use infeditor::ui::recovery::{
    self, capture, clear_session, load_session, write_session, Journal, JOURNAL_INTERVAL,
    MANIFEST_NAME,
};
use infeditor::ui::state::EditorState;
use std::path::PathBuf;
use std::time::Instant;

fn dirty_st() -> (EditorState, PathBuf) {
    let dir = tmp_work_dir("recovery-src");
    let f = write_file(&dir, "doc.txt", "磁盘上的内容\n");
    let mut st = EditorState::minimal();
    st.open_path(f);
    st.insert_text("未保存的编辑 ");
    st.tabs[st.active].caret.pos = 9;
    (st, dir)
}

#[test]
fn a_crashed_session_is_detected_and_fully_restored() {
    let (mut st, work) = dirty_st();
    let session = tmp_work_dir("recovery-session");
    // Go through the production path: the UI heartbeat calls recovery_tick_in
    // (capture + write_session internally) instead of hand-assembling the
    // manifest — otherwise we can't test that "the app really does write the
    // session down".
    st.recovery_tick_in(Some(&session), Instant::now());
    assert!(
        session.join(MANIFEST_NAME).exists(),
        "the first tick must write the manifest"
    );
    assert!(
        std::fs::read_dir(&session).unwrap().count() >= 2,
        "a dirty document must also write a content snapshot"
    );

    // ---- Simulate a new process: the remains are found by the startup check ----
    let mut st2 = EditorState::minimal();
    st2.check_recovery_in(Some(&session));
    assert!(
        st2.pending_recovery.is_some(),
        "the startup check must find the remains and raise the confirmation dialog"
    );
    assert!(
        st2.dialog.is_some(),
        "recovery confirmation dialog must pop (no journal write yet)"
    );

    // ---- User chooses restore ----
    st2.restore_recovery_from(&session);
    assert_eq!(st2.tabs.len(), 1);
    assert_eq!(
        st2.tabs[0].doc.read_all(),
        "未保存的编辑 磁盘上的内容\n",
        "restored content = the last moment before closing"
    );
    assert!(
        st2.tabs[0].doc.is_dirty(),
        "unsaved edits stay dirty after restore"
    );
    assert_eq!(st2.tabs[0].caret.pos, 9, "caret position restored too");
    assert!(
        !session.join(MANIFEST_NAME).exists(),
        "remains must be cleared after restore so the next start doesn't prompt again"
    );
    cleanup(&work);
    cleanup(&session);
}

#[test]
fn clean_documents_restore_from_disk_and_stay_clean() {
    let work = tmp_work_dir("recovery-clean");
    let f = write_file(&work, "clean.txt", "干净内容\n");
    let mut st = EditorState::minimal();
    st.open_path(f.clone());
    assert!(!st.tabs[0].doc.is_dirty());

    let session = tmp_work_dir("recovery-clean-session");
    let (journal, _) = capture(&st, &Default::default());
    assert!(
        journal.backups.is_empty(),
        "clean documents get no content snapshot"
    );
    write_session(&session, &journal).unwrap();

    let mut st2 = EditorState::minimal();
    st2.check_recovery_in(Some(&session));
    st2.restore_recovery_from(&session);
    assert_eq!(st2.tabs[0].doc.read_all(), "干净内容\n");
    assert!(
        !st2.tabs[0].doc.is_dirty(),
        "a clean document must still be clean after restore"
    );
    assert_eq!(st2.tabs[0].doc.meta.path.as_deref(), Some(f.as_path()));
    cleanup(&work);
    cleanup(&session);
}

#[test]
fn a_missing_backup_falls_back_to_the_disk_file_without_panicking() {
    let (st, work) = dirty_st();
    let session = tmp_work_dir("recovery-nobak");
    let (journal, _) = capture(&st, &Default::default());
    write_session(&session, &journal).unwrap();
    // Simulate "the snapshot file was deleted by antivirus / the user"
    let bak = journal.manifest.tabs[0].backup.clone().unwrap();
    std::fs::remove_file(session.join(&bak)).unwrap();

    let mut st2 = EditorState::minimal();
    st2.pending_recovery = Some(load_session(&session).unwrap());
    st2.restore_recovery_from(&session);
    assert_eq!(
        st2.tabs.len(),
        1,
        "falls back to the disk file, no tab lost"
    );
    assert_eq!(
        st2.tabs[0].doc.read_all(),
        "磁盘上的内容\n",
        "missing snapshot restores the disk state"
    );
    cleanup(&work);
    cleanup(&session);
}

#[test]
fn corrupt_or_foreign_manifests_never_become_a_startup_failure() {
    let session = tmp_work_dir("recovery-corrupt");
    let mut st = EditorState::minimal();

    // Directory doesn't exist / no manifest
    st.check_recovery_in(Some(&session));
    assert!(st.pending_recovery.is_none());

    // Bad JSON
    std::fs::write(session.join(MANIFEST_NAME), "{{ 不是 JSON").unwrap();
    st.check_recovery_in(Some(&session));
    assert!(st.pending_recovery.is_none());

    // Foreign format / newer version / empty tabs
    for body in [
        r#"{"format":"other","version":1,"tabs":[{"doc_id":1,"display_name":"x"}]}"#,
        r#"{"format":"infeditor-recovery","version":999,"tabs":[]}"#,
        r#"{"format":"infeditor-recovery","version":1,"tabs":[]}"#,
    ] {
        std::fs::write(session.join(MANIFEST_NAME), body).unwrap();
        st.check_recovery_in(Some(&session));
        assert!(st.pending_recovery.is_none(), "must ignore: {body}");
    }
    cleanup(&session);
}

#[test]
fn discarded_recovery_removes_every_trace() {
    let (st, work) = dirty_st();
    let session = tmp_work_dir("recovery-discard");
    let (journal, _) = capture(&st, &Default::default());
    write_session(&session, &journal).unwrap();

    let mut st2 = EditorState::minimal();
    st2.check_recovery_in(Some(&session));
    st2.discard_recovery_from(&session);
    assert!(st2.pending_recovery.is_none());
    assert!(
        !session.exists(),
        "discard → the whole session directory cleared"
    );
    cleanup(&work);
}

#[test]
fn journaling_is_throttled_and_signature_driven() {
    let (mut st, work) = dirty_st();
    let session = tmp_work_dir("recovery-throttle");

    // First beat: written to disk
    let t0 = Instant::now();
    st.recovery_tick_in(Some(&session), t0);
    assert!(
        session.join(MANIFEST_NAME).exists(),
        "the first tick must hit disk"
    );
    let manifest_v1 = std::fs::read_to_string(session.join(MANIFEST_NAME)).unwrap();

    // Within the throttle window: no write even though content changed again
    // (loss window = 3 seconds, traded for not dragging down editing)
    st.insert_text("更多编辑");
    st.recovery_tick_in(
        Some(&session),
        t0 + JOURNAL_INTERVAL - std::time::Duration::from_millis(100),
    );
    let manifest_v2 = std::fs::read_to_string(session.join(MANIFEST_NAME)).unwrap();
    assert_eq!(
        manifest_v1, manifest_v2,
        "no rewrite within the throttle window"
    );

    // After the window + signature changed: must write
    st.recovery_tick_in(
        Some(&session),
        t0 + JOURNAL_INTERVAL + std::time::Duration::from_millis(1),
    );
    let manifest_v3 = std::fs::read_to_string(session.join(MANIFEST_NAME)).unwrap();
    assert_ne!(
        manifest_v1, manifest_v3,
        "due and changed content must rewrite"
    );
    let loaded = load_session(&session).expect("new manifest readable");
    assert!(
        loaded.tabs[0].dirty,
        "the manifest records the dirty state faithfully"
    );

    // Signature unchanged: no write even after the window (idle quiescence)
    let manifest_v4 = std::fs::read_to_string(session.join(MANIFEST_NAME)).unwrap();
    st.recovery_tick_in(
        Some(&session),
        t0 + 2 * JOURNAL_INTERVAL + std::time::Duration::from_secs(10),
    );
    assert_eq!(
        manifest_v4,
        std::fs::read_to_string(session.join(MANIFEST_NAME)).unwrap(),
        "no rewrite while the signature is unchanged"
    );
    cleanup(&work);
    cleanup(&session);
}

#[test]
fn normal_exit_clears_the_session_directory() {
    // Use an isolated config directory (test_config_dir points
    // INFEDITOR_CONFIG_DIR into a temp directory), so clear_session_best_effort
    // clears the session/ inside the temp directory.
    let config = common::test_config_dir().to_path_buf();
    let session_dir = config.join("session");
    let _ = std::fs::remove_dir_all(&session_dir);
    std::fs::create_dir_all(&session_dir).unwrap();
    std::fs::write(session_dir.join(MANIFEST_NAME), "{}").unwrap();

    recovery::clear_session_best_effort();
    assert!(
        !session_dir.exists(),
        "a normal exit must not leave session remains (or the next start would offer recovery)"
    );
}

#[test]
fn encoding_and_newline_metadata_survive_the_round_trip() {
    let work = tmp_work_dir("recovery-enc");
    let f = write_file(&work, "enc.txt", "line\r\n");
    let mut st = EditorState::minimal();
    st.open_path(f);
    st.set_encoding("GBK");
    st.set_newline("CRLF");
    st.insert_text("中文编辑");
    let session = tmp_work_dir("recovery-enc-session");
    let (journal, _) = capture(&st, &Default::default());
    write_session(&session, &journal).unwrap();

    let mut st2 = EditorState::minimal();
    st2.pending_recovery = Some(load_session(&session).unwrap());
    st2.restore_recovery_from(&session);
    assert_eq!(st2.tabs[0].doc.newline(), Newline::CrLf, "newline restored");
    let enc = st2.tabs[0].doc.meta.encoding.expect("encoding restored");
    assert_eq!(enc.name(), "GBK");
    assert!(
        st2.tabs[0].doc.read_all().contains("中文编辑"),
        "UTF-8 buffer content is not corrupted by the encoding setting"
    );
    cleanup(&work);
    cleanup(&session);
}

#[test]
fn writing_a_session_with_a_dead_reference_stays_consistent() {
    // When a snapshot references a nonexistent file, the manifest itself must
    // remain readable (recovery falls back to disk); crash-instant consistency
    // is guaranteed by "manifest written last": snapshots land first, the
    // manifest is the commit point.
    let session = tmp_work_dir("recovery-commit");
    let (st, work) = dirty_st();
    let (mut journal, _) = capture(&st, &Default::default());
    // Reference a nonexistent snapshot name
    journal.manifest.tabs[0].backup = Some("backup-999.dat".into());
    journal.backups.clear();
    write_session(&session, &journal).unwrap();
    let loaded = load_session(&session).expect("manifest still readable");
    assert_eq!(loaded.tabs[0].backup.as_deref(), Some("backup-999.dat"));

    let mut st2 = EditorState::minimal();
    st2.pending_recovery = Some(loaded);
    st2.restore_recovery_from(&session);
    assert_eq!(
        st2.tabs.len(),
        1,
        "missing snapshot → fall back to disk, no tab lost"
    );
    cleanup(&work);
    cleanup(&session);
}

#[test]
fn sessions_with_no_tabs_are_silently_ignored_at_startup() {
    let session = tmp_work_dir("recovery-empty");
    let manifest = infeditor::ui::recovery::SessionManifest {
        format: recovery::FORMAT.into(),
        version: recovery::VERSION,
        pid: 0,
        saved_at_ms: 0,
        active: 0,
        scroll_top: 0.0,
        tabs: vec![],
    };
    write_session(
        &session,
        &Journal {
            manifest,
            backups: vec![],
        },
    )
    .unwrap();
    let mut st = EditorState::minimal();
    st.check_recovery_in(Some(&session));
    assert!(
        st.pending_recovery.is_none(),
        "an empty session should not bother the user"
    );
    let _ = clear_session(&session, None);
}

#[test]
fn the_recovery_signature_tracks_document_state() {
    let dir = tmp_work_dir("recovery-sig");
    let f = write_file(&dir, "s.txt", "v1\n");
    let mut st = common::state();
    common::open(&mut st, &f);
    let clean = st.recovery_signature();

    st.insert_text("x");
    let edited = st.recovery_signature();
    assert_ne!(edited, clean, "content change → signature change");

    // The signature tracks more than content: the dirty flag and caret take
    // part too — after undo the content is restored, but "a clean state that
    // was undone to" and "a clean state never edited" are two different
    // session states.
    st.undo();
    assert_ne!(
        st.recovery_signature(),
        edited,
        "undo changes state → signature changes again"
    );
    cleanup(&dir);
}
