//! Hard-coded UI text is a **ratchet**: it may only go down.
//!
//! The migration cannot land in one go (about 600 user-visible strings across
//! several crates), and an unmeasured "migrate in batches" plan rots. This test
//! records a baseline per source file:
//!
//! * above baseline → fail, listing the files and the excess (new UI text belongs
//!   in `crates/i18n/src/catalog.rs`, not inline);
//! * below baseline → also a failure, telling you to **lower** the number (a
//!   finished file is settled here).
//!
//! Baselines are printed by `INF_I18N_BASELINE=1`; paste them back, no counting.
//!
//! # What is counted
//!
//! String literals a user can see, not all Chinese:
//!
//! * comments do not count;
//! * `#[cfg(test)] mod …` does not count (assertions are not UI);
//! * a literal counts when it contains a CJK character — `"UTF-8"`, `"Ctrl+B"`,
//!   `"Vim"` are not text.
//!
//! The scanner is approximate, not a full Rust lexer: line comments, block
//! comments, plain strings, escapes and `r#"…"#`. It would rather undercount (a
//! `//` inside a string) than misjudge a whole file — the baseline is a trend
//! tool, not forensics.

use std::path::{Path, PathBuf};

/// Hard-coded strings currently allowed per file (a finished file goes to 0).
const BASELINE: &[(&str, usize)] = &[
    ("src/jsonl.rs", 17),
    ("src/highlight/code.rs", 1),
    ("src/highlight/lang.rs", 1),
    ("src/ui/commands/markdown.rs", 1),
];
// total = 20

fn is_cjk(c: char) -> bool {
    matches!(c as u32, 0x3400..=0x4DBF | 0x4E00..=0x9FFF | 0xF900..=0xFAFF)
}

/// The source with comments and test modules removed.
fn visible_code(src: &str) -> String {
    let b: Vec<char> = src.chars().collect();
    let mut out = String::with_capacity(src.len());
    let mut i = 0usize;
    while i < b.len() {
        // Line comments: `//`, `///` and `//!` all count as comments.
        if b[i] == '/' && i + 1 < b.len() && b[i + 1] == '/' {
            while i < b.len() && b[i] != '\n' {
                i += 1;
            }
            continue;
        }
        // Block comments (nested, as Rust allows).
        if b[i] == '/' && i + 1 < b.len() && b[i + 1] == '*' {
            let mut depth = 1usize;
            i += 2;
            while i < b.len() && depth > 0 {
                if b[i] == '/' && i + 1 < b.len() && b[i + 1] == '*' {
                    depth += 1;
                    i += 2;
                } else if b[i] == '*' && i + 1 < b.len() && b[i + 1] == '/' {
                    depth -= 1;
                    i += 2;
                } else {
                    i += 1;
                }
            }
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    out
}

/// Reads one string literal starting at `i`; returns its content and end offset.
fn string_literal(b: &[char], mut i: usize) -> (String, usize) {
    // Raw string: r"…" / r#"…"# (`i` points at the `r`).
    let mut hash = 0usize;
    let mut j = i;
    if b[j] == 'r' {
        j += 1;
        while j < b.len() && b[j] == '#' {
            hash += 1;
            j += 1;
        }
        if j < b.len() && b[j] == '"' {
            j += 1;
            let mut s = String::new();
            while j < b.len() {
                if b[j] == '"' {
                    let mut k = 0usize;
                    while k < hash && j + 1 + k < b.len() && b[j + 1 + k] == '#' {
                        k += 1;
                    }
                    if k == hash {
                        return (s, j + 1 + hash);
                    }
                }
                s.push(b[j]);
                j += 1;
            }
            return (s, b.len());
        }
    }
    // Plain string.
    i += 1;
    let mut s = String::new();
    while i < b.len() {
        match b[i] {
            '\\' => {
                i += 1;
                s.push(' ');
                i += 1;
                continue;
            }
            '"' => return (s, i + 1),
            c => s.push(c),
        }
        i += 1;
    }
    (s, b.len())
}

/// Number of string literals in `src` that contain a CJK character.
fn cjk_literals(src: &str) -> usize {
    let code = visible_code(src);
    let b: Vec<char> = code.chars().collect();
    let mut n = 0usize;
    let mut i = 0usize;
    while i < b.len() {
        // Raw string (r"…" / r#"…"):
        let raw_start = b[i] == 'r' && b.get(i + 1).is_some_and(|c| *c == '"' || *c == '#');
        if raw_start {
            let (s, next) = string_literal(&b, i);
            if s.chars().any(is_cjk) {
                n += 1;
            }
            i = next;
            continue;
        }
        if b[i] == '"' {
            let (s, next) = string_literal(&b, i);
            if s.chars().any(is_cjk) {
                n += 1;
            }
            i = next;
            continue;
        }
        i += 1;
    }
    n
}

/// Drops everything from the first `#[cfg(test)]` on.
///
/// Only the module-level form is handled: every test module in this repo sits at
/// the end of its file, so cutting at the marker is enough.
fn without_tests(src: &str) -> &str {
    match src.find("#[cfg(test)]") {
        Some(i) => &src[..i],
        None => src,
    }
}

fn collect(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            // The catalog itself is all Chinese by design; vendor is third-party code.
            if p.file_name().is_some_and(|n| n == "i18n" || n == "vendor") {
                continue;
            }
            collect(&p, out);
        } else if p.extension().is_some_and(|e| e == "rs") {
            // `tests.rs` is a test module split out of its parent file; its Chinese
            // literals are expected values, not UI text. `without_tests` only strips
            // the in-file form.
            if p.file_name().is_some_and(|n| n == "tests.rs") {
                continue;
            }
            out.push(p);
        }
    }
}

fn scan(root: &Path) -> Vec<(String, usize)> {
    let mut files = Vec::new();
    collect(&root.join("src"), &mut files);
    let crates = root.join("crates");
    if let Ok(rd) = std::fs::read_dir(&crates) {
        for e in rd.flatten() {
            let p = e.path();
            // The catalog crate does not take part: its literals *are* the translations.
            if p.is_dir() && p.file_name().is_some_and(|n| n != "i18n") {
                collect(&p.join("src"), &mut files);
            }
        }
    }
    let mut rows: Vec<(String, usize)> = files
        .into_iter()
        .filter_map(|p| {
            let src = std::fs::read_to_string(&p).ok()?;
            let n = cjk_literals(without_tests(&src));
            if n == 0 {
                return None;
            }
            let rel = p
                .strip_prefix(root)
                .unwrap_or(&p)
                .to_string_lossy()
                .replace('\\', "/");
            Some((rel, n))
        })
        .collect();
    rows.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    rows
}

#[test]
fn hardcoded_ui_text_only_ever_goes_down() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let actual = scan(&root);

    if std::env::var("INF_I18N_BASELINE").is_ok() {
        let total: usize = actual.iter().map(|(_, n)| n).sum();
        println!("const BASELINE: &[(&str, usize)] = &[");
        for (f, n) in &actual {
            println!("    (\"{f}\", {n}),");
        }
        println!("];");
        println!("// total = {total}");
        return;
    }

    let mut over: Vec<String> = Vec::new();
    let mut under: Vec<String> = Vec::new();
    let mut baseline_total = 0usize;
    for (f, n) in &actual {
        let allowed = BASELINE.iter().find(|(bf, _)| bf == f).map(|(_, bn)| *bn);
        baseline_total += allowed.unwrap_or(0);
        match allowed {
            None => over.push(format!("  {f}: {n} (not in the baseline)")),
            Some(b) if n > &b => over.push(format!("  {f}: {n} > {b}")),
            Some(b) if n < &b => under.push(format!("  {f}: {n} < {b}")),
            _ => {}
        }
    }
    let actual_total: usize = actual.iter().map(|(_, n)| n).sum();

    assert!(
        over.is_empty(),
        "More hard-coded UI text than the baseline allows (new text belongs in \
         crates/i18n/src/catalog.rs):\n{}\n\
         {} in total (baseline {}). Regenerate the baseline with \
         `INF_I18N_BASELINE=1 cargo test -p i18n --test cjk_ratchet -- --nocapture`",
        over.join("\n"),
        actual_total,
        baseline_total
    );
    assert!(
        under.is_empty(),
        "These files are below their baseline — lower the numbers (that is how a \
         finished file is settled):\n{}\n\
         {} in total (baseline {}). Regenerate the baseline with \
         `INF_I18N_BASELINE=1 cargo test -p i18n --test cjk_ratchet -- --nocapture`",
        under.join("\n"),
        actual_total,
        baseline_total
    );
}

/// The scanner needs its own test: a wrong baseline makes the ratchet useless.
#[test]
fn the_scanner_counts_what_a_user_can_see() {
    let src = r##"
// a comment: 注释 does not count
/// neither does a doc comment: 文档注释
fn f() { let a = "保存"; let b = "UTF-8"; let c = r#"原始"#; }
#[cfg(test)]
mod tests { fn t() { assert_eq!("断言里的中文", 1); } }
"##;
    assert_eq!(
        cjk_literals(without_tests(src)),
        2,
        "only the \"保存\" and \"原始\" literals"
    );
}
