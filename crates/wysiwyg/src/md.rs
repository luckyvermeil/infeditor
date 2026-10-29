//! Markdown ↔ Doc.
//!
//! Parsing: pulldown-cmark (CommonMark + GFM extensions) event stream → build the
//! leaf-block tree.
//! Serialization: a hand-written `emit` (design doc §4.2: a controllable output
//! format + block-level partial serialization later).
//!
//! Coverage: paragraphs / ATX headings / fenced code blocks / lists (nested
//! `level`, ordered start numbers, task lists) / quotes (block-level content
//! records its ownership depth as [`Block::Quoted`]) / tables (GFM, with
//! alignment) / horizontal rules / emphasis / inline code / strikethrough /
//! links / images / inline and block-level math / soft and hard line breaks
//! (`Inline::SoftBreak`/`HardBreak`; emit restores them as a single line break or
//! two trailing spaces).
//!
//! Known degradation (the edge of the leaf-block model): images and math are
//! atomic nodes (the caret only ever lands at the two ends of the alt/source).

use crate::model::{wrap_quote, Block, Doc, Inline};
use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};

fn parser_opts() -> Options {
    let mut o = Options::empty();
    o.insert(Options::ENABLE_STRIKETHROUGH);
    o.insert(Options::ENABLE_TABLES);
    o.insert(Options::ENABLE_TASKLISTS);
    o.insert(Options::ENABLE_MATH);
    o
}

/// One level of the inline container stack (a unified stack: emphasis containers
/// and links enter and leave interleaved, in the order they opened).
enum Layer {
    Strong,
    Em,
    Del,
    Link(String),
}

impl Layer {
    fn wrap(self, buf: Vec<Inline>) -> Inline {
        match self {
            Layer::Strong => Inline::Strong(buf),
            Layer::Em => Inline::Em(buf),
            Layer::Del => Inline::Del(buf),
            Layer::Link(url) => Inline::Link { url, inlines: buf },
        }
    }
}

/// Block-level state. `Root` = between blocks; everything else is a block that is
/// "currently collecting inlines / text".
#[derive(Default)]
enum Mode {
    #[default]
    Root,
    Para,
    Head {
        level: u8,
    },
    Code {
        lang: Option<String>,
        buf: String,
    },
    /// GFM HTML block (Tag::HtmlBlock): the raw HTML source is collected verbatim and
    /// at End becomes a whole [`Block::Html`] (the preview passes it through
    /// unescaped). A stray `Event::Html` (outside an HtmlBlock) still goes down the
    /// plain-text channel.
    Html {
        buf: String,
    },
}

/// One level of the list-state stack.
struct ListState {
    ordered: bool,
    /// The next item's number (used by ordered lists).
    next_num: u64,
    /// Nesting level: items of a top-level list have level=0, items of a list nested
    /// inside it level=1, and so on.
    level: u8,
}

/// The list item currently being collected.
struct LiState {
    ordered: bool,
    num: u64,
    level: u8,
    task: Option<bool>,
    /// Quote depth when the item started: a paragraph at a *different* depth is
    /// a child of a nested quote, not a later paragraph of this item.
    depth: usize,
    /// Whether the item's first paragraph has already been flushed to a block (later
    /// paragraphs inside a loose list item must be joined in).
    flushed: bool,
    /// Index of the Li block this item flushed. Later paragraphs merge into
    /// *that* block: `blocks.last()` after a nested list is the nested item (the
    /// text migrated into the wrong item) and after a code block it is not an Li
    /// at all (the text was dropped).
    flushed_at: Option<usize>,
}

/// Table state.
struct TableState {
    seq: u64,
    /// Per-column alignment (from Start(Table)'s alignments).
    aligns: Vec<u8>,
    row: usize,
    col: usize,
}

/// State machine turning the pulldown-cmark event stream into a leaf-block tree.
#[derive(Default)]
struct Builder {
    blocks: Vec<Block>,
    mode: Mode,
    inlines: Vec<Inline>,
    /// The unified inline container stack (emphasis and links interleave in nesting
    /// order).
    cstack: Vec<(Layer, Vec<Inline>)>,
    quote_depth: usize,
    list_stack: Vec<ListState>,
    li_stack: Vec<LiState>,
    table: Option<TableState>,
    next_table_seq: u64,
    /// The image `(url, alt)` being collected (its own field rather than reusing
    /// `mode`, because `mem::take` would reset a `Para` to `Root` and lose the
    /// paragraph's flush — hit on a real machine).
    img: Option<(String, String)>,
    /// Defence for image nesting depth (there should be no image inside an image, but
    /// do not crash when the parser gets garbage in).
    img_depth: usize,
}

impl Builder {
    fn ev(&mut self, ev: Event) {
        // Image collection mode: every inline event only goes into the alt text.
        if self.img_depth > 0 {
            self.ev_in_image(ev);
            return;
        }
        match ev {
            Event::Start(Tag::Paragraph) => {
                self.flush_html_para();
                // A paragraph starting inside a *tight* item interrupts its
                // collected text: flush the item first or `inlines.clear()`
                // throws the text away (`- a\n  > q` lost "a"). Only when text
                // is pending — a task-list marker arrives *after* this event,
                // and flushing an empty item here froze the Li without its
                // checkbox.
                self.flush_pending_li_if_text();
                self.inlines.clear();
                self.mode = Mode::Para;
            }
            Event::End(TagEnd::Paragraph) => {
                self.close_inlines();
                let buf = std::mem::take(&mut self.inlines);
                self.end_paragraph(buf);
                self.mode = Mode::Root;
            }
            Event::Start(Tag::Heading { level, .. }) => {
                self.flush_html_para();
                // Same as Paragraph: a heading can interrupt a tight item.
                self.flush_pending_li_if_text();
                self.inlines.clear();
                self.mode = Mode::Head {
                    level: heading_level(level),
                };
            }
            Event::End(TagEnd::Heading(_)) => {
                self.close_inlines();
                if let Mode::Head { level } = self.mode {
                    let inlines = std::mem::take(&mut self.inlines);
                    self.push_block(Block::Head { level, inlines });
                }
                self.mode = Mode::Root;
            }
            Event::Start(Tag::CodeBlock(kind)) => {
                self.flush_html_para();
                // A code block inside a tight item must not swallow the item's
                // text: flush it before collecting the block body.
                self.flush_pending_li_if_text();
                let lang = match kind {
                    CodeBlockKind::Fenced(info) => {
                        let t = info.trim();
                        if t.is_empty() {
                            None
                        } else {
                            Some(t.split([' ', '\t']).next().unwrap_or("").to_string())
                        }
                    }
                    CodeBlockKind::Indented => None,
                };
                self.mode = Mode::Code {
                    lang,
                    buf: String::new(),
                };
            }
            Event::End(TagEnd::CodeBlock) => {
                if let Mode::Code { lang, buf } = std::mem::take(&mut self.mode) {
                    // Only the single newline pulldown appends to the content is
                    // stripped: trailing blank lines *are* content (leading ones
                    // were already kept, so trimming all of them made the block
                    // shrink on every save).
                    let text = buf.strip_suffix('\n').unwrap_or(&buf).to_string();
                    self.push_block(Block::Code { lang, text });
                }
            }
            Event::Start(Tag::HtmlBlock) => {
                self.flush_html_para();
                self.flush_pending_li_if_text();
                self.mode = Mode::Html { buf: String::new() };
            }
            Event::End(TagEnd::HtmlBlock) => {
                if let Mode::Html { buf } = std::mem::take(&mut self.mode) {
                    // Leading and trailing line breaks are **not trimmed** — buf matches the source
                    // block byte for byte (it really ends with the last line's newline), and
                    // trimming would quietly shrink the line and byte counts. Block boundaries are
                    // emit's job (normalizing the blank lines between blocks) and must not encroach
                    // on the content's own line breaks.
                    self.push_block(Block::Html { text: buf });
                }
            }
            Event::Start(Tag::BlockQuote(_)) => {
                self.flush_html_para();
                self.quote_depth += 1
            }
            Event::End(TagEnd::BlockQuote(_)) => {
                self.flush_html_para();
                self.quote_depth = self.quote_depth.saturating_sub(1)
            }
            Event::Start(Tag::List(start)) => {
                self.flush_html_para();
                // Saturate like `quote_depth_u8`: `as u8` wraps at 256 levels
                // of nesting and flattens the structure on emit.
                let level = self.list_stack.len().min(usize::from(u8::MAX)) as u8;
                self.list_stack.push(ListState {
                    ordered: start.is_some(),
                    next_num: start.unwrap_or(0),
                    level,
                });
            }
            Event::End(TagEnd::List(_)) => {
                self.list_stack.pop();
            }
            Event::Start(Tag::Item) => {
                // If the outer item has un-flushed inline content (a compact list interrupted by
                // a nested list), flush it first.
                self.flush_pending_li();
                self.flush_html_para();
                if let Some(ls) = self.list_stack.last_mut() {
                    let num = ls.next_num;
                    if ls.ordered {
                        ls.next_num += 1;
                    }
                    self.li_stack.push(LiState {
                        ordered: ls.ordered,
                        num,
                        level: ls.level,
                        task: None,
                        depth: self.quote_depth,
                        flushed: false,
                        flushed_at: None,
                    });
                    self.inlines.clear();
                }
            }
            Event::End(TagEnd::Item) => {
                // A compact list's item content carries no Paragraph events, so the inline text
                // lands straight in the inlines buffer — flush it here. A loose list's items were
                // already flushed at End(Paragraph), so the buffer is empty; the two paths are
                // complementary.
                self.flush_pending_li();
                self.li_stack.pop();
            }
            Event::TaskListMarker(checked) => {
                if let Some(li) = self.li_stack.last_mut() {
                    li.task = Some(checked);
                }
            }
            // ---- Table ----
            Event::Start(Tag::Table(alignments)) => {
                self.flush_html_para();
                let aligns = alignments
                    .iter()
                    .map(|a| match a {
                        pulldown_cmark::Alignment::None => 0,
                        pulldown_cmark::Alignment::Left => 1,
                        pulldown_cmark::Alignment::Center => 2,
                        pulldown_cmark::Alignment::Right => 3,
                    })
                    .collect();
                let seq = self.next_table_seq;
                self.next_table_seq += 1;
                self.table = Some(TableState {
                    seq,
                    aligns,
                    row: 0,
                    col: 0,
                });
                self.inlines.clear();
            }
            Event::Start(Tag::TableHead) => {}
            Event::End(TagEnd::TableHead) => {
                // The header row ended; move on to the data rows.
                if let Some(t) = self.table.as_mut() {
                    t.row += 1;
                    t.col = 0;
                }
            }
            Event::Start(Tag::TableRow) => {
                if let Some(t) = self.table.as_mut() {
                    t.col = 0;
                }
                self.inlines.clear();
            }
            Event::End(TagEnd::TableRow) => {
                if let Some(t) = self.table.as_mut() {
                    t.row += 1;
                }
            }
            Event::Start(Tag::TableCell) => {
                self.inlines.clear();
            }
            Event::End(TagEnd::TableCell) => {
                self.close_inlines();
                let buf = std::mem::take(&mut self.inlines);
                let d = self.quote_depth_u8();
                if let Some(t) = self.table.as_mut() {
                    let align = t.aligns.get(t.col).copied().unwrap_or(0);
                    // Cells of a table inside a quote are wrapped in the shell too (`> | a |` no
                    // longer flattens)
                    self.blocks.push(wrap_quote(
                        d,
                        Block::Td {
                            table_seq: t.seq,
                            row: t.row,
                            align,
                            inlines: buf,
                        },
                    ));
                    t.col += 1;
                }
            }
            Event::Rule => {
                self.flush_html_para();
                self.push_block(Block::Hr)
            }

            // ---- Inline ----
            Event::Text(t) => match &mut self.mode {
                Mode::Code { buf, .. } | Mode::Html { buf } => buf.push_str(&t),
                _ => self.push_text(&t),
            },
            Event::Code(t) => self.push(Inline::Code(t.to_string())),
            Event::SoftBreak => self.push(Inline::SoftBreak),
            // A line break from two trailing spaces or a backslash: keep the line-break
            // meaning (it no longer folds into a space)
            Event::HardBreak => self.push(Inline::HardBreak),
            Event::Start(Tag::Strong) => {
                self.cstack.push((Layer::Strong, Vec::new()));
            }
            Event::End(TagEnd::Strong) => self.close_layer(LayerKind::Strong),
            Event::Start(Tag::Emphasis) => {
                self.cstack.push((Layer::Em, Vec::new()));
            }
            Event::End(TagEnd::Emphasis) => self.close_layer(LayerKind::Em),
            Event::Start(Tag::Strikethrough) => {
                self.cstack.push((Layer::Del, Vec::new()));
            }
            Event::End(TagEnd::Strikethrough) => self.close_layer(LayerKind::Del),
            Event::Start(Tag::Link { dest_url, .. }) => {
                self.cstack
                    .push((Layer::Link(dest_url.to_string()), Vec::new()));
            }
            Event::End(TagEnd::Link) => self.close_layer(LayerKind::Link),
            Event::Start(Tag::Image { dest_url, .. }) => {
                // Enter alt-collection mode: every text event inside goes into the alt.
                self.img = Some((dest_url.to_string(), String::new()));
                self.img_depth = 1;
            }
            Event::End(TagEnd::Image) => {
                // Unpaired (defensive): ignore
            }
            Event::InlineMath(s) => self.push(Inline::Math {
                src: s.to_string(),
                display: false,
            }),
            // Block-level math: hung into the current paragraph as an inline Math (the
            // Paragraph event wraps it; pushing a block of our own would add an empty Para —
            // hit on a real machine); the display flag is kept for the preview.
            Event::DisplayMath(s) => self.push(Inline::Math {
                src: s.to_string(),
                display: true,
            }),

            // ---- HTML ----
            // Inline HTML: same semantics as the original infeditor preview — user tags are
            // never executed and the content is kept as visible text (swallowing it would
            // lose content), escaped by escape_html at render time.
            // Exception: `<br>` (and `<br/>` / `<br />`) is **line-break semantics** and maps
            // to HardBreak — Enter inside a cell relies on it to express a break (emit emits
            // `<br>` inside a table), and taking it as literal text would escape it into
            // `&lt;br&gt;` after saving.
            // Block-level HTML is already collected by Mode::Html (Tag::HtmlBlock) and never
            // reaches here; a stray Event::Html (inside a list item and other edge cases) is
            // downgraded to plain text.
            Event::InlineHtml(t) => {
                if is_br_tag(&t) {
                    self.push(Inline::HardBreak);
                } else {
                    self.push_text(&t);
                }
            }
            Event::Html(t) => {
                if let Mode::Html { buf } = &mut self.mode {
                    buf.push_str(&t);
                } else {
                    self.push_text(&t);
                }
            }
            Event::FootnoteReference(label) => self.push_text(&format!("[^{label}]")),
            Event::Start(_) | Event::End(_) => {}
        }
    }

    /// Event handling for the image alt collection mode.
    fn ev_in_image(&mut self, ev: Event) {
        match ev {
            Event::Start(Tag::Image { .. }) => {
                self.img_depth += 1;
            }
            Event::End(TagEnd::Image) => {
                self.img_depth -= 1;
                if self.img_depth == 0 {
                    if let Some((url, alt)) = self.img.take() {
                        self.push(Inline::Image { url, alt });
                    }
                }
            }
            Event::Text(t) => {
                if let Some((_, alt)) = &mut self.img {
                    alt.push_str(&t);
                }
            }
            Event::Code(t) => {
                if let Some((_, alt)) = &mut self.img {
                    alt.push_str(&t);
                }
            }
            Event::SoftBreak | Event::HardBreak => {
                if let Some((_, alt)) = &mut self.img {
                    alt.push(' ');
                }
            }
            // Other tags (emphasis etc.) are downgraded to plain text inside the alt and are
            // caught by the Text event
            _ => {}
        }
    }

    /// Land the paragraph that just ended.
    ///
    /// A paragraph inside a list item goes to the item's own [`Block::Li`]: the
    /// *first* one becomes it, a later one joins it (space separated). Which
    /// block that is has to be remembered — `blocks.last()` after a nested list
    /// is the nested item (the text migrated into the wrong item) and after a
    /// code block it is not an Li at all (the text was dropped). A paragraph at
    /// a different quote depth belongs to a nested quote, not to the item.
    fn end_paragraph(&mut self, buf: Vec<Inline>) {
        let d = self.quote_depth_u8();
        let top = self.li_stack.last().map(|li| {
            (
                li.flushed,
                li.depth,
                li.flushed_at,
                li.ordered,
                li.num,
                li.level,
                li.task,
            )
        });
        match top {
            // First paragraph of the current item: it becomes the item's Li.
            // Items are wrapped in the quote depth too (an item of `> - a` no
            // longer flattens to a top-level block).
            Some((false, _, _, ordered, num, level, task)) => {
                if let Some(li) = self.li_stack.last_mut() {
                    li.flushed = true;
                    li.flushed_at = Some(self.blocks.len());
                }
                self.blocks.push(wrap_quote(
                    d,
                    Block::Li {
                        ordered,
                        num,
                        level,
                        task,
                        inlines: buf,
                    },
                ));
            }
            // Later paragraph of the same item at the same quote depth: join the
            // Li this item flushed.
            Some((true, depth, Some(at), _, _, _, _)) if depth == self.quote_depth => {
                if buf.is_empty() {
                    return;
                }
                let target = self.blocks.get_mut(at).and_then(li_inlines_mut);
                if let Some(inlines) = target {
                    if !inlines.is_empty() {
                        merge_inline_buf(inlines, vec![Inline::Text(" ".into())]);
                    }
                    merge_inline_buf(inlines, buf);
                } else {
                    self.push_block(Block::Para(buf));
                }
            }
            // A paragraph inside a nested quote, or one the item never flushed a
            // block for: its own block. Never dropped.
            Some(_) => self.push_paragraph(buf),
            None if matches!(self.mode, Mode::Para) => self.push_paragraph(buf),
            None => {}
        }
    }

    /// Land a paragraph as its own block (nothing to merge into).
    fn push_paragraph(&mut self, buf: Vec<Inline>) {
        if !buf.is_empty() {
            self.push_block(Block::Para(buf));
        }
    }

    /// Flush the inline buffer into a Para/Quote block. Block-level HTML
    /// (`Event::Html`) has no Paragraph event wrapping it, so its text lands through
    /// the Para channel — flushed in one go when the next block-level event arrives
    /// (or at the end of parsing), which makes consecutive Html lines naturally join
    /// into one paragraph.
    ///
    /// **Not flushed inside a list item**: a compact list's item text also hangs in the
    /// inlines buffer waiting for `End(Item)`, and the Start event of a nested list or
    /// of a quote inside an item must never flush it into a top-level block early (hit
    /// by emit_roundtrip); the HTML text inside an item is collected into the Li by
    /// `flush_pending_li`, so no content is lost.
    fn flush_html_para(&mut self) {
        if !self.li_stack.is_empty() || self.inlines.is_empty() {
            return;
        }
        self.close_inlines();
        let buf = std::mem::take(&mut self.inlines);
        self.push_block(Block::Para(buf));
    }

    /// [`Self::flush_pending_li`] for the block-start events: only worth doing
    /// when text is pending, since the point is to save it from the
    /// `inlines.clear()` that follows.
    fn flush_pending_li_if_text(&mut self) {
        if !self.inlines.is_empty() {
            self.flush_pending_li();
        }
    }

    /// Flush the current inlines buffer into the current item's Li (the compact-list
    /// flush path).
    fn flush_pending_li(&mut self) {
        // Only take the buffer when it will land in a block: a no-op call used to
        // `mem::take` it and drop it on the floor.
        if self.li_stack.last().is_none_or(|li| li.flushed) {
            return;
        }
        self.close_inlines();
        let buf = std::mem::take(&mut self.inlines);
        let d = self.quote_depth_u8();
        if let Some(li) = self.li_stack.last_mut() {
            li.flushed = true;
            li.flushed_at = Some(self.blocks.len());
            self.blocks.push(wrap_quote(
                d,
                Block::Li {
                    ordered: li.ordered,
                    num: li.num,
                    level: li.level,
                    task: li.task,
                    inlines: buf,
                },
            ));
        }
    }

    /// The current quote depth (the u8 shape, fed to [`wrap_quote`]).
    fn quote_depth_u8(&self) -> u8 {
        self.quote_depth.min(usize::from(u8::MAX)) as u8
    }

    /// Landing a block: wrap it in a quote shell for the current quote depth — a
    /// depth-1 pure-inline paragraph normalizes to the `Block::Quote` compatible
    /// shape, everything else is wrapped in `Block::Quoted`.
    fn push_block(&mut self, b: Block) {
        // A block that interrupts a *tight* item arrives before `End(Item)` flushes
        // the item's text; without this the block landed first and the item after
        // it (`- a` + `***` produced `Hr` then `Li("a")`, reordering the content).
        self.flush_pending_li();
        let d = self.quote_depth_u8();
        self.blocks.push(wrap_quote(d, b));
    }

    /// Link reference definitions (`[label]: dest`) are swallowed by pulldown and
    /// produce no events; when nothing references them the whole block silently
    /// disappears. `parse` calls this between the events' **gaps** and scans lines to
    /// restore them (a gap holds only blank lines and the swallowed definitions — the
    /// line content sits inside the event ranges).
    ///
    /// The preserved shape is a **literal-text paragraph**: the model has no dedicated
    /// block (the Block enum does not grow), and emit escapes it so it reparses as
    /// plain text — no content is lost and it is idempotent, but it is no longer a
    /// "live" reference definition. A referenced definition is kept as text too (the
    /// reference site has already become an inline link; better to keep too much than
    /// to lose it).
    /// Known edges: a definition whose title wraps onto the next line is not
    /// restored; definitions inside lists or quotes are not restored.
    fn collect_ref_defs(&mut self, gap: &str) {
        // Only restore between blocks (Root, with no pending content) — never scan inside
        // lists / quotes / tables / HTML / code
        if !(matches!(self.mode, Mode::Root)
            && self.li_stack.is_empty()
            && self.list_stack.is_empty()
            && self.quote_depth == 0
            && self.table.is_none()
            && self.img.is_none()
            && self.inlines.is_empty())
        {
            return;
        }
        for line in gap.lines() {
            if is_ref_def_line(line) {
                self.blocks
                    .push(Block::Para(vec![Inline::Text(line.trim().to_string())]));
            }
        }
    }

    fn push(&mut self, i: Inline) {
        match self.cstack.last_mut() {
            Some((_, buf)) => buf.push(i),
            None => self.inlines.push(i),
        }
    }

    fn push_text(&mut self, t: &str) {
        if t.is_empty() {
            return;
        }
        // Merging adjacent Text: inline-container text fragments are merged into whole
        // runs, basic hygiene for offset consistency.
        let target = match self.cstack.last_mut() {
            Some((_, buf)) => buf,
            None => &mut self.inlines,
        };
        if let Some(Inline::Text(last)) = target.last_mut() {
            last.push_str(t);
            return;
        }
        target.push(Inline::Text(t.to_string()));
    }

    fn close_inlines(&mut self) {
        while let Some((layer, buf)) = self.cstack.pop() {
            let wrapped = layer.wrap(buf);
            self.push(wrapped);
        }
    }

    fn close_layer(&mut self, kind: LayerKind) {
        let Some((layer, buf)) = self.cstack.pop() else {
            return;
        };
        if layer.kind() != kind {
            // Unpaired (should not happen); defensively recover the content (push one by one,
            // no longer treated as a container).
            for i in buf {
                self.push(i);
            }
            return;
        }
        let wrapped = layer.wrap(buf);
        self.push(wrapped);
    }
}

/// The match key of close_layer (so Layer does not have to be moved).
#[derive(PartialEq, Clone, Copy)]
enum LayerKind {
    Strong,
    Em,
    Del,
    Link,
}

impl Layer {
    fn kind(&self) -> LayerKind {
        match self {
            Layer::Strong => LayerKind::Strong,
            Layer::Em => LayerKind::Em,
            Layer::Del => LayerKind::Del,
            Layer::Link(_) => LayerKind::Link,
        }
    }
}

/// Whether one line is a link reference definition (lenient: `[label]: dest
/// [target]`, indentation ≤ 3 spaces). The leniency is safe because the caller only
/// scans the gaps between blocks (the lines pulldown swallowed into definitions,
/// plus blank lines).
fn is_ref_def_line(line: &str) -> bool {
    if line.len() - line.trim_start().len() > 3 {
        return false;
    }
    let s = line.trim();
    let Some(rest) = s.strip_prefix('[') else {
        return false;
    };
    // The label runs to the first unescaped `]`; no `[` may appear along the way
    let mut label_end = None;
    let mut escaped = false;
    for (i, c) in rest.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match c {
            '\\' => escaped = true,
            ']' => {
                label_end = Some(i);
                break;
            }
            '[' => return false,
            _ => {}
        }
    }
    let Some(i) = label_end else {
        return false;
    };
    // `:` follows the label, and the destination comes after (`<…>` or a
    // non-whitespace run)
    let Some(after) = rest[i + 1..].strip_prefix(':') else {
        return false;
    };
    !after.trim_start().is_empty()
}

/// Join buf into inlines, merging the trailing Text with buf's first Text ("a" +
/// " b" leaves no gap).
fn merge_inline_buf(inlines: &mut Vec<Inline>, buf: Vec<Inline>) {
    for i in buf {
        match (inlines.last_mut(), i) {
            (Some(Inline::Text(last)), Inline::Text(t)) => last.push_str(&t),
            (_, other) => inlines.push(other),
        }
    }
}

/// If the previous block is a list item (through the quote shell), return its
/// inline buffer — a loose list item's later paragraphs have to see the Li through
/// the `Quoted` shell when they are joined in.
fn li_inlines_mut(b: &mut Block) -> Option<&mut Vec<Inline>> {
    match b.core_mut() {
        Block::Li { inlines, .. } => Some(inlines),
        _ => None,
    }
}

fn heading_level(l: HeadingLevel) -> u8 {
    match l {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
    }
}

/// Whether an inline HTML tag is a line break (`<br>` / `<br/>` / `<br />`,
/// case-insensitive). `<br class=…>` with attributes is not recognized (to avoid
/// swallowing user markup by mistake).
fn is_br_tag(t: &str) -> bool {
    let s = t.trim();
    if !s.starts_with('<') || !s.ends_with('>') {
        return false;
    }
    let inner = s[1..s.len() - 1].trim();
    let name = inner.trim_end_matches('/').trim();
    name.eq_ignore_ascii_case("br")
}

// ---------------------------------------------------------------------------
// Math brace shielding (parse-time compatibility for escaped LaTeX)
// ---------------------------------------------------------------------------

/// Stand-in bytes for `{` / `}` while pulldown parses a math span: one byte each
/// (every offset stays identical) and control characters a text document cannot
/// contain.
const SHIELD_OPEN: u8 = 0x01;
const SHIELD_CLOSE: u8 = 0x02;

/// Replace `{` / `}` inside every `$…$` / `$$…$$` region with sentinels, so that
/// pulldown-cmark recognizes the formula.
///
/// Why this is needed: pulldown tracks a *brace context* along the text and only
/// pairs two math delimiters that sit in the same one. `$\left{a\right.$` — the
/// shape a Markdown escaper leaves behind (the `\{` delimiter lost its backslash
/// while the command's own backslash survived) — is therefore never recognized
/// as a formula and renders as raw LaTeX. Inside a formula, braces are math
/// characters, not Markdown grouping; hiding them from the block scanner is
/// exactly the difference between "a formula" and "a paragraph of TeX".
///
/// The delimiter rules are not a new dialect — they mirror pulldown's own (see
/// `math_regions_with_braces`). Only regions that carry a brace are patched, and
/// a document that already contains a sentinel byte is skipped outright: the
/// shield is a compatibility shim, never a rewrite of the text.
fn shield_math_braces(src: &str) -> Option<String> {
    let b = src.as_bytes();
    if !b.contains(&b'$')
        || !(b.contains(&b'{') || b.contains(&b'}'))
        || b.contains(&SHIELD_OPEN)
        || b.contains(&SHIELD_CLOSE)
    {
        return None;
    }
    // Two phases on purpose: only a region that actually carries a brace needs
    // the sentinels, and a document full of `$x^2$` must not pay for a full copy
    // of its text on every parse.
    let regions = math_regions_with_braces(b);
    if regions.is_empty() {
        return None;
    }
    let mut buf = b.to_vec();
    for (start, end) in regions {
        for byte in &mut buf[start..end] {
            match *byte {
                b'{' => *byte = SHIELD_OPEN,
                b'}' => *byte = SHIELD_CLOSE,
                _ => {}
            }
        }
    }
    // Only ASCII bytes were substituted, so the buffer is still valid UTF-8.
    String::from_utf8(buf).ok()
}

/// The **content** byte ranges (delimiters excluded) of every `$…$` / `$$…$$`
/// region that contains a brace.
///
/// The delimiter rules mirror pulldown's own (not a new dialect): a delimiter
/// opens when the character right after the first `$` is not whitespace, an
/// inline closer may not be preceded by whitespace, `$$` pairs with the next
/// `$$` regardless of whitespace (that is how a display block delimits itself on
/// a line of its own), and since a formula never contains `$`, the closer is the
/// next run at least as wide as the opener. Runs of three or more dollars are
/// skipped — that shape is not worth mirroring.
fn math_regions_with_braces(b: &[u8]) -> Vec<(usize, usize)> {
    let mut regions = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] != b'$' {
            i += 1;
            continue;
        }
        // The delimiter width: `$$` = display, `$` = inline.
        let mut run = 1;
        while i + run < b.len() && b[i + run] == b'$' {
            run += 1;
        }
        if run > 2 {
            i += run;
            continue;
        }
        let delim = run;
        // can_open: for `$$` the character right after the first dollar is the
        // second one, so a display delimiter on a line of its own still opens
        // (pulldown reads the pair as one unit).
        if i + 1 >= b.len() || b[i + 1].is_ascii_whitespace() {
            i += delim;
            continue;
        }
        // The closer: the next run at least as wide as the opener.
        let mut j = i + delim;
        let mut close = None;
        while j < b.len() {
            if b[j] != b'$' {
                j += 1;
                continue;
            }
            let mut m = 1;
            while j + m < b.len() && b[j + m] == b'$' {
                m += 1;
            }
            if m >= delim && (delim == 2 || (j > 0 && !b[j - 1].is_ascii_whitespace())) {
                close = Some(j);
                break;
            }
            j += m;
        }
        let Some(end) = close else {
            i += delim;
            continue;
        };
        let (start, end) = (i + delim, end);
        if b[start..end].iter().any(|&c| c == b'{' || c == b'}') {
            regions.push((start, end));
        }
        i = end + delim;
    }
    regions
}

/// Put the real braces back. Every string in the model is restored, including
/// regions the shield guessed wrong about: a mis-guessed region stays literal
/// text with its braces intact, so the worst case is "unchanged", never corrupt.
fn unshield_doc(doc: &mut Doc) {
    for block in &mut doc.blocks {
        unshield_block(block);
    }
}

fn unshield_block(block: &mut Block) {
    match block {
        Block::Para(v) | Block::Quote(v) => unshield_inlines(v),
        Block::Head { inlines, .. } | Block::Li { inlines, .. } | Block::Td { inlines, .. } => {
            unshield_inlines(inlines)
        }
        Block::Code { lang, text } => {
            unshield_text(text);
            if let Some(lang) = lang {
                unshield_text(lang);
            }
        }
        Block::Html { text } => unshield_text(text),
        Block::Hr => {}
        Block::Quoted { inner, .. } => unshield_block(inner),
    }
}

fn unshield_inlines(inlines: &mut [Inline]) {
    for inline in inlines {
        match inline {
            Inline::Text(t) | Inline::Code(t) => unshield_text(t),
            Inline::Math { src, .. } => unshield_text(src),
            Inline::Strong(v) | Inline::Em(v) | Inline::Del(v) => unshield_inlines(v),
            Inline::Link { url, inlines } => {
                unshield_text(url);
                unshield_inlines(inlines);
            }
            Inline::Image { url, alt } => {
                unshield_text(url);
                unshield_text(alt);
            }
            Inline::SoftBreak | Inline::HardBreak => {}
        }
    }
}

fn unshield_text(s: &mut String) {
    if !s.as_bytes().contains(&SHIELD_OPEN) && !s.as_bytes().contains(&SHIELD_CLOSE) {
        return;
    }
    *s = s
        .replace(char::from(SHIELD_OPEN), "{")
        .replace(char::from(SHIELD_CLOSE), "}");
}

/// Markdown → Doc.
///
/// The source is first passed through `shield_math_braces`: pulldown-cmark's
/// math spans only pair up when the enclosing text keeps a **balanced brace
/// context** (`$a{b$` never becomes a formula), which is exactly what
/// Markdown-escaped LaTeX produces (`\left{` / `\right}` lost the delimiter's
/// backslash, so the formula never parses). Shielding replaces the braces inside
/// a `$…$` / `$$…$$` region with 1-byte sentinels for the duration of the parse
/// and `unshield_doc` puts them back afterwards — the document text, every
/// offset and the round trip are unchanged.
pub fn parse(src: &str) -> Doc {
    let shielded = shield_math_braces(src);
    let mut doc = parse_source(shielded.as_deref().unwrap_or(src));
    if shielded.is_some() {
        unshield_doc(&mut doc);
    }
    doc
}

/// The parser proper (works on the shielded source; byte-identical in length).
fn parse_source(src: &str) -> Doc {
    let mut b = Builder::default();
    // Link reference definitions are swallowed by pulldown and produce no events —
    // scan the lines in the gaps between the events to restore them
    // (into_offset_iter's Start/End ranges cover the whole structure, so a gap is the
    // "invisible" source: blank lines and the swallowed definitions).
    let mut cursor = 0usize;
    for (ev, range) in Parser::new_ext(src, parser_opts()).into_offset_iter() {
        if range.start > cursor {
            b.collect_ref_defs(&src[cursor..range.start]);
        }
        cursor = cursor.max(range.end);
        b.ev(ev);
    }
    // Flush the block-level HTML text left at the end of the document (the Para
    // channel kept alive lazily)
    b.flush_html_para();
    // Trailing reference definitions at the end of the document (after pulldown's
    // last block event)
    b.collect_ref_defs(&src[cursor..]);
    Doc::new(b.blocks)
}

// ---------------------------------------------------------------------------
// Block line spans: top-level block → source line range (the basis for the host
// mdmap's block splitting)
// ---------------------------------------------------------------------------

/// The category of a top-level block (the info [`BlockSpan`] carries: the host
/// estimates height by category / tests assert on it).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpanKind {
    /// Paragraph (a block-level formula `$$…$$` is wrapped by pulldown in a
    /// Paragraph and counts here).
    Para,
    /// Heading (both ATX and setext — `text\n---` is a heading, not a paragraph
    /// plus an hr).
    Head,
    /// Fenced or indented code block.
    Code,
    /// Quote (the whole quote shell, including every block inside).
    Quote,
    /// List (the whole list, including nested items and quotes inside items).
    List,
    /// GFM table (the whole table, header and every data row).
    Table,
    /// Horizontal rule.
    Rule,
    /// GFM HTML block.
    Html,
}

/// The line range of one top-level block in the source (**0-based, both ends
/// inclusive** `[start, end]`).
///
/// A top-level block is the top-level unit of the physical block sequence (before
/// looking through the quote shell): a list (including nested items), a table (all
/// its rows) and a quote (every block inside) each count as **one** span — the
/// order matches the block sequence of [`parse`]/[`emit`] (when several model
/// blocks map to the same span they are laid out consecutively in source order).
/// Blank lines between blocks and the link reference definition lines swallowed by
/// pulldown are **not** inside any span (the caller fills the gaps itself).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BlockSpan {
    pub start: usize,
    pub end: usize,
    pub kind: SpanKind,
}

/// Top-level block → source line range (0-based, both ends inclusive; in source
/// order, never overlapping).
///
/// Implementation: the events' byte ranges from `Parser::into_offset_iter` → line
/// numbers converted with a newline-prefix sum. A top-level block starts at the
/// Start event's range start when the block container stack is empty, and ends at
/// the last content line corresponding to the **largest** end of every event
/// inside the block (Start/End ranges cover the whole structure, the same
/// accounting as [`parse`]'s gap scan). Inline tags do not count towards the
/// depth; secondary container tags (list items, table rows and cells) only count
/// depth and never open a new top-level block.
pub fn block_line_ranges(text: &str) -> Vec<BlockSpan> {
    // Byte offset table of line starts: `line_of(byte)` = the line number (0-based)
    // that byte is on
    let mut line_starts = vec![0usize];
    for (i, b) in text.bytes().enumerate() {
        if b == b'\n' {
            line_starts.push(i + 1);
        }
    }
    let lines: Vec<&str> = text.lines().collect();
    let mut spans: Vec<BlockSpan> = Vec::new();
    // Block-tag nesting depth: back to 0 means a top-level block ended
    let mut depth = 0usize;
    // The current top-level block: (start byte, largest end byte seen so far, category)
    let mut cur: Option<(usize, usize, SpanKind)> = None;
    for (ev, range) in Parser::new_ext(text, parser_opts()).into_offset_iter() {
        // Merge the event range into the current block (the End and inline events' ranges
        // all fall inside the block; taking the max covers the version differences where
        // a Start range only marks the start)
        if let Some((_, end, _)) = cur.as_mut() {
            *end = (*end).max(range.end);
        }
        match ev {
            Event::Start(tag) => {
                if depth == 0 {
                    if let Some(kind) = span_kind(&tag) {
                        cur = Some((range.start, range.end, kind));
                    }
                }
                if is_block_tag(&tag) {
                    depth += 1;
                }
            }
            Event::End(tag) => {
                if is_block_end(&tag) {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        if let Some((s, e, k)) = cur.take() {
                            push_span(&mut spans, &line_starts, &lines, s, e, k);
                        }
                    }
                }
            }
            // Rule has no Start/End wrapper: at the top level it is a block of its own;
            // nested (inside a quote or list) it belongs to the outer span (its range has
            // already been merged into the max above)
            Event::Rule if depth == 0 => {
                push_span(
                    &mut spans,
                    &line_starts,
                    &lines,
                    range.start,
                    range.end,
                    SpanKind::Rule,
                );
            }
            _ => {}
        }
    }
    // The parser did not pair up (should not happen): close the pending block at the
    // largest end seen
    if let Some((s, e, k)) = cur.take() {
        push_span(&mut spans, &line_starts, &lines, s, e, k);
    }
    spans
}

/// Byte range → line-range span. The end is the line of `end - 1` (when the
/// exclusive end falls at a line start it is still the previous content line's
/// trailing newline, so it still belongs to that line); trailing blank lines are
/// not block content (a pulldown container range swallows the separator blank line
/// after a block — hit on a real machine with lists/quotes), so the end retracts
/// to the last content line; the last line is clamped to the text's real line
/// count.
fn push_span(
    spans: &mut Vec<BlockSpan>,
    line_starts: &[usize],
    lines: &[&str],
    start: usize,
    end: usize,
    kind: SpanKind,
) {
    let line_of = |byte: usize| -> usize {
        line_starts
            .partition_point(|&s| s <= byte)
            .saturating_sub(1)
    };
    let last_line = lines.len().saturating_sub(1);
    let s = line_of(start).min(last_line);
    let mut e = if end > start { line_of(end - 1) } else { s };
    e = e.min(last_line).max(s);
    while e > s && lines[e].trim().is_empty() {
        e -= 1;
    }
    spans.push(BlockSpan {
        start: s,
        end: e,
        kind,
    });
}

/// Start tag → top-level block category (only tags that can open a top-level
/// block are recognized; secondary containers such as list items, table rows and
/// cells return None — they only count depth and never open a new top-level
/// block).
fn span_kind(tag: &Tag) -> Option<SpanKind> {
    match tag {
        Tag::Paragraph => Some(SpanKind::Para),
        Tag::Heading { .. } => Some(SpanKind::Head),
        Tag::CodeBlock(_) => Some(SpanKind::Code),
        Tag::HtmlBlock => Some(SpanKind::Html),
        Tag::BlockQuote(_) => Some(SpanKind::Quote),
        Tag::List(_) => Some(SpanKind::List),
        Tag::Table(_) => Some(SpanKind::Table),
        _ => None,
    }
}

/// Block-level tags (count towards the nesting depth; inline tags do not).
fn is_block_tag(tag: &Tag) -> bool {
    span_kind(tag).is_some()
        || matches!(
            tag,
            Tag::Item | Tag::TableHead | Tag::TableRow | Tag::TableCell
        )
}

/// Block-level closing tags (the mirror of [`is_block_tag`]).
fn is_block_end(tag: &TagEnd) -> bool {
    matches!(
        tag,
        TagEnd::Paragraph
            | TagEnd::Heading(_)
            | TagEnd::CodeBlock
            | TagEnd::HtmlBlock
            | TagEnd::BlockQuote(_)
            | TagEnd::List(_)
            | TagEnd::Item
            | TagEnd::Table
            | TagEnd::TableHead
            | TagEnd::TableRow
            | TagEnd::TableCell
    )
}

// ---------------------------------------------------------------------------
// emit: Doc → Markdown
// ---------------------------------------------------------------------------

/// Doc → Markdown string. Exactly one blank line between blocks; exactly one
/// trailing newline at the end (a newline the content carries is part of the
/// content: an Html block's leading and trailing newlines are kept verbatim, not
/// trimmed).
pub fn emit(doc: &Doc) -> String {
    emit_with_offsets(doc).0
}

/// Each block's **content start** byte offset in [`emit`]'s output (bookkept
/// in the same pass as the emission, byte-aligned, no longer an approximate
/// second-emission conversion). Used by the host bridge: once the whole document is
/// written back to the piece table, it aligns its hidden caret with the wys focus
/// block (it only feeds the status bar and scroll restoration and does not take
/// part in wys editing).
///
/// The semantics are "the first content byte after the marker": a heading skips
/// `# `, a list skips `- ` / `1. ` / `[x] `, a code block skips the fence line, and
/// a table skips `| `. The marker's start must **not** be returned — `Pos.off` is a
/// plain-text offset inside the block, and adding it to the marker would shift
/// everything forward by several bytes, landing in the middle of a multi-byte
/// character in a Chinese heading (on a real machine the status bar panicked while
/// slicing the string).
pub fn block_offsets(doc: &Doc) -> Vec<usize> {
    emit_with_offsets(doc).1
}

/// The prefix length of a block's content in [`emit_block`]'s output (the marker's
/// byte count).
fn block_content_prefix(b: &Block) -> usize {
    match b.core() {
        // An empty heading writes only its markers (no space, see `emit_block`), so
        // its content offset sits right after them.
        Block::Head { level, inlines } => {
            *level as usize + usize::from(inlines.iter().any(|i| !emits_nothing(i)))
        }
        Block::Code { lang, text } => {
            code_fence(text).len() + lang.as_deref().map_or(0, str::len) + 1
        }
        _ => 0,
    }
}

/// One emission pass produces the text and every block's start offset together
/// (`emit` and `block_offsets` share it), which keeps the offsets and the output
/// naturally in sync (segmented prefix-sum bookkeeping, so there is no
/// second-emission drift).
///
/// Blocks are partitioned **by quote depth** — the longest contiguous run at one
/// depth is one region; inside a region the old grouping stands (a run of
/// consecutive Li is one list region, a run of Td with the same seq is one table
/// region, everything else is a single block), and a quote region gets the
/// `> `×depth prefix added line by line (every line of a multi-line list, code
/// block or table carries the shell).
fn emit_with_offsets(doc: &Doc) -> (String, Vec<usize>) {
    let mut out = String::new();
    let mut offsets = vec![0usize; doc.blocks.len()];
    let mut i = 0;
    while i < doc.blocks.len() {
        let depth = doc.blocks[i].quote_depth();
        let start = i;
        while i < doc.blocks.len() && doc.blocks[i].quote_depth() == depth {
            i += 1;
        }
        emit_region(
            &doc.blocks[start..i],
            depth,
            &mut out,
            &mut offsets[start..i],
        );
        // Exactly one blank line between blocks: only ever added, never removed (a line
        // break the content carries is content); nothing is added before the first block.
        if i < doc.blocks.len() && !out.is_empty() {
            while !out.ends_with("\n\n") {
                out.push('\n');
            }
        }
    }
    // End-of-document normalization: an Html ending keeps the content's own line
    // breaks — including the *absence* of a final one (a source block at EOF is kept
    // byte for byte, so a file whose last line has no newline does not grow one on
    // every save); every other block keeps the old behaviour (strip trailing blank
    // lines, exactly one newline).
    if matches!(
        doc.blocks.last().map(|b| b.core()),
        Some(Block::Html { .. })
    ) {
        // Nothing to add: the block's source is emitted verbatim.
    } else if doc
        .blocks
        .iter()
        .all(|b| matches!(b.core(), Block::Para(inl) if inl.is_empty()))
    {
        // A fully empty document (new file / everything deleted): keep the old behaviour
        // of "exactly one line break".
        out.clear();
        out.push('\n');
    } else {
        // The trailing empty paragraph (an empty line created by pressing Enter at the end
        // of the document) must be kept: strip everything and then add line breaks back
        // as "1 + the number of empty paragraphs". The old implementation stripped
        // unconditionally down to exactly one line break — a user pressed Enter to create
        // an empty line, saved, and the empty line vanished (seen on a real machine).
        let trailing_empty = doc
            .blocks
            .iter()
            .rev()
            .take_while(|b| matches!(b.core(), Block::Para(inl) if inl.is_empty()))
            .count();
        while out.ends_with('\n') {
            out.pop();
        }
        out.push('\n');
        for _ in 0..trailing_empty {
            out.push('\n');
        }
    }
    (out, offsets)
}

/// Emitting one region of a single quote depth. `depth == 0` emits directly;
/// `depth > 0` first emits into a temporary buffer and then adds the `> `×depth
/// prefix line by line — the offsets are converted back to the main buffer's
/// coordinates through the **line table**: a block start (the line start) points at
/// the **prefix's first byte** (the same accounting as "indent included" for list
/// items), and a mid-line position (a table cell) points at the content's first
/// byte. A blank line only emits `>` (shorter than `> `), so it cannot be converted
/// with a fixed prefix width, hence the per-line bookkeeping.
fn emit_region(blocks: &[Block], depth: u8, out: &mut String, offsets: &mut [usize]) {
    if depth == 0 {
        emit_region_plain(blocks, out, offsets);
        return;
    }
    let mut tmp = String::new();
    let mut toff = vec![0usize; blocks.len()];
    emit_region_plain(blocks, &mut tmp, &mut toff);
    let pre = "> ".repeat(depth as usize);
    // The line table: (tmp line start, out prefix's first byte, the line's prefix byte
    // count, the line's content byte count)
    let mut lines: Vec<(usize, usize, usize, usize)> = Vec::new();
    let (mut tp, mut op) = (0usize, out.len());
    for seg in tmp.split_inclusive('\n') {
        let content = seg.strip_suffix('\n').unwrap_or(seg);
        let plen = if content.is_empty() {
            pre.trim_end().len()
        } else {
            pre.len()
        };
        lines.push((tp, op, plen, content.len()));
        tp += seg.len();
        op += plen + seg.len();
    }
    for (k, &p) in toff.iter().enumerate() {
        offsets[k] = match lines.is_empty() {
            true => out.len(),
            false => {
                let li = lines.partition_point(|l| l.0 <= p).saturating_sub(1);
                let (tls, ols, plen, clen) = lines[li];
                if p == tls {
                    // The block content's first byte: an ordinary block starts at the line start
                    // (p == tls), while emit's content sits after the `> ` prefix — the prefix must be
                    // added; an empty content line has no content, so the prefix itself is the
                    // position.
                    if clen == 0 {
                        ols
                    } else {
                        ols + plen
                    }
                } else {
                    ols + plen + (p - tls)
                }
            }
        };
    }
    push_prefixed_lines(&tmp, &pre, out);
}

/// Add the quote prefix line by line: a non-empty line is prefixed with `pre`
/// (`"> "×depth`); an empty line only emits `pre` with the trailing whitespace
/// stripped (`>` / `> >` — it reparses back to an empty content line, keeping empty
/// lines inside a code block or between blocks inside a quote faithful character
/// by character). The phantom blank line after tmp's trailing newline gets no
/// prefix.
fn push_prefixed_lines(tmp: &str, pre: &str, out: &mut String) {
    let mut rest = tmp;
    while !rest.is_empty() {
        let (line, has_nl) = match rest.find('\n') {
            Some(i) => (&rest[..i], true),
            None => (rest, false),
        };
        if line.is_empty() {
            out.push_str(pre.trim_end());
        } else {
            out.push_str(pre);
            out.push_str(line);
        }
        if !has_nl {
            break;
        }
        out.push('\n');
        rest = &rest[line.len() + 1..];
    }
}

/// Grouped emission: a run of consecutive Li (through the quote shell) is one list
/// region, a run of Td with the same seq is one table region, everything else is a
/// single block; exactly one blank line between groups (only ever added, never
/// removed).
fn emit_region_plain(blocks: &[Block], out: &mut String, offsets: &mut [usize]) {
    let mut i = 0;
    while i < blocks.len() {
        match blocks[i].core() {
            Block::Li { .. } => {
                let start = i;
                while i < blocks.len() && matches!(blocks[i].core(), Block::Li { .. }) {
                    i += 1;
                }
                emit_list(&blocks[start..i], out, &mut offsets[start..i]);
            }
            Block::Td { table_seq, .. } => {
                let seq = *table_seq;
                let start = i;
                while i < blocks.len()
                    && matches!(blocks[i].core(), Block::Td { table_seq: s, .. } if *s == seq)
                {
                    i += 1;
                }
                emit_table(&blocks[start..i], out, &mut offsets[start..i]);
            }
            b => {
                // The content's start offset (skipping the heading's `# ` / the code fence line)
                // — see block_offsets
                offsets[i] = out.len() + block_content_prefix(b);
                emit_block(b, out);
                i += 1;
            }
        }
        if i < blocks.len() && !out.is_empty() {
            while !out.ends_with("\n\n") {
                out.push('\n');
            }
        }
    }
}

fn emit_block(b: &Block, out: &mut String) {
    // Emit the content only, without the block-tail separator (the separator is
    // normalized in one place by emit_region_plain so it cannot encroach on an Html
    // block's own trailing line breaks). `b` is already the core (emit_region adds
    // the quote shell's `> ` prefix).
    match b {
        Block::Para(inlines) => {
            emit_inlines(inlines, false, true, out);
            escape_block_end_spaces(out);
        }
        Block::Head { level, inlines } => {
            for _ in 0..*level {
                out.push('#');
            }
            // ATX allows the opening run to be followed by a space **or by the end of
            // the line**: an empty heading emits just its markers. Writing the space
            // and then entity-escaping it (the block-end space rule) produced
            // `##&#32;`, which is not a heading at all — it reparsed as a paragraph
            // holding a literal `## `, so one save turned an empty heading into text.
            // Whitespace-only content (a heading whose text is just a line break) has to
            // degrade to the same empty form: its emitted body is nothing but spaces, and
            // `#####&#32;` is a paragraph holding the literal markers, not a heading.
            let mut body = String::new();
            // A heading is a single line (ATX): an in-block line break degrades to a space
            // (the merge path may bring in line breaks, and emitting a newline directly would
            // split the heading into a heading plus a paragraph, drifting the structure)
            emit_inlines(inlines, false, false, &mut body);
            if body.chars().any(|c| !c.is_whitespace()) {
                out.push(' ');
                out.push_str(&body);
                escape_block_end_spaces(out);
            }
        }
        Block::Code { lang, text } => {
            // Fence length = longest run of the marker in the content and the
            // info string, + 1 (the old rule of "contains ```" was not enough).
            // A backtick fence's info string may not contain a backtick —
            // pulldown does not recognise such a line as a fence opener at all,
            // so `~~~a`b` would reparse as a paragraph and the block would be
            // lost. Those language tags get a tilde fence.
            let marker = if lang.as_deref().is_some_and(|l| l.contains('`')) {
                '~'
            } else {
                '`'
            };
            let fence = fence_of(marker, text, lang.as_deref().unwrap_or(""));
            out.push_str(&fence);
            if let Some(l) = lang {
                out.push_str(l);
            }
            out.push('\n');
            out.push_str(text);
            out.push('\n');
            out.push_str(&fence);
        }
        Block::Quote(inlines) => {
            // The `> ` prefix is added by emit_region for the depth; a quote's content starts
            // in a block-level context (`> > x` / `> - x` would nest into new blocks)
            emit_inlines_at(inlines, false, true, true, out);
            escape_block_end_spaces(out);
        }
        // GFM HTML block: the raw source is written back verbatim (markdown does not
        // escape it anyway), so the round trip is lossless.
        // The leading and trailing line breaks belong to the content and are not trimmed.
        Block::Html { text } => {
            out.push_str(text);
        }
        Block::Hr => out.push_str("---"),
        // Grouped blocks (list items / cells) never reach emit_block in a well-formed
        // tree; if a malformed one does, emit its content as a paragraph so the text
        // is preserved instead of panicking mid-save.
        Block::Li { inlines, .. } | Block::Td { inlines, .. } => {
            emit_inlines(inlines, false, true, out);
        }
        // The quote shell's `> ` prefix is added by emit_region; without it the text
        // is still written (structure degrades, content survives).
        Block::Quoted { inner, .. } => emit_block(inner, out),
    }
}

fn emit_list(items: &[Block], out: &mut String, offsets: &mut [usize]) {
    // Per level, a stack of **parent item content column widths**: CommonMark
    // requires a nested block to be indented at least to its parent item's content
    // column (`- ` is 2, `1. ` is 3, a task item adds 4 more) — a fixed 2 spaces
    // would make a sublist under an ordered list reparse as a sibling list, and one
    // round trip would flatten the nesting (saving would corrupt the structure).
    // stack[i] = the marker width of a level-i item; a child item's indent is the sum
    // of its ancestors' marker widths.
    let mut stack: Vec<usize> = Vec::new();
    // The content columns (indents) at which a list is currently running. An item whose
    // indent is not in this set *starts* a new list (nested, or the first item of the
    // region); emitting an item at a shallower indent closes every deeper list.
    let mut open_indents: Vec<usize> = Vec::new();
    for (k, b) in items.iter().enumerate() {
        if let Block::Li {
            ordered,
            num,
            level,
            task,
            inlines,
        } = b.core()
        {
            stack.truncate(usize::from(*level));
            let indent: usize = stack.iter().sum();
            // A list can interrupt a paragraph only when it is an ordered list starting at
            // 1 **and** the item is not empty (CommonMark). A list starting directly under
            // a paragraph — a nested list below its parent item's text is the common case —
            // otherwise reparses as a lazy continuation of that paragraph: `1. one` +
            // nested `2. two` came back as a single item holding the literal `2. two`, so
            // one save flattened the nesting and injected a `. `. A blank line ends the
            // paragraph first, so the list starts a block of its own and keeps its number.
            let starts_list = !open_indents.contains(&indent);
            // An item whose content is only whitespace emits just its marker line: it is an
            // empty list item, which cannot interrupt a paragraph either.
            let blank_item = inlines
                .iter()
                .all(|i| matches!(i, Inline::SoftBreak | Inline::HardBreak) || emits_nothing(i));
            if starts_list
                && ((*ordered && *num != 1) || blank_item)
                && !out.is_empty()
                && !out.ends_with("\n\n")
            {
                out.push('\n');
            }
            open_indents.retain(|&i| i <= indent);
            if starts_list {
                open_indents.push(indent);
            }
            // The offset points at the item's **first content byte** (after the indent,
            // marker and [x]); the marker width is counted in the unchecked `[ ]` form (4
            // columns) for the content offset.
            // **Sublist indentation** uses only the marker itself: the list start indent
            // CommonMark allows is "parent content column .. parent content column + 3", and
            // pulldown treats `[x] ` as inline text — so the parent content column is 2/3 and
            // not 6/7. Indenting by 6 would turn the sublist into a lazy continuation of the
            // parent paragraph (verified: a nested task list is flattened into `- [ ] …`
            // text after one round trip).
            let marker_w = if *ordered {
                format!("{}. ", num).len()
            } else {
                2
            };
            for _ in 0..indent {
                out.push(' ');
            }
            if *ordered {
                out.push_str(&format!("{}. ", num));
            } else {
                out.push_str("- ");
            }
            if let Some(checked) = task {
                out.push_str(if *checked { "[x] " } else { "[ ] " });
            }
            // Content start: after the indent / marker / [x] (Pos.off is a plain-text offset)
            offsets[k] = out.len();
            stack.push(marker_w);
            // The item's content starts in a block-level context (`- - x` / `- 1. x` would
            // nest into new blocks)
            let mut body = String::new();
            emit_inlines_at(inlines, false, true, true, &mut body);
            // A line break at the very start of the item's content means an empty first
            // line inside the item, which Markdown cannot express inside a list item (the
            // continuation would start a new block of the item — our model has no blocks
            // inside a `Li`). Emitting it pushed the following items out of the list as
            // literal text. Dropping the break loses whitespace only, not characters.
            let body = body.trim_start_matches('\n');
            // Continuation lines are indented to the **item's content column** (indent +
            // marker width). A soft break reparses through a lazy continuation anyway, but the
            // first line of a block (a block-level formula's `$$`, an HTML block, …) would be
            // taken as a boundary between blocks: `- $$` followed by two lines of content
            // would otherwise be emitted with the continuation lines flush left, and after
            // reparsing the list shell would be lost (the elements would fall out of the list
            // item). Indented, every continuation line stays inside the item and repeated
            // parsing is stable.
            let cont = indent + marker_w;
            // Only the *last* line's trailing spaces are content: a middle line
            // ends because a SoftBreak/HardBreak was emitted there, and both
            // already entity-escaped the content spaces in front of the break —
            // the two raw spaces left are the hard-break marker and must stay
            // raw (`escape_trailing_spaces` over the whole body turned them into
            // `&#32;`, which reparse as text and downgraded the break).
            let line_count = body.matches('\n').count() + 1;
            for (line_ix, line) in body.split('\n').enumerate() {
                if line_ix > 0 {
                    out.push('\n');
                    if !line.is_empty() {
                        for _ in 0..cont {
                            out.push(' ');
                        }
                    }
                }
                if line_ix + 1 == line_count {
                    out.push_str(&escape_trailing_spaces(line));
                } else {
                    out.push_str(line);
                }
            }
            out.push('\n');
        }
    }
}

/// Table: row 0 is the header, and an alignment separator row follows it. A `|`
/// inside a cell is escaped.
///
/// GFM requires the delimiter row to hold exactly as many columns as the header row,
/// and a data row may not hold more cells than that (the extras are dropped). Editing
/// can leave a ragged table — a column deleted on one row only, a row whose cells were
/// all removed (`DeleteRow`), a run that starts at row 1 — so the table is emitted as a
/// rectangle: the column count is the widest row, missing cells are emitted empty and
/// a missing header row is synthesized empty. Emitting the ragged shape made the whole
/// table reparse as a paragraph of literal `|` lines, i.e. pipe characters appeared in
/// the user's text on save.
fn emit_table(items: &[Block], out: &mut String, offsets: &mut [usize]) {
    // Collect the rows (with their block indices, for the per-cell offset bookkeeping; through the quote shell)
    let mut rows: Vec<Vec<(usize, &Block)>> = Vec::new();
    for (k, b) in items.iter().enumerate() {
        if let Block::Td { row, .. } = b.core() {
            let r = *row;
            if rows.len() <= r {
                rows.resize(r + 1, Vec::new());
            }
            rows[r].push((k, b.core()));
        }
    }
    if rows.is_empty() {
        return;
    }
    let cols = rows.iter().map(Vec::len).max().unwrap_or(0);
    let cells: &[(usize, &Block)] = rows.first().map_or(&[], Vec::as_slice);
    // The header (padded to the column count)
    out.push('|');
    for ci in 0..cols {
        match cells.get(ci) {
            Some((k, Block::Td { inlines, .. })) => {
                out.push(' ');
                // The offset points at the cell content's first byte (after `| `)
                offsets[*k] = out.len();
                // A cell is a single line (GFM): a line break degrades to a space, the row
                // structure wins
                push_cell_text(inlines, out);
                out.push_str(" |");
            }
            _ => out.push_str("  |"),
        }
    }
    out.push('\n');
    // The separator row (alignment taken from the header cells' align)
    out.push('|');
    for ci in 0..cols {
        let align = match cells.get(ci) {
            Some((_, Block::Td { align: 1, .. })) => ":--",
            Some((_, Block::Td { align: 2, .. })) => ":-:",
            Some((_, Block::Td { align: 3, .. })) => "--:",
            _ => "---",
        };
        out.push_str(align);
        out.push('|');
    }
    out.push('\n');
    // Data rows (padded to the column count)
    for row in rows.iter().skip(1) {
        out.push('|');
        for ci in 0..cols {
            match row.get(ci) {
                Some((k, Block::Td { inlines, .. })) => {
                    out.push(' ');
                    offsets[*k] = out.len();
                    push_cell_text(inlines, out);
                    out.push_str(" |");
                }
                _ => out.push_str("  |"),
            }
        }
        out.push('\n');
    }
}

/// `breaks_ok`: an in-block line break may be emitted (multi-line Para/Quote/Li
/// allow it; Head/Td are single-line and a line break degrades to a space — a
/// line break appearing in the model can only have been brought in by an editing
/// merge).
fn emit_inlines(inlines: &[Inline], in_table: bool, breaks_ok: bool, out: &mut String) {
    emit_inlines_at(inlines, in_table, false, breaks_ok, out)
}

/// `at_block_start`: the emit point is the **content start** of its block (the
/// start of a line inside the block). The content start of a list item or a quote
/// is a block-level context — an unescaped `- ` `>` `1.` and friends would reparse
/// as a nested list or quote (breaking the character-by-character fidelity); a
/// heading or a table cell is a purely inline context and has no such problem, so
/// the minimal escaping does not stack. A container (Strong etc.) emits its `*` /
/// `[` markers first, so its child content is never at the start and always gets
/// false.
fn emit_inlines_at(
    inlines: &[Inline],
    in_table: bool,
    at_block_start: bool,
    breaks_ok: bool,
    out: &mut String,
) {
    // Two neighbouring runs of the same container cannot be written as Markdown:
    // the first one's closing fence and the second one's opening fence glue into a
    // single delimiter run (`**a****b**`, `*a**b*`, `~~a~~~~b~~`, `` `a``b` ``),
    // and every one of those reparses as **one span holding the markers as literal
    // text** — one open+save cycle inserted a visible `****` / `~~` / backtick run
    // into the user's text. Merging the two runs is semantically identical (same
    // styling) and works at every nesting level (this function is the recursion
    // point).
    let merged = merge_adjacent(inlines);
    let inlines = merged.as_slice();
    let mut at_start = at_block_start;
    // The content start has emitted nothing but digits so far (see push_escaped_text): a
    // `1` + `.` split across two Text nodes still spells a list marker at a block start.
    let mut after_digits = at_block_start;
    for (k, i) in inlines.iter().enumerate() {
        // An empty Text emits no bytes and does not consume the "content start" state
        let empty_text = matches!(i, Inline::Text(t) if t.is_empty());
        // Zero-byte nodes (and empty container placeholders) keep the digit-run state; so
        // does a Text that is all digits. Everything else ends it.
        let zero_bytes = matches!(i, Inline::Text(t) if t.is_empty())
            || matches!(i, Inline::Strong(c) | Inline::Em(c) | Inline::Del(c) if c.is_empty())
            || matches!(i, Inline::Link { inlines: c, url } if c.is_empty() && url.is_empty());
        let digits_only =
            matches!(i, Inline::Text(t) if !t.is_empty() && t.chars().all(|c| c.is_ascii_digit()));
        // Keeps the run alive **after** this node is emitted (the node itself still needs
        // the incoming state).
        let keeps_digit_run = zero_bytes || digits_only;
        match i {
            // Zero-length placeholder containers (the intermediate state of an empty-selection
            // wrap, see edit.rs `empty_wrapper`) emit no marker bytes: `****` would reparse as
            // a thematic break (Hr) and `[]()` is not a valid link either — both break the
            // round trip. Emitting an empty string is equivalent to "nothing typed yet", i.e.
            // the placeholder semantics of empty content.
            Inline::Strong(c) if c.is_empty() => {}
            Inline::Em(c) if c.is_empty() => {}
            Inline::Del(c) if c.is_empty() => {}
            // A zero-length placeholder *link* has no url either; `[](url)` is a real
            // link (empty text, non-empty url) and dropping the node deleted the URL on
            // every open+save.
            Inline::Link { inlines: c, url } if c.is_empty() && url.is_empty() => {}
            Inline::Text(t) => {
                // A trailing `!` right before a following link's `[` would spell image syntax;
                // escape it. Look past the zero-byte placeholders (`a!` + `Em([])` + `[x](u)`
                // would otherwise reassemble into `a![x](u)`, i.e. an image).
                let bang_end = matches!(
                    inlines.iter().skip(k + 1).find(|n| !emits_nothing(n)),
                    Some(Inline::Link { .. })
                );
                push_escaped_text(t, in_table, bang_end, at_start, after_digits, out);
            }
            // A soft break emits a single `\n` (parse restores the SoftBreak); a hard break
            // emits two trailing spaces (the backslash form degrades into a literal `\` at the
            // end of a paragraph, and two spaces at worst lose the line break).
            // Single-line blocks (Head/Td) degrade to a space — markdown cannot express an
            // in-block line break.
            Inline::SoftBreak => {
                if breaks_ok {
                    // Two or more trailing spaces plus a line break would reparse as a **hard
                    // break**: rewrite the trailing spaces as `&#32;` (which parses back to a literal
                    // space) so a soft break stays faithful.
                    // at_start = the content has not started yet (the trailing space of markers like
                    // `- ` or `> ` is not content, and touching it would break the marker's escaping).
                    if !at_start {
                        escape_line_end_spaces(out);
                    }
                    out.push('\n');
                } else {
                    out.push(' ');
                }
            }
            Inline::HardBreak => {
                if in_table {
                    // A cell is a single-line block: a line break can only be expressed in HTML.
                    // GFM cells allow inline HTML, and `<br>` round trips verbatim (parse maps it
                    // back to a HardBreak).
                    out.push_str("<br>");
                } else if breaks_ok {
                    // Content spaces directly before the break are stripped on
                    // reparse (pulldown drops trailing whitespace before a line
                    // break), so entity-escape them first and append the two
                    // marker spaces raw — otherwise the marker is what gets
                    // consumed and the break degrades to a soft break.
                    escape_line_end_spaces(out);
                    out.push_str("  \n");
                } else {
                    out.push(' ');
                }
            }
            Inline::Code(c) => {
                let fence = inline_code_fence(c);
                out.push_str(&fence);
                // When the content starts or ends with a backtick or a space, pad the
                // inside (CommonMark convention). Empty content gets **one** space: the
                // shortest code span holds a single space, and padding both sides grew
                // the placeholder from "" to "  " on every round trip.
                let pad = code_span_pad(c);
                if pad {
                    out.push(' ');
                }
                // A pipe inside a table breaks the column structure; pulldown_cmark restores `\|`
                // when it splits the cells per the GFM spec (including inside code spans), so the
                // round trip is consistent
                if in_table {
                    push_escaped_pipes(c, out);
                } else {
                    out.push_str(c);
                }
                if pad && !c.is_empty() {
                    out.push(' ');
                }
                out.push_str(&fence);
            }
            // The next sibling's first emitted character decides whether this
            // container's closer can be right-flanking (see `emit_container`).
            Inline::Strong(c) => {
                emit_container("**", c, in_table, breaks_ok, next_first(inlines, k), out)
            }
            Inline::Em(c) => {
                emit_container("*", c, in_table, breaks_ok, next_first(inlines, k), out)
            }
            Inline::Del(c) => {
                emit_container("~~", c, in_table, breaks_ok, next_first(inlines, k), out)
            }
            Inline::Link { url, inlines: c } => {
                out.push('[');
                emit_inlines(c, in_table, breaks_ok, out);
                out.push_str("](");
                emit_url(url, out);
                out.push(')');
            }
            Inline::Image { url, alt } => {
                out.push_str("![");
                // Escape the brackets in the alt by CommonMark's rules
                for ch in alt.chars() {
                    if ch == '[' || ch == ']' {
                        out.push('\\');
                    }
                    out.push(ch);
                }
                out.push_str("](");
                emit_url(url, out);
                out.push(')');
            }
            Inline::Math { src, display } => {
                if *display {
                    // Block form: pulldown's DisplayMath text **already carries** its leading and
                    // trailing line breaks (the src of `$$` … `$$` is a string that starts and ends
                    // with a newline), so joining it straight between the `$$` is the original shape;
                    // adding another pair of line breaks would push blank lines into the formula and
                    // it would reparse as an ordinary paragraph (verified on a real machine: one
                    // round trip turned the formula into a `$$` text line).
                    out.push_str("$$");
                    if in_table {
                        push_escaped_pipes(src, out);
                    } else {
                        out.push_str(src);
                    }
                    out.push_str("$$");
                } else if src.trim().is_empty() {
                    // Inline math needs content that does not start or end with whitespace
                    // (`$ $` is literal text): writing the markers anyway injected two `$`
                    // into the user's text, so a whitespace-only span degrades to its
                    // (whitespace) content — the same rule as an empty container.
                } else {
                    out.push('$');
                    if in_table {
                        push_escaped_pipes(src, out);
                    } else {
                        out.push_str(src);
                    }
                    out.push('$');
                }
            }
        }
        if !empty_text {
            at_start = false;
        }
        after_digits = after_digits && keeps_digit_run;
    }
}

/// Escaping the pipe inside a table cell (`\|`).
fn push_escaped_pipes(s: &str, out: &mut String) {
    for c in s.chars() {
        if c == '|' {
            out.push('\\');
        }
        out.push(c);
    }
}

/// Emit a table cell's content. GFM strips the cell's leading and trailing
/// whitespace (`parse_table_row_inner` / `parse_line`), so those runs are
/// entity-escaped the same way block-end spaces are — a cell holding "a " used
/// to come back as "a".
fn push_cell_text(inlines: &[Inline], out: &mut String) {
    let mut cell = String::new();
    emit_inlines(inlines, true, false, &mut cell);
    let trimmed = cell.trim_matches(' ');
    let lead = cell.len() - cell.trim_start_matches(' ').len();
    let trail = cell.len() - lead - trimmed.len();
    for _ in 0..lead {
        out.push_str("&#32;");
    }
    out.push_str(trimmed);
    for _ in 0..trail {
        out.push_str("&#32;");
    }
}

/// The first character the inline at `k` emits (approximate: only a `Text` can
/// start with an alphanumeric — every other node starts with punctuation: `*`, `~`,
/// `` ` ``, `$`, `!`, `[`). `None` = end of the block (a line end).
fn first_emitted_char(i: &Inline) -> Option<char> {
    match i {
        Inline::Text(t) => t.chars().next(),
        Inline::Code(_) => Some('`'),
        Inline::Math { .. } => Some('$'),
        Inline::Image { .. } => Some('!'),
        Inline::Link { .. } => Some('['),
        Inline::Strong(_) | Inline::Em(_) => Some('*'),
        Inline::Del(_) => Some('~'),
        Inline::SoftBreak | Inline::HardBreak => Some('\n'),
    }
}

/// The first emitted character after the inline at index `k` (zero-byte
/// placeholders skipped): the character that follows this node in the output.
fn next_first(inlines: &[Inline], k: usize) -> Option<char> {
    inlines[k + 1..]
        .iter()
        .find(|n| !emits_nothing(n))
        .and_then(first_emitted_char)
}

/// Characters that the emphasis flanking rules treat as "not alphanumeric" — any
/// symbol, including Unicode punctuation (`!`, `=`, `（`, emoji …). Approximated by
/// "neither whitespace nor alphanumeric"; only used to decide whether an edge has
/// to be moved outside the markers.
fn is_flanking_symbol(c: char) -> bool {
    !c.is_whitespace() && !c.is_alphanumeric()
}

/// One unbreakable unit of emitted inline text: `(byte length, the character the
/// reparse sees)`. A unit is a backslash escape (``\*`` → `*`), a character entity
/// (`&#32;` → a space) or a single character. Splitting a unit apart corrupts the
/// output — slicing after the `&#32` of an entity produced `*&#32*;`, i.e. a literal
/// `&#32;` inserted into the user's text.
fn emit_tokens(s: &str) -> Vec<(usize, char)> {
    let cs: Vec<char> = s.chars().collect();
    let mut out: Vec<(usize, char)> = Vec::with_capacity(cs.len());
    let mut i = 0;
    while i < cs.len() {
        let c = cs[i];
        if c == '\\' {
            if let Some(&n) = cs.get(i + 1) {
                out.push((1 + n.len_utf8(), n));
                i += 2;
                continue;
            }
        }
        if c == '&' {
            // Only the escapes this module emits are decoded; anything else stays one
            // opaque symbol token so it can never be cut in half.
            if let Some(j) = (i + 1..cs.len()).find(|&j| cs[j] == ';' && j - i <= 12) {
                let rep = match &cs[i..=j] {
                    [_, _, '3', '2', ';'] => Some(' '),
                    [_, _, '9', ';'] => Some('\t'),
                    _ => None,
                };
                let len: usize = cs[i..=j].iter().map(|c| c.len_utf8()).sum();
                out.push((len, rep.unwrap_or('\u{0}')));
                i = j + 1;
                continue;
            }
        }
        out.push((c.len_utf8(), c));
        i += 1;
    }
    out
}

/// Byte length of the leading (`from_start`) or trailing run of non-alphanumeric,
/// non-whitespace characters in `s` (whole units only, see [`emit_tokens`]).
fn symbol_run(s: &str, from_start: bool) -> usize {
    let toks = emit_tokens(s);
    let n = if from_start {
        toks.iter()
            .take_while(|(_, c)| is_flanking_symbol(*c))
            .map(|(len, _)| len)
            .sum()
    } else {
        toks.iter()
            .rev()
            .take_while(|(_, c)| is_flanking_symbol(*c))
            .map(|(len, _)| len)
            .sum()
    };
    n
}

/// Byte length of the leading (`from_start`) or trailing whitespace run of `s`,
/// counting entity-encoded spaces (`&#32;`) as whitespace too.
fn whitespace_run(s: &str, from_start: bool) -> usize {
    let toks = emit_tokens(s);
    if from_start {
        toks.iter()
            .take_while(|(_, c)| c.is_whitespace())
            .map(|(len, _)| len)
            .sum()
    } else {
        toks.iter()
            .rev()
            .take_while(|(_, c)| c.is_whitespace())
            .map(|(len, _)| len)
            .sum()
    }
}

/// Emit one emphasis-like container (`marker` … `marker`) so that CommonMark's
/// flanking rules still recognise it after the round trip.
///
/// Three things defeat a naive `**` + body + `**`:
/// * an opener followed by whitespace and a closer preceded by one are literal
///   (`** x**`), so bolding a selection that includes the trailing space silently
///   lost the bold on the first save;
/// * an opener followed by **punctuation** does not open when the character before
///   it is alphanumeric (`a**!b**` is literal);
/// * a closer preceded by punctuation does not close when the next character is
///   alphanumeric (`**a.**b` is literal).
///
/// The edge whitespace / punctuation is therefore emitted on the **other side** of
/// the markers: the plain text is byte-for-byte unchanged (only the styling of
/// those edge characters is lost, which is invisible for spaces and punctuation).
fn emit_container(
    marker: &str,
    inlines: &[Inline],
    in_table: bool,
    breaks_ok: bool,
    next: Option<char>,
    out: &mut String,
) {
    // Emit the body **in place** first (a temporary buffer would lose the
    // line-start state push_escaped_text relies on), then move the edges out.
    let body_start = out.len();
    emit_inlines(inlines, in_table, breaks_ok, out);
    let body = out[body_start..].to_string();
    // An entity-encoded space (`&#32;`) is whitespace content too: leaving it inside
    // the markers makes the opener followed by punctuation, which does not open when
    // the character before it is alphanumeric — `a*x&#32;*` would then reparse as the
    // literal text `a*x *`, adding marker bytes to the document.
    let lead = whitespace_run(&body, true);
    let trail = whitespace_run(&body, false);
    if lead + trail >= body.len() {
        // Only whitespace inside: `**  **` and `**\n**` are literal text in every
        // position, so the container degrades to its (whitespace) content.
        return;
    }
    let mut core = &body[lead..body.len() - trail];
    // The opener is preceded by an alphanumeric → its first character must be one too.
    let before_alnum = out[..body_start]
        .chars()
        .next_back()
        .is_some_and(char::is_alphanumeric);
    let mut left = "";
    if before_alnum {
        let n = symbol_run(core, true);
        left = &core[..n];
        core = &core[n..];
    }
    // The closer is followed by an alphanumeric → its last character must be one too.
    let mut right = "";
    if next.is_some_and(char::is_alphanumeric) {
        let n = symbol_run(core, false);
        right = &core[core.len() - n..];
        core = &core[..core.len() - n];
    }
    out.truncate(body_start);
    // The moved edges are written back **verbatim**: they are already escaped
    // Markdown (they come from the emitted body) and their position in the output
    // is unchanged, so re-escaping them would double every backslash — a `Del`
    // around a ``_`` emitted `b\\\_ c`, which reparsed as a literal backslash and
    // lost a byte of the user's text on save.
    if lead > 0 {
        out.push_str(&body[..lead]);
    }
    if !left.is_empty() {
        out.push_str(left);
    }
    if !core.is_empty() {
        out.push_str(marker);
        out.push_str(core);
        out.push_str(marker);
    }
    if !right.is_empty() {
        out.push_str(right);
    }
    out.push_str(&body[body.len() - trail..]);
}

/// Merge adjacent same-kind containers before emitting (see the note in
/// [`emit_inlines_at`]). Zero-byte placeholders (`Strong([])` from an
/// empty-selection wrap) are skipped first so they cannot block a merge — emit
/// writes no bytes for them anyway.
///
/// Code spans have no alternative delimiter, so joining two neighbouring spans is
/// the only lossless option: the plain text is unchanged (both spans were already
/// adjacent); only the visual gap between two `<code>` runs disappears.
fn merge_adjacent(inlines: &[Inline]) -> Vec<Inline> {
    let mut out: Vec<Inline> = Vec::with_capacity(inlines.len());
    for node in inlines {
        if emits_nothing(node) {
            continue;
        }
        match (out.last_mut(), node) {
            (Some(Inline::Strong(prev)), Inline::Strong(c))
            | (Some(Inline::Em(prev)), Inline::Em(c))
            | (Some(Inline::Del(prev)), Inline::Del(c)) => prev.extend(c.iter().cloned()),
            (Some(Inline::Code(prev)), Inline::Code(c)) => prev.push_str(c),
            _ => out.push(node.clone()),
        }
    }
    out
}

/// True for inline nodes that emit **zero bytes**: the empty placeholder
/// containers of an empty-selection wrap. Text/Image/Math always emit
/// something, so only the empty containers and empty text qualify.
fn emits_nothing(i: &Inline) -> bool {
    match i {
        Inline::Text(t) => t.is_empty(),
        Inline::Strong(c) | Inline::Em(c) | Inline::Del(c) => c.is_empty(),
        Inline::Link { inlines: c, url } => c.is_empty() && url.is_empty(),
        _ => false,
    }
}

/// Rewriting the trailing spaces before a soft break: two or more raw trailing
/// spaces plus a line break would reparse as a **hard break** (keeping the soft
/// break faithful). Rewrite the last trailing space as the entity `&#32;` (which
/// parses back to a literal space; `\ ` is not a CommonMark escape — a space is
/// not punctuation and the result would just add a literal backslash), so the end
/// of the line no longer has ≥2 raw whitespace and the break is necessarily soft.
///
/// Every trailing space is content: pulldown strips **all** of them before a line
/// break, so a single one has to be escaped too (it used to be left alone and
/// simply vanished on reparse).
fn escape_line_end_spaces(out: &mut String) {
    let n = out.len() - out.trim_end_matches(' ').len();
    if n >= 1 {
        out.pop();
        out.push_str("&#32;");
    }
}

/// Escape **every** trailing space of the text emitted for the current block.
///
/// CommonMark strips whitespace at a line end, so a model text ending in `" "`
/// would come back one byte shorter after a save/reload — and every offset after
/// it would shift (the caret that rides on those offsets visibly jumps). Unlike
/// [`escape_line_end_spaces`] (before a soft break, where a single space is
/// treated as invisible), a block-final space is real content.
fn escape_block_end_spaces(out: &mut String) {
    let n = out.len() - out.trim_end_matches(' ').len();
    if n == 0 {
        return;
    }
    out.truncate(out.len() - n);
    for _ in 0..n {
        out.push_str("&#32;");
    }
}

/// The same for one already-emitted line (list items and other multi-line bodies).
fn escape_trailing_spaces(line: &str) -> std::borrow::Cow<'_, str> {
    let n = line.len() - line.trim_end_matches(' ').len();
    if n == 0 {
        return std::borrow::Cow::Borrowed(line);
    }
    let mut s = String::with_capacity(line.len() + n * 4);
    s.push_str(&line[..line.len() - n]);
    for _ in 0..n {
        s.push_str("&#32;");
    }
    std::borrow::Cow::Owned(s)
}

/// Minimal backslash escaping for Text. The principle: Text content must survive
/// parse → emit → parse character for character (idempotent), while escaping as
/// little as possible so it stays readable.
///
/// * Inline trigger characters are always escaped: ``\ ` * _ [ ] # ~ < $`` (`#`
///   only opens a block at a line start, but is always escaped to avoid coupling to
///   position; `$` is the formula delimiter; `[` is always escaped so that both the
///   `!`+`[` image splice and the `]` label truncation stop working);
/// * An `&` in HTML-entity form (`&amp;` / `&#32;` etc.) is escaped — otherwise
///   the reparse decodes it into another character;
/// * At a line start (column 0, or the content start of a block, `block_start`)
///   block-level triggers `-` `+` `=` `>` are backslash-escaped, and the `.` / `)`
///   after `digits.` / `digits)` are escaped (digits cannot be backslash-escaped);
///   a leading space or tab is kept with the entity `&#32;` / `&#9;` (a `\ ` is
///   not an escape and would add a backslash, and the indentation would also get
///   eaten into an indented code block);
/// * Inside a table the `|` escape is added on top.
///
/// Known edges (markdown layout semantics, not trigger characters): trailing
/// spaces at the end of a paragraph and the blank line at the end of the document
/// are not preserved (block-level trailing whitespace is trimmed by the parser
/// anyway); a `digits.` assembled across Text nodes is not checked.
fn push_escaped_text(
    t: &str,
    in_table: bool,
    bang_end: bool,
    block_start: bool,
    after_digits: bool,
    out: &mut String,
) {
    let cs: Vec<char> = t.chars().collect();
    // The output is at column 0 of a line, or at the content start of its block (the
    // start of a line inside a block)
    let mut line_start = block_start || out.is_empty() || out.ends_with('\n');
    // The text emitted so far at this block's content start is a digit run: a `.` / `)`
    // starting this node would still complete a list marker (`1` + `.` split across two
    // Text nodes — the model splits text on every inline command — emitted `1.` and one
    // save turned the paragraph into an ordered list).
    let mut after_digits = after_digits;
    let mut i = 0;
    while i < cs.len() {
        let c = cs[i];
        if c == '\n' {
            out.push('\n');
            line_start = true;
            after_digits = false;
            i += 1;
            continue;
        }
        if after_digits && matches!(c, '.' | ')') {
            out.push('\\');
            out.push(c);
            after_digits = false;
            line_start = false;
            i += 1;
            continue;
        }
        if line_start {
            line_start = false;
            if c == ' ' || c == '\t' {
                // Leading whitespace on a line: keep it literally with an entity (the content
                // still reparses to whitespace)
                out.push_str(if c == ' ' { "&#32;" } else { "&#9;" });
                after_digits = false;
                i += 1;
                continue;
            }
            if matches!(c, '-' | '+' | '=' | '>') {
                out.push('\\');
                out.push(c);
                after_digits = false;
                i += 1;
                continue;
            }
            if c.is_ascii_digit() {
                // A `1.` / `1)` list marker: scan past the digit run and escape the `.` / `)`
                // after it
                let mut j = i;
                while j < cs.len() && cs[j].is_ascii_digit() {
                    j += 1;
                }
                if matches!(cs.get(j), Some('.') | Some(')')) {
                    out.extend(&cs[i..j]);
                    out.push('\\');
                    out.push(cs[j]);
                    after_digits = false;
                    i = j + 1;
                    continue;
                }
                // The run may continue in a following Text node (see `after_digits`)
                after_digits = true;
            }
        }
        match c {
            '\\' | '`' | '*' | '_' | '[' | ']' | '#' | '~' | '<' | '$' => {
                out.push('\\');
                out.push(c);
            }
            '&' if entity_ahead(&cs, i) => {
                out.push('\\');
                out.push('&');
            }
            '!' if bang_end && i + 1 == cs.len() => {
                out.push('\\');
                out.push('!');
            }
            '|' if in_table => {
                out.push('\\');
                out.push('|');
            }
            _ => out.push(c),
        }
        after_digits = after_digits && c.is_ascii_digit();
        i += 1;
    }
}

/// Whether an `&` would form an HTML entity (`&amp;` / `&#32;` / `&#x1F;` …) —
/// emitting it verbatim would make the reparse decode it into a different
/// character. Lenient test: `&` + alphanumeric or a `#x` prefix + `;`.
fn entity_ahead(cs: &[char], i: usize) -> bool {
    let mut j = i + 1;
    if cs.get(j) == Some(&'#') {
        j += 1;
        if matches!(cs.get(j), Some('x') | Some('X')) {
            j += 1;
        }
    }
    let start = j;
    while j < cs.len() && j - start < 32 && cs[j].is_ascii_alphanumeric() {
        j += 1;
    }
    j > start && cs.get(j) == Some(&';')
}

/// URL output: aligned with the character rules of `preview::safe_link_url` —
/// `<` `>` `"` and control characters (including newlines) are percent-encoded
/// (otherwise the angle-bracket form / bare form gets truncated and the whole link
/// reparses into literal text); a URL containing spaces or parentheses is wrapped
/// in angle brackets (the CommonMark destination form). Encoded characters survive
/// the round trip as percent form (URL-wise equivalent).
fn emit_url(url: &str, out: &mut String) {
    let mut enc = String::with_capacity(url.len());
    for c in url.chars() {
        if c == '<' || c == '>' || c == '"' || c.is_control() {
            for b in c.to_string().as_bytes() {
                enc.push_str(&format!("%{b:02X}"));
            }
        } else {
            enc.push(c);
        }
    }
    if enc.contains([' ', '(', ')']) {
        out.push('<');
        out.push_str(&enc);
        out.push('>');
    } else {
        out.push_str(&enc);
    }
}

/// The longest run of consecutive `marker` characters in `s`.
fn max_run_of(marker: char, s: &str) -> usize {
    let mut max = 0;
    let mut cur = 0;
    for c in s.chars() {
        if c == marker {
            cur += 1;
            max = max.max(cur);
        } else {
            cur = 0;
        }
    }
    max
}

/// Inline code fence: the length is the longest run of backticks in the content
/// plus 1. The old rule chose between "contains ```" and "contains a single
/// backtick", so content of one backtick produced a five-backtick fence, and
/// content containing two backticks made the fence stick to the content — it
/// reparsed as literal text or cut the content at the wrong place.
///
/// Shared with the clipboard HTML→Markdown converter, which grew its own
/// (wrong) copy of this rule.
pub(crate) fn inline_code_fence(content: &str) -> String {
    "`".repeat(max_run_of('`', content) + 1)
}

/// Padding inside a code span: content starting or ending with a backtick sticks
/// to the fence, and CommonMark strips one space from each end when both ends of
/// the content are spaces — pad one space on each side to separate and cancel
/// both, keeping the content faithful character by character. Empty content is not
/// representable (the shortest code span's content is a single space), so it
/// degrades to a single space.
pub(crate) fn code_span_pad(content: &str) -> bool {
    content.is_empty()
        || content.starts_with('`')
        || content.ends_with('`')
        || (content.starts_with(' ')
            && content.ends_with(' ')
            && !content.chars().all(|c| c == ' '))
}

fn code_fence(content: &str) -> String {
    fence_of('`', content, "")
}

/// A block fence for `marker`, long enough to outrun any run of `marker` in the
/// content **and** in the info string. Block fences have a floor of 3
/// (CommonMark): sharing the "longest + 1" rule with inline fences would emit a
/// single-backtick fence when the content has no backticks, downgrading the
/// whole block into an inline code span (the round trip breaks outright).
///
/// A backtick fence's info string may not contain a backtick (pulldown rejects
/// such a line as a fence opener), so a language tag carrying one has to use a
/// tilde fence — see the `Code` branch of [`emit_block`].
fn fence_of(marker: char, content: &str, info: &str) -> String {
    let runs = max_run_of(marker, content)
        .max(max_run_of(marker, info))
        .max(2)
        + 1;
    marker.to_string().repeat(runs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Pos;

    fn blocks(src: &str) -> Vec<Block> {
        parse(src).blocks
    }

    /// `<br>` inside a cell: parse maps it to a HardBreak (not literal text) and emit
    /// sends `<br>` back verbatim — Enter's line break inside a cell round trips on
    /// it.
    #[test]
    fn table_cell_br_roundtrips() {
        let src = "| a<br>b | c |\n|---|---|\n";
        let doc = parse(src);
        let has_break = matches!(&doc.blocks[0], Block::Td { inlines, .. }
            if inlines.iter().any(|i| matches!(i, Inline::HardBreak)));
        assert!(
            has_break,
            "the cell's <br> was not parsed into a HardBreak: {:?}",
            doc.blocks[0]
        );
        let out = emit(&doc);
        assert!(out.contains("| a<br>b |"), "{out}");
        assert_eq!(parse(&out), doc, "roundtrip: {out:?}");
        // Variants and case
        for v in ["<br/>", "<BR>", "<br />"] {
            let d = parse(&format!("| x{v}y |\n|---|\n"));
            assert!(
                matches!(&d.blocks[0], Block::Td { inlines, .. }
                    if inlines.iter().any(|i| matches!(i, Inline::HardBreak))),
                "{v} was not recognized: {:?}",
                d.blocks[0]
            );
        }
        // Tags with attributes / other tags are still literal text
        for v in ["<br class=x>", "<hr>"] {
            let d = parse(&format!("| x{v}y |\n|---|\n"));
            assert!(
                !matches!(&d.blocks[0], Block::Td { inlines, .. }
                    if inlines.iter().any(|i| matches!(i, Inline::HardBreak))),
                "{v} was misjudged as a line break"
            );
        }
    }

    /// block_offsets must agree with emit: the same length, every offset on a character
    /// boundary, and the first block pointing at the **content's first byte** (a
    /// heading skips `# ` — the host bridge uses it to align the piece-table caret
    /// with the wys focus block; the marker's start would land a Chinese heading's
    /// offset in the middle of a character).
    #[test]
    fn block_offsets_align_with_emit() {
        let doc = parse("# 标题\n\n正文段落\n\n- 甲\n- 乙\n\n```\ncode\n```\n");
        let md = emit(&doc);
        let offs = block_offsets(&doc);
        assert_eq!(offs.len(), doc.blocks.len());
        assert_eq!(offs[0], 2, "the heading offset should skip \"# \"");
        assert!(md[offs[0]..].starts_with("标题"), "{:?}", &md[offs[0]..]);
        for (i, o) in offs.iter().enumerate() {
            assert!(
                md.is_char_boundary(*o),
                "block {i} offset {o} is not on a character boundary"
            );
        }
        // The list's second item points at the item's content (skipping "- "), not the marker's start
        assert!(
            md[offs[3]..].starts_with("乙"),
            "the list's second item offset {} should point at the content: {:?}",
            offs[3],
            &md[offs[3]..]
        );
        // The code block points at the first content line after the fence line
        assert!(
            md[offs[4]..].starts_with("code"),
            "the code block's offset {} should skip the fence line: {:?}",
            offs[4],
            &md[offs[4]..]
        );
    }

    #[test]
    fn parse_para_head_hr() {
        let bs = blocks("# T1\n\nhello *world*\n\n---\n");
        assert_eq!(bs.len(), 3);
        assert_eq!(
            bs[0],
            Block::Head {
                level: 1,
                inlines: vec![Inline::Text("T1".into())]
            }
        );
        assert_eq!(
            bs[1],
            Block::Para(vec![
                Inline::Text("hello ".into()),
                Inline::Em(vec![Inline::Text("world".into())]),
            ])
        );
        assert_eq!(bs[2], Block::Hr);
    }

    #[test]
    fn parse_list_ordered_and_unordered() {
        let bs = blocks("- a\n- b\n\n1. x\n2. y\n");
        assert_eq!(bs.len(), 4);
        assert_eq!(
            bs[0],
            Block::Li {
                ordered: false,
                num: 0,
                level: 0,
                task: None,
                inlines: vec![Inline::Text("a".into())]
            }
        );
        assert_eq!(
            bs[2],
            Block::Li {
                ordered: true,
                num: 1,
                level: 0,
                task: None,
                inlines: vec![Inline::Text("x".into())]
            }
        );
        assert_eq!(
            bs[3],
            Block::Li {
                ordered: true,
                num: 2,
                level: 0,
                task: None,
                inlines: vec![Inline::Text("y".into())]
            }
        );
    }

    #[test]
    fn parse_nested_list_levels() {
        let bs = blocks("- a\n  - b\n    - c\n- d\n");
        assert_eq!(bs.len(), 4);
        let levels: Vec<u8> = bs
            .iter()
            .map(|b| match b {
                Block::Li { level, .. } => *level,
                _ => 99,
            })
            .collect();
        assert_eq!(levels, vec![0, 1, 2, 0]);
    }

    #[test]
    fn parse_ordered_start_number() {
        let bs = blocks("3. x\n4. y\n");
        assert_eq!(
            bs[0],
            Block::Li {
                ordered: true,
                num: 3,
                level: 0,
                task: None,
                inlines: vec![Inline::Text("x".into())]
            }
        );
        assert_eq!(
            bs[1],
            Block::Li {
                ordered: true,
                num: 4,
                level: 0,
                task: None,
                inlines: vec![Inline::Text("y".into())]
            }
        );
    }

    #[test]
    fn parse_task_list() {
        let bs = blocks("- [ ] todo\n- [x] done\n");
        assert_eq!(bs.len(), 2);
        assert_eq!(
            bs[0],
            Block::Li {
                ordered: false,
                num: 0,
                level: 0,
                task: Some(false),
                inlines: vec![Inline::Text("todo".into())]
            }
        );
        assert_eq!(
            bs[1],
            Block::Li {
                ordered: false,
                num: 0,
                level: 0,
                task: Some(true),
                inlines: vec![Inline::Text("done".into())]
            }
        );
    }

    #[test]
    fn parse_link_and_image() {
        let bs = blocks("[txt](https://x.y) and ![alt text](/img.png)");
        let Block::Para(inl) = &bs[0] else {
            panic!("not para")
        };
        assert_eq!(
            inl[0],
            Inline::Link {
                url: "https://x.y".into(),
                inlines: vec![Inline::Text("txt".into())]
            }
        );
        assert_eq!(
            inl[2],
            Inline::Image {
                url: "/img.png".into(),
                alt: "alt text".into()
            }
        );
    }

    #[test]
    fn parse_link_with_emphasis() {
        let bs = blocks("[**b** t](u)");
        let Block::Para(inl) = &bs[0] else {
            panic!("not para")
        };
        assert_eq!(
            inl[0],
            Inline::Link {
                url: "u".into(),
                inlines: vec![
                    Inline::Strong(vec![Inline::Text("b".into())]),
                    Inline::Text(" t".into())
                ]
            }
        );
    }

    #[test]
    fn parse_math() {
        let bs = blocks("a $x+y$ b\n\n$$z^2$$\n");
        assert_eq!(bs.len(), 2);
        let Block::Para(inl) = &bs[0] else {
            panic!("not para")
        };
        assert_eq!(
            inl[1],
            Inline::Math {
                src: "x+y".into(),
                display: false
            }
        );
        let Block::Para(inl2) = &bs[1] else {
            panic!("not para")
        };
        assert_eq!(
            inl2[0],
            Inline::Math {
                src: "z^2".into(),
                display: true
            }
        );
    }

    // ---- Math brace shielding (escaped-LaTeX compatibility) ----

    /// pulldown pairs math delimiters only inside a balanced brace context, so
    /// the escaped-LaTeX shape `$\left{x\right}$` used to stay literal text.
    /// The shield hides the braces from the block scanner; the model still
    /// carries the original source, braces included.
    #[test]
    fn math_with_unbalanced_brace_is_recognized() {
        let src = "a $\\left{x\\right}$ b\n";
        let bs = blocks(src);
        let Block::Para(inl) = &bs[0] else {
            panic!("not a para: {bs:?}")
        };
        assert_eq!(
            inl[1],
            Inline::Math {
                src: "\\left{x\\right}".into(),
                display: false
            },
            "{inl:?}"
        );
        assert_eq!(emit(&parse(src)), src, "the brace shield must not leak");
    }

    /// Display math delimited on its own lines: the closer has no whitespace
    /// rule there, and its braces are shielded as well.
    #[test]
    fn display_math_with_unbalanced_brace_is_recognized() {
        let src = "$$\n\\left{x\\right.\n$$\n";
        let bs = blocks(src);
        let Block::Para(inl) = &bs[0] else {
            panic!("not a para: {bs:?}")
        };
        let Inline::Math {
            src: inner,
            display,
        } = &inl[0]
        else {
            panic!("not math: {inl:?}")
        };
        assert!(*display);
        assert_eq!(inner.trim(), "\\left{x\\right.");
        assert_eq!(emit(&parse(src)), src);
    }

    /// One unbalanced formula must not shift the `$` pairing of its neighbours.
    /// Before the shield, pulldown swallowed the broken pair as text and then
    /// handed the *following* formula's delimiters to the wrong span, so a
    /// document lost one formula for every unbalanced one.
    #[test]
    fn unbalanced_formula_does_not_shift_later_delimiters() {
        let src = r"甲 $\left{a\right.$ 乙 $x$ 丙 $\left{b\right.$ 丁";
        let bs = blocks(src);
        let Block::Para(inl) = &bs[0] else {
            panic!("not a para: {bs:?}")
        };
        let maths: Vec<&str> = inl
            .iter()
            .filter_map(|i| match i {
                Inline::Math { src, .. } => Some(src.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(maths, [r"\left{a\right.", "x", r"\left{b\right."]);
        assert_eq!(emit(&parse(src)), format!("{src}\n"));
    }

    /// The shield is undone everywhere, not just inside math nodes: a code span
    /// that merely mentions dollars keeps its braces byte for byte.
    #[test]
    fn shield_restores_braces_outside_math() {
        let src = "x `$a{b$` y\n";
        let bs = blocks(src);
        let Block::Para(inl) = &bs[0] else {
            panic!("not a para: {bs:?}")
        };
        assert!(
            inl.iter()
                .any(|i| matches!(i, Inline::Code(t) if t == "$a{b$")),
            "{inl:?}"
        );
        assert_eq!(emit(&parse(src)), src);
    }

    /// The document that drove the shield: the 2019 SUFE analysis exam's first
    /// problem, verbatim from the file. Its `\left{…\right.` pair used to fall
    /// out of the math channel, so the paragraph rendered raw TeX.
    #[test]
    fn escaped_array_formula_parses_as_math() {
        let src = r"满足 $\\left{\\begin{array}{l}x\_n \\ y\_n\\end{array}\\right.$, 求 $\left{x\_n\right}$";
        let bs = blocks(src);
        let Block::Para(inl) = &bs[0] else {
            panic!("not a para: {bs:?}")
        };
        let maths: Vec<&str> = inl
            .iter()
            .filter_map(|i| match i {
                Inline::Math {
                    src,
                    display: false,
                } => Some(src.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(maths.len(), 2, "{inl:?}");
        assert!(maths[0].starts_with("\\\\left{"), "{maths:?}");
        assert_eq!(maths[1], r"\left{x\_n\right}");
        assert_eq!(emit(&parse(src)), format!("{src}\n"), "round trip");
    }

    #[test]
    fn parse_table_with_align() {
        let bs = blocks("| a | b | c |\n|:--|:-:|--:|\n| 1 | 2 | 3 |\n");
        assert_eq!(bs.len(), 6);
        let all_td = bs.iter().all(|b| matches!(b, Block::Td { .. }));
        assert!(all_td);
        let seqs: Vec<u64> = bs
            .iter()
            .map(|b| match b {
                Block::Td { table_seq, .. } => *table_seq,
                _ => 99,
            })
            .collect();
        assert!(seqs.windows(2).all(|w| w[0] == w[1]));
        // The first row (the header) alignment: left / centre / right
        assert_eq!(
            bs[0],
            Block::Td {
                table_seq: seqs[0],
                row: 0,
                align: 1,
                inlines: vec![Inline::Text("a".into())]
            }
        );
        assert_eq!(
            bs[1],
            Block::Td {
                table_seq: seqs[1],
                row: 0,
                align: 2,
                inlines: vec![Inline::Text("b".into())]
            }
        );
        assert_eq!(
            bs[2],
            Block::Td {
                table_seq: seqs[2],
                row: 0,
                align: 3,
                inlines: vec![Inline::Text("c".into())]
            }
        );
        // Data row row=1
        assert_eq!(
            bs[3],
            Block::Td {
                table_seq: seqs[3],
                row: 1,
                align: 1,
                inlines: vec![Inline::Text("1".into())]
            }
        );
    }

    #[test]
    fn parse_quote_and_code() {
        let bs = blocks("> quoted\n\n```rust\nfn a() {}\n```\n");
        assert_eq!(bs[0], Block::Quote(vec![Inline::Text("quoted".into())]));
        assert_eq!(
            bs[1],
            Block::Code {
                lang: Some("rust".into()),
                text: "fn a() {}".into()
            }
        );
    }

    #[test]
    fn parse_nested_emphasis_and_code() {
        let bs = blocks("**bold *inner* end** and `c1`");
        let Block::Para(inl) = &bs[0] else {
            panic!("not para")
        };
        assert_eq!(
            inl[0],
            Inline::Strong(vec![
                Inline::Text("bold ".into()),
                Inline::Em(vec![Inline::Text("inner".into())]),
                Inline::Text(" end".into())
            ])
        );
        assert_eq!(inl[1], Inline::Text(" and ".into()));
        assert_eq!(inl[2], Inline::Code("c1".into()));
    }

    /// Offset consistency: a `Pos`'s off must land inside the block's plain-text
    /// range, and the concatenated plain must match the source.
    #[test]
    fn offsets_match_plain_text() {
        let doc = parse("ab **cd** ef\n\nx");
        assert_eq!(doc.blocks[0].plain_text(), "ab cd ef");
        let len = doc.blocks[0].plain_text().len();
        assert_eq!(Pos::new(0, len).clamped(&doc), Pos::new(0, len));
        assert_eq!(Pos::new(0, len + 1).clamped(&doc), Pos::new(0, len));
    }

    #[test]
    fn emit_roundtrip_stable() {
        let src = "# Title\n\nSome *em* and **strong** with `code`.\n\n- a\n- b\n\n1. one\n2. two\n\n> quote\n\n```rust\nfn x() {}\n```\n\n---\n";
        let doc = parse(src);
        let out = emit(&doc);
        // Idempotence: emit(parse(emit(parse(x)))) == emit(parse(x))
        let out2 = emit(&parse(&out));
        assert_eq!(out, out2);
    }

    /// A sublist nested under an ordered list: the indent must be at least the parent
    /// item's content column (`1. ` = 3), otherwise the reparse downgrades it to a
    /// sibling list — one round trip flattens the nesting (saving would corrupt the
    /// structure).
    /// A block-level formula inside a list item (multi-line content) must have its
    /// continuation lines indented to the item's content column: a `$$` line without
    /// the indentation would reparse as a block boundary and the whole list shell
    /// would be lost.
    #[test]
    fn list_item_display_math_roundtrips() {
        let src = "- $$\n  \\int_0^1 x\\,dx\n  $$\n\n1. a\n2. b\n";
        let doc = parse(src);
        assert!(
            matches!(&doc.blocks[0], Block::Li { inlines, .. }
                if matches!(inlines.first(), Some(Inline::Math { display: true, .. }))),
            "the first block should be a list item containing a block-level formula: {:?}",
            doc.blocks[0]
        );
        let out = emit(&doc);
        assert!(
            out.starts_with("- $$\n  "),
            "continuation lines must be indented to the item's content column: {out:?}"
        );
        assert!(
            out.contains("\n  $$\n"),
            "the closing `$$` is indented too: {out:?}"
        );
        // The block's start offset still points at the content's first byte (the continuation indentation must not shift it past the first line)
        let offs = block_offsets(&doc);
        assert!(
            out[offs[0]..].starts_with("$$"),
            "offsets={offs:?} out={out:?}"
        );
        let re = parse(&out);
        assert_eq!(
            re.blocks, doc.blocks,
            "the list structure was lost in the round trip: {out:?}"
        );
        assert_eq!(emit(&re), out);
    }

    #[test]
    fn emit_roundtrip_nested_under_ordered() {
        let src = "1. x\n   - a\n   - b\n2. y\n";
        let doc = parse(src);
        let out = emit(&doc);
        let re = parse(&out);
        assert_eq!(emit(&re), out, "roundtrip must be idempotent");
        // Structure preserved: after the reparse there are still 4 Li, with a and b
        // nested under x (level 1)
        let levels: Vec<u8> = re
            .blocks
            .iter()
            .filter_map(|b| match b {
                Block::Li { level, .. } => Some(*level),
                _ => None,
            })
            .collect();
        assert_eq!(
            levels,
            vec![0, 1, 1, 0],
            "nested structure survived: {out:?}"
        );
        assert!(
            out.contains("   - a"),
            "indent >= content col of `1. `: {out:?}"
        );
    }

    #[test]
    fn emit_roundtrip_full_syntax() {
        let src = "# H1\n\npara [link](u) ![img](i.png) $m$ ~~del~~\n\n- a\n  - b\n\n- [ ] t1\n- [x] t2\n\n1. x\n2. y\n\n> q\n\n| h1 | h2 |\n|:---|---:|\n| a | b |\n| c | d |\n\n---\n\n```\ncode\n```\n";
        let doc = parse(src);
        let out = emit(&doc);
        let out2 = emit(&parse(&out));
        assert_eq!(out, out2, "roundtrip must be idempotent");
        // Content preservation: every key markup is still there
        assert!(out.contains("[link](u)"), "got: {out}");
        assert!(out.contains("![img](i.png)"));
        assert!(out.contains("$m$"));
        assert!(out.contains("~~del~~"));
        assert!(out.contains("- [x] t2"));
        assert!(out.contains("  - b"));
        assert!(out.contains(":--"), "left align separator missing: {out}");
        assert!(out.contains("--:"), "right align separator missing: {out}");
    }

    #[test]
    fn emit_table_escapes_pipe() {
        let src = "| a |\n|---|\n| x\\|y |\n";
        let out = emit(&parse(src));
        assert!(out.contains("x\\|y"), "got: {out}");
        let out2 = emit(&parse(&out));
        assert_eq!(out, out2);
    }

    #[test]
    fn gfm_html_block_roundtrip() {
        // A GFM html block: parsed into Block::Html, written back verbatim, idempotent
        let src = "<div class=\"box\">\n<p>hi</p>\n</div>\n\nafter\n";
        let doc = parse(src);
        assert!(
            matches!(doc.blocks[0], Block::Html { .. }),
            "first block should be Html: {:?}",
            doc.blocks[0]
        );
        assert!(matches!(doc.blocks[1], Block::Para(_)));
        let out = emit(&doc);
        assert!(out.contains("<div class=\"box\">"), "got: {out}");
        assert!(out.contains("after"));
        let out2 = emit(&parse(&out));
        assert_eq!(out, out2, "roundtrip must be idempotent");
        // plain_text = source (the edit view's Pos mapping depends on it)
        match &doc.blocks[0] {
            Block::Html { text } => assert!(text.contains("<p>hi</p>")),
            _ => unreachable!(),
        }
    }

    #[test]
    fn emit_link_url_with_space() {
        let doc = Doc::new(vec![Block::Para(vec![Inline::Link {
            url: "a b".into(),
            inlines: vec![Inline::Text("t".into())],
        }])]);
        let out = emit(&doc);
        assert!(out.contains("[t](<a b>)"), "got: {out}");
        assert_eq!(emit(&parse(&out)), out);
    }

    #[test]
    fn emit_inline_code_fence() {
        // A code span inside a double-backtick fence contains a single backtick; emit has
        // to pick the fence by the content.
        let doc = parse("a ``b`c`` d");
        assert_eq!(doc.blocks[0].plain_text(), "a b`c d");
        let out = emit(&doc);
        assert!(out.contains("``b`c``"), "got: {out}");
    }

    #[test]
    fn loose_list_merges_paragraphs() {
        let bs = blocks("- a\n\n  b\n");
        assert_eq!(bs.len(), 1);
        assert_eq!(
            bs[0],
            Block::Li {
                ordered: false,
                num: 0,
                level: 0,
                task: None,
                inlines: vec![Inline::Text("a b".into())]
            }
        );
    }

    /// Literal text survives parse → emit → parse character for character (escaping a
    /// trigger character does not change the meaning).
    #[test]
    fn text_escape_roundtrip_triggers() {
        let cases = [
            "*foo*",
            "**a**",
            "_bar_",
            "__b__",
            "[a]",
            "[a](u)",
            "![i](u)",
            "# x",
            "## y",
            "~~t~~",
            "~x~",
            "$x$",
            "$$y$$",
            "<b>",
            "<https://a.io>",
            "a & b",
            "&amp;",
            "&#32;",
            "`c`",
            "a\\b",
            "1. x",
            "1) y",
            "- z",
            "+ q",
            "> r",
            "---",
            "===",
            "a|b",
        ];
        for s in cases {
            let doc = Doc::new(vec![Block::Para(vec![Inline::Text(s.to_string())])]);
            let out = emit(&doc);
            let re = parse(&out);
            assert_eq!(
                re.blocks.len(),
                1,
                "case {s:?} should still be a single paragraph: {out:?}"
            );
            assert_eq!(
                re.blocks[0].plain_text(),
                s,
                "case {s:?} was not preserved: {out:?}"
            );
            assert_eq!(emit(&re), out, "case {s:?} is not idempotent");
        }
    }

    /// Once `\[a\]` / `\*foo\*` / `\# x` escaped in the source are parsed into Text,
    /// emit must put the backslashes back (otherwise the reparse turns them into a
    /// link / emphasis / heading).
    #[test]
    fn text_escape_from_source_escapes() {
        let src = "\\[a\\] \\*foo\\* \\# x";
        let doc = parse(src);
        assert_eq!(
            doc.blocks[0],
            Block::Para(vec![Inline::Text("[a] *foo* # x".into())])
        );
        let out = emit(&doc);
        assert!(out.contains("\\[a\\]"), "{out}");
        assert!(out.contains("\\*foo\\*"), "{out}");
        assert!(out.contains("\\# x"), "{out}");
        assert_eq!(
            parse(&out).blocks,
            doc.blocks,
            "the round trip must be faithful character by character"
        );
    }

    /// Block-level triggers at a line start (list, quote, heading, ordered marker,
    /// indentation).
    #[test]
    fn text_escape_line_start_block_triggers() {
        let cases = [
            ("- item", "\\- item"),
            ("+ item", "\\+ item"),
            ("> quote", "\\> quote"),
            ("1. one", "1\\. one"),
            ("= set", "\\= set"),
            ("    indented", "&#32;   indented"),
        ];
        for (s, want) in cases {
            let doc = Doc::new(vec![Block::Para(vec![Inline::Text(s.into())])]);
            let out = emit(&doc);
            assert_eq!(out.trim_end_matches('\n'), want, "case {s:?}");
            let re = parse(&out);
            assert_eq!(
                re.blocks[0].plain_text(),
                s,
                "case {s:?} was not preserved: {out:?}"
            );
        }
    }

    /// A block's content start (the first column of a list item's or a quote's text)
    /// is a line start too — an unescaped `- ` `>` `1.` and friends would reparse as a
    /// nested list or quote (breaking the character-by-character fidelity).
    #[test]
    fn text_escape_block_content_start_triggers() {
        let cases = ["- x", "+ x", "> x", "1. x", "1) x", "  indented", "= x"];
        for s in cases {
            let inlines = vec![Inline::Text(s.into())];
            for doc in [
                Doc::new(vec![Block::Li {
                    ordered: false,
                    num: 0,
                    level: 0,
                    task: None,
                    inlines: inlines.clone(),
                }]),
                Doc::new(vec![Block::Quote(inlines.clone())]),
            ] {
                let out = emit(&doc);
                let re = parse(&out);
                assert_eq!(
                    re.blocks, doc.blocks,
                    "case {s:?} was not preserved: {out:?}"
                );
                assert_eq!(emit(&re), out, "case {s:?} is not idempotent: {out:?}");
            }
        }
        // Spot-checking the emitted shape (documented shapes)
        let doc = Doc::new(vec![Block::Li {
            ordered: false,
            num: 0,
            level: 0,
            task: None,
            inlines: vec![Inline::Text("- x".into())],
        }]);
        assert_eq!(emit(&doc), "- \\- x\n");
        let doc = Doc::new(vec![Block::Quote(vec![Inline::Text("1. x".into())])]);
        assert_eq!(emit(&doc), "> 1\\. x\n");
    }

    /// A trailing `!` in the text right before a following link's `[` would spell
    /// image syntax and must be escaped.
    #[test]
    fn text_escape_bang_before_link() {
        let doc = Doc::new(vec![Block::Para(vec![
            Inline::Text("a!".into()),
            Inline::Link {
                url: "u".into(),
                inlines: vec![Inline::Text("x".into())],
            },
        ])]);
        let out = emit(&doc);
        assert!(out.starts_with("a\\!["), "{out}");
        let re = parse(&out);
        assert_eq!(re.blocks, doc.blocks);
        assert_eq!(emit(&re), out);
    }

    /// Entity-shaped text such as a literal `&amp;` must not be decoded into a
    /// different character by the reparse.
    #[test]
    fn text_escape_html_entity_ampersand() {
        let s = "a &amp; b &copy; &#32; c";
        let doc = Doc::new(vec![Block::Para(vec![Inline::Text(s.into())])]);
        let out = emit(&doc);
        // Escaping the `&` cuts entity parsing short; `#` is in the always-escaped set,
        // so a `&#32;` shaped text is emitted as `\&\#32;`
        assert!(
            out.contains("\\&amp;") && out.contains("\\&\\#32;"),
            "{out}"
        );
        let re = parse(&out);
        assert_eq!(re.blocks[0].plain_text(), s);
    }

    /// The inline code fence = the longest run of backticks in the content + 1; pad
    /// inside when the content starts or ends with a backtick or a space.
    #[test]
    fn inline_code_fence_edge_cases() {
        let cases = ["`", "``", "```", "`a`", "a`b", " a ", " ", "x", "`` ` ``"];
        for c in cases {
            let doc = Doc::new(vec![Block::Para(vec![Inline::Code(c.into())])]);
            let out = emit(&doc);
            let re = parse(&out);
            assert_eq!(
                re.blocks[0].plain_text(),
                c,
                "case {c:?} was not preserved: {out:?}"
            );
            assert_eq!(emit(&re), out, "case {c:?} is not idempotent: {out:?}");
        }
        // Spot-checking the emitted shape (documented shapes)
        let doc = Doc::new(vec![Block::Para(vec![Inline::Code("`".into())])]);
        assert_eq!(emit(&doc), "`` ` ``\n");
        let doc = Doc::new(vec![Block::Para(vec![Inline::Code("``".into())])]);
        assert_eq!(emit(&doc), "``` `` ```\n");
    }

    /// `<` `>` `"` and the control characters in a URL are percent-encoded (when the
    /// angle-bracket form is truncated the whole link degrades into literal text).
    #[test]
    fn emit_url_percent_encodes_angle_and_controls() {
        let doc = Doc::new(vec![Block::Para(vec![Inline::Link {
            url: "a<b>c\nd\"e".into(),
            inlines: vec![Inline::Text("t".into())],
        }])]);
        let out = emit(&doc);
        assert!(out.contains("[t](a%3Cb%3Ec%0Ad%22e)"), "{out}");
        let re = parse(&out);
        let Block::Para(inl) = &re.blocks[0] else {
            panic!("not para")
        };
        assert!(
            matches!(&inl[0], Inline::Link { url, .. } if url == "a%3Cb%3Ec%0Ad%22e"),
            "{out:?}"
        );
        assert_eq!(emit(&re), out);
    }

    /// An Html block's source (including its leading and trailing line breaks) is kept
    /// byte for byte — the old behaviour trimmed the trailing line break and quietly
    /// shrank the line and byte counts.
    #[test]
    fn html_block_roundtrip_exact() {
        for src in [
            "<div>x</div>\n",
            "<div class=\"box\">\n<p>hi</p>\n</div>\n\nafter\n",
            "<pre>\na\n\nb\n</pre>\n\npara\n",
        ] {
            let doc = parse(src);
            assert_eq!(
                emit(&doc),
                src,
                "the src must round trip byte for byte: {src:?}"
            );
        }
        // Leading and trailing line breaks created by editing are not eaten by emit either
        let doc = Doc::new(vec![Block::Html {
            text: "\n<div>x</div>\n".into(),
        }]);
        assert_eq!(emit(&doc), "\n<div>x</div>\n");
    }

    /// A nested list item's indentation plus the later paragraphs' offsets must agree
    /// with emit's output byte for byte (the old implementation recorded 0 for a
    /// nested item's indent and the offsets after a list region drifted by -1). Every
    /// offset points at the content's first byte: a list item skips the indentation and
    /// the marker.
    #[test]
    fn block_offsets_nested_list_and_paragraphs() {
        let src = "1. x\n   - a\n   - b\n2. y\n\npara 一\n\npara 二\n";
        let doc = parse(src);
        let md = emit(&doc);
        let offs = block_offsets(&doc);
        let expect = ["x", "a", "b", "y", "para 一", "para 二"];
        assert_eq!(offs.len(), expect.len());
        assert_eq!(offs[0], 3, "after the ordered item's \"1. \"");
        for (k, e) in expect.iter().enumerate() {
            assert!(
                md[offs[k]..].starts_with(e),
                "block {k} offset {} does not point at {e:?}: {:?}",
                offs[k],
                &md[offs[k]..]
            );
        }
        assert!(
            offs.windows(2).all(|w| w[0] < w[1]),
            "the offsets must be strictly increasing: {offs:?}"
        );
    }

    /// Link reference definitions are swallowed by pulldown and produce no events —
    /// the gap scan restores them as a literal-text paragraph (no content is lost and
    /// it is idempotent).
    #[test]
    fn link_ref_definitions_preserved() {
        let doc = parse("[foo]: /url\n\ntext\n");
        assert_eq!(doc.blocks.len(), 2);
        assert_eq!(doc.blocks[0].plain_text(), "[foo]: /url");
        let out = emit(&doc);
        assert!(out.starts_with("\\[foo\\]: /url"), "{out}");
        let re = parse(&out);
        assert_eq!(re.blocks, doc.blocks);
        assert_eq!(emit(&re), out);
        // A document of definitions only no longer loses blocks
        let doc = parse("[bar]: /u 't'\n");
        assert_eq!(doc.blocks.len(), 1);
        assert_eq!(doc.blocks[0].plain_text(), "[bar]: /u 't'");
        // A referenced definition is kept too (the reference site reparses into an inline
        // link; better to keep too much than to lose it)
        let doc = parse("见 [foo]。\n\n[foo]: /url\n");
        assert_eq!(doc.blocks.len(), 2);
        assert_eq!(doc.blocks[1].plain_text(), "[foo]: /url");
    }

    #[test]
    fn multiline_code_keeps_newlines() {
        let bs = blocks("```\nline1\nline2\n```");
        assert_eq!(
            bs[0],
            Block::Code {
                lang: None,
                text: "line1\nline2".into()
            }
        );
    }

    /// Soft / hard breaks survive parse (they no longer fold into spaces): two trailing
    /// spaces or a backslash = HardBreak, a single line break = SoftBreak; plain
    /// contains `\n` (so Pos.off measurement agrees with the rendered text flow).
    #[test]
    fn parse_keeps_soft_and_hard_breaks() {
        let bs = blocks("a  \nb\n\nc\\\nd\n\nx\ny\n");
        assert_eq!(bs[0].plain_text(), "a\nb");
        assert_eq!(bs[1].plain_text(), "c\nd");
        assert_eq!(bs[2].plain_text(), "x\ny");
        assert!(
            matches!(&bs[0], Block::Para(i) if matches!(i[1], Inline::HardBreak)),
            "two trailing spaces = a hard break: {:?}",
            bs[0]
        );
        assert!(
            matches!(&bs[1], Block::Para(i) if matches!(i[1], Inline::HardBreak)),
            "a backslash is a hard break: {:?}",
            bs[1]
        );
        assert!(
            matches!(&bs[2], Block::Para(i) if matches!(i[1], Inline::SoftBreak)),
            "a single line break is a soft break: {:?}",
            bs[2]
        );
    }

    /// A document containing line breaks survives parse → emit → parse (emit is
    /// idempotent and the semantics are equivalent; the backslash form normalizes to
    /// the two-space form — same meaning, idempotent and stable).
    #[test]
    fn breaks_roundtrip() {
        for src in [
            "a  \nb\n",
            "c\\\nd\n",
            "x\ny\n",
            "x\ny\nz\n",
            "- a\n  b\n",
            "> q\n  r\n",
            "- a\n  b\n  c\n\n1. x\n   y\n",
        ] {
            let doc = parse(src);
            let out = emit(&doc);
            assert_eq!(
                emit(&parse(&out)),
                out,
                "emit is idempotent: {src:?} → {out:?}"
            );
            assert_eq!(
                parse(&out),
                doc,
                "semantics are preserved: {src:?} → {out:?}"
            );
        }
    }

    /// Trailing spaces on a Text plus a soft break — two or more raw spaces at the end
    /// of a line would reparse as a hard break, so emit has to rewrite the trailing
    /// spaces as `&#32;` to keep the soft break.
    #[test]
    fn soft_break_after_trailing_spaces_stays_soft() {
        let doc = Doc::new(vec![Block::Para(vec![
            Inline::Text("a  ".into()),
            Inline::SoftBreak,
            Inline::Text("b".into()),
        ])]);
        let out = emit(&doc);
        assert!(
            out.contains("&#32;"),
            "trailing spaces go through the entity: {out:?}"
        );
        let re = parse(&out);
        assert_eq!(re.blocks[0].plain_text(), "a  \nb");
        assert!(
            matches!(&re.blocks[0], Block::Para(i) if matches!(i[1], Inline::SoftBreak)),
            "trailing spaces must not turn a soft break into a hard one: {out:?}"
        );
        assert_eq!(emit(&re), out);
    }

    /// A hard line break emits as two trailing spaces (the backslash form degrades into
    /// a literal `\` at the end of a paragraph).
    #[test]
    fn hard_break_emits_two_space_form() {
        let doc = Doc::new(vec![Block::Para(vec![
            Inline::Text("a".into()),
            Inline::HardBreak,
            Inline::Text("b".into()),
        ])]);
        assert_eq!(emit(&doc), "a  \nb\n");
        assert_eq!(parse("a  \nb\n"), doc);
    }

    /// A **soft** line break inside a single-line block (a GFM cell) — brought in by an
    /// editing merge — degrades to a space: the table's row structure wins, and
    /// `| a` followed by `b |` on the next line would break the row apart. A hard line
    /// break (Enter inside a cell, or a `<br>` in the source) does not take this path:
    /// it is emitted as `<br>`, see `table_cell_br_roundtrips`.
    #[test]
    fn break_in_table_cell_degrades_to_space() {
        let seq = 0;
        let doc = Doc::new(vec![
            Block::Td {
                table_seq: seq,
                row: 0,
                align: 0,
                inlines: vec![Inline::Text("h".into())],
            },
            Block::Td {
                table_seq: seq,
                row: 1,
                align: 0,
                inlines: vec![
                    Inline::Text("a".into()),
                    Inline::SoftBreak,
                    Inline::Text("b".into()),
                ],
            },
        ]);
        let out = emit(&doc);
        assert!(out.contains("| a b |"), "got: {out}");
        assert_eq!(
            parse(&out).blocks.len(),
            2,
            "a line break must not split the table structure: {out:?}"
        );
    }

    // ---- Quote ownership (the quote shell round trip) ----

    /// A list inside a quote used to flatten into top-level Li and emit a bare list
    /// (one open-save and the quote shell was gone). Now the items carry a `Quoted`
    /// shell and emit adds the `> ` prefix on every line, so the structure round
    /// trips.
    #[test]
    fn quote_list_roundtrip_keeps_shell() {
        let src = "> - a\n> - b\n";
        let doc = parse(src);
        assert_eq!(doc.blocks.len(), 2);
        for b in &doc.blocks {
            assert_eq!(
                b.quote_depth(),
                1,
                "the block must stay inside the quote shell: {b:?}"
            );
            assert!(
                matches!(b.core(), Block::Li { .. }),
                "not a list item: {b:?}"
            );
        }
        let out = emit(&doc);
        assert!(out.contains("> - a") && out.contains("> - b"), "{out}");
        assert!(
            out.lines().all(|l| l.starts_with("> ")),
            "every line carries the quote prefix: {out:?}"
        );
        assert_eq!(parse(&out), doc, "the structure is preserved: {out:?}");
        assert_eq!(emit(&parse(&out)), out, "roundtrip must be idempotent");
    }

    /// A code block inside a quote keeps its shell — the multi-line content (including
    /// blank lines) gets the prefix on every line and is faithful character by
    /// character.
    #[test]
    fn quote_code_block_roundtrip_keeps_shell() {
        for src in [
            "> ```rust\n> fn x() {}\n> ```\n",
            "> ```\n> a\n>\n> b\n> ```\n",
        ] {
            let doc = parse(src);
            assert_eq!(doc.blocks[0].quote_depth(), 1, "{src:?}");
            assert!(
                matches!(doc.blocks[0].core(), Block::Code { .. }),
                "{src:?}"
            );
            let out = emit(&doc);
            assert!(
                out.lines()
                    .all(|l| l.starts_with('>') && !l.starts_with("> > ")),
                "every line carries one quote level's prefix: {out:?}"
            );
            assert!(out.contains("> ```"), "{out}");
            assert_eq!(
                parse(&out),
                doc,
                "the structure is preserved: {src:?} → {out:?}"
            );
            assert_eq!(emit(&parse(&out)), out, "roundtrip must be idempotent");
        }
        // An empty line inside a code block round trips (a line with only the `>`
        // prefix is an empty content line)
        let doc = parse("> ```\n> a\n>\n> b\n> ```\n");
        assert_eq!(doc.blocks[0].plain_text(), "a\n\nb");
    }

    /// Cells of a table inside a quote keep the shell, emit writes `> | …` per line,
    /// and the table structure does not collapse.
    #[test]
    fn quote_table_roundtrip_keeps_shell() {
        let src = "> | h | i |\n> |---|---|\n> | 1 | 2 |\n";
        let doc = parse(src);
        assert_eq!(doc.blocks.len(), 4);
        for b in &doc.blocks {
            assert_eq!(b.quote_depth(), 1, "{b:?}");
            assert!(b.core().is_td(), "{b:?}");
        }
        let out = emit(&doc);
        assert!(out.contains("> | h | i |"), "{out}");
        assert!(
            out.lines().all(|l| l.starts_with("> ")),
            "every line carries the quote prefix: {out:?}"
        );
        assert_eq!(parse(&out), doc, "the structure is preserved: {out:?}");
        assert_eq!(emit(&parse(&out)), out, "roundtrip must be idempotent");
    }

    /// A nested quote records its ownership by depth (`> > x` = depth 2) and emit
    /// writes a `> > ` prefix.
    #[test]
    fn nested_quote_roundtrip_keeps_depth() {
        let src = "> 段落\n>\n> > 嵌套\n";
        let doc = parse(src);
        assert_eq!(doc.blocks.len(), 2);
        assert_eq!(doc.blocks[0].quote_depth(), 1);
        assert_eq!(doc.blocks[1].quote_depth(), 2);
        let out = emit(&doc);
        assert!(out.contains("> > 嵌套"), "{out}");
        assert_eq!(parse(&out), doc, "the structure is preserved: {out:?}");
        assert_eq!(emit(&parse(&out)), out, "roundtrip must be idempotent");
    }

    /// Mixed content inside a quote (paragraph / list / code block) round trips as a
    /// whole — the shell is not lost and it is idempotent.
    #[test]
    fn quote_mixed_blocks_roundtrip() {
        let src = "> 段落\n>\n> - a\n>\n> ```\n> c\n> ```\n";
        let doc = parse(src);
        let cores: Vec<&str> = doc
            .blocks
            .iter()
            .map(|b| match b.core() {
                Block::Para(_) | Block::Quote(_) => "para",
                Block::Li { .. } => "li",
                Block::Code { .. } => "code",
                _ => "?",
            })
            .collect();
        assert_eq!(cores, ["para", "li", "code"], "{src:?}");
        assert!(
            doc.blocks.iter().all(|b| b.quote_depth() == 1),
            "every block should be inside one quote level"
        );
        let out = emit(&doc);
        assert!(out.contains("> - a") && out.contains("> ```"), "{out}");
        assert_eq!(parse(&out), doc, "the structure is preserved: {out:?}");
        assert_eq!(emit(&parse(&out)), out, "roundtrip must be idempotent");
    }

    /// The block offsets of a quote region agree with emit byte for byte (the
    /// content's first byte, skipping the `> ` prefix and the list markers).
    #[test]
    fn block_offsets_in_quote_align_with_emit() {
        let doc = parse("> - a\n> - b\n\n>段落\n");
        let md = emit(&doc);
        let offs = block_offsets(&doc);
        let expect = ["a", "b", "段落"];
        assert_eq!(offs.len(), expect.len());
        for (k, e) in expect.iter().enumerate() {
            assert!(
                md[offs[k]..].starts_with(e),
                "block {k} offset {} does not point at {e:?}: {:?}",
                offs[k],
                &md[offs[k]..]
            );
        }
        assert!(offs.windows(2).all(|w| w[0] < w[1]), "{offs:?}");
    }

    // ---- Block line ranges (the basis for the host mdmap's block splitting) ----

    /// A top-level block's line range is exact, ordered and non-overlapping (0-based,
    /// both ends inclusive). The document covers a heading / paragraphs / a list
    /// (nested) / a quote (with a list inside) / a code block / a table / a nested
    /// quote / an hr.
    #[test]
    fn block_line_ranges_exact_and_ordered() {
        // 0: # heading
        // 1: (empty)
        // 2: para one
        // 3: para two
        // 4: (empty)
        // 5: - A
        // 6:   - nested
        // 7: - B
        // 8: (empty)
        // 9: > quote paragraph
        // 10: >
        // 11: > - item
        // 12: (empty)
        // 13: ```rust
        // 14: fn x() {}
        // 15: ```
        // 16: (empty)
        // 17: | a | b |
        // 18: |---|---|
        // 19: | 1 | 2 |
        // 20: (empty)
        // 21: > > deep quote
        // 22: (empty)
        // 23: ---
        let src = "# 标题\n\npara 一\npara 二\n\n- 甲\n  - 嵌套\n- 乙\n\n> 引用段\n>\n> - 项\n\n```rust\nfn x() {}\n```\n\n| a | b |\n|---|---|\n| 1 | 2 |\n\n> > 深引用\n\n---\n";
        let spans = block_line_ranges(src);
        let got: Vec<(usize, usize, SpanKind)> =
            spans.iter().map(|s| (s.start, s.end, s.kind)).collect();
        assert_eq!(
            got,
            vec![
                (0, 0, SpanKind::Head),
                (2, 3, SpanKind::Para),
                (5, 7, SpanKind::List), // the nested item joins the top-level list span
                (9, 11, SpanKind::Quote), // the list inside the quote joins the quote shell's span
                (13, 15, SpanKind::Code),
                (17, 19, SpanKind::Table), // header + separator + data rows: one span for the whole table
                (21, 21, SpanKind::Quote), // a nested quote is one span for the whole shell
                (23, 23, SpanKind::Rule),
            ],
            "{spans:?}"
        );
        // Ordered and non-overlapping
        assert!(spans.windows(2).all(|w| w[0].end < w[1].start), "{spans:?}");
        // Every non-blank line is covered by exactly one span (blank lines are the gaps
        // between blocks and the caller fills them)
        for (l, line) in src.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            let hit = spans.iter().filter(|s| s.start <= l && l <= s.end).count();
            assert_eq!(
                hit, 1,
                "line {l} should be covered by exactly one span: {spans:?}"
            );
        }
    }

    /// A setext heading (`text` then `---`) is a heading block rather than a paragraph
    /// plus an hr; a multi-line block-level formula `$$` and an HTML block each get
    /// their own span; a document of blank lines or definitions only does not crash.
    #[test]
    fn block_line_ranges_setext_math_html_and_empty() {
        // setext: `~~strikethrough~~` followed by `---` is one heading span (the old
        // regular expression cut it into text + hr)
        let spans = block_line_ranges("~~删除线~~\n---\n");
        assert_eq!(
            spans,
            vec![BlockSpan {
                start: 0,
                end: 1,
                kind: SpanKind::Head
            }],
            "{spans:?}"
        );
        // A multi-line block-level formula is one block; the paragraph follows
        let spans = block_line_ranges("$$\nx^2\n$$\n\nafter\n");
        assert_eq!(
            spans,
            vec![
                BlockSpan {
                    start: 0,
                    end: 2,
                    kind: SpanKind::Para
                },
                BlockSpan {
                    start: 4,
                    end: 4,
                    kind: SpanKind::Para
                },
            ],
            "{spans:?}"
        );
        // An HTML block is one span for the whole block
        let spans = block_line_ranges("<div>\n<p>hi</p>\n</div>\n\nafter\n");
        assert_eq!(spans.len(), 2, "{spans:?}");
        assert_eq!(
            (spans[0].start, spans[0].end, spans[0].kind),
            (0, 2, SpanKind::Html)
        );
        // Empty text / blank lines only: no span (the gaps all belong to the caller)
        assert!(block_line_ranges("").is_empty());
        assert!(block_line_ranges("\n\n\n").is_empty());
    }

    /// An escaped image `\![…](…)` is literal text (one span for the single
    /// paragraph); the link reference definition lines are gaps (they produce no span
    /// and the caller fills them).
    #[test]
    fn block_line_ranges_escaped_image_and_ref_def() {
        let spans = block_line_ranges("\\![x](img.png)\n");
        assert_eq!(
            spans,
            vec![BlockSpan {
                start: 0,
                end: 0,
                kind: SpanKind::Para
            }],
            "{spans:?}"
        );
        // The definition lines are swallowed by pulldown: they appear in no span, and the
        // content's span is undisturbed
        let spans = block_line_ranges("[foo]: /url\n\ntext\n");
        assert_eq!(
            spans,
            vec![BlockSpan {
                start: 2,
                end: 2,
                kind: SpanKind::Para
            }],
            "{spans:?}"
        );
    }

    /// An empty paragraph at the end of the document (an empty line created by pressing
    /// Enter at the end) must be kept when saving. The old implementation stripped the
    /// trailing line breaks down to exactly one — press Enter to create an empty line,
    /// save, and the line is swallowed (seen on a real machine).
    #[test]
    fn emit_keeps_trailing_empty_paragraphs() {
        let mut e = crate::edit::Editor::new(parse("A\n"));
        e.sel = crate::model::Sel::caret(crate::model::Pos::new(0, 1));
        assert!(e.apply(crate::edit::Cmd::Enter));
        assert_eq!(emit(&e.doc), "A\n\n");
        let d = Doc::new(vec![
            Block::Para(vec![Inline::Text("A".into())]),
            Block::Para(vec![]),
            Block::Para(vec![]),
        ]);
        assert_eq!(emit(&d), "A\n\n\n");
        // A fully empty document keeps the old behaviour: exactly one line break
        let empty = Doc::new(vec![Block::Para(vec![])]);
        assert_eq!(emit(&empty), "\n");
    }

    // ---- Round-trip defects found by corpus/fuzz probing ----

    /// A nested ordered list whose first item is not 1 cannot interrupt its parent
    /// item's paragraph (CommonMark): emitting `1. one` + nested `2. two` on the next
    /// line reparsed as one item holding the literal `2. two`, so a save flattened the
    /// nesting and injected `2. `. A blank line keeps the nesting.
    #[test]
    fn emit_nested_ordered_list_not_starting_at_one() {
        let src = "1. one\n\n   2. two\n   3. three\n";
        let doc = parse(src);
        assert_eq!(
            doc.blocks
                .iter()
                .filter(|b| matches!(b, Block::Li { level: 1, .. }))
                .count(),
            2,
            "fixture must nest two items: {:?}",
            doc.blocks
        );
        let out = emit(&doc);
        let re = parse(&out);
        assert_eq!(re.blocks, doc.blocks, "nesting lost: {out:?}");
        assert_eq!(emit(&re), out, "idempotent: {out:?}");
        // Sibling items are unaffected: no blank line is inserted in a plain list
        let plain = parse("1. a\n2. b\n3. c\n");
        assert_eq!(emit(&plain), "1. a\n2. b\n3. c\n");
        // ... and a level decrease does not add one either
        let back = parse("1. a\n   1. b\n2. c\n");
        assert_eq!(emit(&back), "1. a\n   1. b\n2. c\n");
    }

    /// Emphasis around a punctuation run after a word: the edge is emitted outside the
    /// markers (the opener cannot be followed by punctuation), and it must be written
    /// back **verbatim** — re-escaping it doubled backslashes, so `b` + `Del("_")` +
    /// ` c` saved as `b\\_ c` and a real backslash appeared in the text.
    #[test]
    fn emit_container_edges_are_not_reescaped() {
        let mut doc = Doc::new(vec![Block::Para(vec![Inline::Text("b".into())])]);
        if let Block::Para(inlines) = &mut doc.blocks[0] {
            inlines.push(Inline::Del(vec![Inline::Text("_".into())]));
            inlines.push(Inline::Text(" c".into()));
        }
        let out = emit(&doc);
        let re = parse(&out);
        let text = re.blocks[0].plain_text();
        assert_eq!(text, "b_ c", "text changed by the round trip: {out:?}");
        assert!(!out.contains("\\\\"), "double escape in {out:?}");
        // Bold around a lone `!` after a word: the styling cannot be expressed (the
        // opener would follow an alphanumeric and be followed by punctuation), but the
        // marker bytes must not leak into the text either.
        let mut doc = Doc::new(vec![Block::Para(vec![Inline::Text("a".into())])]);
        if let Block::Para(inlines) = &mut doc.blocks[0] {
            inlines.push(Inline::Strong(vec![Inline::Text("!".into())]));
        }
        let out = emit(&doc);
        assert_eq!(parse(&out).blocks[0].plain_text(), "a!", "got {out:?}");
    }

    /// An entity-encoded space at the container edge (`&#32;`) is whitespace: it has to
    /// be moved outside the markers as a whole unit. Slicing inside the entity produced
    /// `*&#32*;` — a literal `&#32;` was injected into the user's text.
    #[test]
    fn emit_container_moves_entity_spaces_whole() {
        let mut doc = Doc::new(vec![Block::Para(vec![Inline::Text("x".into())])]);
        if let Block::Para(inlines) = &mut doc.blocks[0] {
            inlines.push(Inline::Em(vec![Inline::Text(" * ".into())]));
            inlines.push(Inline::Text("y".into()));
        }
        let out = emit(&doc);
        assert!(
            !out.replace("&#32;", "").contains("&#"),
            "a split entity was emitted: {out:?}"
        );
        let re = parse(&out);
        assert_eq!(re.blocks[0].plain_text(), "x * y", "text changed: {out:?}");
        // Emphasis over a lone `*` between spaces and a word is not expressible (the
        // closer would follow punctuation and precede an alphanumeric), but ordinary
        // content with edge spaces keeps its styling.
        let mut doc = Doc::new(vec![Block::Para(vec![Inline::Text("x".into())])]);
        if let Block::Para(inlines) = &mut doc.blocks[0] {
            inlines.push(Inline::Em(vec![Inline::Text(" a ".into())]));
            inlines.push(Inline::Text("y".into()));
        }
        let out = emit(&doc);
        assert_eq!(out.trim_end(), "x *a* y", "got {out:?}");
        let re = parse(&out);
        let keeps_em = matches!(&re.blocks[0], Block::Para(ins)
            if ins.iter().any(|i| matches!(i, Inline::Em(c) if !c.is_empty())));
        assert!(keeps_em, "the emphasis was lost: {out:?}");
        assert_eq!(re.blocks[0].plain_text(), "x a y");
    }

    /// A heading whose content is only a line break: the emitted body is whitespace, and
    /// `#####&#32;` is not a heading (it reparsed as a paragraph holding the markers).
    #[test]
    fn emit_heading_with_only_a_break_stays_a_heading() {
        let doc = Doc::new(vec![Block::Head {
            level: 5,
            inlines: vec![Inline::SoftBreak],
        }]);
        let out = emit(&doc);
        assert_eq!(out.trim_end(), "#####", "got {out:?}");
        assert!(
            matches!(
                parse(&out).blocks.first(),
                Some(Block::Head { level: 5, .. })
            ),
            "no longer a heading: {out:?}"
        );
    }

    /// A line break at the start of a list item's content is not representable (an empty
    /// first line inside the item). Emitting it pushed the item's content and the items
    /// after it out of the list as literal text.
    #[test]
    fn emit_list_item_leading_break_keeps_the_list() {
        let doc = Doc::new(vec![
            Block::Li {
                ordered: true,
                num: 2,
                level: 0,
                task: None,
                inlines: vec![
                    Inline::SoftBreak,
                    Inline::SoftBreak,
                    Inline::Text("x".into()),
                ],
            },
            Block::Li {
                ordered: true,
                num: 3,
                level: 0,
                task: None,
                inlines: vec![Inline::Text("y".into())],
            },
        ]);
        let out = emit(&doc);
        let re = parse(&out);
        let items: Vec<&str> = re
            .blocks
            .iter()
            .filter_map(|b| match b {
                Block::Li { inlines, .. } => Some(inlines),
                _ => None,
            })
            .map(|_| "")
            .collect();
        assert_eq!(items.len(), 2, "the following item left the list: {out:?}");
        assert_eq!(
            re.blocks.iter().map(|b| b.plain_text()).collect::<Vec<_>>(),
            vec!["x", "y"]
        );
    }

    // ---- Full-feature sample (samples/wys-all-features.md) ----

    const ALL_FEATURES: &str = include_str!("../../../samples/wys-all-features.md");

    /// The sample must be in **emit normal form**: "open → save" leaves it byte for
    /// byte unchanged (otherwise every open rewrites the file and the undo stack and
    /// dirty marker are both thrown off). Maintenance: after editing the sample, run
    /// `cargo run -p wysiwyg --example md_normalize -- samples/wys-all-features.md --write`.
    #[test]
    fn all_features_sample_is_canonical() {
        let doc = parse(ALL_FEATURES);
        let out = emit(&doc);
        assert_eq!(
            out, ALL_FEATURES,
            "the sample is not in emit normal form (run examples/md_normalize --write and check the content)"
        );
        assert_eq!(
            parse(&out),
            doc,
            "the normal form must reparse into the same block tree"
        );
    }

    /// Sample coverage: every block-level and inline node type appears at least once.
    /// When adding syntax support, extend the sample in the same commit — this
    /// assertion is there to remind you not to forget.
    #[test]
    fn all_features_sample_covers_every_block_and_inline_kind() {
        let doc = parse(ALL_FEATURES);
        let mut head = [false; 6];
        let (mut code, mut html, mut hr) = (false, false, false);
        let (mut quote, mut nested_quote, mut quoted_item) = (false, false, false);
        let (mut ul, mut ol, mut task_off, mut task_on, mut nested_item) =
            (false, false, false, false, false);
        let (mut td, mut td_align_left, mut td_align_center, mut td_align_right) =
            (false, false, false, false);
        for b in &doc.blocks {
            // Note: `b.core()` cannot be used here — it strips the `Quoted` shell, so a
            // nested quote (depth 2) and a list item inside a quote would never be found.
            match b {
                Block::Head { level, .. } => head[usize::from(*level).saturating_sub(1)] = true,
                Block::Code { lang, .. } => code |= lang.as_deref() == Some("rust"),
                Block::Html { .. } => html = true,
                Block::Hr => hr = true,
                Block::Quote(_) => quote = true,
                Block::Quoted { depth, inner } => {
                    if *depth >= 2 {
                        nested_quote = true;
                    }
                    if matches!(inner.core(), Block::Li { .. }) {
                        quoted_item = true;
                    }
                }
                Block::Li {
                    ordered,
                    level,
                    task,
                    ..
                } => {
                    if *ordered {
                        ol = true;
                    } else {
                        ul = true;
                    }
                    match task {
                        Some(true) => task_on = true,
                        Some(false) => task_off = true,
                        None => {}
                    }
                    if *level > 0 {
                        nested_item = true;
                    }
                }
                Block::Td { align, .. } => {
                    td = true;
                    match align {
                        1 => td_align_left = true,
                        2 => td_align_center = true,
                        3 => td_align_right = true,
                        _ => {}
                    }
                }
                _ => {}
            }
        }
        assert!(
            head.iter().all(|x| *x),
            "H1–H6 must all be covered: {head:?}"
        );
        assert!(code, "a code block with a language is missing");
        assert!(html, "an HTML block is missing");
        assert!(hr, "a horizontal rule is missing");
        assert!(
            quote && nested_quote,
            "a quote / nested quote is missing: quote={quote} nested={nested_quote}"
        );
        assert!(quoted_item, "a list item inside a quote is missing");
        assert!(ul && ol, "an unordered / ordered list is missing");
        assert!(task_off && task_on, "both task states are missing");
        assert!(nested_item, "a nested list item is missing");
        assert!(td, "a table is missing");
        assert!(
            td_align_left && td_align_center && td_align_right,
            "all three table alignments must be covered"
        );

        fn any_inline(inl: &[Inline], f: &dyn Fn(&Inline) -> bool) -> bool {
            inl.iter().any(|i| {
                if f(i) {
                    return true;
                }
                match i {
                    Inline::Strong(c) | Inline::Em(c) | Inline::Del(c) => any_inline(c, f),
                    Inline::Link { inlines, .. } => any_inline(inlines, f),
                    _ => false,
                }
            })
        }
        let mut strong = false;
        let mut em = false;
        let mut del = false;
        let mut code = false;
        let mut link = false;
        let mut image = false;
        let mut math_inline = false;
        let mut math_display = false;
        let mut hard = false;
        let mut soft = false;
        for b in &doc.blocks {
            let inl = match b.core() {
                Block::Para(i) | Block::Quote(i) => i.clone(),
                Block::Head { inlines, .. }
                | Block::Li { inlines, .. }
                | Block::Td { inlines, .. } => inlines.clone(),
                Block::Quoted { inner, .. } => match inner.core() {
                    Block::Para(i) | Block::Quote(i) => i.clone(),
                    Block::Head { inlines, .. }
                    | Block::Li { inlines, .. }
                    | Block::Td { inlines, .. } => inlines.clone(),
                    _ => vec![],
                },
                _ => vec![],
            };
            strong |= any_inline(&inl, &|i| matches!(i, Inline::Strong(_)));
            em |= any_inline(&inl, &|i| matches!(i, Inline::Em(_)));
            del |= any_inline(&inl, &|i| matches!(i, Inline::Del(_)));
            code |= any_inline(&inl, &|i| matches!(i, Inline::Code(_)));
            link |= any_inline(&inl, &|i| matches!(i, Inline::Link { .. }));
            image |= any_inline(&inl, &|i| matches!(i, Inline::Image { .. }));
            math_inline |= any_inline(&inl, &|i| matches!(i, Inline::Math { display: false, .. }));
            math_display |= any_inline(&inl, &|i| matches!(i, Inline::Math { display: true, .. }));
            hard |= any_inline(&inl, &|i| matches!(i, Inline::HardBreak));
            soft |= any_inline(&inl, &|i| matches!(i, Inline::SoftBreak));
        }
        assert!(
            strong && em && del && code && link && image,
            "inline coverage is incomplete: strong={strong} em={em} del={del} code={code} link={link} image={image}"
        );
        assert!(
            math_inline && math_display,
            "inline / block-level formulas are missing"
        );
        assert!(hard && soft, "hard / soft line breaks are missing");
    }

    /// The sample's **block_offsets** must point at every block's content start, and
    /// their total length must match plain_text — the WYS caret mapping (the host's
    /// status bar / align_caret) depends on it entirely.
    #[test]
    fn all_features_sample_block_offsets_are_content_starts() {
        let doc = parse(ALL_FEATURES);
        let offsets = block_offsets(&doc);
        let text = emit(&doc);
        assert_eq!(offsets.len(), doc.blocks.len());
        for (i, b) in doc.blocks.iter().enumerate() {
            let off = offsets[i];
            assert!(
                off <= text.len(),
                "block {i}'s offset is out of range: {off}"
            );
            assert!(
                text.is_char_boundary(off),
                "block {i}'s offset is not on a character boundary: {off}"
            );
            let plain = b.plain_text();
            if plain.is_empty() {
                continue;
            }
            // The offset is where that block's content begins: the block's opening characters
            // should be readable right after it.
            // The offset is the **content**'s start; the content's first character may be
            // wrapped in inline markup (`**bold**`), so the test looks for the content's first
            // character inside the window that follows, rather than comparing bytes.
            let window: String = text[off..].chars().take(120).collect();
            assert!(
                window.contains(plain.chars().next().unwrap()),
                "block {i}'s offset does not point at the content: off={off} window={window:?} plain={plain:?}"
            );
            if i + 1 < offsets.len() {
                assert!(
                    offsets[i] <= offsets[i + 1],
                    "block_offsets is not monotonic: {i} -> {off} / {}",
                    offsets[i + 1]
                );
            }
        }
    }

    /// The full-feature sample's preview: every block-level tag plus the formula SVGs
    /// and the task checkboxes.
    #[test]
    fn all_features_sample_preview_has_all_tags() {
        let doc = parse(ALL_FEATURES);
        let html = crate::preview_html(&doc, "#111111", None);
        for tag in [
            "<h1>",
            "<h2>",
            "<h3>",
            "<h4>",
            "<h5>",
            "<h6>",
            "<blockquote",
            "<ul",
            "<ol",
            "<li>",
            "<table",
            "<thead>",
            "<tbody>",
            "<th",
            "<td",
            "<pre",
            "<code",
            "<hr",
            "<strong>",
            "<em>",
            "<del>",
            "<a href=",
            "<br>",
        ] {
            assert!(html.contains(tag), "the preview is missing {tag}: {html}");
        }
        assert!(
            html.matches("<svg").count() >= 4,
            "not enough formula SVGs: {html}"
        );
        assert!(
            html.contains('☐') && html.contains('☑'),
            "task checkboxes are missing: {html}"
        );
        assert!(
            html.contains("sample image"),
            "the image alt placeholder is missing: {html}"
        );
    }

    /// The offset ledger the caret rides on: every `block_offsets` entry must be
    /// inside the emitted text, monotonic, and on a character boundary — a
    /// mid-character or drifted offset makes the caret/selection land one byte off.
    #[test]
    fn block_offsets_are_sane_across_shapes() {
        let corpus = [
            "# 标题

段落 text

- a
- [ ] b

> 引用

```
code
```

| a | b |
|---|---|
| 1 | 2 |

$$
x+y
$$

---
",
            "a 

b  

> - 引用列表
> - 第二项
",
            "| 🙂 | **粗** |
|---|---|
| `c` | $x$ |
",
            "- 一级
  - 二级

1. 一
2. 二
",
        ];
        for src in corpus {
            let doc = parse(src);
            let text = emit(&doc);
            let offsets = block_offsets(&doc);
            assert_eq!(offsets.len(), doc.blocks.len(), "one offset per block");
            for (i, &o) in offsets.iter().enumerate() {
                assert!(o <= text.len(), "offset {i} past the emit text: {o}");
                assert!(
                    text.is_char_boundary(o),
                    "offset {i} splits a character: {o} in {text:?}"
                );
                if i > 0 {
                    assert!(o >= offsets[i - 1], "offsets went backwards at {i}");
                }
            }
        }
    }

    /// A cell / list item that shows up as a plain block (malformed tree) must emit
    /// its text instead of panicking mid-save.
    #[test]
    fn ungrouped_cell_and_item_emit_without_panic() {
        let td = Block::Td {
            table_seq: 0,
            row: 0,
            align: 0,
            inlines: vec![Inline::Text("格子".into())],
        };
        let out = emit(&Doc::new(vec![td]));
        assert!(out.contains("格子"), "{out}");

        let li = Block::Li {
            ordered: false,
            num: 0,
            level: 0,
            task: Some(false),
            inlines: vec![Inline::Text("条目".into())],
        };
        let out = emit(&Doc::new(vec![li]));
        assert!(out.contains("条目"), "{out}");
    }

    // ---- Review repro: parse -> emit -> parse must be a fixed point --------

    /// `parse(emit(parse(x))) == parse(x)`, and no text may vanish on the way.
    fn assert_stable(src: &str) {
        let d = parse(src);
        let out = emit(&d);
        let d2 = parse(&out);
        let plain = |doc: &Doc| {
            let mut s = String::new();
            for b in &doc.blocks {
                s.push_str(&b.plain_text());
                s.push('\n');
            }
            s
        };
        assert_eq!(
            plain(&d),
            plain(&d2),
            "text changed on round trip:\n src={src:?}\n out={out:?}"
        );
        assert_eq!(d, d2, "not a fixed point:\n src={src:?}\n out={out:?}");
    }

    /// A later paragraph of a list item must land in *that* item. After a
    /// nested list the merge looked at `blocks.last()`, which is the nested
    /// item, so the text migrated into the child. After a code block there was
    /// no Li at all and the text was dropped.
    #[test]
    fn later_paragraphs_stay_in_their_list_item() {
        assert_stable("- a\n\n  - b\n\n  c\n");
        assert_stable("- a\n\n  ```\n  x\n  ```\n\n  c\n");
        // "c" joins the *outer* item (the model has one Li per item and folds an
        // item's later paragraphs into it), never the nested one
        let d = parse("- a\n\n  - b\n\n  c\n");
        assert_eq!(
            d.blocks.iter().map(|b| b.plain_text()).collect::<Vec<_>>(),
            vec!["a c", "b"],
            "{:?}",
            d.blocks
        );
        // after a code block the text is kept at all (it used to vanish)
        let d = parse("- a\n\n  ```\n  x\n  ```\n\n  c\n");
        let texts: Vec<String> = d.blocks.iter().map(|b| b.plain_text()).collect();
        assert!(
            texts.iter().any(|t| t.contains('c')),
            "the item's later text vanished: {texts:?}"
        );
    }

    /// A block-level interrupt inside a *tight* item: the item text collected
    /// so far is not flushed yet, and the block's start event cleared the
    /// inline buffer (`Start(Paragraph)`) or pushed before the item
    /// (`Rule`/code block) — content vanished or moved behind the item.
    #[test]
    fn blocks_inside_a_tight_item_keep_its_text() {
        assert_stable("- a\n  > q\n");
        assert_stable("- a\n  ***\n");
        assert_stable("- a\n  # h\n");
        assert_stable("- a\n  ```\n  x\n  ```\n");
        let d = parse("- a\n  > q\n");
        assert_eq!(
            d.blocks.iter().map(|b| b.plain_text()).collect::<Vec<_>>(),
            vec!["a", "q"],
            "{:?}",
            d.blocks
        );
    }

    /// `[](url)` parses to a Link with no children; the emitter dropped the
    /// whole node, so one open+save deleted the link *and* its URL.
    #[test]
    fn empty_text_link_keeps_its_url() {
        assert_stable("[](https://a.io)\n");
        let out = emit(&parse("[](https://a.io)\n"));
        assert!(out.contains("https://a.io"), "{out}");
    }

    /// Trailing blank lines inside a code block are content; `trim_end_matches`
    /// removed all of them while the leading ones were kept.
    #[test]
    fn code_block_trailing_blank_lines_are_content() {
        assert_stable("```\na\n\n\n```\n");
        let d = parse("```\na\n\n\n```\n");
        assert!(
            d.blocks[0].plain_text().ends_with("a\n\n"),
            "trailing blank lines lost: {:?}",
            d.blocks[0]
        );
    }

    /// A hard break inside a list item is emitted as two trailing spaces, and
    /// the per-line trailing-space escape then turned those marker bytes into
    /// `&#32;` — the break became a soft break and the text grew two spaces.
    #[test]
    fn hard_break_inside_a_list_item_round_trips() {
        assert_stable("- a  \n  b\n");
    }

    /// One trailing space before a soft break is content too: pulldown strips
    /// all trailing whitespace before the break, so it must be escaped (the
    /// emitter only escaped when there were two or more).
    #[test]
    fn single_trailing_space_before_a_soft_break_survives() {
        let d = Doc::new(vec![Block::Para(vec![
            Inline::Text("a ".into()),
            Inline::SoftBreak,
            Inline::Text("b".into()),
        ])]);
        let out = emit(&d);
        assert_eq!(parse(&out), d, "lost the space: {out:?}");
    }

    /// GFM strips leading/trailing whitespace inside a cell; the emitter only
    /// escaped block ends for paragraphs/headings/quotes/list items.
    #[test]
    fn table_cell_edge_spaces_survive() {
        let d = Doc::new(vec![Block::Td {
            table_seq: 0,
            row: 0,
            align: 0,
            inlines: vec![Inline::Text("a ".into())],
        }]);
        let out = emit(&d);
        assert_eq!(parse(&out), d, "cell spaces lost: {out:?}");
    }

    /// A backtick fence cannot carry a backtick in its info string (pulldown
    /// rejects it as a fence opener), so a language tag containing one needs a
    /// tilde fence — emitting backticks produced a block that no longer parses.
    #[test]
    fn lang_with_a_backtick_uses_a_tilde_fence() {
        let d = Doc::new(vec![Block::Code {
            lang: Some("a`b".into()),
            text: "code".into(),
        }]);
        let out = emit(&d);
        assert_eq!(parse(&out), d, "code block collapsed: {out:?}");
    }

    /// An empty inline code span is the placeholder for an empty selection.
    /// The emitter padded both sides, and the all-spaces rule kept both, so
    /// the content grew from "" to "  ".
    #[test]
    fn empty_inline_code_round_trips() {
        // Markdown cannot spell an empty code span (the shortest one holds a
        // single space), so the placeholder degrades to **one** space — it used
        // to degrade to two and then stay there, drifting from the model.
        let d = Doc::new(vec![Block::Para(vec![Inline::Code(String::new())])]);
        let out = emit(&d);
        let got = parse(&out);
        assert_eq!(
            got,
            Doc::new(vec![Block::Para(vec![Inline::Code(" ".into())])]),
            "empty code span did not degrade to one space: {out:?}"
        );
        // and from there it is a fixed point
        assert_eq!(parse(&emit(&got)), got);
    }

    /// A `!` immediately before a link must not reassemble into an image
    /// syntax on reparse; an empty inline container between them emits zero
    /// bytes and used to hide the adjacency.
    #[test]
    fn bang_before_link_with_an_empty_container_stays_text() {
        let d = Doc::new(vec![Block::Para(vec![
            Inline::Text("a!".into()),
            Inline::Em(vec![]),
            Inline::Link {
                url: "u".into(),
                inlines: vec![Inline::Text("x".into())],
            },
        ])]);
        let out = emit(&d);
        let got = parse(&out);
        // The empty `Em` placeholder is not representable in markdown and
        // legitimately disappears; what must not happen is `a!` + `[x](u)`
        // reassembling into an image.
        assert_eq!(got.blocks[0].plain_text(), "a!x", "{got:?}");
        assert!(
            got.blocks.iter().any(|b| {
                matches!(b.core(), Block::Para(inlines) if inlines.iter().any(|i| matches!(i, Inline::Link { .. })))
            }),
            "the link was lost or became an image: {got:?}"
        );
        // and the surviving shape is stable
        assert_eq!(parse(&emit(&got)), got);
    }
}
