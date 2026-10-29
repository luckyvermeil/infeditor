//! Single instance + hand-off of "open this file" from a later launch.
//!
//! # Why
//!
//! Two editors under one user fight over the same per-user state; the visible
//! casualty is the crash-recovery journal (`ui::recovery`): the second instance
//! starts before it has written a journal of its own, finds the first one's, and
//! offers to *restore the previous session* — an alarming prompt for what is
//! simply "the editor is already running". [`platform::instance`] gives the
//! first process a lock; this module is the other half: what a later launch does
//! instead of opening a window.
//!
//! # Hand-off
//!
//! The newcomer writes its command line into `<config dir>/open-queue/` and
//! exits immediately — before creating a window and before it could touch the
//! session journal. The running instance has a watcher thread that notices the
//! file, pokes the window (`WakeFn`, see `EditorState::set_waker`) and the next
//! frame consumes the queue (`EditorState::poll_remote_open_requests`), so a
//! file dropped on `infeditor.exe` in Explorer opens in the window that is
//! already there.
//!
//! A spool **directory** rather than a named pipe / `WM_COPYDATA`: no window
//! handle to look up (the window class belongs to winit), no message-only window
//! to pump, and the same code path works on every platform the editor supports.
//! Delivery is exactly-once — the receiver deletes each request file after
//! reading it — and the failure mode of a crash between write and read is "the
//! file opens later", never "the file is silently dropped".
//!
//! An **empty** request is meaningful: "bring the running window forward"
//! (double-clicking the executable while it already runs).
//!
//! # Test environment
//!
//! Every free function takes an explicit directory ([`submit_in`], [`take_in`],
//! [`has_requests`]); the pathless wrappers are the only ones that touch the real
//! config directory, whose `macros_io::config_dir` is `None` under `cfg(test)` — so unit
//! tests can never disturb a running editor's queue.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crate::ui::state::WakeFn;

/// Sub-directory of the config directory holding pending requests.
pub const QUEUE_DIR: &str = "open-queue";
/// Prefix / suffix of one request file (`req-<n>.txt`).
const PREFIX: &str = "req-";
const SUFFIX: &str = ".txt";
/// Separator between paths inside a request file. NUL is the one byte a path
/// can never contain, so this is lossless for every path we write out (which is
/// UTF-8 after `to_string_lossy`, the same conversion the recent-files list
/// uses).
const SEP: char = '\0';
/// Watcher cadence. 400 ms is below the "did anything happen at all?" threshold
/// for a human watching Explorer, and one `read_dir` four times a second costs
/// nothing next to a repaint.
const WATCH_INTERVAL: Duration = Duration::from_millis(400);

/// A request is waiting and no frame has picked it up yet.
///
/// Written by the watcher thread, read by the idle-frame probe
/// (`EditorState::needs_idle_poll`), so it must be an atomic — the probe runs on
/// the UI thread with no lock and must stay free at idle.
static PENDING: AtomicBool = AtomicBool::new(false);

/// `<config dir>/open-queue`; `None` when there is no config directory (tests).
pub fn queue_dir() -> Option<PathBuf> {
    crate::ui::macros_io::config_dir().map(|d| d.join(QUEUE_DIR))
}

/// Claim **this config directory** for this process. `None` = another editor
/// already owns it (hand over and exit).
///
/// The lock is named after the config directory rather than being one global
/// name: what two instances must not share is the state in that directory
/// (recovery journal, settings, macros). Runs that point
/// `INFEDITOR_CONFIG_DIR` somewhere else — the GUI test suite sets a fresh one
/// per case, portable mode points it at a stick — have their own state and must
/// be allowed to run side by side with the normal editor.
pub fn claim() -> Option<platform::instance::Guard> {
    match platform::instance::acquire(&instance_name()) {
        platform::instance::Acquire::Primary(guard) => Some(guard),
        platform::instance::Acquire::AlreadyRunning => None,
    }
}

/// Stable lock name for the current config directory.
fn instance_name() -> String {
    let key = crate::ui::macros_io::config_dir()
        .map(|d| std::fs::canonicalize(&d).unwrap_or(d))
        .map(|d| d.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    format!("InfEditor-{:016x}", fnv1a(key.as_bytes()))
}

/// FNV-1a: a tiny stable hash. Not security-relevant — the name only has to be
/// unique per directory and identical across two launches of the same build
/// (where `DefaultHasher` would also be fine, but its output is explicitly not
/// stable across releases).
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// Is a hand-off waiting? The idle frame hook's read-only probe: nothing is
/// consumed, nothing is locked, and it is false in steady state.
pub fn pending() -> bool {
    PENDING.load(Ordering::Relaxed)
}

/// Second-instance side: hand `paths` (possibly empty = "just focus") to the
/// running editor. A failure is not fatal — the caller is about to exit and the
/// worst case is "nothing visible happened", which is exactly what the user
/// would see anyway with a second window suppressed.
pub fn submit(paths: &[PathBuf]) {
    if let Some(dir) = queue_dir() {
        let _ = submit_in(&dir, paths);
    }
}

/// [`submit`] against an explicit directory (testable).
pub fn submit_in(dir: &Path, paths: &[PathBuf]) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    // Unique and monotonically ordered: wall-clock millis first so a
    // lexicographic read_dir sorts requests in the order they were made, then
    // the pid and a counter for two launches inside the same millisecond.
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let name = format!(
        "{PREFIX}{millis:013}-{}-{}{SUFFIX}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    );
    let mut body = String::new();
    for p in paths {
        body.push_str(&p.to_string_lossy());
        body.push(SEP);
    }
    // Write-then-rename so a reader can never observe a half-written request
    // (same discipline as the recovery journal, and it also makes the poll's
    // read_dir/read pair race-free).
    let tmp = dir.join(format!("{name}.tmp"));
    std::fs::write(&tmp, body.as_bytes())?;
    std::fs::rename(&tmp, dir.join(&name))
}

/// Is there anything to read? Cheap `read_dir` probe used by the watcher thread.
pub fn has_requests(dir: &Path) -> bool {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return false;
    };
    rd.filter_map(|e| e.ok())
        .any(|e| is_request(&e.file_name().to_string_lossy()))
}

/// Running-instance side: read and delete every queued request, in the order
/// they were written. Unreadable/vanished entries are skipped (another frame
/// may have won the race, or a request was removed by hand).
pub fn take_in(dir: &Path) -> Vec<PathBuf> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = rd
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| is_request(n))
        .collect();
    names.sort();
    let mut out = Vec::new();
    for name in names {
        let path = dir.join(&name);
        if let Ok(body) = std::fs::read_to_string(&path) {
            out.extend(body.split(SEP).filter(|s| !s.is_empty()).map(PathBuf::from));
        }
        // Remove only after a successful read: an unreadable file (permissions,
        // scanner lock) is retried next poll instead of being dropped.
        let _ = std::fs::remove_file(&path);
    }
    out
}

/// Consume the queue (running instance). `None` = nothing was pending, so the
/// caller can skip the whole path without touching disk.
pub fn take_pending() -> Option<Vec<PathBuf>> {
    if !PENDING.load(Ordering::Relaxed) {
        return None;
    }
    let dir = queue_dir();
    let paths = dir.as_deref().map(take_in).unwrap_or_default();
    // Clear after the read: a request that lands in between is caught by the
    // next watcher tick (it re-arms PENDING when the directory is non-empty).
    PENDING.store(false, Ordering::Relaxed);
    Some(paths)
}

/// Start the watcher thread that turns "a request file appeared" into "the
/// window wakes up". Called once, from `EditorState::set_waker` — the point
/// where a way to poke the window first exists.
///
/// The thread never touches editor state: it may only set `PENDING` and call
/// `wake` (which requests a redraw). Everything else happens on the UI thread in
/// `poll_remote_open_requests`.
pub fn spawn_watcher(wake: WakeFn) {
    static STARTED: AtomicBool = AtomicBool::new(false);
    let Some(dir) = queue_dir() else {
        return;
    };
    if STARTED.swap(true, Ordering::Relaxed) {
        return;
    }
    let spawned = std::thread::Builder::new()
        .name("infeditor-open-queue".to_string())
        .spawn(move || loop {
            std::thread::sleep(WATCH_INTERVAL);
            crate::ui::guard::worker("ui::single::watch", || {
                if !has_requests(&dir) {
                    return;
                }
                // Wake once per arrival: an already-set PENDING means a frame
                // has been asked for and has not run yet.
                if !PENDING.swap(true, Ordering::Relaxed) {
                    wake();
                }
            });
        });
    if spawned.is_err() {
        // No thread: single-instance still holds, only the *hand-off* degrades
        // to "the request waits until the window is used for something else"
        // (the idle poll drains it whenever it runs).
        STARTED.store(false, Ordering::Relaxed);
    }
}

fn is_request(name: &str) -> bool {
    name.starts_with(PREFIX) && name.ends_with(SUFFIX)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        use std::sync::atomic::AtomicU32;
        static SEQ: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "infeditor-open-queue-{}-{tag}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    /// Round trip: what a second launch wrote is exactly what the running
    /// instance opens, in order, including paths with spaces and CJK.
    #[test]
    fn submit_then_take_round_trips_in_order() {
        let dir = scratch("roundtrip");
        let a = PathBuf::from("C:\\Users\\me\\我的 笔记.md");
        let b = PathBuf::from("C:\\tmp\\b.txt");
        submit_in(&dir, std::slice::from_ref(&a)).expect("first request");
        submit_in(&dir, std::slice::from_ref(&b)).expect("second request");
        assert!(has_requests(&dir));

        let got = take_in(&dir);
        assert_eq!(got, vec![a, b]);
        assert!(!has_requests(&dir), "requests must be consumed");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An empty request is legal and must survive the round trip: it means
    /// "bring the window forward", not "nothing to do".
    #[test]
    fn empty_request_is_a_focus_request() {
        let dir = scratch("focus");
        submit_in(&dir, &[]).expect("focus request");
        assert!(has_requests(&dir));
        assert_eq!(take_in(&dir), Vec::<PathBuf>::new());
        assert!(!has_requests(&dir));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A missing queue directory is "nothing pending", not an error: the very
    /// first launch has never created it.
    #[test]
    fn missing_directory_reads_as_empty() {
        let dir = scratch("missing");
        assert!(!has_requests(&dir));
        assert!(take_in(&dir).is_empty());
    }

    /// Non-request debris in the directory (a `.tmp` orphan from a crash, or
    /// somebody else's file) must be ignored and left alone.
    #[test]
    fn foreign_files_are_ignored() {
        let dir = scratch("foreign");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("notes.txt"), "hello").unwrap();
        std::fs::write(dir.join("req-000.tmp"), "x").unwrap();
        assert!(!has_requests(&dir));
        assert!(take_in(&dir).is_empty());
        assert!(dir.join("notes.txt").exists(), "not ours to delete");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
