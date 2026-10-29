//! Rich-paste HTML → Markdown conversion (a two-step sanitize-then-convert
//! flow: sanitize strips dangerous content, then the HTML is turned into
//! Markdown).
//!
//! Safety: **sanitize first, then convert** —
//! - executable or alternative-rendering elements such as
//!   `<script>`/`<style>`/`<svg>` are stripped **with their content**;
//! - comments (including Word's conditional comments) are stripped whole;
//! - tags outside the whitelist are unwrapped, keeping the text (Word/browser
//!   HTML is full of `<span>`/`<div>`; dropping the text would eat the body;
//!   the danger is in scripts/events/URL schemes, not in tag names);
//! - only `href`/`src`/`alt`/`align` attributes are read (everything else,
//!   including `style`/`on*`, is not read at all = never emitted — the
//!   conversion target is Markdown, which has no landing spot for attributes
//!   anyway), and a URL-scheme whitelist blocks `javascript:`/`data:` etc.
//!
//! The output is "HTML-free Markdown": a `<` inside a text node is always
//! escaped to `\<`, so after passing through [`crate::edit::Cmd::Paste`] into
//! the model it will **not** be re-upgraded into an Html block / inline HTML
//! by `md::parse`.

/// Elements stripped along with their content (scripts / styles /
/// alternative-rendering containers).
const DROP_CONTENT: &[&str] = &[
    "script", "style", "head", "title", "noscript", "template", "iframe", "object", "embed", "svg",
    "math", "canvas", "video", "audio", "applet", "frame", "frameset", "select", "option",
];

/// Void elements (no closing tag).
const VOID: &[&str] = &[
    "br", "img", "hr", "meta", "link", "input", "col", "source", "area", "base", "wbr",
];

/// Tag → Markdown-semantics alias (the various browser spellings of bold /
/// italic / strikethrough).
/// The whitelist itself: h1-h6/p/ul/ol/li/table/tr/td/th/blockquote/pre/code/
/// strong/em/del/a/img/br (+ the table-structure wrappers thead/tbody/tfoot).
fn semantic(tag: &str) -> Option<&'static str> {
    Some(match tag {
        "strong" | "b" => "strong",
        "em" | "i" => "em",
        "del" | "s" | "strike" => "del",
        "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => "h",
        "p" => "p",
        "ul" => "ul",
        "ol" => "ol",
        "li" => "li",
        "table" => "table",
        "thead" | "tbody" | "tfoot" => "tbody",
        "tr" => "tr",
        "td" => "td",
        "th" => "th",
        "blockquote" => "blockquote",
        "pre" => "pre",
        "code" => "code",
        "a" => "a",
        "img" => "img",
        "br" => "br",
        _ => return None,
    })
}

/// Block-level semantics (used to decide paragraph separation).
fn is_block_sem(s: &str) -> bool {
    matches!(
        s,
        "h" | "p" | "ul" | "ol" | "li" | "table" | "tr" | "td" | "th" | "blockquote" | "pre"
    )
}

// ---------------------------------------------------------------------------
// HTML parsing (a lenient tag scanner, good enough for sanitize/convert; it
// does not aim for full HTML5 semantics)
// ---------------------------------------------------------------------------

struct Elem {
    tag: String,
    attrs: Vec<(String, String)>,
    kids: Vec<Node>,
}

enum Node {
    Text(String),
    Elem(Elem),
}

impl Elem {
    fn attr(&self, name: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }
}

/// Parse into a node tree. Lenient: unclosed tags are gathered at the end,
/// and stray closing tags are ignored.
fn parse_nodes(html: &str) -> Vec<Node> {
    let bytes: Vec<char> = html.chars().collect();
    let mut roots: Vec<Node> = Vec::new();
    // Open-element stack: (placeholder index path into the roots/kids tree).
    // Backtracking via "paths" is too convoluted, and storing a &mut to the
    // "not-yet-attached children vector" in a stack cannot work here — so use
    // an explicit stack instead: one (tag, attrs, kids) per level, popped and
    // attached to the parent level on close.
    struct Open {
        tag: String,
        attrs: Vec<(String, String)>,
        kids: Vec<Node>,
    }
    let mut stack: Vec<Open> = Vec::new();
    let mut i = 0usize;
    let mut text = String::new();
    macro_rules! flush_text {
        () => {
            if !text.is_empty() {
                let t = decode_entities(&std::mem::take(&mut text));
                match stack.last_mut() {
                    Some(o) => o.kids.push(Node::Text(t)),
                    None => roots.push(Node::Text(t)),
                }
            }
        };
    }
    while i < bytes.len() {
        if bytes[i] != '<' {
            text.push(bytes[i]);
            i += 1;
            continue;
        }
        // Comment / doctype / CDATA / processing instruction: stripped whole
        // (consume through **after** the closer, otherwise `-->`/`>` leaks out
        // as text)
        let rest: String = bytes[i..std::cmp::min(i + 4, bytes.len())].iter().collect();
        if rest.starts_with("<!--") {
            match find_seq(&bytes, i + 4, "-->") {
                Some(end) => i = end + 3,
                None => break,
            }
            continue;
        }
        if rest.starts_with("<!") || rest.starts_with("<?") {
            match find_seq(&bytes, i + 1, ">") {
                Some(end) => i = end + 1,
                None => break,
            }
            continue;
        }
        // Closing tag
        if bytes.get(i + 1) == Some(&'/') {
            let end = find_seq(&bytes, i + 2, ">").unwrap_or(bytes.len());
            let name: String = bytes[i + 2..end].iter().collect();
            let name = name.trim().to_ascii_lowercase();
            let name = name.split_whitespace().next().unwrap_or("").to_string();
            if let Some(pos) = stack.iter().rposition(|o| o.tag == name) {
                flush_text!();
                // Pop through pos (inclusive), attaching to the new stack top
                while stack.len() > pos {
                    let o = stack.pop().unwrap();
                    let node = Node::Elem(Elem {
                        tag: o.tag,
                        attrs: o.attrs,
                        kids: o.kids,
                    });
                    match stack.last_mut() {
                        Some(parent) => parent.kids.push(node),
                        None => roots.push(node),
                    }
                }
            }
            i = end + 1;
            continue;
        }
        // Opening tag
        let end = find_seq(&bytes, i + 1, ">").unwrap_or(bytes.len());
        let inner: String = bytes[i + 1..end].iter().collect();
        let (name, attrs) = parse_tag(&inner);
        i = end + 1;
        if name.is_empty() {
            continue;
        }
        // Dangerous element: skip to its closing tag (if not found, swallow
        // through the end)
        if DROP_CONTENT.contains(&name.as_str()) {
            let close = format!("</{name}");
            // ASCII lowercase preserves length (byte offset = source offset),
            // then convert the offset back to a char count
            let rest: String = bytes[i..].iter().collect();
            match rest.to_ascii_lowercase().find(&close) {
                Some(off) => i += rest[..off].chars().count(),
                None => i = bytes.len(),
            }
            continue;
        }
        let self_closing = inner.trim_end().ends_with('/') || VOID.contains(&name.as_str());
        flush_text!();
        if self_closing {
            let node = Node::Elem(Elem {
                tag: name,
                attrs,
                kids: Vec::new(),
            });
            match stack.last_mut() {
                Some(parent) => parent.kids.push(node),
                None => roots.push(node),
            }
        } else {
            stack.push(Open {
                tag: name,
                attrs,
                kids: Vec::new(),
            });
        }
    }
    flush_text!();
    while let Some(o) = stack.pop() {
        let node = Node::Elem(Elem {
            tag: o.tag,
            attrs: o.attrs,
            kids: o.kids,
        });
        match stack.last_mut() {
            Some(parent) => parent.kids.push(node),
            None => roots.push(node),
        }
    }
    roots
}

/// Find a substring within a char slice, returning the **char index (the
/// absolute position of the opening '<')**.
fn find_seq(hay: &[char], from: usize, needle: &str) -> Option<usize> {
    let n: Vec<char> = needle.chars().collect();
    if n.is_empty() {
        return Some(from);
    }
    (from..hay.len().saturating_sub(n.len() - 1)).find(|&i| hay[i..i + n.len()] == n[..])
}

/// Parse the tag name and attributes (attribute values are entity-decoded;
/// the name is lowercased).
fn parse_tag(inner: &str) -> (String, Vec<(String, String)>) {
    let mut it = inner.chars().peekable();
    let mut name = String::new();
    while let Some(&c) = it.peek() {
        if c.is_whitespace() || c == '/' {
            break;
        }
        name.push(c);
        it.next();
    }
    let name = name.to_ascii_lowercase();
    let mut attrs = Vec::new();
    loop {
        while matches!(it.peek(), Some(c) if c.is_whitespace() || *c == '/') {
            it.next();
        }
        let mut k = String::new();
        while let Some(&c) = it.peek() {
            if c.is_whitespace() || c == '=' || c == '>' || c == '/' {
                break;
            }
            k.push(c);
            it.next();
        }
        if k.is_empty() {
            break;
        }
        while matches!(it.peek(), Some(c) if c.is_whitespace()) {
            it.next();
        }
        let mut v = String::new();
        if it.peek() == Some(&'=') {
            it.next();
            while matches!(it.peek(), Some(c) if c.is_whitespace()) {
                it.next();
            }
            match it.peek() {
                Some(&q) if q == '"' || q == '\'' => {
                    it.next();
                    for c in it.by_ref() {
                        if c == q {
                            break;
                        }
                        v.push(c);
                    }
                }
                _ => {
                    while let Some(&c) = it.peek() {
                        if c.is_whitespace() || c == '>' {
                            break;
                        }
                        v.push(c);
                        it.next();
                    }
                }
            }
        }
        attrs.push((k.to_ascii_lowercase(), decode_entities(&v)));
    }
    (name, attrs)
}

// ---------------------------------------------------------------------------
// Entity decoding / text escaping
// ---------------------------------------------------------------------------

/// HTML entity decoding (shared by text and attribute values): the commonly
/// used named-entity subset + numeric entities.
fn decode_entities(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        rest = &rest[amp..];
        let Some(semi) = rest.find(';').filter(|&p| p <= 10) else {
            out.push('&');
            rest = &rest[1..];
            continue;
        };
        let body = &rest[1..semi];
        let repl = match body {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" | "#39" => Some('\''),
            "nbsp" => Some('\u{a0}'),
            "copy" => Some('©'),
            "reg" => Some('®'),
            "trade" => Some('™'),
            "hellip" => Some('…'),
            "mdash" => Some('—'),
            "ndash" => Some('–'),
            "lsquo" => Some('‘'),
            "rsquo" => Some('’'),
            "ldquo" => Some('“'),
            "rdquo" => Some('”'),
            "middot" => Some('·'),
            "times" => Some('×'),
            "laquo" => Some('«'),
            "raquo" => Some('»'),
            "bull" => Some('•'),
            "deg" => Some('°'),
            "plusmn" => Some('±'),
            "sect" => Some('§'),
            "para" => Some('¶'),
            _ => {
                if let Some(hex) = body.strip_prefix("#x").or_else(|| body.strip_prefix("#X")) {
                    u32::from_str_radix(hex, 16).ok().and_then(char::from_u32)
                } else if let Some(dec) = body.strip_prefix('#') {
                    dec.parse::<u32>().ok().and_then(char::from_u32)
                } else {
                    None
                }
            }
        };
        match repl {
            Some(c) => {
                out.push(c);
                rest = &rest[semi + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// Inline text escaping: escape `\`/`<`/`>`/`&`, so the conversion result can
/// never be re-parsed by md::parse into HTML, quotes, or entities. Other
/// Markdown punctuation is not escaped (same semantics as plain-text paste).
fn escape_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' | '<' | '>' | '&' => {
                out.push('\\');
                out.push(c);
            }
            _ => out.push(c),
        }
    }
    out
}

/// Table-cell text escaping: on top of [`escape_text`] also escape `|` (the
/// GFM cell separator) and squash newlines into spaces (a cell is one line).
fn escape_cell(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' | '<' | '>' | '&' | '|' => {
                out.push('\\');
                out.push(c);
            }
            '\n' | '\r' => out.push(' '),
            _ => out.push(c),
        }
    }
    out
}

/// HTML whitespace collapsing (U+00A0 decoded from `&nbsp;` is not in the
/// collapse set and is preserved by semantics).
fn collapse_ws(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_ws = false;
    for c in s.chars() {
        let ws = matches!(c, ' ' | '\t' | '\r' | '\n');
        if ws {
            if !prev_ws {
                out.push(' ');
            }
        } else {
            out.push(c);
        }
        prev_ws = ws;
    }
    out
}

/// URL-scheme whitelist (href/src): http/https/file/mailto and relative paths
/// pass; `javascript:`/`data:`/`vbscript:` etc. are rejected (whitespace and
/// control characters are stripped before the check, to defeat `java&#10;script:`
/// -style bypasses).
fn safe_url(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let folded: String = trimmed
        .chars()
        .filter(|c| !c.is_whitespace() && !c.is_control())
        .collect::<String>()
        .to_ascii_lowercase();
    // A scheme is present but not whitelisted → reject
    if let Some(idx) = folded.find(':') {
        let before = &folded[..idx];
        // `:` only counts as a scheme if it comes before the first `/` (a
        // relative path `a/b:c` passes)
        let slash = folded.find('/').unwrap_or(usize::MAX);
        if idx < slash {
            let ok = matches!(
                before,
                "http" | "https" | "file" | "mailto" | "ftp" | "ftps" | "tel"
            );
            if !ok {
                return None;
            }
        }
    }
    Some(trimmed.to_string())
}

// ---------------------------------------------------------------------------
// Markdown emission
// ---------------------------------------------------------------------------

/// HTML → Markdown (sanitize + convert in one, see the module docs).
pub fn html_to_markdown(html: &str) -> String {
    let nodes = parse_nodes(html);
    let mut out = String::new();
    blocks_md(&nodes, &mut out);
    out.trim_end().to_string()
}

/// Block-level sequence → Markdown. Consecutive inline nodes are gathered
/// into a paragraph; block-level elements are emitted individually.
fn blocks_md(nodes: &[Node], out: &mut String) {
    let mut run: Vec<&Node> = Vec::new();
    let flush = |run: &mut Vec<&Node>, out: &mut String| {
        if run.is_empty() {
            return;
        }
        let mut para = String::new();
        for n in run.drain(..) {
            inline_node(n, &mut para);
        }
        let para = para.trim();
        if !para.is_empty() {
            out.push_str(para);
            out.push_str("\n\n");
        }
    };
    for n in nodes {
        match n {
            Node::Text(_) => run.push(n),
            Node::Elem(e) => {
                let sem = semantic(&e.tag);
                let is_block = sem.is_some_and(is_block_sem);
                if !is_block {
                    // Outside the whitelist, unwrap and keep the text: inline
                    // nodes keep going into run; block-shaped unknown containers
                    // (div etc.) split by block in a block context (a browser
                    // `<div>` is a paragraph break).
                    if sem.is_none() && elem_looks_block(&e.tag) {
                        flush(&mut run, out);
                        blocks_md(&e.kids, out);
                    } else {
                        run.push(n);
                    }
                    continue;
                }
                flush(&mut run, out);
                let sem = sem.unwrap();
                match sem {
                    "h" => {
                        let lvl = e.tag[1..].parse::<usize>().unwrap_or(1).clamp(1, 6);
                        let mut t = String::new();
                        inline_md(&e.kids, &mut t);
                        let t = t.trim();
                        out.push_str(&"#".repeat(lvl));
                        out.push(' ');
                        out.push_str(t);
                        out.push_str("\n\n");
                    }
                    "p" => {
                        let mut t = String::new();
                        inline_md(&e.kids, &mut t);
                        let t = t.trim();
                        if !t.is_empty() {
                            out.push_str(t);
                            out.push_str("\n\n");
                        }
                    }
                    "ul" | "ol" => list_md(&e.kids, sem == "ol", e.attr("start"), out),
                    "li" => {
                        // A stray li (not inside a list) is emitted as a list item
                        item_md(e, "-", out);
                    }
                    "blockquote" => {
                        let mut inner = String::new();
                        blocks_md(&e.kids, &mut inner);
                        for line in inner.trim_end().lines() {
                            if line.is_empty() {
                                out.push_str(">\n");
                            } else {
                                out.push_str("> ");
                                out.push_str(line);
                                out.push('\n');
                            }
                        }
                        out.push('\n');
                    }
                    "pre" => pre_md(e, out),
                    "table" => table_md(e, out),
                    // tr/td/th outside a table: fall back to inline text
                    "tr" | "td" | "th" => {
                        let mut t = String::new();
                        inline_md(&e.kids, &mut t);
                        let t = t.trim();
                        if !t.is_empty() {
                            out.push_str(t);
                            out.push_str("\n\n");
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    flush(&mut run, out);
}

/// Whether an unknown container splits by block (a browser implicitly breaks
/// paragraphs around `div` etc.).
fn elem_looks_block(tag: &str) -> bool {
    matches!(
        tag,
        "div"
            | "section"
            | "article"
            | "main"
            | "header"
            | "footer"
            | "nav"
            | "aside"
            | "figure"
            | "figcaption"
            | "address"
            | "fieldset"
            | "form"
            | "hr"
            | "center"
            | "dl"
            | "dt"
            | "dd"
            | "dir"
            | "menu"
            | "caption"
    )
}

/// Ordered/unordered list → Markdown (nested lists recurse through
/// `blocks_md`, and the marker indent is backfilled).
fn list_md(kids: &[Node], ordered: bool, start: Option<&str>, out: &mut String) {
    let mut n = start
        .and_then(|s| s.trim().parse::<usize>().ok())
        .unwrap_or(1);
    for k in kids {
        match k {
            Node::Elem(e) if e.tag == "li" => {
                let marker = if ordered {
                    format!("{n}. ")
                } else {
                    "- ".to_string()
                };
                item_md(e, &marker, out);
                n += 1;
            }
            // An li wrapper (the lenient case of a div wrapping li): drill down
            // to find the li
            Node::Elem(e) if semantic(&e.tag).is_none() => list_md(&e.kids, ordered, None, out),
            _ => {}
        }
    }
}

/// One list item: the first line takes the marker, continuation lines are
/// indented by the marker width (nested lists are already emitted at the
/// inner level, so indenting uniformly here lines them up).
fn item_md(e: &Elem, marker: &str, out: &mut String) {
    let mut body = String::new();
    blocks_md(&e.kids, &mut body);
    let body = body.trim_end();
    if body.is_empty() {
        out.push_str(marker);
        out.push('\n');
        return;
    }
    for (i, line) in body.lines().enumerate() {
        if i == 0 {
            out.push_str(marker);
            out.push_str(line);
            out.push('\n');
        } else if line.is_empty() {
            out.push('\n');
        } else {
            out.push_str(&" ".repeat(marker.chars().count()));
            out.push_str(line);
            out.push('\n');
        }
    }
}

/// Fenced code block: the content is taken as plain text (whitespace not
/// collapsed), and the fence is longer than the longest backtick run inside
/// the content.
fn pre_md(e: &Elem, out: &mut String) {
    let mut code = String::new();
    text_content(&e.kids, &mut code);
    let code = code.strip_prefix('\n').unwrap_or(&code);
    let info = first_code_class(&e.kids).unwrap_or_default();
    let max_ticks = code
        .split('\n')
        .map(|l| l.chars().filter(|c| *c == '`').count())
        .max()
        .unwrap_or(0);
    let fence = "`".repeat(max_ticks.max(2) + 1);
    out.push_str(&fence);
    out.push_str(&info);
    out.push('\n');
    out.push_str(code.trim_end_matches('\n'));
    out.push('\n');
    out.push_str(&fence);
    out.push_str("\n\n");
}

/// The info string of a `<pre><code class="language-xxx">`.
fn first_code_class(kids: &[Node]) -> Option<String> {
    for k in kids {
        if let Node::Elem(e) = k {
            if e.tag == "code" {
                if let Some(cls) = e.attr("class") {
                    for w in cls.split_whitespace() {
                        if let Some(lang) = w.strip_prefix("language-") {
                            return Some(lang.to_string());
                        }
                    }
                }
                return None;
            }
        }
    }
    None
}

/// Plain-text content (for pre/code): not collapsed, not escaped, unwrapped
/// but keeping the text.
fn text_content(kids: &[Node], out: &mut String) {
    for k in kids {
        match k {
            Node::Text(t) => out.push_str(t),
            Node::Elem(e) => {
                if e.tag == "br" {
                    out.push('\n');
                } else {
                    text_content(&e.kids, out);
                }
            }
        }
    }
}

/// Table → GFM table: the first row is the header, and alignment comes from
/// the first row's cells (`align` attribute or
/// `style="text-align:…"`).
fn table_md(e: &Elem, out: &mut String) {
    let mut rows: Vec<Vec<String>> = Vec::new();
    let mut aligns: Vec<u8> = Vec::new();
    collect_rows(&e.kids, &mut rows, &mut aligns);
    if rows.is_empty() {
        return;
    }
    let cols = rows.iter().map(|r| r.len()).max().unwrap_or(0);
    if cols == 0 {
        return;
    }
    for r in &mut rows {
        r.resize(cols, String::new());
    }
    aligns.resize(cols, 0);
    let push = |r: &[String], out: &mut String| {
        out.push_str("| ");
        out.push_str(&r.join(" | "));
        out.push_str(" |\n");
    };
    push(&rows[0], out);
    let sep: Vec<String> = aligns
        .iter()
        .map(|a| match a {
            1 => ":---".to_string(),
            2 => ":---:".to_string(),
            3 => "---:".to_string(),
            _ => "---".to_string(),
        })
        .collect();
    push(&sep, out);
    for r in &rows[1..] {
        push(r, out);
    }
    out.push('\n');
}

/// Recursively collect rows (through thead/tbody/tfoot and unwrapped
/// containers).
fn collect_rows(kids: &[Node], rows: &mut Vec<Vec<String>>, aligns: &mut Vec<u8>) {
    for k in kids {
        let Node::Elem(e) = k else { continue };
        if e.tag != "tr" {
            collect_rows(&e.kids, rows, aligns);
            continue;
        }
        let mut cells = Vec::new();
        for c in &e.kids {
            let Node::Elem(c) = c else { continue };
            if !matches!(c.tag.as_str(), "td" | "th") {
                continue;
            }
            if rows.is_empty() {
                // The first row decides alignment
                aligns.push(cell_align(c));
            }
            let mut t = String::new();
            inline_md(&c.kids, &mut t);
            cells.push(escape_cell(t.trim()));
        }
        if !cells.is_empty() {
            rows.push(cells);
        }
    }
}

/// Cell alignment: the `align` attribute or the style's `text-align` (encoded
/// like `Td.align`: 0=default 1=left 2=center 3=right).
fn cell_align(c: &Elem) -> u8 {
    let from_attr = c.attr("align").map(ToOwned::to_owned);
    let from_style = c.attr("style").and_then(|s| {
        let s = s.to_ascii_lowercase().replace(' ', "");
        s.split(';')
            .find_map(|p| p.strip_prefix("text-align:").map(ToOwned::to_owned))
    });
    let a = from_attr.or(from_style).unwrap_or_default();
    match a.trim().to_ascii_lowercase().as_str() {
        "left" | "start" => 1,
        "center" | "middle" => 2,
        "right" | "end" => 3,
        _ => 0,
    }
}

/// Inline sequence → Markdown.
fn inline_md(kids: &[Node], out: &mut String) {
    for k in kids {
        inline_node(k, out);
    }
}

/// Inline emission of a single node.
fn inline_node(n: &Node, out: &mut String) {
    match n {
        Node::Text(t) => {
            let t = collapse_ws(t);
            out.push_str(&escape_text(&t));
        }
        Node::Elem(e) => match semantic(&e.tag) {
            Some("strong") => wrap_inline(&e.kids, "**", out),
            Some("em") => wrap_inline(&e.kids, "*", out),
            Some("del") => wrap_inline(&e.kids, "~~", out),
            Some("code") => {
                let mut t = String::new();
                text_content(&e.kids, &mut t);
                let t = t.trim();
                // Same rule as the document emitter: the longest backtick run
                // in the content plus 1, padded when the content starts or ends
                // with a backtick or a space. The local "count of `` pairs"
                // version emitted `` `a`b` `` for content "a`b", which reparses
                // as code("a") plus the text "b`" — the pasted code lost its
                // tail and grew a stray backtick.
                let fence = crate::md::inline_code_fence(t);
                let pad = crate::md::code_span_pad(t);
                out.push_str(&fence);
                if pad {
                    out.push(' ');
                }
                out.push_str(t);
                if pad {
                    out.push(' ');
                }
                out.push_str(&fence);
            }
            Some("a") => {
                let mut t = String::new();
                inline_md(&e.kids, &mut t);
                let t = t.trim().to_string();
                match e.attr("href").and_then(safe_url) {
                    Some(u) => {
                        let label = if t.is_empty() { u.clone() } else { t };
                        out.push('[');
                        out.push_str(&label);
                        out.push_str("](");
                        if u.contains([' ', '(', ')']) {
                            out.push('<');
                            out.push_str(&u);
                            out.push('>');
                        } else {
                            out.push_str(&u);
                        }
                        out.push(')');
                    }
                    None => out.push_str(&t),
                }
            }
            Some("img") => {
                if let Some(src) = e.attr("src").and_then(safe_url) {
                    let alt = escape_cell(e.attr("alt").unwrap_or(""));
                    out.push_str("![");
                    out.push_str(&alt);
                    out.push_str("](");
                    if src.contains([' ', '(', ')']) {
                        out.push('<');
                        out.push_str(&src);
                        out.push('>');
                    } else {
                        out.push_str(&src);
                    }
                    out.push(')');
                }
            }
            Some("br") => out.push('\n'),
            Some("h") | Some("p") | Some("li") | Some("blockquote") | Some("pre") => {
                // A block-level element in an inline context: its content joins
                // this line, separated by a space
                inline_md(&e.kids, out);
                out.push(' ');
            }
            Some("ul") | Some("ol") | Some("table") | Some("tr") | Some("td") | Some("th")
            | Some("tbody") => {
                // Structures unsupported in an inline context: fall back to text
                let mut t = String::new();
                inline_md(&e.kids, &mut t);
                out.push_str(&escape_cell(t.trim()));
                out.push(' ');
            }
            _ => inline_md(&e.kids, out),
        },
    }
}

fn wrap_inline(kids: &[Node], marker: &str, out: &mut String) {
    let mut inner = String::new();
    inline_md(kids, &mut inner);
    let inner = inner.trim();
    if inner.is_empty() {
        return;
    }
    out.push_str(marker);
    out.push_str(inner);
    out.push_str(marker);
}

#[cfg(test)]
mod tests {
    use super::html_to_markdown;

    #[test]
    fn bold_becomes_strong() {
        assert_eq!(html_to_markdown("<p>a <b>b</b></p>"), "a **b**");
        assert_eq!(html_to_markdown("<strong>x</strong>"), "**x**");
        assert_eq!(html_to_markdown("<i>x</i>"), "*x*");
        assert_eq!(html_to_markdown("<del>x</del>"), "~~x~~");
    }

    #[test]
    fn script_style_stripped_with_content() {
        let md = html_to_markdown(
            "<p>x</p><script>alert(1)</script><style>p{color:red}</style><svg><rect/></svg>",
        );
        assert_eq!(md, "x");
        assert!(!md.contains("alert"));
        assert!(!md.contains("color"));
    }

    #[test]
    fn comments_stripped() {
        let md = html_to_markdown("<!--[if gte mso 9]><xml><o:x></o:x></xml><![endif]--><p>a</p>");
        assert_eq!(md, "a");
    }

    #[test]
    fn on_attrs_and_style_never_survive() {
        let md = html_to_markdown(r#"<p class="x" style="color:red" onclick="evil()">t</p>"#);
        assert_eq!(md, "t");
        assert!(!md.contains("onclick"));
        assert!(!md.contains("evil"));
    }

    #[test]
    fn text_lt_escaped_no_html_revival() {
        // A `<` in the text is escaped to `\<` — the conversion result is never
        // re-upgraded into HTML by md::parse
        let md = html_to_markdown("<p>&lt;img src=x onerror=alert(1)&gt;</p>");
        assert_eq!(md, "\\<img src=x onerror=alert(1)\\>");
        assert!(!md.starts_with('<'));
    }

    #[test]
    fn entities_decoded() {
        assert_eq!(
            html_to_markdown("<p>a &amp; b &lt; c &#65; &#x42;</p>"),
            "a \\& b \\< c A B"
        );
    }

    #[test]
    fn heading_and_paragraphs() {
        assert_eq!(
            html_to_markdown("<h2>T</h2><p>p1</p><p>p2</p>"),
            "## T\n\np1\n\np2"
        );
    }

    #[test]
    fn table_to_gfm() {
        let md = html_to_markdown(
            "<table><tr><th>a</th><th>b</th></tr><tr><td>1</td><td>2</td></tr></table>",
        );
        assert_eq!(md, "| a | b |\n| --- | --- |\n| 1 | 2 |");
    }

    #[test]
    fn table_align_columns() {
        let md = html_to_markdown(
            "<table><tr><td align=\"left\">l</td><td style=\"text-align:center\">c</td><td align=\"right\">r</td></tr>\
             <tr><td>1</td><td>2</td><td>3</td></tr></table>",
        );
        assert_eq!(md, "| l | c | r |\n| :--- | :---: | ---: |\n| 1 | 2 | 3 |");
    }

    #[test]
    fn table_cell_pipe_escaped() {
        let md = html_to_markdown("<table><tr><td>a|b</td></tr></table>");
        assert_eq!(md, "| a\\|b |\n| --- |");
    }

    #[test]
    fn lists_with_nesting() {
        let md = html_to_markdown("<ul><li>a<ul><li>b</li></ul></li></ul>");
        assert_eq!(md, "- a\n\n  - b");
        let md = html_to_markdown("<ol start=\"3\"><li>x</li><li>y</li></ol>");
        assert_eq!(md, "3. x\n4. y");
    }

    #[test]
    fn blockquote_flows() {
        assert_eq!(html_to_markdown("<blockquote><p>q</p></blockquote>"), "> q");
    }

    #[test]
    fn pre_code_fenced_raw() {
        // pre content is entity-decoded into source text (HTML semantics); inside
        // the fence nothing is escaped further
        let md =
            html_to_markdown("<pre><code class=\"language-rust\">let a = 1 &lt; 2;</code></pre>");
        assert_eq!(md, "```rust\nlet a = 1 < 2;\n```");
    }

    #[test]
    fn link_and_image() {
        assert_eq!(
            html_to_markdown(r#"<a href="https://x.y">t</a>"#),
            "[t](https://x.y)"
        );
        assert_eq!(
            html_to_markdown(r#"<img src="https://x/y.png" alt="d">"#),
            "![d](https://x/y.png)"
        );
    }

    #[test]
    fn unsafe_urls_dropped() {
        assert_eq!(
            html_to_markdown(r#"<a href="javascript:alert(1)">t</a>"#),
            "t"
        );
        assert_eq!(
            html_to_markdown(r#"<a href=" java&#10;script:x ">t</a>"#),
            "t"
        );
        assert_eq!(html_to_markdown(r#"<img src="data:text/html,x">"#), "");
    }

    #[test]
    fn span_unwrapped_whitespace_collapsed() {
        assert_eq!(
            html_to_markdown("<div><span style=\"x\">a   b</span></div><div>c</div>"),
            "a b\n\nc"
        );
    }

    #[test]
    fn br_becomes_newline() {
        assert_eq!(html_to_markdown("<p>a<br>b</p>"), "a\nb");
    }

    #[test]
    fn unclosed_tags_tolerated() {
        // Known boundary: bold inside a word (`a**b**`) cannot be re-parsed as
        // emphasis in CommonMark (`**` cannot open emphasis mid-word) — a
        // limitation of Markdown's own expressiveness; the output stays literal.
        assert_eq!(html_to_markdown("<p>a<b>b"), "a**b**");
        assert_eq!(html_to_markdown("<p>a <b>b</b>"), "a **b**");
    }
}
