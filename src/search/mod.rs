//! Search & replace engine: plain / regex / escaped modes with semantics
//! (`\n` in regex patterns matches CRLF, LF or CR).
//!
//! Works over an immutable [`PieceTable`] snapshot so searching runs on a
//! background thread while the UI keeps editing. Huge documents are scanned
//! in 16 MB windows with an 8 MB overlap: matches longer than 8 MB that
//! straddle a window seam can be missed (a pathological case, documented).

use std::ops::Range;

use regex::{Regex, RegexBuilder};

use crate::buffer::piece_table::PieceTable;

const WIN: u64 = 16 * 1024 * 1024;
const OVERLAP: u64 = 8 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SearchMode {
    /// literal text
    Plain,
    /// regular expression
    Regex,
    /// literal text with backslash escapes interpreted
    Esc,
}

#[derive(Clone, Debug)]
pub struct FindOptions {
    pub pattern: String,
    pub mode: SearchMode,
    pub case_sensitive: bool,
    pub whole_word: bool,
}

impl Default for FindOptions {
    fn default() -> Self {
        FindOptions {
            pattern: String::new(),
            mode: SearchMode::Plain,
            case_sensitive: false,
            whole_word: false,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Match {
    pub start: u64,
    pub end: u64,
}

#[derive(Debug)]
pub struct SearchResults {
    pub matches: Vec<Match>,
    /// stopped because the limit was reached
    pub truncated: bool,
    /// stopped because the cancel flag was set
    pub cancelled: bool,
    /// how far the scan got (byte position)
    pub scanned_to: u64,
}

pub struct CompiledFind {
    pub re: Regex,
    pub opts: FindOptions,
}

/// Map `\r` / `\n` escape sequences (outside character classes) to an
/// alternation that matches any line-break style.
pub fn normalize_pattern(pattern: &str, mode: SearchMode) -> String {
    match mode {
        SearchMode::Plain => regex::escape(pattern),
        SearchMode::Esc => interpret_escapes(pattern),
        SearchMode::Regex => normalize_regex_newlines(pattern),
    }
}

fn normalize_regex_newlines(pattern: &str) -> String {
    let mut out = String::with_capacity(pattern.len() + 16);
    let b: Vec<char> = pattern.chars().collect();
    let mut i = 0;
    let mut in_class = false;
    while i < b.len() {
        let c = b[i];
        if c == '\\' {
            if i + 1 < b.len() {
                let n = b[i + 1];
                if !in_class && (n == 'n' || n == 'r') {
                    // \r\n written as two escapes -> one alternation
                    if n == 'r' && i + 3 < b.len() && b[i + 2] == '\\' && b[i + 3] == 'n' {
                        out.push_str("(?:\\r\\n|\\r|\\n)");
                        i += 4;
                        continue;
                    }
                    out.push_str("(?:\\r\\n|\\r|\\n)");
                    i += 2;
                    continue;
                }
                out.push('\\');
                out.push(n);
                // an *escaped* `\[` is a literal bracket, not a class start —
                // treating it as one made `\n` inside `\[a\nb]` keep its
                // escape form instead of being rewritten to the alternation
                i += 2;
                continue;
            }
            out.push('\\');
            i += 1;
            continue;
        }
        if c == '[' {
            in_class = true;
        } else if c == ']' {
            in_class = false;
        }
        out.push(c);
        i += 1;
    }
    out
}

/// Shared escape interpreter for the two public variants: resolves
/// `\n` / `\r` / `\t` / `\0` / `\xHH`; unknown escapes pass through.
fn interpret_escapes_core(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let b: Vec<char> = s.chars().collect();
    let mut i = 0;
    while i < b.len() {
        if b[i] == '\\' && i + 1 < b.len() {
            let n = b[i + 1];
            match n {
                'n' => out.push('\n'),
                'r' => out.push('\r'),
                't' => out.push('\t'),
                '0' => out.push('\0'),
                '\\' => out.push('\\'),
                'x' if i + 3 < b.len() => {
                    let hex: String = b[i + 2..i + 4].iter().collect();
                    if let Ok(v) = u32::from_str_radix(&hex, 16) {
                        if let Some(ch) = char::from_u32(v) {
                            out.push(ch);
                            i += 4;
                            continue;
                        }
                    }
                    out.push('\\');
                    out.push('x');
                    i += 2;
                }
                other => {
                    out.push('\\');
                    out.push(other);
                    i += 2;
                    continue;
                }
            }
            i += 2;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    out
}

fn interpret_escapes(s: &str) -> String {
    // escapes resolved -> treat as literal text
    regex::escape(&interpret_escapes_core(s))
}

impl CompiledFind {
    pub fn new(opts: FindOptions) -> Result<Self, regex::Error> {
        let pat = normalize_pattern(&opts.pattern, opts.mode);
        let pat = if opts.whole_word {
            format!(r"\b(?:{})\b", pat)
        } else {
            pat
        };
        let re = RegexBuilder::new(&pat)
            .case_insensitive(!opts.case_sensitive)
            .multi_line(true)
            .unicode(true)
            .build()?;
        Ok(CompiledFind { re, opts })
    }

    pub fn is_empty(&self) -> bool {
        self.opts.pattern.is_empty()
    }
}

/// Read `[lo, hi)` from the table snapped to UTF-8 boundaries.
fn read_snap(table: &PieceTable, lo: u64, hi: u64) -> (u64, String) {
    let total = table.len();
    let mut lo = lo.min(total);
    let mut hi = hi.min(total);
    // snap lo up to a char boundary, hi down
    while lo < hi && !table.is_boundary(lo) {
        lo += 1;
    }
    while hi > lo && !table.is_boundary(hi) {
        hi -= 1;
    }
    let bytes = table.read(lo..hi);
    match String::from_utf8(bytes) {
        Ok(s) => (lo, s),
        Err(e) => (lo, String::from_utf8_lossy(e.as_bytes()).into_owned()),
    }
}

/// Extra context bytes read on both sides of the window. Whole-word matching
/// (`\b`) needs to see the complete character on both sides of a match to judge a
/// word boundary correctly: reading only 1 extra byte lets the window's start land
/// inside a multi-byte character, and after the snap that character is dropped
/// entirely — hay then begins with a word character, the regex treats the start of
/// hay as a boundary, and you get a false positive (`\bbar\b` matching the `bar`
/// that follows a multi-byte character). UTF-8 is at most 4 bytes, so reading 4
/// extra bytes guarantees any character straddling the window edge enters hay
/// whole.
const CTX: u64 = 4;

fn accept_hi(lo: u64, hi: u64, at_eof: bool) -> u64 {
    if at_eof {
        hi
    } else {
        lo + WIN - OVERLAP
    }
}

/// Find the first match at/after `from` within `[from, limit)`.
pub fn find_next(table: &PieceTable, cf: &CompiledFind, from: u64, limit: u64) -> Option<Match> {
    if cf.is_empty() {
        return None;
    }
    let total = table.len();
    let mut lo = from.min(total);
    loop {
        let hi = (lo + WIN).min(total);
        let at_eof = hi >= total;
        let (base, hay) = read_snap(table, lo.saturating_sub(CTX), (hi + CTX).min(total));
        let upto = accept_hi(lo, hi, at_eof).min(limit);
        for m in cf.re.find_iter(&hay) {
            let s = base + m.start() as u64;
            let e = base + m.end() as u64;
            if s < from {
                continue;
            }
            if s < upto && e <= limit {
                return Some(Match { start: s, end: e });
            }
        }
        if at_eof || hi >= limit {
            return None;
        }
        let next = (hi - OVERLAP).max(lo + 1);
        lo = next;
    }
}

/// Find the last match ending at/before `from` within `[limit_start, from]`.
pub fn find_prev(
    table: &PieceTable,
    cf: &CompiledFind,
    from: u64,
    limit_start: u64,
) -> Option<Match> {
    if cf.is_empty() {
        return None;
    }
    let total = table.len();
    let mut hi = from.min(total);
    loop {
        let lo = hi.saturating_sub(WIN).max(limit_start);
        let (base, hay) = read_snap(table, lo.saturating_sub(CTX), (hi + CTX).min(total));
        let mut best: Option<Match> = None;
        for m in cf.re.find_iter(&hay) {
            let s = base + m.start() as u64;
            let e = base + m.end() as u64;
            if s < limit_start {
                continue;
            }
            if e <= from && s >= lo.max(limit_start) && best.is_none_or(|b| s >= b.start) {
                best = Some(Match { start: s, end: e });
            }
        }
        if let Some(b) = best {
            return Some(b);
        }
        if lo <= limit_start || lo == 0 {
            return None;
        }
        hi = (lo + OVERLAP).min(total); // overlap with the previous window
        if hi <= lo {
            return None;
        }
    }
}

/// Find all matches in `range`, up to `limit`. `cancel` is polled per window.
pub fn find_all(
    table: &PieceTable,
    cf: &CompiledFind,
    range: Range<u64>,
    limit: usize,
    cancel: &std::sync::atomic::AtomicBool,
    mut progress: impl FnMut(u64),
) -> SearchResults {
    let mut results = SearchResults {
        matches: Vec::new(),
        truncated: false,
        cancelled: false,
        scanned_to: range.start,
    };
    if cf.is_empty() {
        return results;
    }
    let total = table.len();
    let range = range.start.min(total)..range.end.min(total);
    let mut lo = range.start;
    loop {
        if cancel.load(std::sync::atomic::Ordering::Relaxed) {
            results.cancelled = true;
            break;
        }
        let hi = (lo + WIN).min(range.end.max(range.start));
        let at_eof = hi >= total || hi >= range.end;
        let (base, hay) = read_snap(table, lo.saturating_sub(CTX), (hi + CTX).min(total));
        let upto = if at_eof {
            range.end
        } else {
            (lo + WIN - OVERLAP).min(range.end)
        };
        for m in cf.re.find_iter(&hay) {
            let s = base + m.start() as u64;
            let e = base + m.end() as u64;
            // s < lo: a match in the context region ([base, lo)) belongs to the
            // previous window's accept region (upto_{N-1} == lo), and collecting it
            // here again would double-count it. The first window has lo == range.start,
            // which `s < range.start` catches.
            if s < range.start || s < lo || s >= upto || e > range.end {
                continue;
            }
            if results.matches.len() >= limit {
                results.truncated = true;
                results.scanned_to = s;
                return results;
            }
            results.matches.push(Match { start: s, end: e });
        }
        results.scanned_to = hi;
        progress(hi);
        if at_eof || hi >= range.end {
            break;
        }
        let next = (hi - OVERLAP).max(lo + 1);
        if next <= lo {
            break;
        }
        lo = next;
    }
    results
}

/// Expand a replacement template for one matched slice.
///
/// In regex mode `$1` / `$0` / `${name}` are expanded from the captures taken
/// against `matched`. In `Esc` mode backslash escapes (`\n`, `\t`, …) are
/// interpreted first (the same rule the replacement box follows).
/// `Plain` mode inserts the replacement **verbatim** — a literal `\n` stays a
/// backslash + n; use `Esc` mode when escapes are wanted.
pub fn expand_replacement(cf: &CompiledFind, matched: &str, replacement: &str) -> String {
    match cf.opts.mode {
        SearchMode::Regex => {
            let mut out = String::new();
            if let Some(caps) = cf.re.captures(matched) {
                caps.expand(replacement, &mut out);
            } else {
                out.push_str(replacement);
            }
            out
        }
        SearchMode::Esc => interpret_escapes_text(replacement),
        SearchMode::Plain => replacement.to_string(),
    }
}

/// Resolve `\n` / `\r` / `\t` / `\0` / `\xHH` escapes into real characters.
pub fn interpret_escapes_text(s: &str) -> String {
    interpret_escapes_core(s)
}

/// Byte budget for one replace-all batch.
///
/// `FIND_LIMIT` bounds the **number** of matches, but a regex can match huge
/// slices, so 50 000 matches say nothing about memory. The `deleted` copies
/// alone are a second copy of every matched slice; this cap keeps the batch
/// (and the undo entry it becomes) bounded. The scan stops early and reports
/// `truncated`, leaving the untouched tail in the document.
pub const REPLACE_MAX_BYTES: usize = 64 * 1024 * 1024;

/// Byte budget for one **streaming** replace batch. Batches are scanned and
/// applied one at a time, so peak memory is bounded by this instead of the
/// whole document. The operation continues automatically until every match
/// has been replaced.
pub const REPLACE_BATCH_BYTES: usize = 8 * 1024 * 1024;
/// Match-count ceiling per batch (a regex can produce millions of tiny hits;
/// the byte budget alone would let the op vector grow by count).
pub const REPLACE_BATCH_MATCHES: usize = 4096;

/// One resumable slice of a replace-all operation.
#[derive(Debug)]
pub struct ReplaceBatch {
    /// Edits already mapped into the **current** document coordinates (the
    /// caller applies them immediately, then asks for the next batch).
    pub ops: Vec<crate::buffer::doc::EditOp>,
    /// Matches represented by `ops` (including no-op replacements).
    pub matches: usize,
    /// Snapshot position to resume scanning from.
    pub next_from: u64,
    /// Cumulative byte delta produced by this and all previous batches.
    pub delta_after: i64,
    pub done: bool,
    pub cancelled: bool,
}

/// Build one bounded replace batch from a snapshot.
///
/// `delta` is the cumulative byte shift caused by earlier batches. Because
/// batches are produced front-to-back, every match in this range is after all
/// earlier edits, so shifting by `delta` maps it to the live document.
// The arguments are the streaming protocol's complete state (range, offsets,
// two budgets, cancel, progress); splitting the struct would only spread one
// calling convention across two places, and there are only 3 call sites.
#[allow(clippy::too_many_arguments)]
pub fn build_replace_batch(
    table: &PieceTable,
    cf: &CompiledFind,
    from: u64,
    to: u64,
    replacement: &str,
    delta: i64,
    match_limit: usize,
    byte_budget: usize,
    cancel: &std::sync::atomic::AtomicBool,
    progress: impl FnMut(u64),
) -> ReplaceBatch {
    let res = find_all(table, cf, from..to, match_limit, cancel, progress);
    if res.cancelled {
        return ReplaceBatch {
            ops: Vec::new(),
            matches: 0,
            next_from: from,
            delta_after: delta,
            done: true,
            cancelled: true,
        };
    }
    let mut ops = Vec::new();
    let mut bytes = 0usize;
    let mut delta_after = delta;
    for m in res.matches.iter() {
        let deleted = table.read_str(m.start..m.end);
        let inserted = expand_replacement(cf, &deleted, replacement);
        let cost = deleted.len().saturating_add(inserted.len());
        // Always keep the first match, even if it alone exceeds the budget:
        // otherwise a single enormous regex hit could stall the stream.
        if !ops.is_empty() && bytes.saturating_add(cost) > byte_budget {
            break;
        }
        // Every match in this batch is in snapshot coordinates; only the shift
        // from *earlier batches* applies here. The in-batch delta is folded
        // into `delta_after` for the next call, never into this batch's ops.
        let Some(pos) = i64::try_from(m.start)
            .ok()
            .and_then(|p| p.checked_add(delta))
            .and_then(|p| u64::try_from(p).ok())
        else {
            break;
        };
        bytes = bytes.saturating_add(cost);
        delta_after += inserted.len() as i64 - deleted.len() as i64;
        ops.push(crate::buffer::doc::EditOp {
            pos,
            deleted,
            inserted,
        });
    }
    let chosen = ops.len();
    ops.reverse();
    let next_from = if chosen < res.matches.len() {
        res.matches[chosen].start
    } else if res.truncated {
        res.scanned_to
    } else {
        to
    };
    let done = chosen == res.matches.len() && !res.truncated || next_from >= to;
    ReplaceBatch {
        ops,
        matches: chosen,
        next_from: next_from.max(from),
        delta_after,
        done,
        cancelled: false,
    }
}

/// Build replace operations for all matches in `range` (applied last-to-first
/// so earlier positions stay valid).
pub fn build_replace_ops(
    table: &PieceTable,
    cf: &CompiledFind,
    range: Range<u64>,
    replacement: &str,
    limit: usize,
    cancel: &std::sync::atomic::AtomicBool,
    progress: impl FnMut(u64),
) -> (Vec<crate::buffer::doc::EditOp>, bool, bool) {
    let res = find_all(table, cf, range, limit, cancel, progress);
    let mut ops = Vec::with_capacity(res.matches.len());
    let mut bytes = 0usize;
    let mut over_budget = false;
    for m in res.matches.iter().rev() {
        let deleted = table.read_str(m.start..m.end);
        let inserted = expand_replacement(cf, &deleted, replacement);
        bytes = bytes
            .saturating_add(deleted.len())
            .saturating_add(inserted.len());
        // Always keep the first op (a single enormous replacement still has to
        // happen); stop once the batch would exceed the budget.
        if !ops.is_empty() && bytes > REPLACE_MAX_BYTES {
            over_budget = true;
            break;
        }
        ops.push(crate::buffer::doc::EditOp {
            pos: m.start,
            deleted,
            inserted,
        });
    }
    (ops, res.truncated || over_budget, res.cancelled)
}

/// Build the single replace operation for one match.
pub fn build_replace_op(
    table: &PieceTable,
    cf: &CompiledFind,
    m: Match,
    replacement: &str,
) -> crate::buffer::doc::EditOp {
    let deleted = table.read_str(m.start..m.end);
    let inserted = expand_replacement(cf, &deleted, replacement);
    crate::buffer::doc::EditOp {
        pos: m.start,
        deleted,
        inserted,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buffer::TextDocument;

    fn table_of(s: &str) -> PieceTable {
        PieceTable::from_bytes(s.as_bytes().to_vec())
    }

    fn all(text: &str, opts: FindOptions) -> Vec<Match> {
        all_in(text, opts, None)
    }

    fn all_in(text: &str, opts: FindOptions, range: Option<Range<u64>>) -> Vec<Match> {
        let t = table_of(text);
        let cf = CompiledFind::new(opts).unwrap();
        let range = range.unwrap_or(0..t.len());
        let cancel = std::sync::atomic::AtomicBool::new(false);
        find_all(&t, &cf, range, 100000, &cancel, |_| {}).matches
    }

    #[test]
    fn plain_search() {
        let m = all(
            "hello world, hello",
            FindOptions {
                pattern: "hello".into(),
                ..Default::default()
            },
        );
        assert_eq!(m.len(), 2);
        assert_eq!((m[0].start, m[0].end), (0, 5));
        assert_eq!((m[1].start, m[1].end), (13, 18));
    }

    #[test]
    fn case_insensitive_default_off() {
        let m = all(
            "Hello hello HELLO",
            FindOptions {
                pattern: "hello".into(),
                ..Default::default()
            },
        );
        assert_eq!(m.len(), 3);
        let m = all(
            "Hello hello HELLO",
            FindOptions {
                pattern: "hello".into(),
                case_sensitive: true,
                ..Default::default()
            },
        );
        assert_eq!(m.len(), 1);
    }

    #[test]
    fn whole_word() {
        let m = all(
            "cat category cat",
            FindOptions {
                pattern: "cat".into(),
                whole_word: true,
                ..Default::default()
            },
        );
        assert_eq!(m.len(), 2);
    }

    #[test]
    fn regex_captures_and_classes() {
        let m = all(
            "a1 b22 c333",
            FindOptions {
                pattern: r"\d+".into(),
                mode: SearchMode::Regex,
                ..Default::default()
            },
        );
        assert_eq!(m.len(), 3);
        assert_eq!((m[2].start, m[2].end), (8, 11));
    }

    #[test]
    fn regex_newline_semantics() {
        // \n must match CRLF, LF and CR
        for text in ["a\r\nb", "a\nb", "a\rb"] {
            let m = all(
                text,
                FindOptions {
                    pattern: r"a\nb".into(),
                    mode: SearchMode::Regex,
                    ..Default::default()
                },
            );
            assert_eq!(m.len(), 1, "text {text:?}");
            assert_eq!(
                (m[0].start, m[0].end),
                (0, text.len() as u64),
                "text {text:?}"
            );
        }
    }

    #[test]
    fn regex_escaped_backslash_n_not_rewritten() {
        // pattern r"\\n" means literal backslash followed by n
        let m = all(
            r"cat \n dog",
            FindOptions {
                pattern: r"\\n".into(),
                mode: SearchMode::Regex,
                ..Default::default()
            },
        );
        assert_eq!(m.len(), 1);
        assert_eq!((m[0].start, m[0].end), (4, 6));
    }

    #[test]
    fn multiline_anchors() {
        let text = "one\ntwo\nthree";
        let m = all(
            text,
            FindOptions {
                pattern: r"^\w+$".into(),
                mode: SearchMode::Regex,
                ..Default::default()
            },
        );
        assert_eq!(m.len(), 3, "{:?}", m);
    }

    #[test]
    fn escaped_mode() {
        let m = all(
            "a\tb\nc",
            FindOptions {
                pattern: r"a\tb".into(),
                mode: SearchMode::Esc,
                ..Default::default()
            },
        );
        assert_eq!(m.len(), 1);
        assert_eq!((m[0].start, m[0].end), (0, 3));
    }

    #[test]
    fn find_next_and_prev() {
        let t = table_of("foo bar foo baz foo");
        let cf = CompiledFind::new(FindOptions {
            pattern: "foo".into(),
            ..Default::default()
        })
        .unwrap();
        let m = find_next(&t, &cf, 1, t.len()).unwrap();
        assert_eq!(m.start, 8);
        let m = find_next(&t, &cf, 9, t.len()).unwrap();
        assert_eq!(m.start, 16);
        assert!(find_next(&t, &cf, 17, t.len()).is_none());
        let m = find_prev(&t, &cf, 16, 0).unwrap();
        assert_eq!(m.start, 8);
        let m = find_prev(&t, &cf, 9, 0).unwrap();
        assert_eq!(m.start, 0);
        assert!(find_prev(&t, &cf, 1, 0).is_none());
    }

    #[test]
    fn replace_ops_reverse_order() {
        let t = table_of("one two one two one");
        let cf = CompiledFind::new(FindOptions {
            pattern: "one".into(),
            ..Default::default()
        })
        .unwrap();
        let cancel = std::sync::atomic::AtomicBool::new(false);
        let (ops, trunc, canc) = build_replace_ops(&t, &cf, 0..t.len(), "1", 1000, &cancel, |_| {});
        assert!(!trunc && !canc);
        assert_eq!(ops.len(), 3);
        // reverse order: last match first
        assert_eq!(ops[0].pos, 16);
        assert_eq!(ops[2].pos, 0);
        // apply to a real document
        let mut d = TextDocument::from("one two one two one");
        d.apply_transaction(ops);
        assert_eq!(d.read_all(), "1 two 1 two 1");
        d.undo().unwrap();
        assert_eq!(d.read_all(), "one two one two one");
    }

    #[test]
    fn regex_replace_expansion() {
        let t = table_of("john smith");
        let cf = CompiledFind::new(FindOptions {
            pattern: r"(\w+) (\w+)".into(),
            mode: SearchMode::Regex,
            ..Default::default()
        })
        .unwrap();
        let cancel = std::sync::atomic::AtomicBool::new(false);
        let (ops, _, _) = build_replace_ops(&t, &cf, 0..t.len(), "$2 $1", 100, &cancel, |_| {});
        let mut d = TextDocument::from("john smith");
        d.apply_transaction(ops);
        assert_eq!(d.read_all(), "smith john");
    }

    #[test]
    fn search_limit_truncates() {
        let text = "x".repeat(5000);
        let t = table_of(&text);
        let cf = CompiledFind::new(FindOptions {
            pattern: "x".into(),
            ..Default::default()
        })
        .unwrap();
        let cancel = std::sync::atomic::AtomicBool::new(false);
        let res = find_all(&t, &cf, 0..t.len(), 100, &cancel, |_| {});
        assert_eq!(res.matches.len(), 100);
        assert!(res.truncated);
    }

    #[test]
    fn replace_batches_stream_to_completion() {
        let text = "x".repeat(1000);
        let t = table_of(&text);
        let cf = CompiledFind::new(FindOptions {
            pattern: "x".into(),
            ..Default::default()
        })
        .unwrap();
        let cancel = std::sync::atomic::AtomicBool::new(false);
        let mut d = TextDocument::from(text);
        let mut from = 0u64;
        let mut delta = 0i64;
        let mut batches = 0;
        let mut replaced = 0usize;
        loop {
            let b = build_replace_batch(
                &t,
                &cf,
                from,
                t.len(),
                "yy",
                delta,
                50,
                1024,
                &cancel,
                |_| {},
            );
            replaced += b.matches;
            batches += 1;
            assert!(!b.cancelled);
            if !b.ops.is_empty() {
                d.apply_transaction(b.ops);
            }
            delta = b.delta_after;
            if b.done {
                break;
            }
            assert!(b.next_from > from, "batch must make progress");
            from = b.next_from;
            assert!(batches < 100, "stream did not terminate");
        }
        assert_eq!(replaced, 1000, "every match must be visited exactly once");
        assert_eq!(d.read_all(), "y".repeat(2000));
    }

    #[test]
    fn replace_batches_shift_remaining_positions() {
        // Each match shrinks by one byte, so later snapshot positions need the
        // cumulative delta before they can touch the live document.
        let text = "aa aa aa aa aa";
        let t = table_of(text);
        let cf = CompiledFind::new(FindOptions {
            pattern: "aa".into(),
            ..Default::default()
        })
        .unwrap();
        let cancel = std::sync::atomic::AtomicBool::new(false);
        let mut d = TextDocument::from(text);
        let mut from = 0u64;
        let mut delta = 0i64;
        loop {
            let b =
                build_replace_batch(&t, &cf, from, t.len(), "a", delta, 2, 1024, &cancel, |_| {});
            if !b.ops.is_empty() {
                d.apply_transaction(b.ops);
            }
            delta = b.delta_after;
            if b.done {
                break;
            }
            from = b.next_from;
        }
        assert_eq!(d.read_all(), "a a a a a");
    }

    #[test]
    fn search_across_piece_boundaries() {
        let mut t = table_of("hello ");
        t.insert(6, b"beautiful ");
        t.insert(16, b"world");
        assert_eq!(t.len(), 21);
        let cf = CompiledFind::new(FindOptions {
            pattern: "hello beautiful world".into(),
            ..Default::default()
        })
        .unwrap();
        let cancel = std::sync::atomic::AtomicBool::new(false);
        let res = find_all(&t, &cf, 0..t.len(), 10, &cancel, |_| {});
        assert_eq!(res.matches.len(), 1);
        assert_eq!((res.matches[0].start, res.matches[0].end), (0, 21));
    }
}
