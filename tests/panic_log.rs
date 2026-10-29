//! `crash.log` is the only evidence a GUI-subsystem build leaves behind, so its
//! format is part of the contract: a record must be greppable and self-describing
//! even when the writer runs inside a panic hook.
//!
//! The lib is compiled without `cfg(test)` here, which is exactly what makes
//! `config_dir()` honor `INFEDITOR_CONFIG_DIR` — the unit tests cannot cover the
//! file path because they force `config_dir()` to `None`.

use std::io::Read;

/// One test on purpose: it mutates the process-wide panic hook and the config-dir
/// env var, and the two records must be read back in order.
#[test]
fn panic_records_land_in_crash_log() {
    let dir = std::env::temp_dir().join(format!("infeditor-paniclog-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    std::env::set_var("INFEDITOR_CONFIG_DIR", &dir);

    // 1) A recovered panic: the firewall's annotation must name its scope.
    infeditor::panic_log::record_recovered("tests::panic_log", "synthetic payload");
    assert_eq!(infeditor::panic_log::recovered_count(), 1);

    // 2) A real panic through the same hook `main.rs` installs: payload, location
    //    and an auto-captured backtrace land in the file without RUST_BACKTRACE.
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(infeditor::panic_log::record_panic));
    let caught = std::panic::catch_unwind(|| panic!("synthetic crash-log panic"));
    std::panic::set_hook(previous);
    assert!(caught.is_err(), "the panic must reach catch_unwind");

    let mut text = String::new();
    std::fs::File::open(dir.join("crash.log"))
        .expect("crash.log must be created")
        .read_to_string(&mut text)
        .expect("crash.log must be readable");

    assert!(text.contains("--- recovered at unix "), "{text}");
    assert!(text.contains("scope tests::panic_log"), "{text}");
    assert!(text.contains("synthetic payload"), "{text}");
    assert!(text.contains("total 1"), "{text}");
    assert!(text.contains(" local | "), "wall clock missing: {text}");

    assert!(text.contains("--- panic at unix "), "{text}");
    assert!(text.contains("synthetic crash-log panic"), "{text}");
    assert!(text.contains("pid "), "process id missing: {text}");
    assert!(
        text.contains("--- backtrace\n"),
        "backtrace missing: {text}"
    );
    assert!(text.contains("--- end"), "{text}");
    // The hook records the location as a compile-time literal; a mid-test panic
    // must therefore point into this file.
    assert!(
        text.contains("tests\\panic_log.rs") || text.contains("tests/panic_log.rs"),
        "source location missing: {text}"
    );

    // Inspect the real record (including what symbolization yields in this profile)
    // by running the test with INFEDITOR_LOG=1 -- --nocapture.
    if std::env::var_os("INFEDITOR_LOG").is_some() {
        eprintln!("--- crash.log ---\n{text}");
    }

    std::env::remove_var("INFEDITOR_CONFIG_DIR");
    let _ = std::fs::remove_dir_all(&dir);
}
