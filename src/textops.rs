//! Pure text transformations behind the Edit menu / editor context menu:
//! case conversion, blank operations, line operations and line sorting, plus
//! the character-class predicate shared by the motion and selection code.
//!
//! Everything here works on plain `String`s so it can be unit-tested without a
//! document, a UI or a window. The state-level wrappers in
//! [`crate::ui::commands`] decide *which* lines to feed in (the selection, or
//! the whole document) and put the result back as a single undo step.

/// Length in bytes of the UTF-8 sequence whose lead byte is `b` (1..=4).
///
/// The whole-repo's only "lead byte → codepoint length" implementation. Returns 1 for stray
/// continuation bytes and invalid leads (0xF8..=0xFF) — callers advancing by the length always
/// make progress and never stall; on valid UTF-8 input it is byte-for-byte equivalent to any
/// high-bit-based approach.
#[inline]
pub fn utf8_seq_len(b: u8) -> usize {
    if b < 0x80 {
        1
    } else if b >> 5 == 0b110 {
        2
    } else if b >> 4 == 0b1110 {
        3
    } else if b >> 3 == 0b11110 {
        4
    } else {
        1 // stray continuation byte: never let the cursor stall
    }
}

/// Whether `c` is a word character for the editor's run-based rules — the class
/// that `Ctrl+←/→` word motion, double-click selection and Vim's `w` / `b` / `e`
/// step over.
///
/// Letters, digits and `_` are one class; everything else is "not a word", so
/// punctuation and every non-alphanumeric symbol form runs of their own (see the
/// class table in `ui::commands::multiclick`).
///
/// Deliberately one home: its consumers must agree on where a word starts and
/// ends, and a drifted copy is a silent failure — `dw` and `Ctrl+→` would stop
/// at different characters in the same document.
#[inline]
pub fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Case conversions exposed by Edit → Case Conversion.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CaseKind {
    /// ALL UPPERCASE
    Upper,
    /// all lowercase
    Lower,
    /// Every Word Capitalised ("Proper Case")
    Proper,
    /// First letter of each sentence capitalised
    Sentence,
    /// sWAP eVERY cHARACTER'S cASE
    Invert,
    /// Half-width → full-width: ASCII letters/digits/punctuation
    /// become their full-width forms, space becomes U+3000.
    FullWidth,
    /// Full-width → half-width: the inverse of [`CaseKind::FullWidth`].
    HalfWidth,
}

pub fn convert_case(s: &str, kind: CaseKind) -> String {
    match kind {
        CaseKind::Upper => s.to_uppercase(),
        CaseKind::Lower => s.to_lowercase(),
        CaseKind::Invert => s
            .chars()
            .map(|c| {
                if c.is_uppercase() {
                    c.to_lowercase().next().unwrap_or(c)
                } else if c.is_lowercase() {
                    c.to_uppercase().next().unwrap_or(c)
                } else {
                    c
                }
            })
            .collect(),
        CaseKind::Proper => {
            let mut out = String::with_capacity(s.len());
            let mut at_word_start = true;
            for c in s.chars() {
                if c.is_alphanumeric() {
                    if at_word_start {
                        out.extend(c.to_uppercase());
                    } else {
                        out.extend(c.to_lowercase());
                    }
                    at_word_start = false;
                } else {
                    at_word_start = true;
                    out.push(c);
                }
            }
            out
        }
        CaseKind::Sentence => {
            let mut out = String::with_capacity(s.len());
            let mut at_sentence_start = true;
            for c in s.chars() {
                if at_sentence_start && c.is_alphabetic() {
                    out.extend(c.to_uppercase());
                    at_sentence_start = false;
                } else {
                    out.extend(c.to_lowercase());
                    if matches!(c, '.' | '!' | '?' | '\n') {
                        at_sentence_start = true;
                    }
                }
            }
            out
        }
        // Full/half-width: only the ASCII block and space have well-defined, lossless
        // mappings (U+FF01..U+FF5E ↔ U+0021..U+007E, U+3000 ↔ U+0020). Katakana
        // half/full-width conversion is deliberately out of scope: the mapping
        // is one-to-many with voiced marks and would corrupt data rather than
        // convert it.
        CaseKind::FullWidth => {
            let mut out = String::with_capacity(s.len() * 3);
            for c in s.chars() {
                match c {
                    ' ' => out.push('\u{3000}'),
                    c @ '\u{21}'..='\u{7e}' => {
                        out.push(char::from_u32(0xfee0 + c as u32).unwrap_or(c))
                    }
                    c => out.push(c),
                }
            }
            out
        }
        CaseKind::HalfWidth => {
            let mut out = String::with_capacity(s.len());
            for c in s.chars() {
                match c {
                    '\u{3000}' => out.push(' '),
                    c @ '\u{ff01}'..='\u{ff5e}' => {
                        out.push(char::from_u32(c as u32 - 0xfee0).unwrap_or(c))
                    }
                    c => out.push(c),
                }
            }
            out
        }
    }
}

/// Unicode normalization: NFC (composed) or NFD (decomposed).
pub fn unicode_normalize(s: &str, nfd: bool) -> String {
    use unicode_normalization::UnicodeNormalization;
    if nfd {
        s.chars().nfd().collect()
    } else {
        s.chars().nfc().collect()
    }
}
pub fn trim(s: &str, leading: bool, trailing: bool) -> String {
    match (leading, trailing) {
        (true, true) => s.trim().to_string(),
        (true, false) => s.trim_start().to_string(),
        (false, true) => s.trim_end().to_string(),
        (false, false) => s.to_string(),
    }
}

/// Expand tabs to spaces ("TAB to Space").
///
/// A tab advances to the next multiple of `tab_size`, exactly like the glyph is
/// laid out, so the visual column of the following text is preserved.
pub fn tabs_to_spaces(s: &str, tab_size: usize) -> String {
    let tab_size = tab_size.max(1);
    let mut out = String::with_capacity(s.len());
    // visual column within the current line — resets at every newline, so a
    // tab on line N+1 advances relative to line N+1's start, not line N's end
    let mut col = 0usize;
    for c in s.chars() {
        match c {
            '\t' => {
                let pad = tab_size - (col % tab_size);
                for _ in 0..pad {
                    out.push(' ');
                }
                col += pad;
            }
            '\n' => {
                out.push(c);
                col = 0;
            }
            _ => {
                out.push(c);
                col += 1;
            }
        }
    }
    out
}

/// Replace runs of spaces with tabs ("Space to TAB").
///
/// `leading_only` limits the conversion to the indentation of each line, which
/// is what "Space to TAB (Leading)" does; the plain variant converts every run.
pub fn spaces_to_tabs(s: &str, tab_size: usize, leading_only: bool) -> String {
    let tab_size = tab_size.max(1);
    let mut out = String::with_capacity(s.len());
    let mut col = 0usize;
    let mut i = 0usize;
    let bytes = s.as_bytes();
    // "leading" is a per-line property: the flag must reset at every newline,
    // otherwise a multi-line input whose first line happens to start with a
    // space would have *every* space run (mid-line, trailing) converted.
    let mut at_line_start = true;
    while i < bytes.len() {
        if bytes[i] == b' ' && (!leading_only || at_line_start) {
            let start = i;
            while i < bytes.len() && bytes[i] == b' ' {
                i += 1;
            }
            let run = i - start;
            let mut rem = run;
            while rem > 0 {
                let to_tab = tab_size - (col % tab_size);
                if rem >= to_tab {
                    out.push('\t');
                    col += to_tab;
                    rem -= to_tab;
                } else {
                    for _ in 0..rem {
                        out.push(' ');
                    }
                    col += rem;
                    rem = 0;
                }
            }
            at_line_start = false;
        } else if bytes[i] == b'\t' {
            out.push('\t');
            col += tab_size - (col % tab_size);
            i += 1;
            at_line_start = false;
        } else {
            let ch = s[i..].chars().next().unwrap_or('\0');
            out.push(ch);
            col += 1;
            i += ch.len_utf8();
            if ch == '\n' {
                at_line_start = true;
                col = 0;
            } else {
                at_line_start = false;
            }
        }
    }
    out
}

/// Drop empty lines.
///
/// When `also_whitespace_only` is set, lines that consist only of blanks are
/// removed too — "Remove Empty Lines (Containing Blank
/// characters)" as opposed to the plain "Remove Empty Lines".
pub fn remove_empty(lines: &[String], also_whitespace_only: bool) -> Vec<String> {
    lines
        .iter()
        .filter(|l| {
            if also_whitespace_only {
                !l.trim().is_empty()
            } else {
                !l.is_empty()
            }
        })
        .cloned()
        .collect()
}

/// Drop duplicate lines, either globally or only adjacent ones (the
/// "Remove Duplicate Lines" vs "Remove Consecutive Duplicate Lines" pair).
pub fn remove_duplicates(lines: &[String], consecutive_only: bool) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(lines.len());
    if consecutive_only {
        for l in lines {
            if out.last() != Some(l) {
                out.push(l.clone());
            }
        }
    } else {
        let mut seen = std::collections::HashSet::new();
        for l in lines {
            if seen.insert(l.clone()) {
                out.push(l.clone());
            }
        }
    }
    out
}

/// Key used by Lines → Sort.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SortKey {
    /// Byte/char order.
    Text,
    /// Case-insensitive text order.
    TextIgnoreCase,
    /// By character count.
    Length,
    /// By the first integer found on the line.
    Integer,
    /// By the first decimal found on the line, `.` as the separator.
    DecimalDot,
    /// By the first decimal found on the line, `,` as the separator.
    DecimalComma,
}

/// Numeric key of a line: the first signed number found in it.
///
/// `dec` selects the decimal separator that is accepted (`None` = integers
/// only). The value is a real `f64` rather than a scaled integer, because
/// scaled integers only compare correctly when every line carries the same
/// number of decimals (`3.5` → 35 vs `3.25` → 325 would sort backwards).
/// A line with no number at all yields 0, which is what makes "sort by integer"
/// put unnumbered lines first.
fn first_number(line: &str, dec: Option<char>) -> f64 {
    let mut int_part = String::new();
    let mut frac_part = String::new();
    let mut sign = 1.0f64;
    let mut started = false;
    let mut in_frac = false;
    for c in line.chars() {
        if !started {
            if c == '-' {
                sign = -1.0;
            } else if c.is_ascii_digit() {
                int_part.push(c);
            } else {
                continue;
            }
            started = true;
            continue;
        }
        if c.is_ascii_digit() {
            if in_frac {
                frac_part.push(c);
            } else {
                int_part.push(c);
            }
        } else if Some(c) == dec && !in_frac {
            in_frac = true;
        } else {
            break;
        }
    }
    if !started {
        return 0.0;
    }
    let mut v: f64 = int_part.parse().unwrap_or(0.0);
    if !frac_part.is_empty() {
        let f: f64 = frac_part.parse().unwrap_or(0.0);
        v += f / 10f64.powi(frac_part.len() as i32);
    }
    sign * v
}

/// Sort lines in place.
///
/// The sort is performed on a permutation of indices: the keys are computed
/// once (the numeric ones do real parsing), the permutation is sorted, then
/// applied to `lines` **in place by cycle-walking** — no snapshot copy and no
/// per-line clone on write-back. `guard_mass_op` lets selections up to 128MB
/// in here; the previous snapshot + text-key + clone-rewrite model peaked at
/// ~4-5× the selection size. Ties are broken by the original line index, so
/// equal keys keep their previous relative order in *both* ascending and
/// descending mode.
pub fn sort_lines(lines: &mut [String], key: SortKey, desc: bool) {
    let n = lines.len();
    let mut order: Vec<usize> = (0..n).collect();
    // Comparison wrapper: desc inverts + original index ascending as the tiebreaker (stability
    // semantics match the old implementation)
    let wrap = |ord: std::cmp::Ordering, a: usize, b: usize| {
        let mut o = if desc { ord.reverse() } else { ord };
        if o == std::cmp::Ordering::Equal {
            o = a.cmp(&b);
        }
        o
    };
    match key {
        // Plain text order: compare by borrowing the line itself (no key materialized)
        SortKey::Text => {
            order.sort_by(|&a, &b| wrap(lines[a].cmp(&lines[b]), a, b));
        }
        // Ignore case: the only mode that needs keys materialized (one lowercased copy per
        // line, 1×)
        SortKey::TextIgnoreCase => {
            let keys: Vec<String> = lines.iter().map(|l| l.to_lowercase()).collect();
            order.sort_by(|&a, &b| wrap(keys[a].cmp(&keys[b]), a, b));
        }
        SortKey::Length => {
            let keys: Vec<f64> = lines.iter().map(|l| l.chars().count() as f64).collect();
            order.sort_by(|&a, &b| {
                wrap(
                    keys[a]
                        .partial_cmp(&keys[b])
                        .unwrap_or(std::cmp::Ordering::Equal),
                    a,
                    b,
                )
            });
        }
        SortKey::Integer => {
            let keys: Vec<f64> = lines.iter().map(|l| first_number(l, None)).collect();
            order.sort_by(|&a, &b| {
                wrap(
                    keys[a]
                        .partial_cmp(&keys[b])
                        .unwrap_or(std::cmp::Ordering::Equal),
                    a,
                    b,
                )
            });
        }
        SortKey::DecimalDot => {
            let keys: Vec<f64> = lines.iter().map(|l| first_number(l, Some('.'))).collect();
            order.sort_by(|&a, &b| {
                wrap(
                    keys[a]
                        .partial_cmp(&keys[b])
                        .unwrap_or(std::cmp::Ordering::Equal),
                    a,
                    b,
                )
            });
        }
        SortKey::DecimalComma => {
            let keys: Vec<f64> = lines.iter().map(|l| first_number(l, Some(','))).collect();
            order.sort_by(|&a, &b| {
                wrap(
                    keys[a]
                        .partial_cmp(&keys[b])
                        .unwrap_or(std::cmp::Ordering::Equal),
                    a,
                    b,
                )
            });
        }
    }
    // Apply the permutation in place: new[i] = old[order[i]]. Move along cycles (one temporary
    // slot per cycle, filled from hold when the cycle closes), zero clones.
    let mut seen = vec![false; n];
    for i in 0..n {
        if seen[i] {
            continue;
        }
        let hold = std::mem::take(&mut lines[i]);
        let mut cur = i;
        loop {
            seen[cur] = true;
            let src = order[cur];
            if src == i {
                lines[cur] = hold;
                break;
            }
            lines[cur] = std::mem::take(&mut lines[src]);
            cur = src;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn case_conversions() {
        assert_eq!(convert_case("aBc dEf", CaseKind::Upper), "ABC DEF");
        assert_eq!(convert_case("aBc dEf", CaseKind::Lower), "abc def");
        assert_eq!(convert_case("hello world", CaseKind::Proper), "Hello World");
        assert_eq!(convert_case("aBc dEf", CaseKind::Invert), "AbC DeF");
        assert_eq!(
            convert_case("hi there. bye", CaseKind::Sentence),
            "Hi there. Bye"
        );
        // Full/half-width round trip: ASCII + space lossless
        let src = "AbZ 09 !?~";
        assert_eq!(
            convert_case(src, CaseKind::FullWidth),
            "ＡｂＺ\u{3000}０９\u{3000}！？～"
        );
        assert_eq!(
            convert_case(&convert_case(src, CaseKind::FullWidth), CaseKind::HalfWidth),
            src
        );
        // Unmapped characters are kept as-is
        assert_eq!(convert_case("中文café", CaseKind::FullWidth), "中文ｃａｆé");
    }

    #[test]
    fn unicode_normalization_round_trips() {
        let s = "caf\u{e9}"; // é precomposed
        assert_eq!(unicode_normalize(s, false), s);
        let decomposed = unicode_normalize(s, true);
        assert_ne!(decomposed, s); // e + U+0301
        assert_eq!(unicode_normalize(&decomposed, false), s); // back to NFC
    }

    #[test]
    fn trims() {
        assert_eq!(trim("  a  ", true, false), "a  ");
        assert_eq!(trim("  a  ", false, true), "  a");
        assert_eq!(trim("  a  ", true, true), "a");
        assert_eq!(trim("  a  ", false, false), "  a  ");
    }

    #[test]
    fn tab_expansion_respects_columns() {
        assert_eq!(tabs_to_spaces("\tx", 4), "    x");
        assert_eq!(tabs_to_spaces("a\tx", 4), "a   x");
        assert_eq!(tabs_to_spaces("abcd\tx", 4), "abcd    x");
    }

    #[test]
    fn space_to_tab_roundtrip() {
        assert_eq!(spaces_to_tabs("    x", 4, true), "\tx");
        // A run that lands exactly on a tab stop collapses to one tab.
        assert_eq!(spaces_to_tabs("a   b", 4, false), "a\tb");
        // A run that overshoots keeps the remainder as spaces so the visual
        // column of the following text is preserved.
        assert_eq!(spaces_to_tabs("a    b", 4, false), "a\t b");
        // leading-only leaves interior runs alone
        assert_eq!(spaces_to_tabs("a    b", 4, true), "a    b");
        // and it is the inverse of `tabs_to_spaces` on indentation
        let expanded = tabs_to_spaces("\t\tx", 4);
        assert_eq!(spaces_to_tabs(&expanded, 4, true), "\t\tx");
    }

    #[test]
    fn empty_and_duplicate_removal() {
        let v: Vec<String> = ["a", "", "  ", "a", "b", "b"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(remove_empty(&v, false).len(), 5); // drops only the empty line
        assert_eq!(remove_empty(&v, true).len(), 4); // drops "" and "  "
        assert_eq!(
            remove_duplicates(&v, true),
            ["a", "", "  ", "a", "b"]
                .iter()
                .map(|s| s.to_string())
                .collect::<Vec<_>>()
        );
        assert_eq!(
            remove_duplicates(&v, false),
            ["a", "", "  ", "b"]
                .iter()
                .map(|s| s.to_string())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn numeric_keys() {
        assert_eq!(first_number("  -12abc", None), -12.0);
        assert_eq!(first_number("x42", None), 42.0);
        assert_eq!(first_number("3.25", Some('.')), 3.25);
        assert_eq!(first_number("3,25", Some(',')), 3.25);
        // A dot is not a separator when the comma variant is requested.
        assert_eq!(first_number("3.25", Some(',')), 3.0);
        assert_eq!(first_number("no digits", None), 0.0);
    }

    #[test]
    fn sorts_by_text_and_length() {
        let mut v: Vec<String> = ["banana", "apple", "Cherry"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        sort_lines(&mut v, SortKey::Text, false);
        assert_eq!(v, ["Cherry", "apple", "banana"]);

        let mut v: Vec<String> = ["ccc", "a", "bb"].iter().map(|s| s.to_string()).collect();
        sort_lines(&mut v, SortKey::Length, false);
        assert_eq!(v, ["a", "bb", "ccc"]);
        sort_lines(&mut v, SortKey::Length, true);
        assert_eq!(v, ["ccc", "bb", "a"]);
    }

    #[test]
    fn sorts_numerically() {
        let mut v: Vec<String> = ["10", "2", "1"].iter().map(|s| s.to_string()).collect();
        sort_lines(&mut v, SortKey::Integer, false);
        assert_eq!(v, ["1", "2", "10"]);

        let mut v: Vec<String> = ["3.5 x", "3.25 x", "3.75 x"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        sort_lines(&mut v, SortKey::DecimalDot, false);
        assert_eq!(v, ["3.25 x", "3.5 x", "3.75 x"]);
    }

    #[test]
    fn sort_is_stable_for_equal_keys() {
        // Equal keys must keep their original order, ascending *and* descending.
        let mut v: Vec<String> = ["b1", "a2", "a1", "b2"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        sort_lines(&mut v, SortKey::Length, false);
        assert_eq!(v, ["b1", "a2", "a1", "b2"]);
        sort_lines(&mut v, SortKey::Length, true);
        assert_eq!(v, ["b1", "a2", "a1", "b2"]);
    }
}
