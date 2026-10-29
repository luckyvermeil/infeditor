//! Code block coloring: the host implementation of wysiwyg's `hl` hook.
//!
//! Fenced code blocks in Markdown preview / WYSIWYG are colored by language,
//! using the same lexer as the editor ([`super::lexer`]), with colors from the
//! theme's [`super::theme::Theme::color`].
//!
//! Language resolution happens in two steps: the fence info string is a
//! lowercase short name (`rust` / `js` / `py`) while the language table holds
//! display names (`Rust` / `JavaScript` / `Python`) — first look up the alias
//! table, then match by display name; if neither hits, return an empty string
//! and the caller escapes it as plain text (**no coloring when unrecognized** —
//! better uncolored than chopping ordinary text into colored fragments).

use super::{lang, theme};
use crate::highlight::lexer::{lex_line, LexState};

/// Register into `wysiwyg::hl`. Called once at startup.
pub fn register() {
    wysiwyg::hl::set_code_highlighter(highlight);
}

/// (language name, raw code, primary foreground) → escaped colored HTML; empty string when not
/// colored.
fn highlight(lang_name: &str, code: &str, fg: &str) -> String {
    let Some(spec) = resolve(lang_name) else {
        return String::new();
    };
    let t = theme_for(fg);
    let mut out = String::with_capacity(code.len() + code.len() / 2);
    let mut st = LexState::normal();
    for line in code.split_inclusive('\n') {
        let mut toks = Vec::new();
        st = lex_line(spec, line, st, &mut toks);
        let mut pos = 0usize;
        for tk in toks {
            if tk.start > pos {
                out.push_str(&wysiwyg::escape_html(&line[pos..tk.start]));
            }
            // Same-colored tokens do not start a new span (one span = one DOM node)
            let color = t.color(tk.kind);
            out.push_str("<span style=\"color:");
            out.push_str(color);
            out.push_str("\">");
            out.push_str(&wysiwyg::escape_html(&line[tk.start..tk.end]));
            out.push_str("</span>");
            pos = tk.end.max(pos);
        }
        if pos < line.len() {
            out.push_str(&wysiwyg::escape_html(&line[pos..]));
        }
    }
    out
}

/// Plain-text entry test. The language table's first entry is a **clone** of
/// [`lang::LangSpec::plain`] (a different address), so `ptr::eq` never matches — the test must
/// go by name, or plain text would be colored as a language.
fn is_plain(spec: &lang::LangSpec) -> bool {
    spec.name == lang::LangSpec::plain().name
}

/// Fence info string → language table entry. Unrecognized → `None` (no coloring).
///
/// Plain-text aliases are excluded first, then three-tier resolution covers **every** language
/// table entry:
/// 1. Dedicated aliases (spellings that are neither display names nor extensions, e.g.
///    `golang`/`objc`/`pwsh`);
/// 2. Display names (```Rust / ```PowerShell / ```日志);
/// 3. Extensions (```rs / ```vue / ```scss / ```log …) — the language table's `exts` is the
///    authoritative alias source: a new language is colorable as soon as it enters the table,
///    with no change needed here.
fn resolve(name: &str) -> Option<&'static lang::LangSpec> {
    let n = name.trim().to_ascii_lowercase();
    if n.is_empty() {
        return None;
    }
    // Plain text: explicitly not colored
    if matches!(
        n.as_str(),
        "text" | "txt" | "plain" | "plaintext" | "none" | "Plain Text"
    ) {
        return None;
    }
    // Dedicated aliases: spellings that are neither display names nor extensions
    let canonical = match n.as_str() {
        "python3" | "py3" => "Python",
        "node" | "nodejs" => "JavaScript",
        "objc" | "objectivec" | "objective-c" | "obj-c" | "objcpp" | "objective-c++" => "C/C++",
        "csharp" => "C#",
        "golang" => "Go",
        "json-lines" => "JSON Lines",
        "console" | "terminal" | "shell-session" | "shellsession" => "Shell",
        "pwsh" => "PowerShell",
        "logs" | "logcat" | "logging" | "日志" => "Log",
        _ => "",
    };
    if !canonical.is_empty() {
        let spec = lang::by_name(canonical);
        return (!is_plain(spec)).then_some(spec);
    }
    // Display names (case-insensitive)
    for spec in lang::all() {
        if !is_plain(spec) && spec.name.eq_ignore_ascii_case(&n) {
            return Some(spec);
        }
    }
    // Extensions — the language table's exts is the authoritative alias source
    for spec in lang::all() {
        if is_plain(spec) {
            continue;
        }
        if spec.exts.iter().any(|e| e.eq_ignore_ascii_case(&n)) {
            return Some(spec);
        }
    }
    None
}

/// Pick the light/dark palette from the primary foreground (same criterion as the preview side's
/// `gh_theme`).
fn theme_for(fg: &str) -> &'static theme::Theme {
    if hex_luma(fg) > 0.5 {
        &theme::DARK
    } else {
        &theme::LIGHT
    }
}

/// Relative luma of `#rrggbb` (0..1); an unparseable value falls back to neutral 0.5 (light
/// palette).
fn hex_luma(hex: &str) -> f32 {
    let h = hex.trim_start_matches('#');
    if h.len() != 6 {
        return 0.5;
    }
    let r = u32::from_str_radix(&h[0..2], 16).unwrap_or(128);
    let g = u32::from_str_radix(&h[2..4], 16).unwrap_or(128);
    let b = u32::from_str_radix(&h[4..6], 16).unwrap_or(128);
    (0.2126 * r as f32 + 0.7152 * g as f32 + 0.0722 * b as f32) / 255.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_language_in_the_table_resolves() {
        // The master switch for "every language's code gets colored": each language table
        // entry (except plain text) must be resolvable by display name and by **every
        // extension** — a new language is covered automatically once it enters the table.
        for spec in lang::all() {
            if is_plain(spec) {
                continue;
            }
            assert!(
                resolve(spec.name).is_some(),
                "display name {:?} should resolve",
                spec.name
            );
            for ext in spec.exts {
                assert!(
                    resolve(ext).is_some(),
                    "extension {:?} ({}) should resolve",
                    ext,
                    spec.name
                );
            }
        }
    }

    #[test]
    fn common_fence_names_resolve() {
        // Fence spellings that actually appear in documents (short names / aliases / display
        // names / mixed case)
        for f in [
            "rs",
            "rust",
            "Rust",
            "py",
            "python",
            "python3",
            "js",
            "javascript",
            "node",
            "ts",
            "typescript",
            "tsx",
            "c",
            "cpp",
            "c++",
            "h",
            "objc",
            "objective-c",
            "c#",
            "cs",
            "csharp",
            "go",
            "golang",
            "json",
            "jsonc",
            "jsonl",
            "ndjson",
            "json-lines",
            "html",
            "htm",
            "vue",
            "svelte",
            "xml",
            "svg",
            "css",
            "scss",
            "less",
            "sass",
            "yaml",
            "yml",
            "toml",
            "ini",
            "cfg",
            "conf",
            "md",
            "markdown",
            "mdx",
            "sql",
            "lua",
            "bat",
            "cmd",
            "batch",
            "powershell",
            "ps1",
            "pwsh",
            "sh",
            "bash",
            "shell",
            "zsh",
            "fish",
            "console",
            "terminal",
            "php",
            "ruby",
            "rb",
            "perl",
            "pl",
            "diff",
            "patch",
            "csv",
            "tsv",
            "log",
            "logs",
            "logcat",
            "Log",
            "日志",
            "PowerShell",
            "JSON",
            "C/C++",
        ] {
            assert!(resolve(f).is_some(), "{f} should be colorable");
        }
    }

    #[test]
    fn plain_and_unknown_do_not_resolve() {
        for f in [
            "",
            "text",
            "txt",
            "plain",
            "plaintext",
            "none",
            "Plain Text",
            "wat",
            "unknown",
        ] {
            assert!(resolve(f).is_none(), "{f} should not be colored");
        }
    }

    #[test]
    fn every_language_produces_colored_spans() {
        // Every language colors at least one sample successfully (numbers / strings / comments
        // / tables / diff / headings, whatever suits it) — guarding against "resolves but the
        // lexer emits nothing" false coverage.
        let samples = [
            "42",
            "\"s\"",
            "// c",
            "/* c */",
            "# c",
            "-- c",
            "<!-- c -->",
            "+add",
            "a,b",
            "# head",
            "$x",
            "@annot",
        ];
        for spec in lang::all() {
            if is_plain(spec) {
                continue;
            }
            let ok = samples
                .iter()
                .any(|s| highlight(spec.name, s, "#d4d4d4").contains("<span style=\"color:"));
            assert!(ok, "{} should color some sample", spec.name);
        }
    }

    #[test]
    fn plain_display_name_is_not_colored() {
        // Plain text's display name must not be colored either: the table's first entry is a
        // clone of plain(); back when ptr::eq was used it never matched, and `Plain Text` was chopped
        // into colored fragments as a language.
        assert_eq!(highlight("Plain Text", "42", "#d4d4d4"), "");
    }

    #[test]
    fn highlight_emits_escaped_colored_spans() {
        let html = highlight("rust", "fn main() { let a = 1 < 2; }", "#d4d4d4");
        assert!(
            html.contains("<span style=\"color:"),
            "there must be a colored span: {html}"
        );
        assert!(
            html.contains("&lt;"),
            "angle brackets must be escaped: {html}"
        );
        assert!(
            !html.contains("< 2"),
            "the original < must not enter HTML verbatim"
        );
    }

    #[test]
    fn unknown_language_is_left_alone() {
        assert_eq!(highlight("wat", "fn main() {}", "#d4d4d4"), "");
    }
}
