//! Panic telemetry: every panic — fatal or swallowed — lands in `crash.log`.
//!
//! A release build is a `windows_subsystem = "windows"` process with no stderr
//! receiver, so the file is the only evidence. Two writers share it:
//!
//! * the panic hook (`main.rs`) records **every** panic before unwinding starts:
//!   wall-clock stamp, pid, os/arch, thread, `file:line:column`, payload and a
//!   backtrace captured right there (`Backtrace::force_capture`, so nobody has
//!   to re-run with `RUST_BACKTRACE=1`);
//! * the firewalls (`crate::ui::guard::worker`, winshell's wndproc) append a
//!   `--- recovered` line naming the scope and the running total — that is what
//!   tells "the app survived this one" apart from a fatal record.
//!
//! Reading one record answers: what broke, where (source location *and* call
//! path), on which thread, in which process — and whether the process lived.
//!
//! Backtraces need debug info: release builds keep line tables
//! (`debug = "line-tables-only"` in the workspace `Cargo.toml`). On Windows the
//! symbols live in the `.pdb` next to the executable: local builds write one,
//! so frames resolve; a shipped `.exe` without its `.pdb` still gets this
//! crate's `file:line` (recorded by the hook as compile-time literals) but the
//! call path degrades to addresses.

use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering};

/// Panics caught and swallowed by a firewall since process start.
static RECOVERED: AtomicU64 = AtomicU64::new(0);

/// How many panics have been recovered from since start.
pub fn recovered_count() -> u64 {
    RECOVERED.load(Ordering::Relaxed)
}

/// Human-readable panic payload (`panic!("…")` / `panic!("{x}")` / anything else).
pub fn payload_str(payload: &(dyn std::any::Any + Send)) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|s| (*s).to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "<non-string panic payload>".into())
}

/// Append one full panic record (called from the panic hook in `main.rs`).
///
/// Runs inside the panic hook, i.e. **before** unwinding: any error here must be
/// swallowed (see `append`), and the backtrace is captured on this thread.
pub fn record_panic(info: &std::panic::PanicHookInfo<'_>) {
    let secs = unix_secs();
    let (y, mo, d, h, mi, s) = platform::local_civil(secs as i64);
    let payload = payload_str(info.payload());
    let location = info
        .location()
        .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
        .unwrap_or_else(|| "<unknown>".into());
    let thread = std::thread::current();
    let mut text = format!(
        "--- panic at unix {secs} | {y:04}-{mo:02}-{d:02} {h:02}:{mi:02}:{s:02} local \
         | infeditor {} | {}/{} | pid {} | recovered-so-far {} | thread {:?} | {location}\n\
         {payload}\n",
        env!("CARGO_PKG_VERSION"),
        std::env::consts::OS,
        std::env::consts::ARCH,
        std::process::id(),
        recovered_count(),
        thread.name().unwrap_or("<unnamed>"),
    );
    // Always capture: a GUI user cannot reproduce with an env var, and the trace is
    // the difference between "somewhere in the renderer" and a fixable bug.
    let mut bt = std::backtrace::Backtrace::force_capture().to_string();
    bt = renumber_frames(&trim_hook_frames(&bt));
    cap_backtrace(&mut bt);
    text.push_str("--- backtrace\n");
    text.push_str(&bt);
    if !bt.ends_with('\n') {
        text.push('\n');
    }
    text.push_str("--- end\n");
    append(&text);
}

/// Annotate that `scope` caught a panic and kept the process alive.
///
/// The matching `--- panic` record (with location and backtrace) was already
/// written by the hook; this line adds *who swallowed it* and the running total,
/// so `grep recovered crash.log` shows what is being papered over without losing
/// the original evidence.
pub fn record_recovered(scope: &str, payload: &str) {
    let total = RECOVERED.fetch_add(1, Ordering::Relaxed) + 1;
    let secs = unix_secs();
    let (y, mo, d, h, mi, s) = platform::local_civil(secs as i64);
    let thread = std::thread::current();
    append(&format!(
        "--- recovered at unix {secs} | {y:04}-{mo:02}-{d:02} {h:02}:{mi:02}:{s:02} local \
         | pid {} | scope {scope} | thread {:?} | total {total}\n{payload}\n",
        std::process::id(),
        thread.name().unwrap_or("<unnamed>"),
    ));
}

fn unix_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Drop the leading frames that belong to the panic machinery itself
/// (`force_capture` → `record_panic` → `panic_fmt` …), so the first frame in the
/// log is the code that actually panicked. Later frames are kept untouched — a
/// `catch_unwind` deeper in the path is context, not noise.
///
/// The test is the **symbol**, not the source path: std's frames resolve to the
/// rustup toolchain directory on a developer machine and to `/rustc/…` in other
/// builds, so path matching missed half of them (observed: the log opened at
/// `std::env::set_var::closure$0`). Anything outside `std`/`core`/`alloc` stops
/// the trim, which keeps the application's own `catch_unwind` frames visible.
fn trim_hook_frames(bt: &str) -> String {
    let lines: Vec<&str> = bt.lines().collect();
    let mut i = 0usize;
    while i < lines.len() {
        let line = lines[i].trim_start();
        // A frame header is "   3: some::symbol" (the `at …` line is not).
        if !line.starts_with(|c: char| c.is_ascii_digit()) {
            i += 1;
            continue;
        }
        let Some((_, symbol)) = line.split_once(": ") else {
            i += 1;
            continue;
        };
        let machinery = symbol.starts_with("std::")
            || symbol.starts_with("core::")
            || symbol.starts_with("alloc::")
            || symbol.starts_with("rustc_")
            || symbol.contains("infeditor::panic_log::record_panic");
        if !machinery {
            return lines[i..].join("\n");
        }
        i += 1;
        // The frame's `at file:line` continuation line.
        if i < lines.len() && lines[i].trim_start().starts_with("at ") {
            i += 1;
        }
    }
    // Everything matched (a panic inside the hook itself): keep the original trace.
    bt.to_string()
}

/// Renumber frame headers after the trim so the log starts at `0:`. A record that
/// opens at `13:` reads like a truncated file and invites "where are the rest?".
fn renumber_frames(bt: &str) -> String {
    let mut out = String::with_capacity(bt.len() + 8);
    let mut n = 0usize;
    for line in bt.lines() {
        let header = line.trim_start();
        let is_frame =
            header.starts_with(|c: char| c.is_ascii_digit()) && header.split_once(": ").is_some();
        if is_frame {
            let (_, symbol) = header.split_once(": ").unwrap_or(("", header));
            out.push_str(&format!("{n:4}: {symbol}\n"));
            n += 1;
        } else {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

/// Keep a backtrace bounded: a pathological panic (deep recursion) can produce a
/// multi-megabyte trace, and the log is rotated at 256 KB anyway.
fn cap_backtrace(text: &mut String) {
    const MAX: usize = 16 * 1024;
    if text.len() <= MAX {
        return;
    }
    let mut cut = 0usize;
    for (i, c) in text.char_indices() {
        if i > MAX {
            break;
        }
        cut = i + c.len_utf8();
    }
    text.truncate(cut);
    text.push_str("\n... (backtrace truncated)");
}

/// Append one record; rotate a `.1` copy when the log exceeds the cap. Any IO failure
/// is silent — the fallback of the fallback must not crash again.
fn append(text: &str) {
    if std::env::var_os("INFEDITOR_LOG").is_some() {
        eprintln!("{text}");
    }
    // Records now carry a backtrace (~10 KB each); 512 KB keeps roughly the last
    // 30 panics before the `.1` rotation — enough to see a crash burst.
    const CRASH_LOG_MAX: u64 = 512 * 1024;
    let Some(path) = crash_log_path() else {
        return;
    };
    if std::fs::metadata(&path).is_ok_and(|m| m.len() > CRASH_LOG_MAX) {
        let _ = std::fs::rename(&path, path.with_file_name("crash.log.1"));
    }
    let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    else {
        return;
    };
    let _ = f.write_all(text.as_bytes());
    // Flush explicitly: the fatal path may abort right after the hook returns.
    let _ = f.flush();
}

fn crash_log_path() -> Option<std::path::PathBuf> {
    let dir = crate::ui::macros_io::config_dir()?;
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir.join("crash.log"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The log must open at the panicking code, not at seven frames of hook
    /// machinery — that noise is what makes a crash log unreadable in a report.
    #[test]
    fn backtrace_trim_starts_at_the_panicking_code() {
        let bt = concat!(
            "   0: std::backtrace::Backtrace::force_capture\n",
            "             at /rustc/hash/library\\std\\src\\backtrace.rs:312\n",
            "   1: infeditor::panic_log::record_panic\n",
            "             at .\\src\\panic_log.rs:70\n",
            "   2: std::panicking::panic_with_hook\n",
            "             at /rustc/hash/library\\std\\src\\panicking.rs:823\n",
            "   3: std::panicking::begin_panic_handler::closure$0\n",
            "             at /rustc/hash/library\\std\\src\\panicking.rs:681\n",
            "   4: infeditor::buffer::doc::edit\n",
            "             at .\\src\\buffer\\doc.rs:42\n",
            "   5: std::panicking::catch_unwind\n",
            "             at /rustc/hash/library\\std\\src\\panicking.rs:544",
        );
        let out = trim_hook_frames(bt);
        assert!(
            out.starts_with("   4: infeditor::buffer::doc::edit"),
            "{out}"
        );
        assert!(
            out.contains("5: std::panicking::catch_unwind"),
            "frames after the panic site stay untouched: {out}"
        );
    }

    /// An unrecognized trace (no hook frames at all) must pass through unchanged.
    #[test]
    fn backtrace_trim_keeps_unrecognized_traces() {
        let bt = "   0: my_crate::boom\n             at .\\src\\lib.rs:1";
        assert_eq!(trim_hook_frames(bt), bt);
    }

    /// A trace made only of machinery is kept whole rather than emptied.
    #[test]
    fn backtrace_trim_never_empties_a_trace() {
        let bt = concat!(
            "   0: std::backtrace::Backtrace::force_capture\n",
            "             at /rustc/hash/library\\std\\src\\backtrace.rs:312",
        );
        assert_eq!(trim_hook_frames(bt), bt);
    }

    /// Regression: std frames resolve through the rustup toolchain path on a
    /// developer machine, and an unresolved frame has no `at` line at all — both
    /// used to survive the trim and push the real panic site down the log.
    #[test]
    fn backtrace_trim_does_not_depend_on_the_source_path() {
        let bt = concat!(
            "   0: std::env::set_var::closure$0<ref$<str$>,ref$<std::path::PathBuf> >\n",
            "   1: std::panicking::catch_unwind\n",
            "             at C:/Users/dev/.rustup/toolchains/stable/library/std/src/panicking.rs:544\n",
            "   2: my_app::run\n",
            "             at ./src/main.rs:10",
        );
        let out = trim_hook_frames(bt);
        assert!(out.starts_with("   2: my_app::run"), "{out}");
    }

    /// After trimming, frames start at 0 again — a log opening at `13:` looks like
    /// a corrupted file and invites "where are the rest?".
    #[test]
    fn backtrace_frames_are_renumbered_after_trimming() {
        let bt = concat!(
            "   0: std::panicking::panic_fmt\n",
            "             at /rustc/hash/library\\std\\src\\panicking.rs:1\n",
            "   3: my_app::run\n",
            "             at ./src/main.rs:10\n",
            "   4: main\n",
            "             at ./src/main.rs:3",
        );
        let out = renumber_frames(&trim_hook_frames(bt));
        assert!(out.starts_with("   0: my_app::run"), "{out}");
        assert!(out.contains("   1: main"), "{out}");
    }
}
