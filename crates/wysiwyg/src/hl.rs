//! Code-block coloring hook (optional; by default only escaping, no
//! coloring).
//!
//! wysiwyg is a standalone crate and does not depend on any particular
//! language lexer; the coloring capability is registered by the host
//! (infeditor uses `highlight::lexer` + a theme palette). When the host does
//! not register one, the behavior is as before: code blocks are escaped and
//! emitted as plain text.
//!
//! The hook is an `fn`, not a closure: registered once, shared within the
//! process, so call sites do not have to thread a parameter through (the
//! function signatures of the `preview` and `render` paths stay stable, and
//! the 313 existing tests are untouched).

use std::sync::OnceLock;

/// `(language name, raw code, primary foreground)` → **already-escaped**
/// colored HTML.
///
/// Returning an empty string is equivalent to "no coloring"; the caller falls
/// back to [`crate::preview::escape_html`]. `fg` lets the host pick the
/// light/dark palette (the preview side splits the GitHub-style baseline the
/// same way).
pub type CodeHighlighter = fn(&str, &str, &str) -> String;

static HOOK: OnceLock<CodeHighlighter> = OnceLock::new();

/// Register the colorizer. **Call only once at startup**; a repeated call is a
/// no-op.
pub fn set_code_highlighter(f: CodeHighlighter) {
    let _ = HOOK.set(f);
}

/// Try to color a piece of code. Not registered, or the colorizer returned an
/// empty string → `None` (the caller escapes).
pub fn highlight(lang: &str, code: &str, fg: &str) -> Option<String> {
    HOOK.get()
        .map(|f| f(lang, code, fg))
        .filter(|s| !s.is_empty())
}
