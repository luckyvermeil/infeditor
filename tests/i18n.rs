//! Integration tests for the UI language.
//!
//! State only, no window: every visible effect of a language switch comes from
//! `EditorState::locale`, and the menu, status bar and dialogs are pure functions
//! of it.
//!
//! Two themes:
//!
//! 1. **It changes on switch** — the same state renders in the other language, and
//!    not only after a restart;
//! 2. **Derived strings follow** — the untitled placeholder name is derived, so it
//!    must be recomputed. This is the easy one to miss, because a rendered string
//!    was stored in document state.
//!
//! The process mirror (`i18n::current()`) is asserted in one test only, under a
//! mutex: it is process-wide, and parallel tests writing it would make the
//! assertion flaky.

mod common;

use infeditor::ui::actions::{self, Caption};
use infeditor::ui::settings::{self, AppSettings};
use infeditor::ui::state::EditorState;
use infeditor::ui::Fmt;

use infeditor::ui::{Locale, Msg};

/// `EditorState::set_locale` writes the process-wide mirror as a side effect,
/// and `load_settings` writes it too — tests in this file run in parallel
/// threads of ONE process, so every test that switches language holds this
/// lock. (A per-test static once guarded only the mirror test against
/// itself, which is no serialization at all — flaked under full runs.)
static LOCALE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn switching_locale_retranslates_menu_captions_and_items() {
    let _guard = LOCALE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut st = EditorState::minimal();
    assert_eq!(st.tr(Msg::MenuFile), "文件");

    st.set_locale(Locale::EnUs);

    assert_eq!(st.tr(Msg::MenuFile), "File");
    let caps: Vec<String> = actions::menu_bar_items(&st)
        .iter()
        .map(|i| i.label().to_string())
        .collect();
    assert_eq!(caps[0], "File");
    assert_eq!(caps[1], "Edit");
    assert_eq!(caps[3], "View");

    // Leaves follow too: the first entry of the File menu.
    let file_items = actions::menu_bar_items(&st)
        .into_iter()
        .nth(Caption::File.index())
        .expect("File menu exists");
    let labels: Vec<String> = match file_items {
        actions::Item::Sub { items, .. } => items.iter().map(|i| i.label().to_string()).collect(),
        _ => panic!("File must be a submenu"),
    };
    assert_eq!(
        labels[0], "New",
        "first entry should be New, got {labels:?}"
    );

    st.set_locale(Locale::ZhCn);
    assert_eq!(st.tr(Msg::MenuFile), "文件");
}

#[test]
fn parameterized_messages_reorder_their_words_per_language() {
    let _guard = LOCALE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut st = EditorState::minimal();
    assert_eq!(st.tr(Fmt::StatusLineCol { line: 7, col: 3 }), "行 7, 列 3");

    st.set_locale(Locale::EnUs);
    assert_eq!(st.tr(Fmt::StatusLineCol { line: 7, col: 3 }), "Ln 7, Col 3");

    // English verb agreement is handled inside the en arm with `i18n::plural`;
    // Chinese has no inflection.
    let one = st.tr(Fmt::DlgQuitBody { dirty: 1 });
    assert_eq!(one, "1 document has unsaved changes. Quit anyway?");
    let many = st.tr(Fmt::DlgQuitBody { dirty: 3 });
    assert_eq!(many, "3 documents have unsaved changes. Quit anyway?");

    // Chinese puts the measure word before the noun and has no plural: the same
    // `Fmt` value, two different sentence shapes.
    st.set_locale(Locale::ZhCn);
    assert_eq!(
        st.tr(Fmt::DlgQuitBody { dirty: 3 }),
        "有 3 个文档尚未保存，确定要退出吗？"
    );
}

#[test]
fn untitled_document_names_are_derived_and_follow_the_locale() {
    let _guard = LOCALE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut st = EditorState::minimal();
    st.new_doc();
    let names: Vec<String> = st
        .tabs
        .iter()
        .map(|t| t.doc.meta.display_name.clone())
        .collect();
    assert_eq!(names, vec!["未命名", "未命名2"]);

    st.set_locale(Locale::EnUs);
    let names: Vec<String> = st
        .tabs
        .iter()
        .map(|t| t.doc.meta.display_name.clone())
        .collect();
    assert_eq!(names, vec!["Untitled", "Untitled 2"], "must be recomputed");

    // Switching back has to recognize "Untitled 2": an implementation that only
    // knows the current language loses the number here.
    st.set_locale(Locale::ZhCn);
    let names: Vec<String> = st
        .tabs
        .iter()
        .map(|t| t.doc.meta.display_name.clone())
        .collect();
    assert_eq!(names, vec!["未命名", "未命名2"]);
}

#[test]
fn a_language_choice_is_persisted_as_a_tag_not_an_index() {
    let _guard = LOCALE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut st = EditorState::minimal();
    st.set_locale(Locale::EnUs);
    let captured = AppSettings::capture(&st);
    assert_eq!(captured.locale.as_deref(), Some("en-US"));

    // What is written is the tag; the next launch reads it back (a settings round
    // trip).
    let back = AppSettings {
        locale: captured.locale.clone(),
        ..AppSettings::default()
    };
    let mut fresh = EditorState::minimal();
    back.apply(&mut fresh);
    assert_eq!(fresh.locale(), Locale::EnUs);

    // An unknown tag must not blank the UI: it counts as "never chosen", i.e. it
    // falls through to the same startup probe as `None` (env var → system
    // language → default). The property to pin is "same answer as never chosen",
    // not `Locale::DEFAULT`: on an English runner the probe legitimately answers
    // `EnUs`, and asserting the default would encode this machine's language.
    let odd = AppSettings {
        locale: Some("xx-YY".into()),
        ..AppSettings::default()
    };
    let mut st2 = EditorState::minimal();
    odd.apply(&mut st2);
    assert_eq!(st2.locale(), settings::resolve_locale(None));
}

/// A language switch closes the menu: the open state is a caption identity, but
/// the **fly-out** is still located by row position, and a stale position would
/// fly out a different submenu.
#[test]
fn switching_locale_closes_an_open_menu() {
    let _guard = LOCALE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut st = EditorState::minimal();
    st.menu_toggle(Caption::Edit, 10.0);
    assert!(st.menu.open.is_some());
    st.set_locale(Locale::EnUs);
    assert!(st.menu.open.is_none(), "the menu must close on a switch");
    assert!(st.menu.sub.is_none());
}

/// The process mirror never diverges from the state: `EditorState::set_locale` is
/// its only writer, and this pins that.
///
/// A mutex serializes the process-wide assertion; other tests write it too.
#[test]
fn process_mirror_tracks_the_state() {
    let _guard = LOCALE_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    let mut st = EditorState::minimal();
    st.set_locale(Locale::EnUs);
    assert_eq!(
        i18n::current(),
        Locale::EnUs,
        "mirror must follow the state"
    );
    st.set_locale(Locale::ZhCn);
    assert_eq!(i18n::current(), Locale::ZhCn);
    // Repeating the same language is a no-op; state and mirror still agree.
    st.set_locale(Locale::ZhCn);
    assert_eq!(i18n::current(), Locale::ZhCn);
}

/// The Preferences language page produces `Cmd::SetLocale`, so the switch takes
/// the same dispatch path as every other command instead of a private channel.
#[test]
fn the_prefs_language_page_routes_through_the_command_bus() {
    // Isolate the config dir first: a language switch is a settings command, so
    // the dispatcher writes settings.json at the end.
    let _guard = LOCALE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let config = common::test_config_dir().clone();
    let mut st = EditorState::minimal();
    actions::run(&mut st, actions::Cmd::SetLocale(Locale::EnUs));
    assert_eq!(
        st.locale(),
        Locale::EnUs,
        "the command must reach the state"
    );

    let text = std::fs::read_to_string(config.join("settings.json")).expect("settings.json");
    assert!(
        text.contains("\"en-US\""),
        "settings.json should carry en-US, got: {text}"
    );

    // Reset so the process mirror is left on the default for other tests.
    actions::run(&mut st, actions::Cmd::SetLocale(Locale::ZhCn));
}
