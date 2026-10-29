//! Pure parsing / flattening layer for single-line JSON (JSON Lines).
//!
//! No UI, no Signal dependencies; independently unit-testable. Design points:
//! - **Hand-written recursive descent**, not `serde_json::Value`: `Value` uses
//!   `BTreeMap`, which reorders key order, and cannot provide information like
//!   "the real child count even after truncation".
//! - Strings **keep the raw text** (including outer quotes and escape
//!   sequences); only the length is clipped by `MAX_VALUE_CHARS`.
//! - All hard limits (bytes / nodes / rows / depth / value length) are
//!   enforced in this layer, so callers need no extra guarding.
//! - `flatten` is O(node count); it is called only on "click" and "collapse
//!   children" — **never per frame**.

use std::collections::BTreeSet;

/// Reject lines over 4 MiB
pub const MAX_PARSE_BYTES: usize = 4 << 20;
/// Maximum node count per line
pub const MAX_NODES: usize = 20_000;
/// Maximum rendered rows when expanded
pub const MAX_ROWS: usize = 4_000;
/// Recursion depth cap (guards against stack overflow / malicious nesting)
pub const MAX_DEPTH: usize = 64;
/// Render clipping length for a single scalar value
pub const MAX_VALUE_CHARS: usize = 2_048;
/// Maximum number of rows expanded per document (for the view-layer LRU)
pub const MAX_OPEN: usize = 32;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Object,
    Array,
    String,
    Number,
    Bool,
    Null,
}

/// Parse result. `id` is a **pre-order sequence number**, assigned once during
/// parsing and never changed, so "which nodes are collapsed" can be expressed
/// stably via `id` (reordering / collapsing never changes ids).
#[derive(Clone, Debug)]
pub struct Node {
    pub id: u32,
    pub kind: Kind,
    /// Object member name (escapes decoded)
    pub key: Option<String>,
    /// Array index
    pub index: Option<usize>,
    /// Scalar display text (String is the raw snippet with quotes);
    /// containers are ""
    pub text: String,
    pub children: Vec<Node>,
    /// Real child count (including truncated parts with no node, for
    /// "… N items omitted")
    pub n_children: usize,
    /// Truncation note node ("… N items omitted (limit reached)");
    /// `flatten` renders it as an Ellipsis segment
    pub note: bool,
}

impl Node {
    fn scalar(
        id: u32,
        kind: Kind,
        key: Option<String>,
        index: Option<usize>,
        text: String,
    ) -> Self {
        Node {
            id,
            kind,
            key,
            index,
            text,
            children: Vec::new(),
            n_children: 0,
            note: false,
        }
    }
    pub fn is_container(&self) -> bool {
        matches!(self.kind, Kind::Object | Kind::Array)
    }
}

/// Semantic category of a screen line's content (not color: color varies with
/// the theme and is mapped to TokenKind at render time)
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tok {
    Key,
    Punct,
    Str,
    Num,
    Bool,
    Null,
    /// Collapsed summary ("{…} N items"), rendered as dark comment
    Preview,
    /// Truncation note ("… N items omitted")
    Ellipsis,
}

/// One screen line = several text segments with semantic categories. No
/// indentation (the render layer computes it from `depth`).
#[derive(Clone, Debug)]
pub struct Row {
    /// Corresponding Node::id (used when clicking to collapse)
    pub node: u32,
    pub depth: usize,
    pub segs: Vec<(Tok, String)>,
    /// Container (collapsible)
    pub foldable: bool,
    pub collapsed: bool,
}

/// Parse **one line** of JSONL. Returns `Err(msg)` on failure (the caller
/// shows a one-line red hint, no panic).
pub fn parse_line(src: &str) -> Result<Node, String> {
    if src.len() > MAX_PARSE_BYTES {
        return Err(format!(
            "该行 {} 字节，超过 {} MiB 上限，未解析",
            src.len(),
            MAX_PARSE_BYTES >> 20
        ));
    }
    let mut p = Parser {
        src,
        b: src.as_bytes(),
        i: 0,
        nodes: 0,
        next_id: 0,
    };
    p.ws();
    let n = p.value(0, None, None)?;
    p.ws();
    if p.i != p.b.len() {
        return Err(p.err_at("出现多余字符"));
    }
    Ok(n)
}

/// Flatten the tree into screen rows according to the current collapse set.
/// Returns `(rows, truncated)`; `truncated` means the `MAX_ROWS` cap was hit.
/// O(node count); called only on "click" and "collapse children".
pub fn flatten(root: &Node, collapsed: &BTreeSet<u32>) -> (Vec<Row>, bool) {
    let mut rows = Vec::new();
    let mut truncated = false;
    emit(root, 0, true, collapsed, &mut rows, &mut truncated);
    (rows, truncated)
}

fn emit(
    node: &Node,
    depth: usize,
    tail: bool,
    collapsed: &BTreeSet<u32>,
    rows: &mut Vec<Row>,
    truncated: &mut bool,
) {
    if rows.len() >= MAX_ROWS {
        *truncated = true;
        return;
    }
    if node.note {
        rows.push(Row {
            node: node.id,
            depth,
            segs: vec![(Tok::Ellipsis, node.text.clone())],
            foldable: false,
            collapsed: false,
        });
        return;
    }
    let mut prefix: Vec<(Tok, String)> = Vec::new();
    if let Some(k) = &node.key {
        prefix.push((Tok::Key, json_quote(k)));
        prefix.push((Tok::Punct, ": ".to_string()));
    }
    match node.kind {
        Kind::Object | Kind::Array => {
            let is_coll = collapsed.contains(&node.id);
            if is_coll {
                let mut segs = prefix;
                let brace = if node.kind == Kind::Object {
                    "{…}"
                } else {
                    "[…]"
                };
                segs.push((Tok::Punct, brace.to_string()));
                if node.n_children > 0 {
                    segs.push((Tok::Preview, format!("{} 项", node.n_children)));
                }
                if !tail {
                    segs.push((Tok::Punct, ",".to_string()));
                }
                rows.push(Row {
                    node: node.id,
                    depth,
                    segs,
                    foldable: true,
                    collapsed: true,
                });
            } else {
                let mut segs = prefix;
                let brace = if node.kind == Kind::Object { "{" } else { "[" };
                segs.push((Tok::Punct, brace.to_string()));
                rows.push(Row {
                    node: node.id,
                    depth,
                    segs,
                    foldable: true,
                    collapsed: false,
                });
                let last = node.children.len();
                for (i, ch) in node.children.iter().enumerate() {
                    emit(ch, depth + 1, i + 1 == last, collapsed, rows, truncated);
                }
                if rows.len() < MAX_ROWS {
                    let brace = if node.kind == Kind::Object { "}" } else { "]" };
                    let mut segs = vec![(Tok::Punct, brace.to_string())];
                    if !tail {
                        segs.push((Tok::Punct, ",".to_string()));
                    }
                    rows.push(Row {
                        node: node.id,
                        depth,
                        segs,
                        foldable: false,
                        collapsed: false,
                    });
                } else {
                    *truncated = true;
                }
            }
        }
        _ => {
            let tok = match node.kind {
                Kind::String => Tok::Str,
                Kind::Number => Tok::Num,
                Kind::Bool => Tok::Bool,
                _ => Tok::Null,
            };
            let mut segs = prefix;
            segs.push((tok, node.text.clone()));
            if !tail {
                segs.push((Tok::Punct, ",".to_string()));
            }
            rows.push(Row {
                node: node.id,
                depth,
                segs,
                foldable: false,
                collapsed: false,
            });
        }
    }
}

// ---------------------------------------------------------------- parser ---

struct Parser<'a> {
    src: &'a str,
    b: &'a [u8],
    i: usize,
    nodes: usize,
    next_id: u32,
}

impl<'a> Parser<'a> {
    fn ws(&mut self) {
        while let Some(c) = self.b.get(self.i) {
            match c {
                b' ' | b'\t' | b'\n' | b'\r' => self.i += 1,
                _ => break,
            }
        }
    }
    fn peek(&self) -> u8 {
        *self.b.get(self.i).unwrap_or(&0)
    }
    fn err_at(&self, msg: &str) -> String {
        // report a *character* column: byte offsets don't match what the user
        // sees once the line contains multi-byte characters
        let col = self.b[..self.i.min(self.b.len())]
            .iter()
            .filter(|&&b| b & 0xC0 != 0x80)
            .count()
            + 1;
        format!("第 {} 列：{}", col, msg)
    }

    fn value(
        &mut self,
        depth: usize,
        key: Option<String>,
        index: Option<usize>,
    ) -> Result<Node, String> {
        if depth > MAX_DEPTH {
            return Err("嵌套过深（上限 64 层）".to_string());
        }
        self.ws();
        let id = self.next_id;
        self.next_id += 1;
        self.nodes += 1;
        match self.peek() {
            b'{' => self.object(id, depth, key, index),
            b'[' => self.array(id, depth, key, index),
            b'"' => {
                let (raw, _) = self.string_raw()?;
                Ok(Node::scalar(id, Kind::String, key, index, clip_str(&raw)))
            }
            b't' => {
                self.lit("true")?;
                Ok(Node::scalar(id, Kind::Bool, key, index, "true".into()))
            }
            b'f' => {
                self.lit("false")?;
                Ok(Node::scalar(id, Kind::Bool, key, index, "false".into()))
            }
            b'n' => {
                self.lit("null")?;
                Ok(Node::scalar(id, Kind::Null, key, index, "null".into()))
            }
            _ => {
                let text = self.scan_number()?;
                Ok(Node::scalar(id, Kind::Number, key, index, clip_str(&text)))
            }
        }
    }

    fn object(
        &mut self,
        id: u32,
        depth: usize,
        key: Option<String>,
        index: Option<usize>,
    ) -> Result<Node, String> {
        self.i += 1; // '{'
        let mut node = Node {
            id,
            kind: Kind::Object,
            key,
            index,
            text: String::new(),
            children: Vec::new(),
            n_children: 0,
            note: false,
        };
        let mut real = 0usize;
        let mut skipped = 0usize;
        self.ws();
        if self.peek() == b'}' {
            self.i += 1;
            node.n_children = 0;
            return Ok(node);
        }
        loop {
            self.ws();
            if self.peek() != b'"' {
                return Err(self.err_at("对象键应为字符串"));
            }
            let (_raw_k, dec_k) = self.string_raw()?;
            self.ws();
            if self.peek() != b':' {
                return Err(self.err_at("缺少 ':'"));
            }
            self.i += 1;
            if self.nodes >= MAX_NODES {
                // Over the limit: only scan and count, no more nodes are built
                // (the real child count stays accurate). The container itself
                // must not be marked as note — a note container is rendered in
                // emit as a single Ellipsis row with empty text and the whole
                // tree disappears; the sentinel child below already expresses
                // "truncated" well enough.
                self.skip_value(depth + 1)?;
                skipped += 1;
            } else {
                let child = self.value(depth + 1, Some(clip_str(&dec_k)), None)?;
                node.children.push(child);
                real += 1;
            }
            self.ws();
            match self.peek() {
                b',' => self.i += 1,
                b'}' => {
                    self.i += 1;
                    break;
                }
                _ => return Err(self.err_at("缺少 ',' 或 '}'")),
            }
        }
        node.n_children = real + skipped;
        if skipped > 0 {
            let mut n = Node::scalar(
                self.next_id,
                Kind::Null,
                None,
                None,
                format!("… 已省略 {} 项（已到上限）", skipped),
            );
            n.note = true;
            self.next_id += 1;
            node.children.push(n);
        }
        Ok(node)
    }

    fn array(
        &mut self,
        id: u32,
        depth: usize,
        key: Option<String>,
        index: Option<usize>,
    ) -> Result<Node, String> {
        self.i += 1; // '['
        let mut node = Node {
            id,
            kind: Kind::Array,
            key,
            index,
            text: String::new(),
            children: Vec::new(),
            n_children: 0,
            note: false,
        };
        let mut real = 0usize;
        let mut skipped = 0usize;
        self.ws();
        if self.peek() == b']' {
            self.i += 1;
            node.n_children = 0;
            return Ok(node);
        }
        loop {
            if self.nodes >= MAX_NODES {
                self.skip_value(depth + 1)?;
                skipped += 1;
            } else {
                let child = self.value(depth + 1, None, Some(real))?;
                node.children.push(child);
                real += 1;
            }
            self.ws();
            match self.peek() {
                b',' => self.i += 1,
                b']' => {
                    self.i += 1;
                    break;
                }
                _ => return Err(self.err_at("缺少 ',' 或 ']'")),
            }
        }
        node.n_children = real + skipped;
        if skipped > 0 {
            let mut n = Node::scalar(
                self.next_id,
                Kind::Null,
                None,
                None,
                format!("… 已省略 {} 项（已到上限）", skipped),
            );
            n.note = true;
            self.next_id += 1;
            node.children.push(n);
        }
        Ok(node)
    }

    /// Skip one value (no node built). Only structural validation; the caller
    /// counts one `skip_value` as one direct child ("item") — nesting depth
    /// does not affect the "N items omitted" semantics.
    fn skip_value(&mut self, depth: usize) -> Result<(), String> {
        if depth > MAX_DEPTH {
            return Err("嵌套过深（上限 64 层）".to_string());
        }
        self.ws();
        match self.peek() {
            b'{' => {
                self.i += 1;
                self.ws();
                if self.peek() == b'}' {
                    self.i += 1;
                    return Ok(());
                }
                loop {
                    self.ws();
                    if self.peek() != b'"' {
                        return Err(self.err_at("对象键应为字符串"));
                    }
                    self.string_raw()?;
                    self.ws();
                    if self.peek() != b':' {
                        return Err(self.err_at("缺少 ':'"));
                    }
                    self.i += 1;
                    self.skip_value(depth + 1)?;
                    self.ws();
                    match self.peek() {
                        b',' => self.i += 1,
                        b'}' => {
                            self.i += 1;
                            return Ok(());
                        }
                        _ => return Err(self.err_at("缺少 ',' 或 '}'")),
                    }
                }
            }
            b'[' => {
                self.i += 1;
                self.ws();
                if self.peek() == b']' {
                    self.i += 1;
                    return Ok(());
                }
                loop {
                    self.skip_value(depth + 1)?;
                    self.ws();
                    match self.peek() {
                        b',' => self.i += 1,
                        b']' => {
                            self.i += 1;
                            return Ok(());
                        }
                        _ => return Err(self.err_at("缺少 ',' 或 ']'")),
                    }
                }
            }
            b'"' => {
                self.string_raw()?;
                Ok(())
            }
            b't' => {
                self.lit("true")?;
                Ok(())
            }
            b'f' => {
                self.lit("false")?;
                Ok(())
            }
            b'n' => {
                self.lit("null")?;
                Ok(())
            }
            _ => {
                self.scan_number()?;
                Ok(())
            }
        }
    }

    /// Read one string literal, returning `(raw text with quotes, decoded text)`.
    fn string_raw(&mut self) -> Result<(String, String), String> {
        if self.peek() != b'"' {
            return Err(self.err_at("应为字符串"));
        }
        let start = self.i;
        self.i += 1;
        loop {
            match self.b.get(self.i) {
                None => return Err(self.err_at("字符串未闭合")),
                Some(b'"') => {
                    self.i += 1;
                    break;
                }
                Some(b'\\') => self.i += 2, // skip the escape sequence plus the following character
                Some(_) => self.i += 1,
            }
        }
        let raw = self.src[start..self.i].to_string();
        let dec = decode_json_string(&raw);
        Ok((raw, dec))
    }

    fn lit(&mut self, word: &str) -> Result<(), String> {
        if self.b[self.i..].starts_with(word.as_bytes()) {
            let after = self.b.get(self.i + word.len()).copied().unwrap_or(b' ');
            if after.is_ascii_alphanumeric() {
                return Err(self.err_at("无效的字面量"));
            }
            self.i += word.len();
            Ok(())
        } else {
            Err(self.err_at("无效的字面量"))
        }
    }

    /// Scan and validate a JSON number ('-'? int frac? exp?), returning the raw text.
    fn scan_number(&mut self) -> Result<String, String> {
        let start = self.i;
        if self.peek() == b'-' {
            self.i += 1;
        }
        match self.peek() {
            b'0' => self.i += 1,
            b'1'..=b'9' => {
                while self.peek().is_ascii_digit() {
                    self.i += 1;
                }
            }
            _ => return Err(self.err_at("应为数字")),
        }
        if self.peek() == b'.' {
            self.i += 1;
            if !self.peek().is_ascii_digit() {
                return Err(self.err_at("小数点后应为数字"));
            }
            while self.peek().is_ascii_digit() {
                self.i += 1;
            }
        }
        if matches!(self.peek(), b'e' | b'E') {
            self.i += 1;
            if matches!(self.peek(), b'+' | b'-') {
                self.i += 1;
            }
            if !self.peek().is_ascii_digit() {
                return Err(self.err_at("指数部分应为数字"));
            }
            while self.peek().is_ascii_digit() {
                self.i += 1;
            }
        }
        Ok(self.src[start..self.i].to_string())
    }
}

// ------------------------------------------------------------- helpers ---

/// Clip to `MAX_VALUE_CHARS` characters, appending an ellipsis when over-long.
fn clip_str(s: &str) -> String {
    if s.chars().count() <= MAX_VALUE_CHARS {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(MAX_VALUE_CHARS).collect();
        out.push('…');
        out
    }
}

/// Decode escape sequences in a JSON string literal (`raw` includes the outer
/// quotes). Invalid escapes fall back to U+FFFD.
fn decode_json_string(raw: &str) -> String {
    let inner = &raw[1..raw.len() - 1];
    let mut out = String::with_capacity(inner.len());
    let mut it = inner.chars();
    while let Some(c) = it.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match it.next() {
            Some('"') => out.push('"'),
            Some('\\') => out.push('\\'),
            Some('/') => out.push('/'),
            Some('b') => out.push('\u{8}'),
            Some('f') => out.push('\u{c}'),
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('t') => out.push('\t'),
            Some('u') => {
                let hi = take4(&mut it);
                let cp = if (0xD800..0xDC00).contains(&hi) {
                    // Expect a low surrogate \uXXXX to form a surrogate pair.
                    // Probe two characters with a clone first: a direct
                    // `it.next()` would consume a character when it is not
                    // `\u` ("uD800ab" would lose the 'a').
                    let mut probe = it.clone();
                    let ok = probe.next() == Some('\\') && probe.next() == Some('u');
                    if ok {
                        it = probe;
                        let lo = take4(&mut it);
                        if (0xDC00..0xE000).contains(&lo) {
                            0x10000 + ((hi - 0xD800) << 10) + (lo - 0xDC00)
                        } else {
                            0xFFFD
                        }
                    } else {
                        0xFFFD
                    }
                } else {
                    hi
                };
                out.push(char::from_u32(cp).unwrap_or('\u{FFFD}'));
            }
            _ => out.push('\u{FFFD}'),
        }
    }
    out
}

fn take4(it: &mut std::str::Chars<'_>) -> u32 {
    let mut v = 0u32;
    for _ in 0..4 {
        match it.next().and_then(|c| c.to_digit(16)) {
            Some(d) => v = v * 16 + d,
            None => return 0xFFFD,
        }
    }
    v
}

/// Re-encode a decoded key into quoted display text (object member names have
/// no "keep raw" requirement).
fn json_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

// ---------------------------------------------------------------- tests ---

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_order_is_preserved() {
        let n = parse_line(r#"{"b":1,"a":{"z":2,"y":3},"c":[10,20]}"#).unwrap();
        let keys: Vec<_> = n.children.iter().map(|c| c.key.clone().unwrap()).collect();
        assert_eq!(keys, ["b", "a", "c"]);
        let inner = &n.children[1];
        let ikeys: Vec<_> = inner
            .children
            .iter()
            .map(|c| c.key.clone().unwrap())
            .collect();
        assert_eq!(ikeys, ["z", "y"]);
        // Array indices
        let arr = &n.children[2];
        assert_eq!(arr.children[1].index, Some(1));
    }

    #[test]
    fn scalars_and_kinds() {
        let n = parse_line("  42 ").unwrap();
        assert_eq!(n.kind, Kind::Number);
        assert_eq!(n.text, "42");
        assert_eq!(parse_line("-1.5e+3").unwrap().kind, Kind::Number);
        assert_eq!(parse_line("true").unwrap().kind, Kind::Bool);
        assert_eq!(parse_line("null").unwrap().kind, Kind::Null);
        assert_eq!(parse_line("\"hi\"").unwrap().kind, Kind::String);
        // Number grammar validation
        assert!(parse_line("01").is_err());
        assert!(parse_line("1.").is_err());
        assert!(parse_line("-").is_err());
        assert!(parse_line("1e").is_err());
    }

    #[test]
    fn invalid_inputs_are_rejected() {
        for bad in [
            "",
            "   ",
            "{",
            "}",
            "[1,",
            "{\"a\"}",
            "{\"a\":}",
            "tru",
            "nul",
            "{\"a\":1,}",
            "[,]",
            "\"unterminated",
            "{}{}",
            "[1] extra",
        ] {
            assert!(parse_line(bad).is_err(), "should reject {bad:?}");
        }
    }

    #[test]
    fn string_raw_text_is_preserved_and_escaped_keys_decoded() {
        let n = parse_line(r#"{"a\nb":"x\u4e2dy"}"#).unwrap();
        let child = &n.children[0];
        // Value keeps the raw text (quotes and escapes included)
        assert_eq!(child.text, r#""x\u4e2dy""#);
        // Key is decoded
        assert_eq!(child.key.as_deref(), Some("a\nb"));
        // Surrogate pair
        let n2 = parse_line(r#""😀""#).unwrap();
        assert_eq!(n2.kind, Kind::String);
    }

    #[test]
    fn depth_cap_is_enforced() {
        let deep = "[".repeat(MAX_DEPTH + 8) + "1" + &"]".repeat(MAX_DEPTH + 8);
        assert!(parse_line(&deep).unwrap_err().contains("嵌套过深"));
        // Exactly at the cap should succeed
        let ok = "[".repeat(MAX_DEPTH) + "1" + &"]".repeat(MAX_DEPTH);
        assert!(parse_line(&ok).is_ok());
    }

    #[test]
    fn long_string_is_clipped() {
        let s = format!("\"{}\"", "a".repeat(MAX_VALUE_CHARS + 500));
        let n = parse_line(&s).unwrap();
        assert_eq!(n.text.chars().count(), MAX_VALUE_CHARS + 1); // + ellipsis
        assert!(n.text.ends_with('…'));
    }

    #[test]
    fn node_cap_truncates_but_counts_real_children() {
        let mut s = String::from("[");
        for i in 0..25_000 {
            if i > 0 {
                s.push(',');
            }
            s.push_str(&i.to_string());
        }
        s.push(']');
        let n = parse_line(&s).unwrap();
        assert_eq!(n.n_children, 25_000, "real child count must be accurate");
        // Containers are not marked note: a note container would be rendered
        // by emit as a single empty Ellipsis row and the whole tree
        // disappears. Truncation is expressed by the sentinel child.
        assert!(!n.note);
        let note = n.children.last().unwrap();
        assert!(note.note);
        assert!(note.text.contains("已省略"));
        assert!(
            note.text.contains("5,001") || note.text.contains("5001"),
            "note should contain the omitted count: {}",
            note.text
        );
        // Built nodes + note node <= cap + 1
        assert!(n.children.len() <= MAX_NODES + 1);
        // After flattening, the tree must be visible (old bug: a note
        // container rendered the whole tree as one blank row).
        // MAX_ROWS(4000) < MAX_NODES, flatten hits the row cap before the
        // sentinel, so the last row is an ordinary child row — the point is
        // that content rows exist rather than an empty Ellipsis.
        let (rows, truncated) = flatten(&n, &BTreeSet::new());
        assert_eq!(
            rows.len(),
            MAX_ROWS,
            "row count should hit the MAX_ROWS cap"
        );
        assert!(truncated);
        assert!(rows
            .iter()
            .all(|r| r.segs.iter().any(|(_, s)| !s.is_empty())));
    }

    #[test]
    fn nested_objects_share_the_node_budget() {
        // MAX_NODES+100 nested single-element objects: objects and arrays
        // share the same node budget
        let mut s = String::from("[");
        for i in 0..MAX_NODES + 100 {
            if i > 0 {
                s.push(',');
            }
            s.push_str(&format!(r#"{{"v":{i}}}"#));
        }
        s.push(']');
        let n = parse_line(&s).unwrap();
        assert_eq!(n.n_children, MAX_NODES + 100);
        assert!(
            !n.note,
            "containers are no longer marked note (truncation is expressed by the sentinel child)"
        );
        // Indices are consecutive (skipped parts no longer occupy an index)
        let idxs: Vec<usize> = n.children.iter().filter_map(|c| c.index).collect();
        assert!(idxs.windows(2).all(|w| w[1] == w[0] + 1));
    }

    #[test]
    fn flatten_open_tree_matches_plan_example_shape() {
        let n = parse_line(r#"{"id":7,"ok":true}"#).unwrap();
        let (rows, truncated) = flatten(&n, &BTreeSet::new());
        assert!(!truncated);
        assert_eq!(rows.len(), 4); // { / id / ok / }
        assert_eq!(rows[0].segs, vec![(Tok::Punct, "{".into())]);
        assert!(rows[0].foldable);
        assert_eq!(rows[1].segs[0], (Tok::Key, "\"id\"".into()));
        assert!(rows[1].segs.iter().any(|&(t, _)| t == Tok::Num));
        assert_eq!(rows[1].depth, 1);
        assert_eq!(rows[3].segs[0], (Tok::Punct, "}".into()));
    }

    #[test]
    fn flatten_collapsed_container_shows_preview() {
        let n = parse_line(r#"{"user":{"name":"a","tags":[1,2]},"ok":true}"#).unwrap();
        let user_id = n.children[0].id;
        let mut collapsed = BTreeSet::new();
        collapsed.insert(user_id);
        let (rows, _) = flatten(&n, &collapsed);
        // { / user(collapsed) / ok / }
        assert_eq!(rows.len(), 4);
        let r = &rows[1];
        assert!(r.collapsed && r.foldable);
        assert_eq!(r.node, user_id);
        assert!(r.segs.iter().any(|&(t, _)| t == Tok::Preview));
        let joined: String = r.segs.iter().map(|(_, s)| s.as_str()).collect();
        assert!(
            joined.contains("2 项"),
            "preview should contain the child count: {joined}"
        );
    }

    #[test]
    fn flatten_respects_max_rows() {
        let mut s = String::from("[");
        for i in 0..(MAX_ROWS + 50) {
            if i > 0 {
                s.push(',');
            }
            s.push_str(&i.to_string());
        }
        s.push(']');
        let n = parse_line(&s).unwrap();
        let (rows, truncated) = flatten(&n, &BTreeSet::new());
        assert!(truncated);
        assert_eq!(rows.len(), MAX_ROWS);
    }

    #[test]
    fn flatten_of_scalar_root_is_one_row() {
        let n = parse_line("123").unwrap();
        let (rows, t) = flatten(&n, &BTreeSet::new());
        assert!(!t);
        assert_eq!(rows.len(), 1);
        assert!(!rows[0].foldable);
    }

    #[test]
    fn too_long_line_is_rejected_upfront() {
        let s = " ".repeat(MAX_PARSE_BYTES + 1);
        assert!(parse_line(&s).unwrap_err().contains("MiB"));
    }

    #[test]
    fn empty_containers_flatten_to_two_rows() {
        let n = parse_line(r#"{"a":{},"b":[]}"#).unwrap();
        let (rows, _) = flatten(&n, &BTreeSet::new());
        // { / a:{}(2) / b:[](2) / } = 6
        assert_eq!(rows.len(), 6);
    }
}
