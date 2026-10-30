//! Language definitions for the syntax highlighter.
//!
//! A [`LangSpec`] is a data-driven description of a language: comments,
//! strings, escapes, keyword sets and a few mode flags. The generic lexer in
//! [`super::lexer`] interprets it, so adding a language means adding an entry
//! to `build_all` and nothing else.

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::OnceLock;

/// Semantic token classes produced by the lexer and mapped to colors by the
/// theme.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TokenKind {
    Plain,
    Comment,
    DocComment,
    String,
    Char,
    Number,
    Keyword,
    Control,
    Type,
    Ident,
    Function,
    Operator,
    Punct,
    Preproc,
    Annotation,
    /// XML/HTML tag name
    Tag,
    AttrName,
    AttrValue,
    Variable,
    // --- markdown ---
    Heading,
    Bold,
    Italic,
    Code,
    Link,
    Quote,
    ListMark,
    Rule,
    /// Math formula body (`$…$`, `$$…$$`, `\(…\)`, `\[…\]` — LaTeX source)
    Math,
    /// The math delimiters themselves (`$` / `$$` / `\(` / `\)` …), dimmed so
    /// the formula reads without the fences shouting
    MathDelim,
    /// A LaTeX command inside a formula (`\frac`, `\alpha`, `\text`, `\\`)
    MathCmd,
    // --- diff ---
    Added,
    Removed,
    Meta,
    // --- csv / table ---
    /// Column index (wraps modulo the theme's column palette)
    Col(u8),
}

/// How escapes inside strings are written.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum EscapeKind {
    /// no escape sequences (SQL `'x''y'` style handled by doubling below)
    #[default]
    None,
    /// C-style `\n`, `\t`, `\\`, `\"` ...
    Backslash,
    /// doubled quote (`''` / `""`)
    Doubled,
    /// both (e.g. VB-ish / some dialects)
    Both,
}

/// CSV/TSV field splitting configuration.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct CsvSpec {
    pub delim: u8,
    pub quote: u8,
}

/// Static description of one language.
#[derive(Clone, Debug)]
pub struct LangSpec {
    pub name: &'static str,
    /// lower-case file extensions (without the dot)
    pub exts: &'static [&'static str],
    /// exact file names matched case-insensitively (Makefile, Dockerfile ...)
    pub file_names: &'static [&'static str],
    pub line_comments: &'static [&'static str],
    /// (open, close) for block comments
    pub block: Option<(&'static str, &'static str)>,
    /// block comments nest (Rust, Kotlin, Swift)
    pub nested_block: bool,
    /// line-comment prefixes that are documentation (rendered differently)
    pub doc_line: &'static [&'static str],
    /// single-char string delimiters (`"`, `'`)
    pub str_delims: &'static [&'static str],
    /// `'` starts a character literal (Rust, C, Java)
    pub char_delim: bool,
    /// `"""` / `'''` multi-line strings (Python)
    pub triple_str: bool,
    /// `r"..."` / `r#"..."#` raw strings (Rust)
    pub raw_str: bool,
    pub escape: EscapeKind,
    /// **Per-delimiter** escape style (positions align with
    /// [`Self::str_delims`]; the default falls back to [`Self::escape`]):
    /// a language-level EscapeKind cannot express "different delimiters in
    /// the same language escape differently" — in Shell, `\` escapes inside
    /// double quotes but is a literal character inside single quotes; this
    /// once pushed the close point of `'don\'t'` back and let an unterminated
    /// `'` recolor across lines.
    pub str_escapes: &'static [(&'static str, EscapeKind)],
    pub keywords: BTreeSet<&'static str>,
    /// control-flow keywords get their own color
    pub control: BTreeSet<&'static str>,
    pub types: BTreeSet<&'static str>,
    /// `#directive` at line start (C/C++/C#)
    pub preproc: bool,
    /// `@Name` annotations
    pub annotation: bool,
    /// `$name` is part of an identifier (PHP, shell, Perl)
    pub dollar_ident: bool,
    /// extra characters allowed inside identifiers (`-` in CSS, `?!` in Ruby)
    pub ident_extra: &'static str,
    pub case_insensitive: bool,
    pub numbers: bool,
    /// scan `<tag attr="value">` constructs
    pub html_like: bool,
    /// markdown line mode
    pub md: bool,
    /// unified-diff line mode
    pub diff: bool,
    /// csv/tsv column mode
    pub csv: Option<CsvSpec>,
}

static SPECS: OnceLock<Vec<LangSpec>> = OnceLock::new();
static PLAIN: OnceLock<LangSpec> = OnceLock::new();

/// Build the keyword set from a static slice (case preserved).
fn w(list: &[&'static str]) -> BTreeSet<&'static str> {
    list.iter().copied().collect()
}

impl Default for LangSpec {
    fn default() -> Self {
        LangSpec {
            name: "",
            exts: &[],
            file_names: &[],
            line_comments: &[],
            block: None,
            nested_block: false,
            doc_line: &[],
            str_delims: &["\""],
            char_delim: false,
            triple_str: false,
            raw_str: false,
            escape: EscapeKind::Backslash,
            str_escapes: &[],
            keywords: BTreeSet::new(),
            control: BTreeSet::new(),
            types: BTreeSet::new(),
            preproc: false,
            annotation: false,
            dollar_ident: false,
            ident_extra: "",
            case_insensitive: false,
            numbers: true,
            html_like: false,
            md: false,
            diff: false,
            csv: None,
        }
    }
}

impl LangSpec {
    /// Escape style for the delimiter: look up [`Self::str_escapes`] first
    /// (position-aligned with str_delims), falling back to the language-level
    /// [`Self::escape`]. `quote` is the delimiter's first byte.
    pub fn escape_for(&self, quote: u16) -> EscapeKind {
        let q = char::from_u32(u32::from(quote)).unwrap_or('"');
        self.str_delims
            .iter()
            .position(|d| d.starts_with(q))
            .and_then(|i| self.str_escapes.get(i))
            .map(|(_, e)| *e)
            .unwrap_or(self.escape)
    }

    /// The "no highlighting" spec.
    pub fn plain() -> &'static LangSpec {
        PLAIN.get_or_init(|| LangSpec {
            name: "Plain Text",
            // no string semantics: a plain-text apostrophe ("don't") must not
            // open an unterminated string that recolors the rest of the file
            str_delims: &[],
            escape: EscapeKind::None,
            case_insensitive: true,
            numbers: false,
            ..Default::default()
        })
    }

    /// Keywords are matched case-insensitively for this language.
    pub fn lookup(&self, word: &str) -> Option<TokenKind> {
        let probe: &str = if self.case_insensitive {
            // allocate only when needed; keywords are stored lower-cased
            return self.lookup_ci(word);
        } else {
            word
        };
        if self.keywords.contains(probe) {
            return Some(TokenKind::Keyword);
        }
        if self.control.contains(probe) {
            return Some(TokenKind::Control);
        }
        if self.types.contains(probe) {
            return Some(TokenKind::Type);
        }
        None
    }

    fn lookup_ci(&self, word: &str) -> Option<TokenKind> {
        // Lower-case on the stack: `LangSpec::plain()` is also case_insensitive,
        // Plain Text / Large log: every word goes through here — one
        // `to_ascii_lowercase` heap allocation per word is unacceptable. Zero
        // allocation when the word is ASCII and fits the buffer; only
        // over-long / non-ASCII (rare) words fall back to the heap, with
        // semantics identical to `to_ascii_lowercase` (non-ASCII bytes are
        // kept as-is).
        const CAP: usize = 64;
        if word.is_ascii() && word.len() <= CAP {
            let mut buf = [0u8; CAP];
            buf[..word.len()].copy_from_slice(word.as_bytes());
            for b in &mut buf[..word.len()] {
                b.make_ascii_lowercase();
            }
            // ASCII transforms cannot produce invalid UTF-8
            let lower = std::str::from_utf8(&buf[..word.len()]).unwrap_or(word);
            return self.ci_probe(lower);
        }
        self.ci_probe(&word.to_ascii_lowercase())
    }

    fn ci_probe(&self, lower: &str) -> Option<TokenKind> {
        if self.keywords.contains(lower) {
            return Some(TokenKind::Keyword);
        }
        if self.control.contains(lower) {
            return Some(TokenKind::Control);
        }
        if self.types.contains(lower) {
            return Some(TokenKind::Type);
        }
        None
    }
}

/// All known languages (plain text first).
pub fn all() -> &'static [LangSpec] {
    SPECS.get_or_init(build_all).as_slice()
}

/// Resolve a language by display name (used by the menu / settings).
pub fn by_name(name: &str) -> &'static LangSpec {
    // Legacy identifier: the Log language was called "日志" before it got an
    // English canonical name + a localized label. Macros persist
    // `Lang:<name>` to disk, so old macro files keep working.
    let name = if name.eq_ignore_ascii_case("日志") {
        "Log"
    } else {
        name
    };
    all()
        .iter()
        .find(|s| s.name.eq_ignore_ascii_case(name))
        .unwrap_or_else(LangSpec::plain)
}

/// Every selectable language name (for the View menu).
pub fn names() -> Vec<&'static str> {
    all().iter().map(|s| s.name).collect()
}

/// Detect a language from a file path.
pub fn for_path(path: Option<&Path>) -> &'static LangSpec {
    let Some(p) = path else {
        return LangSpec::plain();
    };
    let name = p
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let lower = name.to_ascii_lowercase();
    for spec in all() {
        for f in spec.file_names {
            if lower == f.to_ascii_lowercase() {
                return spec;
            }
        }
    }
    let ext = p
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    if !ext.is_empty() {
        for spec in all() {
            if spec.exts.contains(&ext.as_str()) {
                return spec;
            }
        }
    }
    // fall back to the full lower-case name (handles e.g. "Dockerfile.dev")
    for spec in all() {
        for f in spec.file_names {
            if lower.starts_with(&f.to_ascii_lowercase()) {
                return spec;
            }
        }
    }
    LangSpec::plain()
}

/// Refine detection with the first line (shebang / modeline).
pub fn for_path_and_first_line(path: Option<&Path>, first: &str) -> &'static LangSpec {
    if let Some(rest) = first.strip_prefix("#!") {
        let cmd = rest.trim_start();
        let mut it = cmd.split_whitespace();
        let mut base = it
            .next()
            .unwrap_or("")
            .rsplit('/')
            .next()
            .unwrap_or("")
            .to_string();
        // `#!/usr/bin/env python3` — the interpreter is the next word
        if base == "env" {
            base = it
                .next()
                .unwrap_or("")
                .rsplit('/')
                .next()
                .unwrap_or("")
                .to_string();
        }
        let guess = if matches!(
            base.as_str(),
            "sh" | "bash" | "dash" | "zsh" | "ksh" | "fish"
        ) {
            "Shell"
        } else if base.starts_with("python") {
            "Python"
        } else if base.starts_with("perl") {
            "Perl"
        } else if base.starts_with("ruby") {
            "Ruby"
        } else if base == "node" || base == "nodejs" {
            "JavaScript"
        } else if base.starts_with("pwsh") || base == "powershell" {
            "PowerShell"
        } else if base.starts_with("lua") {
            "Lua"
        } else if base.starts_with("php") {
            "PHP"
        } else {
            ""
        };
        if !guess.is_empty() {
            return by_name(guess);
        }
    }
    // vim/emacs modeline: -*- mode: python -*-
    if first.contains("-*-") && first.contains("mode:") {
        for spec in all() {
            let n = spec.name.to_ascii_lowercase();
            if first.to_ascii_lowercase().contains(&format!("mode: {}", n)) {
                return spec;
            }
        }
    }
    for_path(path)
}

// ---------------------------------------------------------------------------
// language table
// ---------------------------------------------------------------------------

/// Shorthand for a language entry: only the non-default fields are listed.
macro_rules! spec {
    ($name:expr, $($field:ident : $value:expr),* $(,)?) => {
        LangSpec {
            name: $name,
            $($field: $value,)*
            ..Default::default()
        }
    };
}

fn build_all() -> Vec<LangSpec> {
    let mut v: Vec<LangSpec> = vec![LangSpec::plain().clone()];

    // ---- C / C++ -----------------------------------------------------------
    v.push(spec! {
        "C/C++",
        exts: &["c", "h", "cc", "cpp", "cxx", "hpp", "hh", "hxx", "c++", "ino", "m", "mm"],
        line_comments: &["//"],
        doc_line: &["///", "//!"],
        block: Some(("/*", "*/")),
        str_delims: &["\""],
        char_delim: true,
        preproc: true,
        numbers: true,
        keywords: w(&["auto", "const", "constexpr", "consteval", "constinit", "extern", "inline",
            "mutable", "register", "static", "static_assert", "thread_local", "typedef", "using",
            "namespace", "alignas", "alignof", "decltype", "concept", "requires", "template",
            "typename", "class", "struct", "union", "enum", "explicit", "export", "friend",
            "operator", "private", "protected", "public", "virtual", "volatile", "noexcept",
            "sizeof", "typeid", "asm", "co_await", "co_return", "co_yield", "nullptr", "this",
            "final", "override"]),
        control: w(&["if", "else", "switch", "case", "default", "for", "while", "do", "break",
            "continue", "return", "goto", "try", "catch", "throw"]),
        types: w(&["void", "bool", "char", "char8_t", "char16_t", "char32_t", "wchar_t", "short",
            "int", "long", "float", "double", "signed", "unsigned", "size_t", "ptrdiff_t",
            "int8_t", "int16_t", "int32_t", "int64_t", "uint8_t", "uint16_t", "uint32_t",
            "uint64_t", "true", "false", "NULL"]),
    });

    // ---- C# ----------------------------------------------------------------
    v.push(spec! {
        "C#",
        exts: &["cs", "csx"],
        line_comments: &["//"],
        doc_line: &["///"],
        block: Some(("/*", "*/")),
        str_delims: &["\""],
        char_delim: true,
        preproc: true,
        annotation: true,
        raw_str: true,
        keywords: w(&["abstract", "as", "base", "checked", "const", "delegate", "event",
            "explicit", "extern", "fixed", "implicit", "in", "internal", "is", "lock", "namespace",
            "operator", "out", "override", "params", "partial", "private", "protected", "public",
            "readonly", "ref", "sealed", "sizeof", "stackalloc", "static", "this", "unchecked",
            "unsafe", "using", "virtual", "volatile", "new", "nameof", "dynamic", "async",
            "await", "var", "record", "required", "global", "managed", "unmanaged"]),
        control: w(&["if", "else", "switch", "case", "default", "for", "foreach", "while", "do",
            "break", "continue", "return", "goto", "try", "catch", "finally", "throw", "when",
            "yield", "select", "from", "where", "let", "orderby", "group", "join", "into"]),
        types: w(&["bool", "byte", "char", "decimal", "double", "float", "int", "long", "nint",
            "nuint", "object", "sbyte", "short", "string", "uint", "ulong", "ushort", "void",
            "true", "false", "null"]),
    });

    // ---- Rust --------------------------------------------------------------
    v.push(spec! {
        "Rust",
        exts: &["rs"],
        line_comments: &["//"],
        doc_line: &["///", "//!"],
        block: Some(("/*", "*/")),
        nested_block: true,
        str_delims: &["\""],
        char_delim: true,
        raw_str: true,
        annotation: true,
        keywords: w(&["as", "const", "crate", "dyn", "enum", "extern", "fn", "impl", "in", "let",
            "mod", "move", "mut", "pub", "ref", "static", "struct", "trait", "type", "union",
            "unsafe", "use", "where", "async", "await", "box", "macro_rules", "abstract",
            "become", "do", "final", "override", "priv", "typeof", "unsized", "virtual", "yield",
            "try"]),
        control: w(&["break", "continue", "else", "for", "if", "loop", "match", "return", "while",
            "self", "Self", "super"]),
        types: w(&["bool", "char", "str", "usize", "isize", "u8", "u16", "u32", "u64", "u128",
            "i8", "i16", "i32", "i64", "i128", "f32", "f64", "true", "false", "Option", "Result",
            "String", "Vec", "Box", "Some", "None", "Ok", "Err"]),
    });

    // ---- Python ------------------------------------------------------------
    v.push(spec! {
        "Python",
        exts: &["py", "pyw", "pyi", "pyx"],
        line_comments: &["#"],
        str_delims: &["\"", "'"],
        triple_str: true,
        raw_str: true,
        annotation: true,
        keywords: w(&["as", "assert", "class", "del", "def", "from", "global", "import",
            "lambda", "nonlocal", "pass", "with", "yield", "async", "await", "match", "case",
            "in", "is", "not", "and", "or", "self", "cls"]),
        control: w(&["break", "continue", "else", "except", "finally", "for", "if", "elif",
            "raise", "return", "try", "while"]),
        types: w(&["bool", "int", "float", "str", "bytes", "list", "dict", "set", "tuple",
            "None", "True", "False", "object", "type", "range", "len", "print", "super"]),
    });

    // ---- JavaScript --------------------------------------------------------
    let js_kw = w(&[
        "class",
        "const",
        "debugger",
        "delete",
        "export",
        "extends",
        "function",
        "get",
        "import",
        "in",
        "instanceof",
        "let",
        "new",
        "of",
        "set",
        "static",
        "super",
        "this",
        "typeof",
        "var",
        "void",
        "async",
        "await",
        "yield",
        "declare",
        "namespace",
        "module",
        "abstract",
        "implements",
        "readonly",
        "keyof",
        "infer",
        "satisfies",
        "as",
        "is",
    ]);
    let js_ctrl = w(&[
        "break", "case", "catch", "continue", "default", "do", "else", "finally", "for", "if",
        "return", "switch", "throw", "try", "while", "with",
    ]);
    let js_types = w(&[
        "true",
        "false",
        "null",
        "undefined",
        "NaN",
        "Infinity",
        "Object",
        "Array",
        "String",
        "Number",
        "Boolean",
        "Symbol",
        "BigInt",
        "Promise",
        "Map",
        "Set",
        "WeakMap",
        "WeakSet",
        "JSON",
        "Math",
        "Date",
        "RegExp",
        "Error",
        "Function",
        "window",
        "document",
        "console",
        "globalThis",
        "process",
        "require",
        "module",
        "exports",
    ]);

    v.push(spec! {
        "JavaScript",
        exts: &["js", "jsx", "mjs", "cjs"],
        line_comments: &["//"],
        doc_line: &["///"],
        block: Some(("/*", "*/")),
        str_delims: &["\"", "'", "`"],
        keywords: js_kw.clone(),
        control: js_ctrl.clone(),
        types: js_types.clone(),
    });

    let mut ts = spec! {
        "TypeScript",
        exts: &["ts", "tsx", "mts", "cts", "d.ts"],
        line_comments: &["//"],
        doc_line: &["///"],
        block: Some(("/*", "*/")),
        str_delims: &["\"", "'", "`"],
        keywords: js_kw.clone(),
        control: js_ctrl.clone(),
        types: js_types.clone(),
    };
    ts.keywords.extend(
        [
            "type",
            "interface",
            "enum",
            "public",
            "private",
            "protected",
        ]
        .iter()
        .copied(),
    );
    ts.types.extend(
        [
            "any", "unknown", "never", "void", "string", "number", "boolean", "object",
        ]
        .iter()
        .copied(),
    );
    v.push(ts);

    // ---- JSON --------------------------------------------------------------
    v.push(spec! {
        "JSON",
        exts: &["json", "jsonc", "json5"],
        line_comments: &["//"],
        block: Some(("/*", "*/")),
        str_delims: &["\""],
        keywords: w(&["true", "false", "null"]),
    });

    // ---- JSON Lines / NDJSON (the view layer auto-enables the JSONL preview,
    // see ui/state/jsonl.rs) ----
    v.push(spec! {
        "JSON Lines",
        exts: &["jsonl", "ndjson", "jsonlines"],
        line_comments: &["//"],
        str_delims: &["\""],
        keywords: w(&["true", "false", "null"]),
    });

    // ---- HTML / XML --------------------------------------------------------
    v.push(spec! {
        "HTML",
        exts: &["html", "htm", "xhtml", "vue", "svelte"],
        line_comments: &[],
        block: Some(("<!--", "-->")),
        str_delims: &["\"", "'"],
        html_like: true,
        keywords: w(&["true", "false", "null"]),
    });
    v.push(spec! {
        "XML",
        exts: &["xml", "xaml", "xsl", "xslt", "xsd", "svg", "plist", "csproj", "vbproj",
                "fsproj", "props", "targets", "resx", "config", "manifest", "ui", "glade"],
        line_comments: &[],
        block: Some(("<!--", "-->")),
        str_delims: &["\"", "'"],
        html_like: true,
    });

    // ---- CSS ---------------------------------------------------------------
    v.push(spec! {
        "CSS",
        exts: &["css", "scss", "sass", "less"],
        line_comments: &["//"],
        block: Some(("/*", "*/")),
        str_delims: &["\"", "'"],
        ident_extra: "-",
        keywords: w(&["important", "media", "supports", "keyframes", "font-face", "import",
            "charset", "namespace", "page", "var", "calc", "min", "max", "clamp", "url",
            "from", "to", "not", "and", "only"]),
        types: w(&["inherit", "initial", "unset", "auto", "none", "block", "flex", "grid",
            "absolute", "relative", "fixed", "sticky", "hidden", "visible", "solid", "dashed",
            "dotted", "bold", "italic", "center", "left", "right", "top", "bottom", "transparent",
            "currentColor"]),
    });

    // ---- YAML --------------------------------------------------------------
    v.push(spec! {
        "YAML",
        exts: &["yml", "yaml"],
        file_names: &[".clang-format", ".clang-tidy"],
        line_comments: &["#"],
        str_delims: &["\"", "'"],
        ident_extra: "-",
        keywords: w(&["true", "false", "null", "yes", "no", "on", "off", "~"]),
    });
    // ---- TOML --------------------------------------------------------------
    v.push(spec! {
        "TOML",
        exts: &["toml"],
        file_names: &["Cargo.lock"],
        line_comments: &["#"],
        str_delims: &["\"", "'"],
        ident_extra: "-.",
        keywords: w(&["true", "false"]),
    });

    // ---- INI ---------------------------------------------------------------
    v.push(spec! {
        "INI",
        exts: &["ini", "cfg", "conf", "properties", "reg", "desktop", "editorconfig"],
        file_names: &[".gitconfig", ".gitattributes", ".editorconfig", ".npmrc"],
        line_comments: &["#", ";"],
        str_delims: &["\""],
        ident_extra: "-.",
        case_insensitive: true,
        keywords: w(&["true", "false", "yes", "no", "on", "off"]),
    });

    // ---- Markdown ----------------------------------------------------------
    v.push(spec! {
        "Markdown",
        exts: &["md", "markdown", "mdown", "mkd", "mdx"],
        md: true,
        str_delims: &[],
        numbers: false,
    });

    // ---- SQL ---------------------------------------------------------------
    v.push(spec! {
        "SQL",
        exts: &["sql", "ddl", "dml"],
        line_comments: &["--"],
        block: Some(("/*", "*/")),
        str_delims: &["'", "\""],
        escape: EscapeKind::Doubled,
        case_insensitive: true,
        keywords: w(&["select", "from", "where", "insert", "into", "values", "update", "set",
            "delete", "create", "table", "drop", "alter", "add", "column", "primary", "key",
            "foreign", "references", "join", "left", "right", "inner", "outer", "full", "cross",
            "on", "group", "by", "order", "having", "limit", "offset", "distinct", "as", "and",
            "or", "not", "null", "is", "in", "between", "like", "exists", "union", "all", "case",
            "when", "then", "else", "end", "index", "view", "truncate", "default", "constraint",
            "unique", "check", "with", "returning", "begin", "commit", "rollback", "transaction",
            "explain", "analyze", "vacuum", "pragma", "asc", "desc", "count", "sum", "avg", "min",
            "max", "coalesce", "cast", "extract", "over", "partition", "row_number", "rank"]),
        types: w(&["int", "integer", "bigint", "smallint", "tinyint", "decimal", "numeric",
            "float", "real", "double", "char", "varchar", "text", "boolean", "date", "time",
            "timestamp", "blob", "json", "uuid"]),
    });

    // ---- Java --------------------------------------------------------------
    v.push(spec! {
        "Java",
        exts: &["java", "jsp"],
        line_comments: &["//"],
        doc_line: &["///"],
        block: Some(("/*", "*/")),
        str_delims: &["\""],
        char_delim: true,
        annotation: true,
        keywords: w(&["abstract", "assert", "class", "enum", "extends", "final", "implements",
            "import", "instanceof", "interface", "native", "new", "package", "private",
            "protected", "public", "static", "strictfp", "super", "synchronized", "this",
            "transient", "volatile", "record", "sealed", "var", "yield", "exports", "module",
            "requires"]),
        control: w(&["break", "case", "catch", "continue", "default", "do", "else", "finally",
            "for", "if", "return", "switch", "throw", "throws", "try", "while"]),
        types: w(&["boolean", "byte", "char", "double", "float", "int", "long", "short", "void",
            "true", "false", "null", "String", "Integer", "List", "Map", "Set"]),
    });

    // ---- Go ----------------------------------------------------------------
    v.push(spec! {
        "Go",
        exts: &["go"],
        line_comments: &["//"],
        block: Some(("/*", "*/")),
        str_delims: &["\""],
        char_delim: true,
        raw_str: true,
        keywords: w(&["chan", "const", "defer", "func", "go", "import", "interface", "map",
            "package", "range", "select", "struct", "type", "var", "fallthrough", "goto"]),
        control: w(&["break", "case", "continue", "default", "else", "for", "if", "return",
            "switch"]),
        types: w(&["bool", "byte", "complex64", "complex128", "error", "float32", "float64",
            "int", "int8", "int16", "int32", "int64", "rune", "string", "uint", "uint8",
            "uint16", "uint32", "uint64", "uintptr", "true", "false", "nil", "iota", "append",
            "cap", "close", "copy", "delete", "len", "make", "new", "panic", "print", "println",
            "recover"]),
    });

    // ---- Lua ---------------------------------------------------------------
    v.push(spec! {
        "Lua",
        exts: &["lua"],
        line_comments: &["--"],
        doc_line: &["---"],
        block: Some(("--[[", "]]")),
        str_delims: &["\"", "'"],
        keywords: w(&["and", "break", "do", "else", "elseif", "end", "for", "function", "goto",
            "if", "in", "local", "not", "or", "repeat", "return", "then", "until", "while"]),
        types: w(&["nil", "true", "false", "self"]),
    });

    // ---- Batch -------------------------------------------------------------
    v.push(spec! {
        "Batch",
        exts: &["bat", "cmd"],
        line_comments: &["rem", "::"],
        case_insensitive: true,
        dollar_ident: true,
        str_delims: &["\""],
        keywords: w(&["echo", "set", "call", "exit", "goto", "pause", "shift", "start", "title",
            "if", "else", "for", "in", "do", "not", "exist", "defined", "errorlevel", "local",
            "enabledelayedexpansion", "endlocal", "setlocal", "pushd", "popd", "type", "copy",
            "move", "del", "rd", "md", "cd"]),
    });

    // ---- PowerShell --------------------------------------------------------
    v.push(spec! {
        "PowerShell",
        exts: &["ps1", "psm1", "psd1"],
        line_comments: &["#"],
        doc_line: &["##"],
        block: Some(("<#", "#>")),
        str_delims: &["\"", "'"],
        dollar_ident: true,
        case_insensitive: true,
        ident_extra: "-",
        keywords: w(&["function", "param", "filter", "begin", "process", "end", "return", "throw",
            "try", "catch", "finally", "if", "else", "elseif", "foreach", "for", "while", "do",
            "until", "switch", "break", "continue", "exit", "set", "get", "new", "out", "write",
            "invoke", "test", "add", "remove", "import", "export", "select", "where", "sort",
            "group", "measure", "format"]),
        types: w(&["true", "false", "null", "$null", "string", "int", "bool", "array", "hashtable"]),
    });

    // ---- Shell -------------------------------------------------------------
    v.push(spec! {
        "Shell",
        exts: &["sh", "bash", "zsh", "ksh", "fish"],
        file_names: &[".bashrc", ".zshrc", ".profile", ".bash_profile"],
        line_comments: &["#"],
        str_delims: &["\"", "'"],
        // `\` escapes inside double quotes; **inside single quotes `\` is a
        // literal character** (bash semantics, `'don\'t'` is two concatenated
        // pieces) — the language-level escape fallback used to be Backslash,
        // pushing the close point of single-quoted strings back and letting an
        // unterminated `'` recolor across lines
        str_escapes: &[("\"", EscapeKind::Backslash), ("'", EscapeKind::None)],
        dollar_ident: true,
        keywords: w(&["if", "then", "else", "elif", "fi", "for", "while", "until", "do", "done",
            "case", "esac", "function", "in", "select", "time", "coproc", "local", "export",
            "readonly", "declare", "typeset", "source", "alias", "return", "break", "continue",
            "shift", "exit", "trap", "set", "unset", "eval", "exec", "echo", "cd", "test"]),
    });

    // ---- PHP ---------------------------------------------------------------
    v.push(spec! {
        "PHP",
        exts: &["php", "phtml", "php5"],
        line_comments: &["//", "#"],
        doc_line: &["///"],
        block: Some(("/*", "*/")),
        str_delims: &["\"", "'"],
        dollar_ident: true,
        html_like: true,
        keywords: w(&["abstract", "as", "break", "callable", "case", "catch", "class", "clone",
            "const", "continue", "declare", "default", "do", "echo", "else", "elseif",
            "enddeclare", "endfor", "endforeach", "endif", "endswitch", "endwhile", "extends",
            "final", "finally", "fn", "for", "foreach", "function", "global", "goto", "if",
            "implements", "include", "include_once", "instanceof", "insteadof", "interface",
            "match", "namespace", "new", "print", "private", "protected", "public", "require",
            "require_once", "return", "static", "switch", "throw", "trait", "try", "use", "var",
            "while", "yield", "array", "list", "isset", "unset", "empty", "exit", "die"]),
        types: w(&["true", "false", "null", "int", "float", "string", "bool", "array", "object",
            "mixed", "void", "self", "parent"]),
    });

    // ---- Ruby --------------------------------------------------------------
    v.push(spec! {
        "Ruby",
        exts: &["rb", "rbw", "rake", "gemspec"],
        file_names: &["Gemfile", "Rakefile"],
        line_comments: &["#"],
        block: Some(("=begin", "=end")),
        str_delims: &["\"", "'"],
        dollar_ident: true,
        ident_extra: "?!",
        keywords: w(&["alias", "and", "begin", "break", "case", "class", "def", "defined?",
            "do", "else", "elsif", "end", "ensure", "for", "if", "in", "module", "next", "nil",
            "not", "or", "redo", "rescue", "retry", "return", "self", "super", "then", "undef",
            "unless", "until", "when", "while", "yield", "require", "require_relative", "attr_accessor"]),
        types: w(&["true", "false", "nil", "self"]),
    });

    // ---- Perl --------------------------------------------------------------
    v.push(spec! {
        "Perl",
        exts: &["pl", "pm", "pod", "t"],
        line_comments: &["#"],
        block: Some(("=pod", "=cut")),
        str_delims: &["\"", "'", "`"],
        dollar_ident: true,
        keywords: w(&["if", "elsif", "else", "unless", "while", "until", "for", "foreach", "do",
            "sub", "my", "our", "local", "package", "use", "no", "require", "return", "last",
            "next", "redo", "goto", "die", "warn", "print", "printf", "open", "close", "defined",
            "undef", "shift", "unshift", "push", "pop", "split", "join", "sort", "map", "grep"]),
    });

    // ---- Diff --------------------------------------------------------------
    v.push(spec! {
        "Diff",
        exts: &["diff", "patch"],
        diff: true,
        str_delims: &[],
        numbers: false,
    });

    // ---- CSV / TSV ---------------------------------------------------------
    v.push(spec! {
        "CSV",
        exts: &["csv", "tsv", "psv"],
        csv: Some(CsvSpec { delim: b',', quote: b'"' }),
        str_delims: &["\""],
        numbers: false,
    });

    // ---- Log ---------------------------------------------------------------
    // Canonical identifier is English ("Log"); the per-language display label
    // is `Msg::LangLog` (see `ui::lang_label`).
    v.push(spec! {
        "Log",
        exts: &["log"],
        line_comments: &[],
        str_delims: &["\""],
        numbers: true,
    });

    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_by_extension() {
        assert_eq!(for_path(Some(Path::new("a/b.rs"))).name, "Rust");
        assert_eq!(for_path(Some(Path::new("x.YAML"))).name, "YAML");
        assert_eq!(for_path(Some(Path::new("notes.md"))).name, "Markdown");
        assert_eq!(for_path(Some(Path::new("data.csv"))).name, "CSV");
        assert_eq!(for_path(Some(Path::new("a.log"))).name, "Log");
        assert_eq!(
            for_path(Some(Path::new("CMakeLists.txt"))).name,
            "Plain Text"
        );
    }

    /// The Log language used to be identified as "日志" (macros persist
    /// `Lang:<name>`); both spellings must resolve to the same spec.
    #[test]
    fn log_language_answers_to_legacy_and_canonical_names() {
        assert_eq!(by_name("Log").name, "Log");
        assert_eq!(by_name("日志").name, "Log");
        assert_eq!(by_name("LOG").name, "Log");
    }

    #[test]
    fn detect_by_filename() {
        assert_eq!(for_path(Some(Path::new("Makefile"))).name, "Plain Text");
        assert_eq!(for_path(Some(Path::new("Gemfile"))).name, "Ruby");
        assert_eq!(for_path(Some(Path::new(".bashrc"))).name, "Shell");
    }

    #[test]
    fn shebang_wins() {
        assert_eq!(
            for_path_and_first_line(Some(Path::new("run")), "#!/usr/bin/env python3").name,
            "Python"
        );
        assert_eq!(
            for_path_and_first_line(Some(Path::new("x.rs")), "#!/bin/bash").name,
            "Shell"
        );
        assert_eq!(
            for_path_and_first_line(Some(Path::new("a.rs")), "fn main() {}").name,
            "Rust"
        );
    }

    #[test]
    fn keyword_lookup() {
        let rust = by_name("Rust");
        assert_eq!(rust.lookup("fn"), Some(TokenKind::Keyword));
        assert_eq!(rust.lookup("return"), Some(TokenKind::Control));
        assert_eq!(rust.lookup("u32"), Some(TokenKind::Type));
        assert_eq!(rust.lookup("foo"), None);
        let sql = by_name("SQL");
        assert_eq!(sql.lookup("SELECT"), Some(TokenKind::Keyword));
        assert_eq!(sql.lookup("select"), Some(TokenKind::Keyword));
    }
}
