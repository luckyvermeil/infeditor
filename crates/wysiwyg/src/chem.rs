//! Chemical-formula / LaTeX delimiter normalization (**preview only**).
//!
//! Two real-world document shapes that pulldown does not recognize
//! (`md::parse` only understands `$…$`/`$$…$$`) fall back to plain text
//! when previewed directly:
//!
//! 1. MathJax-style delimiters `\(...\)` / `\[...\]`;
//! 2. The **double-backslash** variant `\\(...\\)` produced by AI exports /
//!    JSON escaping (the sample doc "Why the Earth has so much silicon.md"
//!    is exactly this: `\\(\mathrm{SiO_4}\\)`), with mixed escape levels in
//!    the same document (opening `\\(`, body `\mathrm`, subscript written
//!    as `\_2`).
//!
//! This module normalizes those shapes into pulldown formula delimiters
//! before parsing, and expands the minimal mhchem subset (`\ce{…}`) into
//! latex-rust typesettable `\mathrm{…}` (`math_probe` verified: arrows /
//! `\cdot` / charge superscripts all work inside `\mathrm`). A bare
//! `\ce{…}` (not inside any formula delimiter) expands **with its own
//! `$…$`** — otherwise it comes out as ordinary text and pulldown will not
//! treat it as a formula; a `\ce` inside a delimiter is only expanded, with
//! no `$` added, since the outer delimiter decides the wrapping shape.
//!
//! **Boundaries (must be maintained):**
//! * **Preview only** (`mdmap::md_rebuild` goes through
//!   [`parse_for_preview`]). The edit view (wys seeding) never passes
//!   through here — `\(…\)` stays plain text in the block tree, and
//!   write-back keeps the original delimiters; WYSIWYG's emit normalization
//!   is an established trade-off unrelated to this.
//! * **Code is never converted**: fenced blocks (``` / ~~~) are skipped
//!   whole, inline code spans are skipped byte-for-byte — a `\\(` inside a
//!   URL / code must not be rewritten.
//! * The normalization heuristics (strip `\\`+letter to a single
//!   backslash, restore `\_`+alphanumeric to a subscript) apply only to
//!   extracted formula content; the rest of the body is preserved verbatim.
//! * Fast path: when the source contains no candidate delimiter, return
//!   borrowed with zero allocation (per-block preview is a hot path; we
//!   cannot build a throwaway String for every block).
//! * Scanning is byte-wise (delimiters are all ASCII) and **the copy path is
//!   multibyte-safe**: any non-ASCII run is moved wholesale by its UTF-8
//!   sequence length, so mixed CJK/English text is never split mid-character.

use std::borrow::Cow;

use crate::md;
use crate::model::Doc;

/// Preview-only parse entry: delimiter normalization → standard [`md::parse`].
pub fn parse_for_preview(src: &str) -> Doc {
    md::parse(&normalize(src))
}

/// Whether the source is worth a normalization scan (fast-path predicate;
/// kept a superset of the slow-path rewrite condition — an extra scan is
/// harmless, a miss leaves a formula as body text).
fn has_candidate(src: &str) -> bool {
    src.contains("\\(") || src.contains("\\[") || src.contains("\\ce{")
}

/// Normalize: `\(...\)`/`\[…\]` (including the `\\` double-backslash
/// variant) → `$…$`/`$$…$$`, `\ce{…}` → `\mathrm{…}`. With no candidate,
/// returns borrowed (zero allocation).
pub fn normalize(src: &str) -> Cow<'_, str> {
    if !has_candidate(src) {
        return Cow::Borrowed(src);
    }
    let b = src.as_bytes();
    let mut out = String::with_capacity(src.len());
    let mut i = 0;
    while i < b.len() {
        // Fenced block (≤3 spaces then ``` or ~~~ at line start): copy
        // verbatim through the end of the closing fence line; an unclosed
        // fence extends as code to EOF per CommonMark (its `\(` is not
        // converted).
        if at_line_start(b, i) {
            if let Some(end) = fence_end(b, i) {
                out.push_str(&src[i..end]);
                i = end;
                continue;
            }
        }
        let c = b[i];
        // Inline code span: copy verbatim between a pair of equal-length
        // backticks (if unclosed, the rest is copied as-is).
        if c == b'`' {
            let n = run_len(b, i, b'`');
            match find_run_exact(b, i + n, b'`', n) {
                Some(j) => {
                    out.push_str(&src[i..j + n]);
                    i = j + n;
                }
                None => {
                    out.push_str(&src[i..]);
                    i = b.len();
                }
            }
            continue;
        }
        // `\ce{` in bare text (not inside a formula delimiter): the expansion
        // **ships with its own inline formula delimiters** — emitting only
        // `\mathrm{…}` without `$` would leave pulldown treating it as plain
        // text and the formula would not render.
        if src[i..].starts_with("\\ce{") {
            if let Some(j) = match_brace(b, i + 3) {
                out.push_str("$\\mathrm{");
                out.push_str(&ce_to_latex(&src[i + 4..j]));
                out.push_str("}$");
                i = j + 1;
                continue;
            }
        }
        // Backslash run + `(`/`[`: try as a formula opener (n≤2; n=1 is
        // standard MathJax, n=2 is the export-escaped variant). The closer
        // must be the **same run length** of backslashes + the closing bracket.
        if c == b'\\' {
            let n = run_len(b, i, b'\\');
            if n <= 2 && i + n < b.len() {
                let (close, pre, post) = match b[i + n] {
                    b'(' => (b')', "$", "$"),
                    b'[' => (b']', "$$", "$$"),
                    _ => (0, "", ""),
                };
                if close != 0 {
                    if let Some(j) = find_closer(b, i + n + 1, n, close) {
                        let body = unescape_chem(&src[i + n + 1..j]);
                        // A `\ce{…}` inside the delimiter must also be expanded
                        // (latex-rust does not understand mhchem); here the
                        // expansion carries **no** `$`, because the outer
                        // delimiter already supplied it.
                        let body = expand_ce(body.trim());
                        // Content contains `$`: rewriting to dollar delimiters
                        // would collide with pulldown's formula scan (the first
                        // `$` closes early and swallows/spits out the rest), so
                        // we prefer keeping `\(…\)` verbatim over creating
                        // confusion.
                        if !body.contains('$') {
                            // trim: pulldown inline `$…$` does not allow
                            // leading/trailing whitespace inside (multi-line
                            // block source carries its own newlines/indent).
                            out.push_str(pre);
                            out.push_str(&body);
                            out.push_str(post);
                            i = j + n + 1;
                            continue;
                        }
                    }
                }
            }
        }
        // Everything else: copy verbatim per character (multibyte-safe).
        let ch_len = utf8_len(c);
        out.push_str(&src[i..i + ch_len]);
        i += ch_len;
    }
    Cow::Owned(out)
}

/// Expand `\ce{…}` (possibly several) inside formula content into
/// `\mathrm{…}` — **without** `$`; the caller decides the wrapping shape
/// from the surrounding delimiter (`\(…\)`→`$…$`, `\[…\]`→`$$…$$`).
/// latex-rust does not understand mhchem, so without expansion the whole
/// formula falls back to text. An unbalanced `\ce{` is kept verbatim.
fn expand_ce(s: &str) -> String {
    if !s.contains("\\ce{") {
        return s.to_string();
    }
    let b = s.as_bytes();
    let mut out = String::with_capacity(s.len() + 8);
    let mut i = 0;
    while i < b.len() {
        if s[i..].starts_with("\\ce{") {
            if let Some(j) = match_brace(b, i + 3) {
                out.push_str("\\mathrm{");
                out.push_str(&ce_to_latex(&s[i + 4..j]));
                out.push('}');
                i = j + 1;
                continue;
            }
        }
        let ch_len = utf8_len(b[i]);
        out.push_str(&s[i..i + ch_len]);
        i += ch_len;
    }
    out
}

/// Normalize formula content (applies only to extracted formula bodies):
/// * `\\`+ASCII letter → `\`+letter (the export tool doubled the command
///   backslashes too);
/// * `\_`+alphanumeric → `_` (the sample writes "subscripts" as an escaped
///   underscore; in TeX `\_` means a literal underscore character, but in a
///   chemical formula the author's intent is clearly a subscript).
fn unescape_chem(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\' && i + 2 < b.len() {
            if b[i + 1] == b'\\' && b[i + 2].is_ascii_alphabetic() {
                out.push('\\');
                out.push(b[i + 2] as char);
                i += 3;
                continue;
            }
            if b[i + 1] == b'_' && b[i + 2].is_ascii_alphanumeric() {
                out.push('_');
                i += 2;
                continue;
            }
        }
        let ch_len = utf8_len(b[i]);
        out.push_str(&s[i..i + ch_len]);
        i += ch_len;
    }
    out
}

/// Minimal mhchem subset → LaTeX (the result is wrapped by `\mathrm{…}`):
/// * a digit run after a letter / `)` / `]` / `}` → `_{n}` (stoichiometric
///   subscript);
/// * `^` charge: `^{…}` passes through as-is, `^`+digit/sign run → `^{…}`;
/// * `->` / `→` → `\rightarrow`, `<=>` → `\rightleftharpoons`,
///   `<->` → `\leftrightarrow`; `*` → `\cdot` (hydrate).
/// * Everything else (element symbols, brackets, `+`/`-`, spaces) is kept
///   verbatim.
fn ce_to_latex(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = String::with_capacity(s.len() + 8);
    let mut i = 0;
    let mut last = 0u8; // previous byte's ASCII (0 = start / multibyte)
    while i < b.len() {
        if s[i..].starts_with("<=>") {
            out.push_str("\\rightleftharpoons");
            i += 3;
            sep_or_copy(&mut out, b, i);
            last = b' ';
            continue;
        }
        if s[i..].starts_with("<->") {
            out.push_str("\\leftrightarrow");
            i += 3;
            sep_or_copy(&mut out, b, i);
            last = b' ';
            continue;
        }
        if s[i..].starts_with("->") {
            out.push_str("\\rightarrow");
            i += 2;
            sep_or_copy(&mut out, b, i);
            last = b' ';
            continue;
        }
        if s[i..].starts_with('→') {
            out.push_str("\\rightarrow");
            i += '→'.len_utf8();
            sep_or_copy(&mut out, b, i);
            last = b' ';
            continue;
        }
        match b[i] {
            b'*' => {
                out.push_str("\\cdot");
                i += 1;
                sep_or_copy(&mut out, b, i);
                last = b' ';
            }
            b'^' if i + 1 < b.len() && b[i + 1] == b'{' => {
                // `^{…}` is already a valid LaTeX superscript group; pass
                // through as-is (unbalanced → treated as ordinary text)
                match match_brace(b, i + 1) {
                    Some(j) => {
                        out.push_str(&s[i..=j]);
                        i = j + 1;
                        last = b'}';
                    }
                    None => {
                        out.push('^');
                        i += 1;
                        last = b'^';
                    }
                }
            }
            b'^' => {
                // A digit/sign run after `^` is folded into a superscript
                // group (mhchem charge notation)
                let mut j = i + 1;
                while j < b.len() && (b[j].is_ascii_digit() || b[j] == b'+' || b[j] == b'-') {
                    j += 1;
                }
                if j > i + 1 {
                    out.push_str("^{");
                    out.push_str(&s[i + 1..j]);
                    out.push('}');
                    i = j;
                    last = b'}';
                } else {
                    out.push('^');
                    i += 1;
                    last = b'^';
                }
            }
            c if c.is_ascii_digit()
                && matches!(
                    last,
                    b'a'..=b'z' | b'A'..=b'Z' | b')' | b']' | b'}'
                ) =>
            {
                // A digit run after an element / closing bracket → subscript
                let mut j = i;
                while j < b.len() && b[j].is_ascii_digit() {
                    j += 1;
                }
                out.push_str("_{");
                out.push_str(&s[i..j]);
                out.push('}');
                i = j;
                last = b'}';
            }
            _ => {
                let ch_len = utf8_len(b[i]);
                out.push_str(&s[i..i + ch_len]);
                last = if ch_len == 1 { b[i] } else { 0 };
                i += ch_len;
            }
        }
    }
    out
}

// ---- Small scanning helpers (byte-level; only ASCII delimiters, stepping is
// multibyte-safe) ----

/// Separator after an arrow / `\cdot` macro: if the following character is
/// whitespace it is copied from the source, otherwise a space is inserted
/// (a LaTeX command name swallows the letters right after it —
/// `\rightarrow2` would be read as an unknown command).
fn sep_or_copy(out: &mut String, b: &[u8], i: usize) {
    if i < b.len() && !b[i].is_ascii_whitespace() {
        out.push(' ');
    }
}

/// Length of the ASCII byte run of `c` starting at `b[i]`.
fn run_len(b: &[u8], i: usize, c: u8) -> usize {
    let mut n = 0;
    while i + n < b.len() && b[i + n] == c {
        n += 1;
    }
    n
}

/// Find, from `from`, the start of the first run of **exactly** n `c`s
/// (longer runs are skipped whole, so searching for `` in `x```y` does not
/// land on the prefix of ```).
fn find_run_exact(b: &[u8], from: usize, c: u8, n: usize) -> Option<usize> {
    let mut i = from;
    while i < b.len() {
        if b[i] == c {
            let r = run_len(b, i, c);
            if r == n {
                return Some(i);
            }
            i += r;
        } else {
            i += utf8_len(b[i]);
        }
    }
    None
}

/// Find, from `from`, the **backslash start** of the first
/// "n backslashes + `close`" (a formula closer). Backslash runs of other
/// lengths met along the way are skipped whole: a `\\alpha` in the content
/// must not be mistaken for an n=1 closer candidate.
fn find_closer(b: &[u8], from: usize, n: usize, close: u8) -> Option<usize> {
    let mut k = from;
    while k < b.len() {
        if b[k] == b'\\' {
            let r = run_len(b, k, b'\\');
            if r == n && k + n < b.len() && b[k + n] == close {
                return Some(k);
            }
            k += r;
        } else {
            k += utf8_len(b[k]);
        }
    }
    None
}

/// `b[open]` is `{`; return the index of the matching `}`, or None if
/// unbalanced.
fn match_brace(b: &[u8], open: usize) -> Option<usize> {
    let mut depth = 0usize;
    let mut i = open;
    while i < b.len() {
        match b[i] {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// Line-start test (i==0 or the previous byte is `\n`).
fn at_line_start(b: &[u8], i: usize) -> bool {
    i == 0 || b[i - 1] == b'\n'
}

/// When `i` is a line start followed by a fence opener (≤3 spaces then at least
/// three backticks or tildes), return the index of the **end of the closing
/// fence line** (the end of the interval copied verbatim as one fence block).
///
/// An unclosed fence extends **as code all the way to EOF** per CommonMark
/// (returns `b.len()`): a `\(` there is a code example and must never be
/// rewritten into a formula before being handed to pulldown for typesetting.
fn fence_end(b: &[u8], i: usize) -> Option<usize> {
    let mut j = i;
    let mut spaces = 0;
    while j < b.len() && b[j] == b' ' && spaces < 3 {
        j += 1;
        spaces += 1;
    }
    let fence = *b.get(j)?;
    if fence != b'`' && fence != b'~' {
        return None;
    }
    let n = run_len(b, j, fence);
    if n < 3 {
        return None;
    }
    // End of the opening fence line (the info string is not part of it)
    let mut k = j + n;
    while k < b.len() && b[k] != b'\n' {
        k += 1;
    }
    if k >= b.len() {
        return Some(b.len()); // the opening line is the last line: unclosed, all code
    }
    loop {
        k += 1; // step over \n to the next line start
        if k >= b.len() {
            return Some(b.len()); // unclosed: the whole span is code
        }
        let mut m = k;
        let mut sp = 0;
        while m < b.len() && b[m] == b' ' && sp < 3 {
            m += 1;
            sp += 1;
        }
        if b.get(m) == Some(&fence) && run_len(b, m, fence) >= n {
            while m < b.len() && b[m] != b'\n' {
                m += 1;
            }
            return Some(m);
        }
        while k < b.len() && b[k] != b'\n' {
            k += 1;
        }
        if k >= b.len() {
            return Some(b.len());
        }
    }
}

/// UTF-8 first byte → sequence length (the first byte is never a
/// continuation byte; out-of-range values step by 1 to avoid an infinite
/// loop).
fn utf8_len(first: u8) -> usize {
    match first {
        0x00..=0x7F => 1,
        0xC0..=0xDF => 2,
        0xE0..=0xEF => 3,
        0xF0..=0xF7 => 4,
        _ => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The actual shape in the sample doc "Why the Earth has so much
    /// silicon.md" (double backslashes opening/closing, single backslash in
    /// the body, subscript written as `\_`).
    #[test]
    fn sample_file_doubled_delims() {
        assert_eq!(normalize(r"\\(\mathrm{SiO_4}\\)"), r"$\mathrm{SiO_4}$");
        assert_eq!(normalize(r"\\(\mathrm{SiO_2}\\)"), r"$\mathrm{SiO_2}$");
        // `\_2` → `_2`: the author's intent is a subscript
        assert_eq!(
            normalize(r"\\(\mathrm{(Mg,Fe)\_2SiO_4}\\)"),
            r"$\mathrm{(Mg,Fe)_2SiO_4}$"
        );
    }

    /// Standard MathJax single-backslash delimiters: inline and display.
    #[test]
    fn standard_single_escape_delims() {
        assert_eq!(normalize(r"\(\alpha\)"), r"$\alpha$");
        assert_eq!(normalize(r"\[x^2+y^2\]"), r"$$x^2+y^2$$");
        assert_eq!(
            normalize(r"前文 \(\mathrm{CO_2}\) 后文"),
            r"前文 $\mathrm{CO_2}$ 后文"
        );
    }

    /// Non-formula body text is preserved verbatim; with no candidate the
    /// fast path is taken (borrowed, zero allocation).
    #[test]
    fn plain_text_untouched() {
        assert!(matches!(normalize("普通文本 (a) [b]"), Cow::Borrowed(_)));
        // An opener with no closer: passed through as-is
        assert!(matches!(normalize(r"\(\alpha 剩余"), Cow::Owned(_)));
        assert_eq!(normalize(r"\(\alpha 剩余"), r"\(\alpha 剩余");
    }

    /// Code is never converted: a `\(` inside a fenced block or an inline
    /// code span is kept verbatim.
    #[test]
    fn code_regions_untouched() {
        assert_eq!(normalize("```\n\\(x\\)\n```"), "```\n\\(x\\)\n```");
        assert_eq!(normalize("~~~\n\\(x\\)\n~~~"), "~~~\n\\(x\\)\n~~~");
        assert_eq!(
            normalize(r"前 `\(x\)` 后 \(y\) 尾"),
            r"前 `\(x\)` 后 $y$ 尾"
        );
        // An unclosed inline code span: the rest is all copied as-is
        assert_eq!(normalize(r"a `\(x\) b"), r"a `\(x\) b");
        // An unclosed fence: per CommonMark the whole span is code (not
        // converted)
        assert_eq!(normalize("```txt\n\\(x\\)"), "```txt\n\\(x\\)");
    }

    /// Formula content contains `$`: rewriting would collide with the dollar
    /// delimiters, so `\(…\)` is kept verbatim.
    #[test]
    fn dollar_inside_content_stays_verbatim() {
        assert_eq!(normalize(r"\(a $ b\)"), r"\(a $ b\)");
        assert_eq!(normalize(r"\(x\) 与 \(a $ b\)"), r"$x$ 与 \(a $ b\)");
    }

    /// Pairing basics: ordinary parentheses inside the content do not
    /// interfere (only backslash + closing bracket counts), and a newline
    /// does not break pairing.
    #[test]
    fn pairing_basics() {
        assert_eq!(normalize(r"\(f(x)\)"), r"$f(x)$");
        assert_eq!(normalize("\\(a\nb\\)"), "$a\nb$");
    }

    /// A `\\alpha` (double-backslash command) in the content does not skew
    /// the n=1 closer search.
    #[test]
    fn closer_search_skips_wrong_runs() {
        assert_eq!(normalize(r"\(\\alpha\)"), r"$\alpha$");
    }

    /// Minimal mhchem subset: stoichiometric subscripts, charges, arrows,
    /// hydrates. A bare `\ce{…}` comes with its own inline delimiters
    /// (otherwise pulldown treats it as plain text and does not render);
    /// subscripts always carry braces `_{n}` (isomorphic for one or two
    /// digits, equivalent for latex-rust rendering).
    #[test]
    fn mhchem_subset_expansion() {
        assert_eq!(normalize(r"\ce{H2O}"), r"$\mathrm{H_{2}O}$");
        assert_eq!(
            normalize(r"\ce{2H2 + O2 -> 2H2O}"),
            r"$\mathrm{2H_{2} + O_{2} \rightarrow 2H_{2}O}$"
        );
        // ^ run charge
        assert_eq!(normalize(r"\ce{SO4^2-}"), r"$\mathrm{SO_{4}^{2-}}$");
        // ^{} passes through as-is
        assert_eq!(normalize(r"\ce{Fe^{3+}}"), r"$\mathrm{Fe^{3+}}$");
        // Hydrate
        assert_eq!(
            normalize(r"\ce{CuSO4*5H2O}"),
            r"$\mathrm{CuSO_{4}\cdot 5H_{2}O}$"
        );
        // Reversible arrow
        assert_eq!(
            normalize(r"\ce{CaCO3 <=> CaO + CO2}"),
            r"$\mathrm{CaCO_{3} \rightleftharpoons CaO + CO_{2}}$"
        );
        // Digits after a bracket are also a subscript
        assert_eq!(normalize(r"\ce{Fe(CN)6^3-}"), r"$\mathrm{Fe(CN)_{6}^{3-}}$");
        // Unbalanced braces: passed through whole, no panic
        assert_eq!(normalize(r"\ce{H2O"), r"\ce{H2O");
    }

    /// A `\ce{…}` inside a formula delimiter is also expanded, and does
    /// **not** get a duplicate `$` (a missed expansion once made latex-rust
    /// reject the `\ce`, so the whole formula fell back to text).
    #[test]
    fn ce_inside_delimiters_expands_without_extra_dollars() {
        assert_eq!(normalize(r"\(\ce{H2O}\)"), r"$\mathrm{H_{2}O}$");
        assert_eq!(normalize(r"\[\ce{SO4^2-}\]"), r"$$\mathrm{SO_{4}^{2-}}$$");
        // Mixed with ordinary LaTeX
        assert_eq!(normalize(r"\(2\ce{H2O} + x\)"), r"$2\mathrm{H_{2}O} + x$");
    }

    /// End to end: a list line from the sample parses to a Math inline, and
    /// the preview HTML contains the formula SVG.
    #[test]
    fn end_to_end_list_line_renders_math() {
        let src = r"- 石英：\\(\mathrm{SiO_2}\\)";
        let doc = parse_for_preview(src);
        let has_math = doc.blocks.iter().any(|b| {
            let crate::model::Block::Li { inlines, .. } = b else {
                return false;
            };
            inlines
                .iter()
                .any(|i| matches!(i, crate::model::Inline::Math { .. }))
        });
        assert!(
            has_math,
            "the chemical formula in the list line should parse to Math: {doc:?}"
        );
        let html = crate::preview_html(&doc, crate::math::DEFAULT_FG, None);
        assert!(
            html.contains("<svg"),
            "the preview should inline the formula SVG: {html}"
        );
        assert!(
            !html.contains("mathrm"),
            "the LaTeX source must not leak out as text: {html}"
        );
    }

    /// End to end: MathJax delimiters in an ordinary paragraph also become
    /// formulas; the two render as separate SVGs each (an un-expanded `\ce`
    /// inside a delimiter would fall back to text, leaving only 1 svg).
    #[test]
    fn end_to_end_display_and_inline() {
        let doc = parse_for_preview(r"段 \(\mathrm{H2O}\) 与 \[\ce{SO4^2-}\]");
        let html = crate::preview_html(&doc, crate::math::DEFAULT_FG, None);
        assert_eq!(html.matches("<svg").count(), 2, "{html}");
    }

    /// End to end: a bare `\ce{…}` paragraph renders directly as formulas
    /// (no hand-written delimiters needed).
    #[test]
    fn end_to_end_bare_ce_renders() {
        let html = crate::preview_html(
            &parse_for_preview(r"水是 \ce{H2O}，二氧化碳是 \ce{CO2}。"),
            crate::math::DEFAULT_FG,
            None,
        );
        assert_eq!(html.matches("<svg").count(), 2, "{html}");
    }

    /// Multibyte body text does not break the scan: delimiters pair
    /// normally on both sides of CJK text.
    #[test]
    fn multibyte_text_safe() {
        assert_eq!(
            normalize(r"硅与氧结合成 \\(\mathrm{SiO_4}\\) 四面体。"),
            "硅与氧结合成 $\\mathrm{SiO_4}$ 四面体。"
        );
    }

    /// A multi-line chemical formula inside a list item (block source with
    /// newlines) still pairs up.
    #[test]
    fn multiline_block_pairs() {
        let src = "- 橄榄石：\\\\(\n  \\\\mathrm{(Mg,Fe)\\\\_2SiO_4}\\\\)";
        let html = crate::preview_html(&parse_for_preview(src), crate::math::DEFAULT_FG, None);
        assert!(html.contains("<svg"), "{html}");
    }
}
