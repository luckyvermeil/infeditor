//! Document model: the block tree. The single source of truth; the DOM is
//! purely derived (design doc §3).
//!
//! Blocks are "leaf blocks": list items / quote paragraphs / table cells are
//! flattened straight into `Doc::blocks`, and rendering and coordinate
//! mapping are unified as "one block = one block-level element carrying
//! `data-bid` = one inline root". Container information (list grouping, table
//! rows/columns) is recovered from **the types and fields of adjacent
//! blocks** — `render` and `md::emit` use the same grouping rules (see
//! `render::content_html` / `md::emit`).
//!
//! Quote ownership: block nesting does not go into the model; the quote shell
//! is recorded on the block — a leaf block is placed inside
//! [`Block::Quoted`] (with its nesting depth), emit adds a `> ` prefix line by
//! line by depth, and render wraps it in a quote-styled container; a
//! pure-inline paragraph at depth 1 keeps the [`Block::Quote`] compatible
//! shape.
//!
//! Every offset is a UTF-8 byte offset into the block's **plain text** — the
//! same metric as parley's byte index, avoiding char/byte conversions (design
//! doc §4.1).

/// Inline content. The `plain_text` order is the `Pos.off` metric order.
#[derive(Debug, Clone, PartialEq)]
pub enum Inline {
    Text(String),
    /// Inline code. Its inner text does not participate in emphasis parsing and
    /// renders as `<code>`.
    Code(String),
    Strong(Vec<Inline>),
    Em(Vec<Inline>),
    Del(Vec<Inline>),
    /// Link. Renders `<a>`; Ctrl+click opens it.
    Link {
        url: String,
        inlines: Vec<Inline>,
    },
    /// Image. `alt` participates in the plain text (the caret can land on it);
    /// renders `<img>`.
    Image {
        url: String,
        alt: String,
    },
    /// Math formula (`$…$` inline / `$$…$$` block-level, distinguished by
    /// `display`). Plain text = the LaTeX source; renders SVG (the caret
    /// entering shows the source). The edit view renders both as inline SVG;
    /// `display` drives the preview's block-level centered form.
    Math {
        src: String,
        display: bool,
    },
    /// Soft line break (a break inside a paragraph; md source = a single
    /// newline, inserted by Shift+Enter). Plain text = `"\n"` (1 byte, the
    /// caret can land before/after it); emit outputs a single `\n`, parse
    /// restores it.
    SoftBreak,
    /// Hard line break (md source = two trailing spaces or a backslash +
    /// newline). Plain text same as [`Inline::SoftBreak`] = `"\n"`; emit
    /// outputs the two-trailing-spaces form, parse restores it.
    HardBreak,
}

/// Block. All are leaf blocks.
#[derive(Debug, Clone, PartialEq)]
pub enum Block {
    Para(Vec<Inline>),
    /// ATX heading. `level` is 1..=6.
    Head {
        level: u8,
        inlines: Vec<Inline>,
    },
    /// Fenced code block. `text` excludes the fence lines and has no trailing
    /// newline.
    Code {
        lang: Option<String>,
        text: String,
    },
    /// List item. Consecutive items with the same `ordered`/`level` form one
    /// list (a nested list is a run of adjacent items with a larger `level`,
    /// rendered as a nested `<ul>`/`<ol>`); `num` is the ordered-list number;
    /// when `task` = Some(checked) it renders as a task-list checkbox.
    Li {
        ordered: bool,
        num: u64,
        level: u8,
        task: Option<bool>,
        inlines: Vec<Inline>,
    },
    /// A paragraph inside a quote (the compatible shape = a pure-inline
    /// paragraph at one quote level, effective depth 1). Consecutive `Quote`s
    /// each become their own blockquote; **block-level** content inside a
    /// quote (lists / code blocks / tables / headings …) goes through the
    /// [`Block::Quoted`] ownership mechanism.
    Quote(Vec<Inline>),
    /// Table cell. Consecutive `Td`s (same `table_seq`) form one table; `row`
    /// decides the `<tr>` boundary; `align` is 0=default 1=left 2=center
    /// 3=right. `table_seq` is assigned at parse time (unique within one
    /// parse) and stays unchanged during editing until the table boundary is
    /// broken.
    Td {
        table_seq: u64,
        row: usize,
        align: u8,
        inlines: Vec<Inline>,
    },
    /// GFM HTML block (Tag::HtmlBlock): the raw HTML source is kept as one
    /// block. The preview exit **passes it through verbatim** (GFM semantics:
    /// block-level HTML is not escaped); the edit view shows it as source text
    /// (a monospace block like a code block) and serialization writes it back
    /// unchanged — a lossless round trip. Inline HTML (InlineHtml) is not in
    /// this category: it is still escaped as plain text.
    Html {
        text: String,
    },
    Hr,
    /// Quote-ownership wrapper: a leaf block placed inside a quote shell,
    /// `depth` = quote nesting depth (1 = a single `>`, 2 = `> >`). The block
    /// is still flattened in `Doc::blocks` (caret/edit addressing by block
    /// index is unchanged); emit adds a `> ` prefix line by line by depth —
    /// the quote shell is no longer lost just because block-level content is
    /// flattened (the old defect where one open-save cycle corrupted the
    /// structure). The inner block is an ordinary leaf block
    /// ([`wrap_quote`] normalizes so `Quoted`/`Quote` never nest).
    Quoted {
        depth: u8,
        inner: Box<Block>,
    },
}

/// Place a quote shell on a leaf block by quote depth. Depth 0 is returned
/// unchanged; a pure-inline paragraph at depth 1 is normalized into the
/// [`Block::Quote`] compatible shape (so the round-trip form of a simple
/// `> one line` is unchanged); everything else is wrapped in
/// [`Block::Quoted`]. A block already in a shell is not wrapped a second time
/// (defensive: callers pass leaf blocks).
pub fn wrap_quote(depth: u8, b: Block) -> Block {
    if matches!(b, Block::Quote(_) | Block::Quoted { .. }) || depth == 0 {
        return b;
    }
    if depth == 1 {
        if let Block::Para(inlines) = b {
            return Block::Quote(inlines);
        }
    }
    Block::Quoted {
        depth,
        inner: Box::new(b),
    }
}

impl Block {
    /// The substantive block: strip the quote-ownership wrapper
    /// ([`Block::Quoted`], recursively). Type grouping / match semantics look
    /// at this (the quote shell only affects the emit prefix and render style).
    pub fn core(&self) -> &Block {
        match self {
            Block::Quoted { inner, .. } => inner.core(),
            other => other,
        }
    }

    /// Mutable version of [`Block::core`] (editing penetrates the quote shell
    /// to change content).
    pub fn core_mut(&mut self) -> &mut Block {
        match self {
            Block::Quoted { inner, .. } => inner.core_mut(),
            other => other,
        }
    }

    /// Quote-ownership depth (0 = top level). [`Block::Quote`] is the
    /// compatible shape of "a pure-inline paragraph inside one quote level",
    /// effective depth 1; [`Block::Quoted`] records an explicit depth.
    pub fn quote_depth(&self) -> u8 {
        match self {
            Block::Quote(_) => 1,
            Block::Quoted { depth, .. } => *depth,
            _ => 0,
        }
    }

    /// True only for a table cell (penetrates the quote shell).
    pub fn is_td(&self) -> bool {
        matches!(self.core(), Block::Td { .. })
    }

    /// A table cell's `table_seq` (`None` for a non-cell; penetrates the quote
    /// shell).
    pub fn table_seq(&self) -> Option<u64> {
        match self.core() {
            Block::Td { table_seq, .. } => Some(*table_seq),
            _ => None,
        }
    }

    /// A list item's indent level (0 for a non-list block; penetrates the
    /// quote shell).
    pub fn list_level(&self) -> u8 {
        match self.core() {
            Block::Li { level, .. } => *level,
            _ => 0,
        }
    }
}

/// The block-tree document.
#[derive(Debug, Clone, PartialEq)]
pub struct Doc {
    pub blocks: Vec<Block>,
    /// The next table's `table_seq` (assigned by parse; not bumped during edits
    /// — the same seq means the same table).
    pub next_table_seq: u64,
}

impl Default for Doc {
    /// Hand-written Default: goes through [`Doc::new`]'s invariant (at least
    /// one block). `derive(Default)` would produce `blocks: []`, and downstream
    /// code (e.g. `delete_back`) would panic indexing `blocks[focus.block]`
    /// directly.
    fn default() -> Self {
        Doc::new(vec![])
    }
}

impl Doc {
    pub fn new(blocks: Vec<Block>) -> Self {
        // Invariant: a document has at least one block (an empty document = one
        // empty paragraph). Caret/edit commands all assume
        // `blocks[focus.block]` exists.
        let mut blocks = blocks;
        if blocks.is_empty() {
            blocks.push(Block::Para(vec![]));
        }
        Self {
            blocks,
            next_table_seq: 0,
        }
    }

    pub fn block_count(&self) -> usize {
        self.blocks.len()
    }
}

impl Inline {
    /// Append the plain text (into `out`). This is what `Pos.off` measures.
    pub fn push_plain(&self, out: &mut String) {
        match self {
            Inline::Text(s) | Inline::Code(s) | Inline::Math { src: s, .. } => out.push_str(s),
            Inline::Image { alt, .. } => out.push_str(alt),
            Inline::Strong(c)
            | Inline::Em(c)
            | Inline::Del(c)
            | Inline::Link { inlines: c, .. } => {
                for i in c {
                    i.push_plain(out);
                }
            }
            // Line breaks count in the plain text (1 byte): same bytes as the
            // render text stream (geom mapping)
            Inline::SoftBreak | Inline::HardBreak => out.push('\n'),
        }
    }

    pub fn plain(&self) -> String {
        let mut s = String::new();
        self.push_plain(&mut s);
        s
    }

    /// Byte length of [`Inline::plain`] (no allocation; hot path).
    pub fn plain_len(&self) -> usize {
        match self {
            Inline::Text(s) | Inline::Code(s) | Inline::Math { src: s, .. } => s.len(),
            Inline::Image { alt, .. } => alt.len(),
            Inline::Strong(c)
            | Inline::Em(c)
            | Inline::Del(c)
            | Inline::Link { inlines: c, .. } => c.iter().map(Inline::plain_len).sum(),
            Inline::SoftBreak | Inline::HardBreak => 1,
        }
    }
}

impl Block {
    /// Plain-byte ranges of the **math formulas** in a block (in order of
    /// appearance, through Strong/Em/Del/Link containers and the quote shell).
    /// A range = the formula's source position in `plain_text`, used for click
    /// hit-testing (`geom::math_at`) and atomic-move decisions.
    pub fn math_spans(&self) -> Vec<(usize, usize)> {
        let mut out = Vec::new();
        let mut acc = 0usize;
        match self {
            Block::Para(i)
            | Block::Head { inlines: i, .. }
            | Block::Li { inlines: i, .. }
            | Block::Quote(i)
            | Block::Td { inlines: i, .. } => walk_math_spans(i, &mut acc, &mut out),
            Block::Quoted { inner, .. } => return inner.math_spans(),
            Block::Code { .. } | Block::Html { .. } | Block::Hr => {}
        }
        out
    }

    /// Is `off` one of the **two edges** of a display formula (`$$…$$`) that *is*
    /// this block?
    ///
    /// Such a block has no caret position besides the atom's own edges unless the
    /// user explicitly steps inside it (a double-click lands in the source), so
    /// the edges keep the typeset form: expanding there would leave the block
    /// permanently in source — a document that starts with a display formula
    /// would open as raw LaTeX, the block could never be read, and `Home`/`End`
    /// would address the LaTeX lines instead of the block (`Enter` then splits the
    /// block at `$$`). The atom's interior is the edit state and does expand (see
    /// [`crate::reveal`]).
    ///
    /// An *inline* formula alone in a paragraph is not this case: `$x$` is written
    /// mid-sentence, so its source is the text the user edits.
    pub fn is_display_block_edge(&self, off: usize) -> bool {
        if !self.has_math() {
            return false;
        }
        let len = self.plain_len();
        self.display_math_span_at(off)
            .is_some_and(|(s, e)| s == 0 && e >= len && (off == s || off == e))
    }

    /// Plain-byte range of the **display** formula (`$$…$$`) that contains `off`,
    /// if there is one (no allocation; the edge rule needs to tell a display
    /// formula from an inline one at the same offsets).
    fn display_math_span_at(&self, off: usize) -> Option<(usize, usize)> {
        let mut acc = 0usize;
        let mut out = None;
        match self {
            Block::Para(i)
            | Block::Head { inlines: i, .. }
            | Block::Li { inlines: i, .. }
            | Block::Quote(i)
            | Block::Td { inlines: i, .. } => walk_display_math_at(i, &mut acc, off, &mut out),
            Block::Quoted { inner, .. } => return inner.display_math_span_at(off),
            Block::Code { .. } | Block::Html { .. } | Block::Hr => {}
        }
        out
    }

    /// Plain-byte ranges of the **image alts** in a block (same walk and
    /// coordinate system as [`Block::math_spans`]). Allocates nothing when the
    /// block has no image.
    pub fn image_spans(&self) -> Vec<(usize, usize)> {
        let mut out = Vec::new();
        let mut acc = 0usize;
        match self {
            Block::Para(i)
            | Block::Head { inlines: i, .. }
            | Block::Li { inlines: i, .. }
            | Block::Quote(i)
            | Block::Td { inlines: i, .. } => walk_image_spans(i, &mut acc, &mut out),
            Block::Quoted { inner, .. } => return inner.image_spans(),
            Block::Code { .. } | Block::Html { .. } | Block::Hr => {}
        }
        out
    }

    /// Whether the block contains math (no allocation; a quick short-circuit
    /// before click hit-testing).
    pub fn has_math(&self) -> bool {
        match self {
            Block::Para(i)
            | Block::Head { inlines: i, .. }
            | Block::Li { inlines: i, .. }
            | Block::Quote(i)
            | Block::Td { inlines: i, .. } => inlines_have_math(i),
            Block::Quoted { inner, .. } => inner.has_math(),
            Block::Code { .. } | Block::Html { .. } | Block::Hr => false,
        }
    }

    /// The block's plain text. `Pos.off` is relative to it; an empty block
    /// yields `""` (the render layer is responsible for the placeholder).
    pub fn plain_text(&self) -> String {
        match self {
            Block::Para(inlines)
            | Block::Head { inlines, .. }
            | Block::Li { inlines, .. }
            | Block::Quote(inlines)
            | Block::Td { inlines, .. } => {
                let mut s = String::new();
                for i in inlines {
                    i.push_plain(&mut s);
                }
                s
            }
            Block::Code { text, .. } => text.clone(),
            Block::Html { text } => text.clone(),
            Block::Hr => String::new(),
            // The quote shell carries no text: plain text = the inner block
            // (recursive through)
            Block::Quoted { inner, .. } => inner.plain_text(),
        }
    }

    /// Byte length of [`Self::plain_text`] **without allocating** — used to clamp a
    /// stale caret on every command entry (a `plain_text()` per keystroke would be a
    /// whole-block copy).
    pub fn plain_len(&self) -> usize {
        match self {
            Block::Para(inlines)
            | Block::Head { inlines, .. }
            | Block::Li { inlines, .. }
            | Block::Quote(inlines)
            | Block::Td { inlines, .. } => inlines.iter().map(Inline::plain_len).sum(),
            Block::Code { text, .. } | Block::Html { text } => text.len(),
            Block::Hr => 0,
            Block::Quoted { inner, .. } => inner.plain_len(),
        }
    }
}

/// Collect the byte ranges of inline math formulas within the block's plain
/// text (through inline containers).
fn walk_math_spans(inl: &[Inline], acc: &mut usize, out: &mut Vec<(usize, usize)>) {
    for i in inl {
        match i {
            Inline::Math { src, .. } => {
                out.push((*acc, *acc + src.len()));
                *acc += src.len();
            }
            Inline::Strong(c)
            | Inline::Em(c)
            | Inline::Del(c)
            | Inline::Link { inlines: c, .. } => walk_math_spans(c, acc, out),
            other => *acc += other.plain_len(),
        }
    }
}

/// Same walk as [`walk_math_spans`], collecting only the **display** formula that
/// contains `off` (short-circuits; no allocation).
fn walk_display_math_at(
    inl: &[Inline],
    acc: &mut usize,
    off: usize,
    out: &mut Option<(usize, usize)>,
) {
    for i in inl {
        match i {
            Inline::Math { src, display: true } if *acc <= off && off <= *acc + src.len() => {
                *out = Some((*acc, *acc + src.len()));
                return;
            }
            Inline::Math { src, .. } => *acc += src.len(),
            Inline::Strong(c)
            | Inline::Em(c)
            | Inline::Del(c)
            | Inline::Link { inlines: c, .. } => {
                walk_display_math_at(c, acc, off, out);
                if out.is_some() {
                    return;
                }
            }
            other => *acc += other.plain_len(),
        }
    }
}

/// Same walk as [`walk_math_spans`], collecting the image alts instead.
fn walk_image_spans(inl: &[Inline], acc: &mut usize, out: &mut Vec<(usize, usize)>) {
    for i in inl {
        match i {
            Inline::Image { alt, .. } => {
                out.push((*acc, *acc + alt.len()));
                *acc += alt.len();
            }
            Inline::Strong(c)
            | Inline::Em(c)
            | Inline::Del(c)
            | Inline::Link { inlines: c, .. } => walk_image_spans(c, acc, out),
            other => *acc += other.plain_len(),
        }
    }
}

/// Whether an inline contains math (short-circuits early, no range table
/// built).
fn inlines_have_math(inl: &[Inline]) -> bool {
    inl.iter().any(|i| match i {
        Inline::Math { .. } => true,
        Inline::Strong(c) | Inline::Em(c) | Inline::Del(c) | Inline::Link { inlines: c, .. } => {
            inlines_have_math(c)
        }
        _ => false,
    })
}

/// Logical selection: anchor (the fixed end) + focus (the moving end).
/// `focus` is the caret position. anchor == focus = a collapsed caret.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sel {
    pub anchor: Pos,
    pub focus: Pos,
}

impl Sel {
    pub fn caret(pos: Pos) -> Self {
        Self {
            anchor: pos,
            focus: pos,
        }
    }

    pub fn collapsed(&self) -> bool {
        self.anchor == self.focus
    }

    /// Normalized range (start ≤ end).
    pub fn range(&self) -> (Pos, Pos) {
        if self.anchor.key() <= self.focus.key() {
            (self.anchor, self.focus)
        } else {
            (self.focus, self.anchor)
        }
    }

    /// The covered block-index range (inclusive of both ends).
    pub fn block_range(&self) -> std::ops::RangeInclusive<usize> {
        let (a, b) = self.range();
        a.block..=b.block
    }
}

/// Logical caret/position: block index + plain-text byte offset within the
/// block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pos {
    pub block: usize,
    pub off: usize,
}

impl Pos {
    pub fn new(block: usize, off: usize) -> Self {
        Self { block, off }
    }

    /// Clamp to the document's range (the document may have changed; a stale
    /// `Pos` must be clamped before use).
    /// `off` is snapped down to the nearest UTF-8 character boundary: a stale
    /// offset can land in the middle of a multibyte character (CJK/emoji), and
    /// the downstream `&plain[..off]` slice would panic (E1).
    pub fn clamped(self, doc: &Doc) -> Self {
        let block = self.block.min(doc.blocks.len().saturating_sub(1));
        let plain = doc
            .blocks
            .get(block)
            .map(|b| b.plain_text())
            .unwrap_or_default();
        Self {
            block,
            off: floor_boundary(&plain, self.off),
        }
    }

    /// Lexicographic comparison by (block, off).
    fn key(self) -> (usize, usize) {
        (self.block, self.off)
    }
}

/// Snap a byte offset down to the nearest UTF-8 character boundary (also
/// clamped to `s.len()`).
///
/// Every byte offset that comes from outside the model (a click position, a
/// stale caret, an IME commit) is **untrusted**: it can land inside a
/// multibyte character, and slicing/inserting there panics with
/// "byte index N is not a char boundary". Normalize at the point of use —
/// this is the single source of truth for that normalization.
pub fn floor_boundary(s: &str, off: usize) -> usize {
    let mut off = off.min(s.len());
    while off > 0 && !s.is_char_boundary(off) {
        off -= 1;
    }
    off
}

/// Snap a byte offset up to the nearest UTF-8 character boundary (clamped to
/// `s.len()`). Used where dropping a partially covered character would lose
/// data (keeping a suffix / a selection tail).
pub fn ceil_boundary(s: &str, off: usize) -> usize {
    let mut off = off.min(s.len());
    while off < s.len() && !s.is_char_boundary(off) {
        off += 1;
    }
    off
}

/// Clamp a possibly inverted / out-of-bounds / mid-character byte span into a
/// valid, ordered slice range of `s`: the start is floored, the end is ceiled.
///
/// Spans that define a *selection* must never be narrowed (a selection ending
/// mid-character counts the whole character), so the end goes up.
pub fn clamp_span(s: &str, a: usize, b: usize) -> (usize, usize) {
    let a = floor_boundary(s, a);
    let b = ceil_boundary(s, b).max(a);
    (a, b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_concatenates_all_inline_kinds() {
        let b = Block::Para(vec![
            Inline::Text("a".into()),
            Inline::Strong(vec![Inline::Text("b".into()), Inline::Code("c".into())]),
            Inline::Link {
                url: "u".into(),
                inlines: vec![Inline::Text("L".into())],
            },
            Inline::Image {
                url: "i".into(),
                alt: "IMG".into(),
            },
            Inline::Math {
                src: "x+y".into(),
                display: false,
            },
        ]);
        assert_eq!(b.plain_text(), "abcLIMGx+y");
    }

    /// Formula ranges share the source of truth with plain offsets: they pass
    /// through Strong/Link, and the text / image-alt bytes between ranges are
    /// all counted (geom's click hit-testing uses this directly to compute a
    /// `Pos`).
    #[test]
    fn math_spans_follow_plain_offsets() {
        let b = Block::Para(vec![
            Inline::Text("a".into()),
            Inline::Math {
                src: "x+y".into(),
                display: false,
            },
            Inline::Strong(vec![
                Inline::Text("b".into()),
                Inline::Math {
                    src: "z".into(),
                    display: true,
                },
            ]),
            Inline::Image {
                url: "u".into(),
                alt: "im".into(),
            },
            Inline::Math {
                src: "w".into(),
                display: false,
            },
        ]);
        assert_eq!(b.plain_text(), "ax+ybzimw");
        assert_eq!(b.math_spans(), vec![(1, 4), (5, 6), (8, 9)]);
        assert!(b.has_math());
        // Every range lands on a character boundary of plain (downstream can
        // slice safely)
        let plain = b.plain_text();
        for (s, e) in b.math_spans() {
            assert!(plain.is_char_boundary(s) && plain.is_char_boundary(e));
        }
        // Quote-shell penetration; quick verdict for a block without formulas
        let q = wrap_quote(2, b);
        assert!(q.has_math());
        assert_eq!(q.math_spans().len(), 3);
        assert!(!Block::Hr.has_math());
        assert!(!Block::Code {
            lang: None,
            text: "$x$".into()
        }
        .has_math());
    }

    #[test]
    fn sel_range_and_blocks() {
        let s = Sel {
            anchor: Pos::new(1, 3),
            focus: Pos::new(0, 4),
        };
        let (a, b) = s.range();
        assert_eq!(a, Pos::new(0, 4));
        assert_eq!(b, Pos::new(1, 3));
        assert_eq!(s.block_range(), 0..=1);
        assert!(!s.collapsed());
    }

    #[test]
    fn pos_clamped_to_doc_and_block() {
        let doc = Doc::new(vec![Block::Para(vec![Inline::Text("hello".into())])]);
        assert_eq!(Pos::new(0, 3).clamped(&doc), Pos::new(0, 3));
        assert_eq!(Pos::new(0, 99).clamped(&doc), Pos::new(0, 5));
        assert_eq!(Pos::new(9, 0).clamped(&doc), Pos::new(0, 0));
    }

    #[test]
    fn code_block_text_is_plain() {
        let b = Block::Code {
            lang: None,
            text: "fn x() {}".into(),
        };
        assert_eq!(b.plain_text(), "fn x() {}");
    }

    /// E1: when a stale offset lands mid-multibyte-character, `clamped` aligns
    /// to a character boundary instead of panicking.
    #[test]
    fn clamped_snaps_to_char_boundary() {
        // "中文" is 3 bytes per char; "😀" is 4 bytes
        let doc = Doc::new(vec![Block::Para(vec![Inline::Text("中文a😀b".into())])]);
        let plain = "中文a😀b";
        assert_eq!(plain.len(), 3 + 3 + 1 + 4 + 1);
        // off=1/2 land inside "中" → aligned to 0
        assert_eq!(Pos::new(0, 1).clamped(&doc), Pos::new(0, 0));
        assert_eq!(Pos::new(0, 2).clamped(&doc), Pos::new(0, 0));
        assert_eq!(Pos::new(0, 3).clamped(&doc), Pos::new(0, 3));
        // off=8/9/10 land inside "😀" (bytes 7..11) → aligned to 7
        assert_eq!(Pos::new(0, 8).clamped(&doc), Pos::new(0, 7));
        assert_eq!(Pos::new(0, 10).clamped(&doc), Pos::new(0, 7));
        assert_eq!(Pos::new(0, 11).clamped(&doc), Pos::new(0, 11));
        // The upper clamp still applies, and the result is always on a character
        // boundary (downstream can slice safely)
        let p = Pos::new(0, 999).clamped(&doc);
        assert_eq!(p, Pos::new(0, plain.len()));
        let _ = &plain[..p.off];
    }

    /// E2: Default must go through Doc::new's invariant (at least one block) and
    /// must not produce an empty blocks.
    #[test]
    fn default_doc_is_non_empty() {
        let d = Doc::default();
        assert_eq!(d.block_count(), 1);
        assert_eq!(d.blocks[0], Block::Para(vec![]));
        assert_eq!(d.next_table_seq, 0);
    }

    /// The normalization primitives used by every string-mutating command: a
    /// mid-character offset is snapped, never panicked on.
    #[test]
    fn boundary_helpers_snap_instead_of_panicking() {
        let s = "a例如b"; // a=0, 例=1..4, 如=4..7, b=7
        assert_eq!(floor_boundary(s, 2), 1);
        assert_eq!(ceil_boundary(s, 2), 4);
        assert_eq!(floor_boundary(s, 5), 4, "inside 如 floors to its start");
        assert_eq!(ceil_boundary(s, 5), 7, "inside 如 ceils to its end");
        assert_eq!(floor_boundary(s, 99), s.len());
        assert_eq!(ceil_boundary(s, 99), s.len());
        assert_eq!(clamp_span(s, 2, 5), (1, 7), "start floored, end ceiled");
        assert_eq!(
            clamp_span(s, 5, 2),
            (4, 4),
            "an inverted span collapses onto a boundary instead of panicking"
        );
    }
}
