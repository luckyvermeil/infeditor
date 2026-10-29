//! Common facilities for integration tests.
//!
//! # These tests are **headless**
//!
//! Every case under `tests/` only drives `EditorState` / command dispatch / the
//! document model; no window is created and no keyboard/mouse input is injected —
//! they are integration tests of the "logic layer + state machine", and they run
//! as fast as ordinary unit tests. Real GUI end-to-end testing (real process,
//! real keyboard, screenshots/UIA) lives in `tests_py/`, and requires
//! `cargo build --release` before running.
//!
//! # Isolation principle (safety first)
//!
//! Integration tests link against the **normally compiled** library: `cfg!(test)`
//! is false inside the library, so `config_dir()` returns the real
//! `%APPDATA%\InfEditor`. Any path that could touch the config directory
//! (`persist_settings`, parameterless `check_recovery`, etc.) must first have
//! this module point `INFEDITOR_CONFIG_DIR` at a temporary directory; all other
//! scenarios use the `*_in` variants with an explicit directory parameter
//! (`recovery_tick_in` / `check_recovery_in` / ...).
//!
//! Iron rule: **tests must never read or write the user's real files**.

// Each test binary only uses a subset of these; unused helpers are fine.
#![allow(dead_code)]

use infeditor::ui::actions::{self, Cmd};
use infeditor::ui::state::EditorState;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

// ---------------------------------------------------------------------------
// Directory isolation
// ---------------------------------------------------------------------------

/// An isolated config directory, one per test-binary process.
pub fn test_config_dir() -> &'static PathBuf {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        let root = std::env::temp_dir().join(format!("infeditor-it-{}", std::process::id()));
        let config = root.join("config");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&config).unwrap();
        // The library's config_dir() reads this first → persist_settings /
        // session / macros all land in the temp directory, fully isolated from
        // the user's real configuration.
        std::env::set_var("INFEDITOR_CONFIG_DIR", &config);
        config
    })
}

/// A test-specific temporary working directory (kept separate from the config
/// directory; the test cleans it up itself when done).
///
/// Every call within the same process is unique (atomic counter): parallel
/// tests using the same tag won't delete each other's directories — tests A's
/// cleanup once deleted test B's input file because of this.
pub fn tmp_work_dir(tag: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let d = std::env::temp_dir().join(format!(
        "infeditor-it-work-{}-{tag}-{n}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// Writes `content` to `dir/name` and returns the full path.
pub fn write_file(dir: &Path, name: &str, content: &str) -> PathBuf {
    let p = dir.join(name);
    std::fs::write(&p, content).unwrap();
    p
}

/// The only allowed "outside the current directory" path assertion helper in
/// tests: clear a directory, ignoring failures.
pub fn cleanup(dir: &Path) {
    let _ = std::fs::remove_dir_all(dir);
}

// ---------------------------------------------------------------------------
// Editor state
// ---------------------------------------------------------------------------

/// A brand-new blank editor state with the config directory isolated.
///
/// This is the starting point for most cases; call it again when "restart"
/// semantics are needed (multiple states in the same process share no memory;
/// only the config files are shared).
pub fn state() -> EditorState {
    let _ = test_config_dir();
    EditorState::minimal()
}

/// Opens `path` and asserts success: no error reported, and the active tab is it.
pub fn open(st: &mut EditorState, path: &Path) {
    st.open_path(path.to_path_buf());
    assert!(
        !st.status_text().contains("打开失败"),
        "failed to open {}: {}",
        path.display(),
        st.status_text()
    );
    assert_eq!(
        st.active_doc().meta.path.as_deref(),
        Some(path),
        "the active tab should be the just-opened document"
    );
}

/// Writes `content` to `dir/name` → opens it → returns `(state, file path)`.
pub fn open_file(dir: &Path, name: &str, content: &str) -> (EditorState, PathBuf) {
    let path = write_file(dir, name, content);
    let mut st = state();
    open(&mut st, &path);
    (st, path)
}

/// Runs a command through command dispatch (the same path as menus / shortcuts).
pub fn run(st: &mut EditorState, cmd: Cmd) {
    actions::run(st, cmd);
}
