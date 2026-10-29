//! Row-level state-machine lexer.
//!
//! One call tokenizes a single line given the lexer state left over from the
//! previous line, so highlighting is O(visible lines) regardless of file size.
//! Multi-line constructs (block comments, triple-quoted strings, raw strings,
//! markdown fences) are carried across by [`LexState`].

use super::lang::{CsvSpec, EscapeKind, LangSpec, TokenKind};

/// Lexer continuation state between lines.
///
/// Encoded as three bytes so it can be cached cheaply in checkpoints.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct LexState {
    /// one of the `M_*` constants
    pub mode: u8,
    /// quote delimiter / block depth / hash count / fence char.
    ///
    /// `u16` instead of `u8`: block comment depth lives here — 256+ nested
    /// levels would saturate in u8, letting a stray `*/` close the comment
    /// early. The other uses (quote byte, hash count, fence char) fit in u16.
    pub a: u16,
    /// fence length / spare
    pub b: u8,
}

pub const M_NORMAL: u8 = 0;
pub const M_BLOCK: u8 = 1;
pub const M_STR: u8 = 2;
pub const M_RAW: u8 = 3;
pub const M_TRIPLE: u8 = 4;
pub const M_FENCE: u8 = 5;
pub const M_XMLC: u8 = 6;

impl LexState {
    pub const fn normal() -> Self {
        LexState {
            mode: M_NORMAL,
            a: 0,
            b: 0,
        }
    }
    pub const fn block(depth: u16) -> Self {
        let d = if depth < 1 { 1 } else { depth };
        LexState {
            mode: M_BLOCK,
            a: d,
            b: 0,
        }
    }
    pub const fn string(delim: u8) -> Self {
        LexState {
            mode: M_STR,
            a: delim as u16,
            b: 0,
        }
    }
    pub const fn raw(hashes: u8) -> Self {
        LexState {
            mode: M_RAW,
            a: hashes as u16,
            b: 0,
        }
    }
    pub const fn triple(delim: u8) -> Self {
        LexState {
            mode: M_TRIPLE,
            a: delim as u16,
            b: 0,
        }
    }
    pub const fn fence(ch: u8, len: u8) -> Self {
        LexState {
            mode: M_FENCE,
            a: ch as u16,
            b: len,
        }
    }
    pub const fn xml_comment() -> Self {
        LexState {
            mode: M_XMLC,
            a: 0,
            b: 0,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Token {
    pub start: usize,
    pub end: usize,
    pub kind: TokenKind,
}

// ---- byte helpers ----------------------------------------------------------

/// Length in bytes of the UTF-8 character starting at `i` (1..=4).
///
/// Always goes through [`crate::textops::utf8_seq_len`].
#[inline]
fn clen(b: &[u8], i: usize) -> usize {
    crate::textops::utf8_seq_len(b[i])
}

#[inline]
fn starts_with(b: &[u8], i: usize, pat: &str) -> bool {
    let p = pat.as_bytes();
    i + p.len() <= b.len() && &b[i..i + p.len()] == p
}

#[inline]
fn starts_with_ci(b: &[u8], i: usize, pat: &str) -> bool {
    let p = pat.as_bytes();
    if i + p.len() > b.len() {
        return false;
    }
    p.iter()
        .zip(&b[i..i + p.len()])
        .all(|(a, c)| a.eq_ignore_ascii_case(c))
}

#[inline]
fn is_space(c: u8) -> bool {
    c == b' ' || c == b'\t' || c == 0x0b || c == 0x0c
}

#[inline]
fn dec(b: &[u8], i: usize) -> char {
    // caller guarantees `i` is a char boundary
    let l = clen(b, i);
    std::str::from_utf8(&b[i..i + l])
        .ok()
        .and_then(|s| s.chars().next())
        .unwrap_or('\u{fffd}')
}

/// Advance over a run of `close` repetitions, honouring escape rules.
/// Returns the byte index just past the closing quote, or `None`.
fn find_close(b: &[u8], from: usize, quote: u16, esc: EscapeKind) -> Option<usize> {
    let n = b.len();
    let mut i = from;
    while i < n {
        let c = b[i];
        match esc {
            EscapeKind::Backslash | EscapeKind::Both if c == b'\\' => {
                i += clen(b, i);
                if i < n {
                    i += clen(b, i);
                }
                continue;
            }
            EscapeKind::Doubled | EscapeKind::Both if u16::from(c) == quote => {
                if i + 1 < n && u16::from(b[i + 1]) == quote {
                    i += 2;
                    continue;
                }
                return Some(i);
            }
            _ => {}
        }
        if u16::from(c) == quote {
            return Some(i);
        }
        i += clen(b, i);
    }
    None
}

fn scan_number(b: &[u8], i: usize) -> usize {
    let n = b.len();
    let mut i = i;
    let start = i;
    let mut hex = false;
    if i + 1 < n && b[i] == b'0' && (b[i + 1] | 0x20) == b'x' {
        hex = true;
        i += 2;
    }
    while i < n {
        let c = b[i];
        if c.is_ascii_digit() || c == b'_' || c == b'.' {
            i += 1;
            continue;
        }
        if hex {
            if c.is_ascii_hexdigit() {
                i += 1;
                continue;
            }
            break;
        }
        let l = c | 0x20;
        if (l == b'e' || l == b'p') && i + 1 < n && (b[i + 1] == b'+' || b[i + 1] == b'-') {
            i += 2;
            continue;
        }
        if l == b'e' && i + 1 < n && b[i + 1].is_ascii_digit() {
            // unsigned exponent: 1e5
            i += 1;
            continue;
        }
        if c.is_ascii_alphabetic() {
            // type/radix suffixes only (u8, i32, f32, 0b1, 0o7, L/UL/LL);
            // anything else ends the number so "3rd place" and "1abc" do
            // not get swallowed whole
            if matches!(l, b'u' | b'i' | b'f' | b'l' | b'b' | b'o')
                && (i == start || b[i - 1].is_ascii_digit())
            {
                i += 1;
                continue;
            }
            break;
        }
        break;
    }
    i
}

// ---- emitter ---------------------------------------------------------------

struct Emit<'a> {
    out: &'a mut Vec<Token>,
    plain: usize,
}

impl<'a> Emit<'a> {
    fn tok(&mut self, s: usize, e: usize, k: TokenKind) {
        if e <= s {
            return;
        }
        if s > self.plain {
            self.out.push(Token {
                start: self.plain,
                end: s,
                kind: TokenKind::Plain,
            });
        }
        self.out.push(Token {
            start: s,
            end: e,
            kind: k,
        });
        self.plain = e;
    }
    fn finish(&mut self, n: usize) {
        if n > self.plain {
            self.out.push(Token {
                start: self.plain,
                end: n,
                kind: TokenKind::Plain,
            });
        }
    }
}

// ---- markdown --------------------------------------------------------------

fn lex_markdown(b: &[u8], st: LexState, out: &mut Vec<Token>) -> LexState {
    let n = b.len();
    let mut e = Emit { out, plain: 0 };

    // inside a fenced code block
    if st.mode == M_FENCE {
        let fence = st.a;
        let need = st.b.max(3) as usize;
        let mut j = 0;
        while j < n && is_space(b[j]) {
            j += 1;
        }
        let mut k = j;
        while k < n && u16::from(b[k]) == fence {
            k += 1;
        }
        let rest_is_ws = (k..n).all(|x| is_space(b[x]));
        if k - j >= need && rest_is_ws {
            e.tok(0, n, TokenKind::Code);
            e.finish(n);
            return LexState::normal();
        }
        e.tok(0, n, TokenKind::Code);
        e.finish(n);
        return st;
    }

    // fenced block opener
    let mut j = 0;
    while j < n && is_space(b[j]) {
        j += 1;
    }
    if j < n && (b[j] == b'`' || b[j] == b'~') {
        let mut k = j;
        while k < n && b[k] == b[j] {
            k += 1;
        }
        if k - j >= 3 {
            e.tok(0, n, TokenKind::Code);
            e.finish(n);
            return LexState::fence(b[j], (k - j).min(255) as u8);
        }
    }

    let line = std::str::from_utf8(b).unwrap_or("");

    // ATX heading
    if j < n && b[j] == b'#' {
        let mut k = j;
        while k < n && b[k] == b'#' {
            k += 1;
        }
        if k - j <= 6 && (k >= n || is_space(b[k])) {
            e.tok(0, n, TokenKind::Heading);
            e.finish(n);
            return LexState::normal();
        }
    }

    // thematic break
    let trimmed = line.trim();
    if trimmed.len() >= 3
        && (trimmed.chars().all(|c| c == '-')
            || trimmed.chars().all(|c| c == '*')
            || trimmed.chars().all(|c| c == '_')
            || trimmed.chars().all(|c| c == '='))
    {
        e.tok(0, n, TokenKind::Rule);
        e.finish(n);
        return LexState::normal();
    }

    // block quote
    if j < n && b[j] == b'>' {
        e.tok(0, n, TokenKind::Quote);
        e.finish(n);
        return LexState::normal();
    }

    // markdown table row -> alternating column colors
    if j < n && b[j] == b'|' {
        let mut i = 0usize;
        let mut col = 0u8;
        while i < n {
            let c = b[i];
            if c == b'|' {
                e.tok(i, i + 1, TokenKind::Punct);
                col = col.wrapping_add(1);
                i += 1;
                continue;
            }
            let s = i;
            while i < n && b[i] != b'|' {
                if b[i] == b'\\' && i + 1 < n && b[i + 1] == b'|' {
                    i += 2;
                    continue;
                }
                i += clen(b, i);
            }
            e.tok(s, i, TokenKind::Col(col));
        }
        e.finish(n);
        return LexState::normal();
    }

    // list marker, then inline scan of the remainder
    if j < n && (b[j] == b'-' || b[j] == b'*' || b[j] == b'+') && j + 1 < n && is_space(b[j + 1]) {
        e.tok(j, j + 1, TokenKind::ListMark);
        inline_md(b, j + 1, &mut e);
        e.finish(n);
        return LexState::normal();
    }
    if j < n && b[j].is_ascii_digit() {
        let mut k = j;
        while k < n && b[k].is_ascii_digit() {
            k += 1;
        }
        if k < n && (b[k] == b'.' || b[k] == b')') && k + 1 < n && is_space(b[k + 1]) {
            e.tok(j, k + 1, TokenKind::ListMark);
            inline_md(b, k + 1, &mut e);
            e.finish(n);
            return LexState::normal();
        }
    }

    inline_md(b, 0, &mut e);
    e.finish(n);
    LexState::normal()
}

/// Inline markdown spans: `code`, **bold**, *italic*, [text](url).
fn inline_md(b: &[u8], from: usize, e: &mut Emit) {
    let n = b.len();
    let mut i = from;
    while i < n {
        let c = b[i];
        if c == b'`' {
            let mut k = i;
            while k < n && b[k] == b'`' {
                k += 1;
            }
            let run = k - i;
            let pat: Vec<u8> = vec![b'`'; run];
            if let Some(s) = (k..n).find(|&x| x + run <= n && &b[x..x + run] == pat.as_slice()) {
                e.tok(i, s + run, TokenKind::Code);
                i = s + run;
                continue;
            }
            i = k;
            continue;
        }
        if c == b'*' || c == b'_' || c == b'~' {
            let mut k = i;
            while k < n && b[k] == c {
                k += 1;
            }
            let run = k - i; // 1 = italic, 2 = bold, 3 = bold+italic
            if run <= 3 {
                let pat = vec![c; run];
                if let Some(s) = (k..n).find(|&x| x + run <= n && &b[x..x + run] == pat.as_slice())
                {
                    let kind = if run == 1 {
                        TokenKind::Italic
                    } else {
                        TokenKind::Bold
                    };
                    e.tok(i, s + run, kind);
                    i = s + run;
                    continue;
                }
            }
            i = k;
            continue;
        }
        if c == b'[' {
            if let Some(close) = (i + 1..n).find(|&x| b[x] == b']') {
                if close + 1 < n && b[close + 1] == b'(' {
                    if let Some(end) = (close + 2..n).find(|&x| b[x] == b')') {
                        e.tok(i, end + 1, TokenKind::Link);
                        i = end + 1;
                        continue;
                    }
                }
            }
        }
        i += clen(b, i);
    }
}

// ---- diff ------------------------------------------------------------------

fn lex_diff(b: &[u8], out: &mut Vec<Token>) -> LexState {
    let n = b.len();
    let mut e = Emit { out, plain: 0 };
    let kind = if starts_with(b, 0, "+++")
        || starts_with(b, 0, "---")
        || starts_with(b, 0, "@@")
        || starts_with(b, 0, "diff ")
        || starts_with(b, 0, "index ")
        || starts_with(b, 0, "new file")
        || starts_with(b, 0, "deleted file")
        || starts_with(b, 0, "Binary ")
    {
        TokenKind::Meta
    } else if n > 0 && b[0] == b'+' {
        TokenKind::Added
    } else if n > 0 && b[0] == b'-' {
        TokenKind::Removed
    } else {
        TokenKind::Plain
    };
    if kind != TokenKind::Plain {
        e.tok(0, n, kind);
    }
    e.finish(n);
    LexState::normal()
}

// ---- csv -------------------------------------------------------------------

fn lex_csv(b: &[u8], spec: CsvSpec, out: &mut Vec<Token>) -> LexState {
    let n = b.len();
    let mut e = Emit { out, plain: 0 };
    let mut i = 0usize;
    let mut col = 0u8;
    while i < n {
        let s = i;
        if b[i] == spec.quote {
            i += 1;
            while i < n {
                if b[i] == spec.quote {
                    if i + 1 < n && b[i + 1] == spec.quote {
                        i += 2;
                        continue;
                    }
                    i += 1;
                    break;
                }
                i += clen(b, i);
            }
        } else {
            while i < n && b[i] != spec.delim {
                i += clen(b, i);
            }
        }
        e.tok(s, i, TokenKind::Col(col));
        col = col.wrapping_add(1);
        if i < n && b[i] == spec.delim {
            e.tok(i, i + 1, TokenKind::Punct);
            i += 1;
        }
    }
    e.finish(n);
    LexState::normal()
}

// ---- generic code lexer ----------------------------------------------------

/// Outcome of a step that consumed a construct.
enum Handled {
    /// Construct closed at `next`; the scan loop continues from there.
    At(usize),
    /// Line consumed to EOL; the returned state is final.
    End(LexState),
}

/// Outcome of trying to continue a multi-line construct at line start.
enum Resumed {
    /// Construct closed at `end`; normal scanning continues from there.
    At(usize),
    /// Construct still open at EOL; the returned state is final.
    Open(LexState),
}

/// Continue a multi-line construct opened on an earlier line (`st.mode`).
///
/// Each arm mirrors its opener's scan: consume up to the closer (emitting the
/// consumed span and resuming normal scanning after it) or to EOL (emitting
/// the whole line and returning the still-open state).
fn resume_multiline(spec: &LangSpec, b: &[u8], st: LexState, e: &mut Emit) -> Resumed {
    let n = b.len();
    match st.mode {
        M_BLOCK => {
            let mut depth = st.a.max(1) as usize;
            let mut j = 0usize;
            while j < n {
                if spec.nested_block && j + 2 <= n && &b[j..j + 2] == b"/*" {
                    depth += 1;
                    j += 2;
                    continue;
                }
                if j + 2 <= n && &b[j..j + 2] == b"*/" {
                    depth -= 1;
                    j += 2;
                    if depth == 0 {
                        break;
                    }
                    continue;
                }
                j += clen(b, j);
            }
            if depth == 0 {
                e.tok(0, j, TokenKind::Comment);
                e.plain = j;
                Resumed::At(j)
            } else {
                e.tok(0, n, TokenKind::Comment);
                e.finish(n);
                Resumed::Open(LexState::block(depth.min(u16::MAX as usize) as u16))
            }
        }
        M_XMLC => match (0..n).find(|&x| x + 3 <= n && &b[x..x + 3] == b"-->") {
            Some(x) => {
                e.tok(0, x + 3, TokenKind::Comment);
                e.plain = x + 3;
                Resumed::At(x + 3)
            }
            None => {
                e.tok(0, n, TokenKind::Comment);
                e.finish(n);
                Resumed::Open(LexState::xml_comment())
            }
        },
        M_STR => match find_close(b, 0, st.a, spec.escape_for(st.a)) {
            Some(x) => {
                let end = x + 1;
                e.tok(0, end, TokenKind::String);
                e.plain = end;
                Resumed::At(end)
            }
            None => {
                e.tok(0, n, TokenKind::String);
                e.finish(n);
                Resumed::Open(st)
            }
        },
        M_TRIPLE => {
            let q = st.a;
            let mut j = 0usize;
            let mut closed = false;
            while j + 3 <= n {
                let c = b[j];
                if c == b'\\' && spec.escape != EscapeKind::None {
                    j += 2.min(n - j);
                    continue;
                }
                if u16::from(c) == q
                    && j + 3 <= n
                    && u16::from(b[j + 1]) == q
                    && u16::from(b[j + 2]) == q
                {
                    j += 3;
                    closed = true;
                    break;
                }
                j += clen(b, j);
            }
            if closed {
                e.tok(0, j, TokenKind::String);
                e.plain = j;
                Resumed::At(j)
            } else {
                e.tok(0, n, TokenKind::String);
                e.finish(n);
                Resumed::Open(st)
            }
        }
        M_RAW => {
            // terminator: `"` followed by `st.a` hashes
            let hashes = st.a as usize;
            let mut j = 0usize;
            let mut closed = false;
            while j < n {
                if b[j] == b'"' && j + 1 + hashes <= n && (0..hashes).all(|k| b[j + 1 + k] == b'#')
                {
                    j += 1 + hashes;
                    closed = true;
                    break;
                }
                j += clen(b, j);
            }
            if closed {
                e.tok(0, j, TokenKind::String);
                e.plain = j;
                Resumed::At(j)
            } else {
                e.tok(0, n, TokenKind::String);
                e.finish(n);
                Resumed::Open(st)
            }
        }
        _ => Resumed::At(0),
    }
}

/// Consume one element inside an XML/HTML tag: an attribute name, an `=`/value,
/// or the closing bracket. Returns the new index and whether the tag is still
/// open (never terminates the line).
fn scan_in_tag(b: &[u8], i: usize, e: &mut Emit) -> (usize, bool) {
    let n = b.len();
    let c = b[i];
    if c == b'>' {
        e.tok(i, i + 1, TokenKind::Punct);
        return (i + 1, false);
    }
    if c == b'/' && i + 1 < n && b[i + 1] == b'>' {
        e.tok(i, i + 2, TokenKind::Punct);
        return (i + 2, false);
    }
    if c == b'=' {
        e.tok(i, i + 1, TokenKind::Operator);
        return (i + 1, true);
    }
    if c == b'"' || c == b'\'' {
        if let Some(x) = find_close(b, i + 1, u16::from(c), EscapeKind::None) {
            e.tok(i, x + 1, TokenKind::AttrValue);
            return (x + 1, true);
        }
        e.tok(i, n, TokenKind::AttrValue);
        return (n, true);
    }
    if b[i].is_ascii_alphanumeric() || b[i] == b'-' || b[i] == b':' || b[i] == b'_' || b[i] == b'.'
    {
        let s = i;
        let mut j = i;
        while j < n
            && (b[j].is_ascii_alphanumeric()
                || b[j] == b'-'
                || b[j] == b':'
                || b[j] == b'_'
                || b[j] == b'.')
        {
            j += 1;
        }
        e.tok(s, j, TokenKind::AttrName);
        return (j, true);
    }
    // anything else inside a tag (template expressions, `?`, `#`, ...)
    (i + clen(b, i), true)
}

/// Line comment (documentation variants first). `Batch `rem`` must not
/// swallow `remove` — the case-insensitive guard skips a marker that is
/// followed by an alphanumeric character. A comment always consumes the
/// rest of the line; `Some` is the final state.
fn try_line_comment(spec: &LangSpec, b: &[u8], i: usize, e: &mut Emit) -> Option<LexState> {
    let n = b.len();
    for d in spec.doc_line {
        if starts_with(b, i, d) {
            e.tok(i, n, TokenKind::DocComment);
            e.finish(n);
            return Some(LexState::normal());
        }
    }
    for lc in spec.line_comments {
        let hit = if spec.case_insensitive {
            starts_with_ci(b, i, lc)
        } else {
            starts_with(b, i, lc)
        };
        if hit {
            if spec.case_insensitive && i + lc.len() < n {
                let nx = b[i + lc.len()];
                if nx.is_ascii_alphanumeric() {
                    continue;
                }
            }
            e.tok(i, n, TokenKind::Comment);
            e.finish(n);
            return Some(LexState::normal());
        }
    }
    None
}

/// Block comment (`spec.block`), honoring nesting when the spec allows it.
/// An unclosed comment carries its depth into the next line's state.
fn try_block_comment(spec: &LangSpec, b: &[u8], i: usize, e: &mut Emit) -> Option<Handled> {
    let (open, close) = spec.block?;
    if !starts_with(b, i, open) {
        return None;
    }
    let n = b.len();
    let after = i + open.len();
    let mut j = after;
    let mut depth = 1usize;
    let mut closed = false;
    while j < n {
        if spec.nested_block && j + open.len() <= n && starts_with(b, j, open) {
            depth += 1;
            j += open.len();
            continue;
        }
        if j + close.len() <= n && starts_with(b, j, close) {
            depth -= 1;
            j += close.len();
            if depth == 0 {
                closed = true;
                break;
            }
            continue;
        }
        j += clen(b, j);
    }
    if closed {
        let is_doc = spec
            .doc_line
            .iter()
            .any(|d| d.starts_with(open) && d.len() > open.len());
        e.tok(
            i,
            j,
            if is_doc {
                TokenKind::DocComment
            } else {
                TokenKind::Comment
            },
        );
        Some(Handled::At(j))
    } else {
        e.tok(i, n, TokenKind::Comment);
        e.finish(n);
        Some(Handled::End(LexState::block(
            depth.min(u16::MAX as usize) as u16
        )))
    }
}

/// Triple-quoted string (`"""…"""` / `'''…'''`).
fn try_triple_string(spec: &LangSpec, b: &[u8], i: usize, e: &mut Emit) -> Option<Handled> {
    let n = b.len();
    if !(spec.triple_str
        && (i + 3 <= n)
        && ((b[i] == b'"' && b[i + 1] == b'"' && b[i + 2] == b'"')
            || (b[i] == b'\'' && b[i + 1] == b'\'' && b[i + 2] == b'\'')))
    {
        return None;
    }
    let q = b[i];
    let mut j = i + 3;
    let mut closed = false;
    while j + 3 <= n {
        if b[j] == b'\\' {
            j += 2.min(n - j);
            continue;
        }
        if b[j] == q && b[j + 1] == q && b[j + 2] == q {
            j += 3;
            closed = true;
            break;
        }
        j += clen(b, j);
    }
    if closed {
        e.tok(i, j, TokenKind::String);
        Some(Handled::At(j))
    } else {
        e.tok(i, n, TokenKind::String);
        e.finish(n);
        Some(Handled::End(LexState::triple(q)))
    }
}

/// Raw string `r"…"` / `r#"…"#`. A lone `r` (or hashes without a quote) is
/// not claimed — normal scanning classifies it as an identifier.
fn try_raw_string(spec: &LangSpec, b: &[u8], i: usize, e: &mut Emit) -> Option<Handled> {
    let n = b.len();
    if !(spec.raw_str && b[i] == b'r' && i + 1 < n && (b[i + 1] == b'"' || b[i + 1] == b'#')) {
        return None;
    }
    let mut h = 0usize;
    let mut j = i + 1;
    while j < n && b[j] == b'#' {
        h += 1;
        j += 1;
    }
    if !(j < n && b[j] == b'"') {
        return None;
    }
    j += 1;
    let mut k = j;
    let mut closed = false;
    while k < n {
        if b[k] == b'"' && k + 1 + h <= n && (0..h).all(|x| b[k + 1 + x] == b'#') {
            k += 1 + h;
            closed = true;
            break;
        }
        k += clen(b, k);
    }
    if closed {
        e.tok(i, k, TokenKind::String);
        Some(Handled::At(k))
    } else {
        e.tok(i, n, TokenKind::String);
        e.finish(n);
        Some(Handled::End(LexState::raw(h.min(255) as u8)))
    }
}

/// Quoted string with one of the spec's delimiters. Escape style depends on
/// the delimiter (inside Shell single quotes, `\` is a literal character).
fn try_string(spec: &LangSpec, b: &[u8], i: usize, e: &mut Emit) -> Option<Handled> {
    let c = b[i];
    if !spec.str_delims.iter().any(|d| d.as_bytes() == [c]) {
        return None;
    }
    let n = b.len();
    match find_close(b, i + 1, u16::from(c), spec.escape_for(u16::from(c))) {
        Some(x) => {
            e.tok(i, x + 1, TokenKind::String);
            Some(Handled::At(x + 1))
        }
        None => {
            e.tok(i, n, TokenKind::String);
            e.finish(n);
            Some(Handled::End(LexState::string(c)))
        }
    }
}

/// Character literal `'x'` / `'\n'` / `'\u{1F600}'`. A lone `'` (Rust
/// lifetime, `'static`) is left as punctuation: the inner text must be an
/// escape or exactly one character, and the closer must not be followed by
/// an identifier character (`'a'x` would be an identifier). A char literal
/// never ends the line — `Some` is the new index.
fn try_char_literal(
    spec: &LangSpec,
    line: &str,
    b: &[u8],
    i: usize,
    e: &mut Emit,
) -> Option<usize> {
    if !(spec.char_delim && b[i] == b'\'' && i + 1 < b.len()) {
        return None;
    }
    let x = find_close(b, i + 1, u16::from(b'\''), spec.escape)?;
    let inner = &line[i + 1..x];
    let shaped = inner.starts_with('\\') || inner.chars().count() == 1;
    let clean_end = x + 1 >= b.len() || {
        let ch = dec(b, x + 1);
        !(ch.is_alphanumeric() || ch == '_' || ch == '\'')
    };
    if !(shaped && clean_end) {
        return None;
    }
    e.tok(i, x + 1, TokenKind::Char);
    Some(x + 1)
}

/// `<!-- … -->` comment inside html-like code (called on the `<`).
/// `Handled` only — the caller has already matched the opener, so this
/// either closes on the line or carries `M_XMLC` into the next line.
fn try_xml_comment(b: &[u8], i: usize, e: &mut Emit) -> Handled {
    let n = b.len();
    e.tok(i, i + 4, TokenKind::Punct);
    let mut j = i + 4;
    let mut closed = false;
    while j + 3 <= n {
        if &b[j..j + 3] == b"-->" {
            j += 3;
            closed = true;
            break;
        }
        j += clen(b, j);
    }
    if closed {
        e.tok(i + 4, j, TokenKind::Comment);
        Handled::At(j)
    } else {
        e.tok(i + 4, n, TokenKind::Comment);
        e.finish(n);
        Handled::End(LexState::xml_comment())
    }
}

/// Tokenize one line (without its terminator).
pub fn lex_line(spec: &LangSpec, line: &str, st: LexState, out: &mut Vec<Token>) -> LexState {
    if spec.md {
        return lex_markdown(line.as_bytes(), st, out);
    }
    if spec.diff {
        return lex_diff(line.as_bytes(), out);
    }
    if let Some(csv) = spec.csv {
        return lex_csv(line.as_bytes(), csv, out);
    }

    let b = line.as_bytes();
    let n = b.len();
    let mut e = Emit { out, plain: 0 };
    let mut i = match resume_multiline(spec, b, st, &mut e) {
        Resumed::At(j) => j,
        Resumed::Open(s) => return s,
    };

    // ---- normal scanning ----
    let mut in_tag = false;
    while i < n {
        let c = b[i];

        // inside an XML/HTML tag: attributes, values, the closing bracket
        if in_tag {
            let (next, still_in_tag) = scan_in_tag(b, i, &mut e);
            i = next;
            in_tag = still_in_tag;
            continue;
        }

        // preprocessor directive (only at line start)
        if spec.preproc && c == b'#' && line[..i].trim().is_empty() {
            e.tok(i, n, TokenKind::Preproc);
            break;
        }

        // whitespace
        if is_space(c) {
            i += 1;
            continue;
        }

        // line comments (documentation variants first)
        if let Some(s) = try_line_comment(spec, b, i, &mut e) {
            return s;
        }

        // block comments / html comments
        match try_block_comment(spec, b, i, &mut e) {
            Some(Handled::At(j)) => {
                i = j;
                continue;
            }
            Some(Handled::End(s)) => return s,
            None => {}
        }

        // triple-quoted strings
        match try_triple_string(spec, b, i, &mut e) {
            Some(Handled::At(j)) => {
                i = j;
                continue;
            }
            Some(Handled::End(s)) => return s,
            None => {}
        }

        // raw strings: r"..." / r#"..."#
        match try_raw_string(spec, b, i, &mut e) {
            Some(Handled::At(j)) => {
                i = j;
                continue;
            }
            Some(Handled::End(s)) => return s,
            None => {}
        }

        // strings
        match try_string(spec, b, i, &mut e) {
            Some(Handled::At(j)) => {
                i = j;
                continue;
            }
            Some(Handled::End(s)) => return s,
            None => {}
        }

        // char literals: `'x'`, `'\n'`, `'\u{1F600}'`.
        // A lone `'` (Rust lifetime, `'static`) is left as punctuation.
        if let Some(j) = try_char_literal(spec, line, b, i, &mut e) {
            i = j;
            continue;
        }

        // numbers
        if spec.numbers
            && (c.is_ascii_digit() || (c == b'.' && i + 1 < n && b[i + 1].is_ascii_digit()))
        {
            let j = scan_number(b, i);
            e.tok(i, j, TokenKind::Number);
            i = j;
            continue;
        }

        // xml/html tags and attributes
        if spec.html_like && !in_tag && c == b'<' && i + 1 < n {
            let nx = b[i + 1];
            if nx.is_ascii_alphabetic() || nx == b'/' || nx == b'!' || nx == b'?' {
                if starts_with(b, i, "<!--") {
                    match try_xml_comment(b, i, &mut e) {
                        Handled::At(j) => {
                            i = j;
                            in_tag = false;
                            continue;
                        }
                        Handled::End(s) => return s,
                    }
                }
                e.tok(i, i + 1, TokenKind::Punct);
                i += 1;
                if b[i] == b'/' {
                    e.tok(i, i + 1, TokenKind::Punct);
                    i += 1;
                }
                let s = i;
                while i < n
                    && (b[i].is_ascii_alphanumeric()
                        || b[i] == b'-'
                        || b[i] == b':'
                        || b[i] == b'_')
                {
                    i += 1;
                }
                e.tok(s, i, TokenKind::Tag);
                in_tag = true;
                continue;
            }
        }
        // annotations
        if spec.annotation && c == b'@' && i + 1 < n && dec(b, i + 1).is_alphanumeric() {
            let s = i;
            i += 1;
            while i < n {
                let ch = dec(b, i);
                if ch.is_alphanumeric() || ch == '_' || ch == '.' {
                    i += clen(b, i);
                } else {
                    break;
                }
            }
            e.tok(s, i, TokenKind::Annotation);
            continue;
        }

        // identifiers & keywords
        let is_ident_start = c.is_ascii_alphabetic()
            || c == b'_'
            || (spec.dollar_ident && c == b'$')
            || (!c.is_ascii() && dec(b, i).is_alphabetic());
        if is_ident_start {
            let s = i;
            if spec.dollar_ident && c == b'$' {
                i += 1;
            }
            while i < n {
                let ch = dec(b, i);
                if ch.is_alphanumeric() || ch == '_' || spec.ident_extra.contains(ch) {
                    i += clen(b, i);
                } else {
                    break;
                }
            }
            let word = &line[s..i];
            let kind = spec.lookup(word).unwrap_or_else(|| {
                // `name (` -> function call
                let mut j = i;
                while j < n && (b[j] == b' ' || b[j] == b'\t') {
                    j += 1;
                }
                if j < n && b[j] == b'(' {
                    TokenKind::Function
                } else if spec.dollar_ident && word.starts_with('$') {
                    TokenKind::Variable
                } else {
                    TokenKind::Ident
                }
            });
            e.tok(s, i, kind);
            continue;
        }

        // operators & punctuation
        if b"+-*/%=<>!&|^~?:".contains(&c) {
            e.tok(i, i + 1, TokenKind::Operator);
        } else if !c.is_ascii() {
            i += clen(b, i);
            continue;
        } else {
            e.tok(i, i + 1, TokenKind::Punct);
        }
        i += 1;
    }

    e.finish(n);
    LexState::normal()
}

// ---- checkpointing highlighter ---------------------------------------------

/// Maximum number of lines we are willing to scan to reach a requested line.
/// Beyond this we assume "normal" state so scrolling stays interactive on
/// gigantic files (highlighting may then restart mid-construct).
const MAX_SCAN: u64 = 20_000;
/// Distance between state checkpoints.
const CK_STEP: u64 = 256;
/// Upper bound on retained checkpoints. 4096 × [`CK_STEP`] = 1M lines of
/// coverage before any thinning kicks in, so realistic documents never form
/// the multi-thousand-line gaps that used to force whole-gap replays.
const CK_MAX: usize = 4096;
/// Replay caps for the render-path variant ([`Highlighter::state_at_budgeted`]):
/// one call must never blow the frame budget. 2048 lines ≈ 1.2ms of lexing on
/// typical source (the sequential line-read fast path in `buffer::doc` keeps
/// each read at O(line length)), and 1MB bounds the pathological "every line is
/// huge" case — a single 100k-char line measures ~0.5ms to lex alone. When the
/// budget runs out the true frontier state is checkpointed and the replay
/// resumes from there on a later frame.
const FRAME_LINES: u64 = 2048;
const FRAME_BYTES: u64 = 1024 * 1024;

/// Per-document highlighter with lazy state checkpoints.
///
/// The checkpoint cache is *derived* state (same-generation idempotent), so it
/// lives behind interior mutability: `state_at` takes `&self`, which lets the
/// viewport render from a shared (`Signal::read`) borrow instead of a write
/// lock — a render-path write would mark the whole app dirty every frame.
pub struct Highlighter {
    spec: &'static LangSpec,
    ck: std::cell::RefCell<Vec<(u64, LexState)>>,
    gen: std::cell::Cell<u64>,
    scratch: std::cell::RefCell<Vec<Token>>,
}

impl Highlighter {
    pub fn new(spec: &'static LangSpec) -> Self {
        Highlighter {
            spec,
            ck: std::cell::RefCell::new(Vec::new()),
            gen: std::cell::Cell::new(u64::MAX),
            scratch: std::cell::RefCell::new(Vec::new()),
        }
    }

    pub fn spec(&self) -> &'static LangSpec {
        self.spec
    }

    pub fn set_spec(&mut self, spec: &'static LangSpec) {
        if !std::ptr::eq(self.spec, spec) {
            self.spec = spec;
            self.ck.borrow_mut().clear();
        }
    }

    /// Drop all state checkpoints (document gen unchanged, but the cached
    /// per-line states were derived from data that has since been corrected —
    /// e.g. a progressive-open index patch refined the line→byte mapping).
    pub fn invalidate_checkpoints(&self) {
        self.ck.borrow_mut().clear();
    }

    /// Generation advance + **per-line incremental invalidation**:
    /// `keep_before` is the first line affected by this edit
    /// (`TextDocument::take_edit_from_line`) — checkpoints before it stay
    /// valid, those after (including it) are dropped. `u64::MAX` = no info →
    /// clear all (same as the old behavior).
    ///
    /// Without this entry point every keystroke re-lexed from line 0 to the
    /// first visible line: on a 10k-line document, typing lag grew linearly
    /// with line number (20k lexes + 20k piece-table line reads per key).
    pub fn sync_gen(&self, gen: u64, keep_before: u64) {
        if self.gen.get() == gen {
            return;
        }
        self.gen.set(gen);
        let mut ck = self.ck.borrow_mut();
        if keep_before == 0 || keep_before == u64::MAX {
            ck.clear();
        } else {
            ck.retain(|c| c.0 < keep_before);
        }
    }

    /// Lexer state at the start of `line`. `get` returns the text of a line.
    ///
    /// `&self` (the checkpoint cache lives behind interior mutability): the
    /// render path calls this with a shared borrow; a cache update is not a
    /// semantic change (same-generation idempotent) and must not dirty the
    /// signal.
    pub fn state_at(&self, line: u64, gen: u64, get: &dyn Fn(u64) -> String) -> LexState {
        self.replay(line, gen, get, u64::MAX, u64::MAX).0
    }

    /// Render-path variant of [`Self::state_at`]: the replay is capped at
    /// `FRAME_LINES` lines / `FRAME_BYTES` bytes so one call can never
    /// blow the frame budget (an uncapped cold replay measured 141ms — an
    /// 8-frame stall during scrollbar drags and jump scrolls on large
    /// documents).
    ///
    /// Returns `(state, reached)`. When `reached` is `false` the state is the
    /// scan *frontier's* (a true intermediate state, so multi-line constructs
    /// that already started render correctly), the frontier is checkpointed,
    /// and the next call resumes from it — continuation rides whatever frames
    /// happen anyway (scrolling, caret blink), never a frame storm of its own.
    pub fn state_at_budgeted(
        &self,
        line: u64,
        gen: u64,
        get: &dyn Fn(u64) -> String,
    ) -> (LexState, bool) {
        self.replay(line, gen, get, FRAME_LINES, FRAME_BYTES)
    }

    /// Shared replay core: advance from the nearest checkpoint to `line`,
    /// giving up after `max_lines` lines / `max_bytes` of line text.
    fn replay(
        &self,
        line: u64,
        gen: u64,
        get: &dyn Fn(u64) -> String,
        max_lines: u64,
        max_bytes: u64,
    ) -> (LexState, bool) {
        if self.gen.get() != gen {
            self.ck.borrow_mut().clear();
            self.gen.set(gen);
        }
        if line == 0 {
            return (LexState::normal(), true);
        }

        let mut ck = self.ck.borrow_mut();
        let mut base: (u64, LexState) = (0, LexState::normal());
        for c in ck.iter() {
            if c.0 <= line && c.0 >= base.0 {
                base = *c;
            }
        }
        if line - base.0 > MAX_SCAN {
            // too far: approximate
            push_ck_into(&mut ck, line, LexState::normal());
            return (LexState::normal(), true);
        }
        let mut st = base.1;
        let mut l = base.0;
        let mut bytes = 0u64;
        let mut scratch = self.scratch.borrow_mut();
        while l < line {
            if l - base.0 >= max_lines || bytes >= max_bytes {
                // Budget spent: checkpoint the frontier (a true state, so the
                // next call picks up exactly here) and report the approximation.
                push_ck_into(&mut ck, l, st);
                return (st, false);
            }
            let text = get(l);
            bytes += text.len() as u64;
            scratch.clear();
            st = lex_line(self.spec, &text, st, &mut scratch);
            l += 1;
            if l % CK_STEP == 0 {
                push_ck_into(&mut ck, l, st);
            }
        }
        push_ck_into(&mut ck, line, st);
        (st, true)
    }
}

/// Update the state checkpoint at `line`; thin out beyond [`CK_MAX`].
fn push_ck_into(ck: &mut Vec<(u64, LexState)>, line: u64, st: LexState) {
    if let Some(c) = ck.iter_mut().find(|c| c.0 == line) {
        c.1 = st;
        return;
    }
    ck.push((line, st));
    if ck.len() > CK_MAX {
        // thin out: keep every other checkpoint, but never let the gap
        // between adjacent checkpoints exceed 8 strides ([`CK_STEP`] × 8).
        // Larger gaps make `state_at` replay more lines in one go — and once a
        // gap passes [`MAX_SCAN`] it gives up with a "normal" approximation
        // outright, mis-highlighting deep multi-line constructs (block
        // comments, triple-quoted strings).
        //
        // Sort first: scrolling up appends smaller line numbers (ck is no
        // longer ascending), and the `c.0 - last` below would underflow when
        // c.0 < last (debug panic / release wraparound).
        ck.sort_unstable_by_key(|c| c.0);
        let max_gap = CK_STEP * 8;
        let mut keep: Vec<(u64, LexState)> = Vec::with_capacity(CK_MAX);
        let mut last = 0u64;
        for (i, c) in ck.iter().enumerate() {
            if i % 2 == 0 || c.0.saturating_sub(last) > max_gap {
                keep.push(*c);
                last = c.0;
            }
        }
        *ck = keep;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::highlight::lang::by_name;

    fn toks(spec: &LangSpec, lines: &[&str]) -> (Vec<Vec<Token>>, LexState) {
        let mut st = LexState::normal();
        let mut out = Vec::new();
        for l in lines {
            let mut v = Vec::new();
            st = lex_line(spec, l, st, &mut v);
            out.push(v);
        }
        (out, st)
    }

    fn kind_at(tokens: &[Token], idx: usize) -> TokenKind {
        tokens
            .iter()
            .find(|t| t.start <= idx && idx < t.end)
            .map(|t| t.kind)
            .unwrap_or(TokenKind::Plain)
    }

    #[test]
    fn rust_keywords_and_strings() {
        let spec = by_name("Rust");
        let (t, _) = toks(spec, &[r#"fn main() { let s = "hi\n"; }"#]);
        assert_eq!(kind_at(&t[0], 0), TokenKind::Keyword); // fn
        assert_eq!(kind_at(&t[0], 3), TokenKind::Function); // main
        assert_eq!(kind_at(&t[0], 12), TokenKind::Keyword); // let
        assert_eq!(kind_at(&t[0], 22), TokenKind::String); // "hi\n"
        assert_eq!(kind_at(&t[0], 16), TokenKind::Ident); // s
    }

    #[test]
    fn rust_block_comment_spans_lines() {
        let spec = by_name("Rust");
        let (t, st) = toks(spec, &["/* start", "still", "end */ x"]);
        assert_eq!(kind_at(&t[0], 0), TokenKind::Comment);
        assert_eq!(kind_at(&t[1], 0), TokenKind::Comment);
        assert_eq!(st, LexState::normal());
        assert_eq!(kind_at(&t[2], 0), TokenKind::Comment);
        // after the comment ends, `x` is an identifier
        assert_eq!(kind_at(&t[2], 7), TokenKind::Ident);
    }

    #[test]
    fn rust_nested_block_comment() {
        let spec = by_name("Rust");
        let (t, st) = toks(spec, &["/* a /* b */ c */", "after"]);
        assert_eq!(st, LexState::normal());
        assert_eq!(kind_at(&t[1], 0), TokenKind::Ident);
    }

    /// Regression: block comment depth used to be u8 (saturating at 255) —
    /// with 300 nested levels an early `*/` would wrongly close the whole
    /// comment. After widening to u16, 300 levels must pair exactly;
    /// asserts both the cross-line continuation state and the closing keyword.
    #[test]
    fn deeply_nested_block_comment_does_not_saturate() {
        let spec = by_name("Rust");
        // 300 levels on one line, fully closed
        let one = format!("{}{} fn f() {{}}", "/*".repeat(300), "*/".repeat(300));
        let (t, st) = toks(spec, &[one.as_str()]);
        assert_eq!(st, LexState::normal(), "300 levels should close completely");
        assert_eq!(
            kind_at(&t[0], 1201),
            TokenKind::Keyword,
            "the closing fn should be a keyword"
        );

        // Cross-line: line 1 opens 300 levels, closes one per line, and the
        // last line resumes normal lexing
        let mut lines: Vec<String> = vec![];
        lines.push(format!("let x = 1; {}", "/*".repeat(300)));
        for _ in 0..299 {
            lines.push("*/".to_string());
        }
        lines.push("*/".to_string());
        lines.push("let y = 2;".to_string());
        let refs: Vec<&str> = lines.iter().map(|s| s.as_str()).collect();
        let (t2, st2) = toks(spec, &refs);
        assert_eq!(
            st2,
            LexState::normal(),
            "cross-line 300 levels should close completely"
        );
        assert_eq!(
            kind_at(&t2[refs.len() - 1], 0),
            TokenKind::Keyword,
            "the line after closing should resume normal lexing"
        );
    }

    #[test]
    fn rust_raw_string() {
        let spec = by_name("Rust");
        let (t, st) = toks(spec, &[r##"let a = r#"x "y" z"#;"##, "let b = 1;"]);
        // the `"y"` inside the raw string must not terminate it
        assert_eq!(kind_at(&t[0], 8), TokenKind::String);
        assert_eq!(kind_at(&t[0], 14), TokenKind::String);
        assert_eq!(st, LexState::normal());
        assert_eq!(kind_at(&t[1], 0), TokenKind::Keyword);
    }

    #[test]
    fn rust_lifetime_is_not_a_char() {
        let spec = by_name("Rust");
        let (t, _) = toks(spec, &["fn f<'a>(x: &'a str) {}"]);
        assert_eq!(kind_at(&t[0], 4), TokenKind::Operator); // <
        assert_eq!(kind_at(&t[0], 5), TokenKind::Punct); // '
        assert_eq!(kind_at(&t[0], 6), TokenKind::Ident); // a
        assert_eq!(kind_at(&t[0], 16), TokenKind::Type); // str
    }

    #[test]
    fn rust_char_literals() {
        let spec = by_name("Rust");
        let (t, _) = toks(spec, &[r"let c = 'x'; let d = '\n'; let e = ' ';"]);
        assert_eq!(kind_at(&t[0], 8), TokenKind::Char); // 'x'
        assert_eq!(kind_at(&t[0], 21), TokenKind::Char); // '\n'
        assert_eq!(kind_at(&t[0], 36), TokenKind::Char); // ' '
    }

    #[test]
    fn rust_generic_lifetimes_are_not_chars() {
        // `'a, '` must not be mistaken for a character literal
        let spec = by_name("Rust");
        let (t, _) = toks(spec, &["fn f<'a, 'b>(x: &'a T) {}"]);
        assert_eq!(kind_at(&t[0], 5), TokenKind::Punct);
        assert_eq!(kind_at(&t[0], 6), TokenKind::Ident);
    }

    #[test]
    fn python_triple_string() {
        let spec = by_name("Python");
        let (t, st) = toks(spec, &["s = \"\"\"abc", "def\"\"\"", "x = 1"]);
        assert_eq!(kind_at(&t[0], 4), TokenKind::String);
        assert_eq!(kind_at(&t[1], 0), TokenKind::String);
        assert_eq!(st, LexState::normal());
        assert_eq!(kind_at(&t[2], 0), TokenKind::Ident);
    }

    #[test]
    fn c_preproc_and_comment() {
        let spec = by_name("C/C++");
        let (t, _) = toks(spec, &["#include <stdio.h>", "// note", "int x = 42;"]);
        assert_eq!(kind_at(&t[0], 0), TokenKind::Preproc);
        assert_eq!(kind_at(&t[1], 0), TokenKind::Comment);
        assert_eq!(kind_at(&t[2], 0), TokenKind::Type);
        assert_eq!(kind_at(&t[2], 8), TokenKind::Number);
    }

    #[test]
    fn sql_is_case_insensitive() {
        let spec = by_name("SQL");
        let (t, _) = toks(spec, &["select ID from Users where ID = 1"]);
        assert_eq!(kind_at(&t[0], 0), TokenKind::Keyword); // SELECT
        assert_eq!(kind_at(&t[0], 10), TokenKind::Keyword); // from
        assert_eq!(kind_at(&t[0], 21), TokenKind::Keyword); // where
        assert_eq!(kind_at(&t[0], 15), TokenKind::Ident); // Users
        assert_eq!(kind_at(&t[0], 32), TokenKind::Number);
    }

    #[test]
    fn html_tag_and_attrs() {
        let spec = by_name("HTML");
        let (t, _) = toks(spec, &[r#"<div class="main" id='x'>hi</div>"#]);
        assert_eq!(kind_at(&t[0], 1), TokenKind::Tag); // div
        assert_eq!(kind_at(&t[0], 5), TokenKind::AttrName); // class
        assert_eq!(kind_at(&t[0], 12), TokenKind::AttrValue); // "main"
        assert_eq!(kind_at(&t[0], 18), TokenKind::AttrName); // id
        assert_eq!(kind_at(&t[0], 30), TokenKind::Tag); // closing div
        assert_eq!(kind_at(&t[0], 25), TokenKind::Ident); // text content
    }

    #[test]
    fn markdown_headings_fences_and_inline() {
        let spec = by_name("Markdown");
        let (t, st) = toks(
            spec,
            &[
                "# Title",
                "text with `code` and **bold**",
                "```rust",
                "fn a() {}",
                "```",
                "- item",
            ],
        );
        assert_eq!(kind_at(&t[0], 0), TokenKind::Heading);
        assert_eq!(kind_at(&t[1], 11), TokenKind::Code);
        assert_eq!(kind_at(&t[1], 22), TokenKind::Bold);
        assert_eq!(kind_at(&t[2], 0), TokenKind::Code);
        assert_eq!(kind_at(&t[3], 0), TokenKind::Code);
        assert_eq!(kind_at(&t[4], 0), TokenKind::Code);
        assert_eq!(st, LexState::normal());
        assert_eq!(kind_at(&t[5], 0), TokenKind::ListMark);
    }

    #[test]
    fn markdown_table_columns() {
        let spec = by_name("Markdown");
        let (t, _) = toks(spec, &["| a | b | c |"]);
        assert_eq!(kind_at(&t[0], 0), TokenKind::Punct);
        assert_eq!(kind_at(&t[0], 2), TokenKind::Col(1));
        assert_eq!(kind_at(&t[0], 6), TokenKind::Col(2));
    }

    #[test]
    fn diff_lines() {
        let spec = by_name("Diff");
        let (t, _) = toks(
            spec,
            &["--- a.txt", "+++ b.txt", "-old", "+new", " context"],
        );
        assert_eq!(kind_at(&t[0], 0), TokenKind::Meta);
        assert_eq!(kind_at(&t[1], 0), TokenKind::Meta);
        assert_eq!(kind_at(&t[2], 0), TokenKind::Removed);
        assert_eq!(kind_at(&t[3], 0), TokenKind::Added);
        assert_eq!(kind_at(&t[4], 0), TokenKind::Plain);
    }

    #[test]
    fn csv_columns() {
        let spec = by_name("CSV");
        let (t, _) = toks(spec, &["a,b,\"c,d\",e"]);
        assert_eq!(kind_at(&t[0], 0), TokenKind::Col(0));
        assert_eq!(kind_at(&t[0], 2), TokenKind::Col(1));
        assert_eq!(kind_at(&t[0], 4), TokenKind::Col(2)); // quoted field with comma
        assert_eq!(kind_at(&t[0], 10), TokenKind::Col(3));
    }

    #[test]
    fn tokens_cover_the_whole_line() {
        let spec = by_name("Rust");
        let (t, _) = toks(spec, &["let x = 1; // c"]);
        let mut pos = 0;
        for tk in &t[0] {
            assert_eq!(tk.start, pos, "tokens must be contiguous");
            pos = tk.end;
        }
        assert_eq!(pos, "let x = 1; // c".len());
    }

    #[test]
    fn highlighter_checkpoints() {
        let spec = by_name("Python");
        let lines: Vec<String> = vec![
            "a = 1".into(),
            "s = \"\"\"x".into(),
            "y".into(),
            "\"\"\"".into(),
            "b = 2".into(),
        ];
        let h = Highlighter::new(spec);
        let st = h.state_at(4, 0, &|i| lines[i as usize].clone());
        let mut v = Vec::new();
        let _ = lex_line(spec, &lines[4], st, &mut v);
        assert_eq!(
            kind_at(&v, 0),
            TokenKind::Ident,
            "state must carry past the triple string"
        );
        assert_eq!(kind_at(&v, 4), TokenKind::Number);
    }

    #[test]
    fn utf8_lines_are_safe() {
        let spec = by_name("Rust");
        let (t, _) = toks(spec, &["let 中文 = \"你好\"; // 注释"]);
        assert_eq!(kind_at(&t[0], 0), TokenKind::Keyword); // let
        assert_eq!(kind_at(&t[0], 4), TokenKind::Ident); // Chinese
        assert_eq!(kind_at(&t[0], 13), TokenKind::String); // "你好"
        assert_eq!(kind_at(&t[0], 23), TokenKind::Comment); // // 注释
    }

    /// Per-line incremental invalidation: checkpoints before keep_before stay
    /// valid after the gen advance — the state at line 4 must match a
    /// "clear-all rescan" token by token (the triple-quoted string starts at
    /// line 1; with the line-2 checkpoint kept, cross-line state is still
    /// carried).
    #[test]
    fn sync_gen_keeps_checkpoints_before_edit_line() {
        let spec = by_name("Python");
        let lines: Vec<String> = vec![
            "a = 1".into(),
            "s = \"\"\"x".into(),
            "y".into(),
            "\"\"\"".into(),
            "b = 2".into(),
        ];
        let h = Highlighter::new(spec);
        // Warm-up: gen 0 builds the checkpoints
        let st0 = h.state_at(4, 0, &|i| lines[i as usize].clone());
        // The edit happens at line 2: invalidate from line 2 on, checkpoints
        // for lines 0/1 are kept
        h.sync_gen(7, 2);
        let st = h.state_at(4, 7, &|i| lines[i as usize].clone());
        assert_eq!(st, st0, "re-lex from kept checkpoints must agree");
        // No info (u64::MAX) → clear all: the result must still be correct
        h.sync_gen(9, u64::MAX);
        let st = h.state_at(4, 9, &|i| lines[i as usize].clone());
        assert_eq!(st, st0);
    }

    /// Render-path budgeting: a cold replay taller than the budget must stop
    /// at the frontier (reporting `reached == false`) and return the frontier's
    /// *true* state — not "normal" — and the next call resumes from the
    /// checkpointed frontier and lands.
    #[test]
    fn budgeted_replay_stops_at_frontier_and_resumes() {
        let spec = by_name("Rust");
        // Line 0 opens a block comment that closes at line 2900: every state
        // in between is "in block comment", so the frontier state is
        // recognizable.
        let lines: Vec<String> = (0..3000)
            .map(|i| match i {
                0 => "/* open".to_string(),
                2900 => "close */".to_string(),
                _ => "let x = 1;".to_string(),
            })
            .collect();
        let get = |i: u64| lines[i as usize].clone();
        let h = Highlighter::new(spec);
        // FRAME_LINES is 2048: the first call cannot reach line 2500
        let (st, reached) = h.state_at_budgeted(2500, 0, &get);
        assert!(
            !reached,
            "2500 lines past the budget must not land in one call"
        );
        assert_eq!(
            st.mode, M_BLOCK,
            "the frontier state must carry the open comment"
        );
        // The second call resumes from the frontier and lands
        let (st, reached) = h.state_at_budgeted(2500, 0, &get);
        assert!(reached, "the resumed replay must land");
        assert_eq!(st.mode, M_BLOCK);
        // Parity with the unbounded replay
        assert_eq!(st, Highlighter::new(spec).state_at(2500, 0, &get));
    }

    /// A replay that fits in the budget must be indistinguishable from the
    /// unbounded one — budgeting only ever defers, never changes, the result.
    #[test]
    fn budgeted_replay_matches_unbounded_when_it_lands() {
        let spec = by_name("Python");
        let lines: Vec<String> = vec![
            "s = \"\"\"x".into(),
            "y".into(),
            "z".into(),
            "\"\"\"".into(),
            "b = 2".into(),
        ];
        let get = |i: u64| lines[i as usize].clone();
        let (st, reached) = Highlighter::new(spec).state_at_budgeted(4, 0, &get);
        assert!(reached);
        assert_eq!(st, Highlighter::new(spec).state_at(4, 0, &get));
    }
}
