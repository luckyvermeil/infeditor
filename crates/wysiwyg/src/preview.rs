//! Doc → preview HTML (clean semantic tags, injected via
//! `dangerous_inner_html`).
//!
//! Unlike [`crate::render`] (the edit HTML: `data-bid` / absolutely-positioned
//! markers / offset bookkeeping), the preview has no caret/offset constraints
//! and emits semantic tags `h1-h6`/`p`/`ul`/`ol`/`li`/`blockquote`/`pre><code`/
//! `table`/`a`/`img`/`hr` — styling falls back to blitz's UA stylesheet, and
//! the host `<style>` prefix (`styled_prefix`) only customizes img/table.
//!
//! **Preview and edit share one source**: both derive from the same block tree
//! ([`crate::md::parse`], pulldown-cmark CommonMark + GFM), so their semantics
//! are naturally consistent — this is exactly the mechanism behind "preview
//! and visual editing aligned to GFM", replacing infeditor's old hand-written
//! line-based renderer (1433 lines, since deleted).
//!
//! Security model:
//! * **Inline** HTML and stray HTML text are always inerted (the md layer keeps
//!   them as text; here `escape_html` escapes them);
//! * **GFM HTML blocks** (`Block::Html`) are **passed through verbatim** per GFM
//!   semantics (the user asked to support the html block of
//!   <https://github.github.com/gfm/>): block-level raw HTML injected via
//!   `dangerous_inner_html` really executes — this is GFM's established
//!   behavior (GitHub additionally sanitizes; per the user's request this
//!   project does not sanitize);
//! * Link/image URLs pass a scheme whitelist (`safe_link_url`/`safe_image_url`);
//!   `javascript:`/`data:` etc. are rejected and fall back to plain text / alt
//!   text, and content is never swallowed;
//! * GFM extended autolinks: a bare `http(s)://…` / `www.…` is split on the Text
//!   node (splitting on the tree needs no host-side HTML-tag state machine —
//!   `<code>`/`<a>` content never passes through the Text branch at all); an
//!   explicit `<https://…>` is parsed into a Link by pulldown per CommonMark.
//!   A bare URL inside link **text** is not split (the `link_depth` guard) —
//!   a nested `<a>` is illegal.
//!
//! Known boundaries (established trade-offs of the leaf-block model, see the
//! crate docs): quotes are flattened (consecutive Quotes each become their own
//! blockquote, nested quotes do not nest); soft breaks fold into spaces
//! (CommonMark allows it).

use std::path::Path;

use crate::math;
use crate::model::{Block, Doc, Inline};

/// Append the HTML-significant characters into `out` (the same rules for text
/// nodes and double-quoted attribute values).
/// The render layer (`render`) and the preview share it — two hand-written
/// escapers drift sooner or later, and the render side once missed `"`
/// (attribute truncation).
pub fn push_escape_html(out: &mut String, s: &str) {
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(c),
        }
    }
}

/// Escape the HTML-significant characters (shared by text and attribute
/// values; attributes use double quotes).
pub fn escape_html(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    push_escape_html(&mut o, s);
    o
}

/// Math rendering context: only carries the theme foreground.
///
/// The formula glyph fill is baked into the SVG and must follow the theme color
/// (see [`math::MathSpec`]). Typeset results go through `MathSpec::cached`'s
/// process-level cache (key = display|fg|px|src): reused across doc.gen and
/// across Tabs, so each Tab no longer needs to keep its own HashMap. What is
/// cached is the **bare SVG**, and the wrapping is spliced fresh each time —
/// the same formula in a heading and in a paragraph share one typesetting
/// result. Reference definitions are already consumed by pulldown at the parse
/// layer (link events carry their own dest).
pub struct MathCtx<'a> {
    fg: &'a str,
    /// Typesetting body size (CSS px) for the block currently being emitted.
    ///
    /// Formula SVGs are sized in px and therefore do not inherit the
    /// surrounding `font-size`: a formula in an `<h1>` (2em) would stay at body
    /// size and look small next to its own heading. `emit_block`'s head branch
    /// raises this for the duration of the heading and restores it after.
    px: f32,
}

impl<'a> MathCtx<'a> {
    pub fn new(fg: &'a str) -> Self {
        Self { fg, px: BODY_PX }
    }

    fn render(&mut self, src: &str, display: bool, phrasing: bool) -> Option<String> {
        let spec = if display {
            math::MathSpec::block(src, self.fg, self.px)
        } else {
            math::MathSpec::inline(src, self.fg, self.px)
        };
        Some(wrap_math(&spec.cached()?, display, phrasing))
    }
}

/// Preview body font size (aligned to GitHub's body 16px).
const BODY_PX: f32 = 16.0;

/// Heading font size relative to the body, h1..h6 (= github-markdown-css's
/// 2/1.5/1.25/1/0.875/0.85 em). The **single source** for both the preview
/// stylesheet ([`github_css`]) and the formula typesetting scale (see
/// `MathCtx::px`) — when these two drift, a heading's formulas are the wrong
/// size again.
pub const HEAD_EM: [f32; 6] = [2.0, 1.5, 1.25, 1.0, 0.875, 0.85];

/// Formula HTML wrapping: inline is vertically centered; block-level occupies
/// its own line, centered.
///
/// When `phrasing=true` (a place that only allows phrasing content, such as a
/// heading/emphasis/link), a block-level formula uses a `display:block`
/// `<span>` instead — a `<div>` in those containers violates the content model
/// and the parser would break the heading apart.
fn wrap_math(svg: &str, display: bool, phrasing: bool) -> String {
    match (display, phrasing) {
        (false, _) => format!("<span style=\"vertical-align:middle;\">{svg}</span>"),
        (true, false) => format!("<div style=\"margin:10px 0;text-align:center;\">{svg}</div>"),
        (true, true) => format!("<span style=\"display:block;margin:10px 0;\">{svg}</span>"),
    }
}

/// Doc → preview HTML body. `fg` is the theme foreground (`#rrggbb`, the
/// formula SVG fill), `math_cache` is held by the caller (a Tab-level formula
/// cache), and `base` is the document path — relative-path images are resolved
/// against it into file:// URLs.
pub fn preview_html(doc: &Doc, fg: &str, base: Option<&Path>) -> String {
    let mut ctx = Ctx {
        out: String::with_capacity(doc.blocks.len() * 64),
        fg,
        math: MathCtx::new(fg),
        base,
        zebra: Some(gh_theme(fg).code_bg),
        link_depth: 0,
        phrasing: 0,
    };
    let mut i = 0;
    while i < doc.blocks.len() {
        // Grouping does not cross quote-depth boundaries; a Quoted (block-level
        // content inside a quote) is wrapped in a blockquote, and the
        // Block::Quote compatible shape already carries its own blockquote
        // (keeping "each Quote block becomes its own segment").
        let depth = doc.blocks[i].quote_depth();
        let quoted = matches!(doc.blocks[i], Block::Quoted { .. });
        match doc.blocks[i].core() {
            // Consecutive Li group into a list region (nested by level,
            // orderedness per level).
            Block::Li { .. } => {
                let start = i;
                while i < doc.blocks.len()
                    && doc.blocks[i].quote_depth() == depth
                    && matches!(doc.blocks[i].core(), Block::Li { .. })
                {
                    i += 1;
                }
                let mark = ctx.out.len();
                ctx.emit_list_region(&doc.blocks[start..i]);
                if quoted {
                    ctx.wrap_blockquote(mark, depth);
                }
            }
            // Consecutive same-seq Td group into a table.
            Block::Td { table_seq, .. } => {
                let seq = *table_seq;
                let start = i;
                while i < doc.blocks.len()
                    && doc.blocks[i].quote_depth() == depth
                    && matches!(doc.blocks[i].core(), Block::Td { table_seq: s, .. } if *s == seq)
                {
                    i += 1;
                }
                let mark = ctx.out.len();
                ctx.emit_table(&doc.blocks[start..i]);
                if quoted {
                    ctx.wrap_blockquote(mark, depth);
                }
            }
            b => {
                let mark = ctx.out.len();
                ctx.emit_block(b);
                if quoted {
                    ctx.wrap_blockquote(mark, depth);
                }
                i += 1;
            }
        }
    }
    ctx.out
}

struct Ctx<'a> {
    out: String,
    /// Primary foreground: the code-block coloring hook picks the light/dark
    /// palette from it.
    fg: &'a str,
    math: MathCtx<'a>,
    base: Option<&'a Path>,
    /// Table zebra-striping background (the GitHub baseline's even data-row
    /// color; None = none).
    zebra: Option<&'static str>,
    /// Link-text nesting depth: when >0, Text no longer splits bare URLs (a
    /// nested `<a>` is illegal).
    link_depth: usize,
    /// Phrasing-only context depth (h1-h6/strong/em/del/a): a display formula
    /// must not emit a `<div>` (a content-model violation), so it wraps in a
    /// span instead.
    phrasing: usize,
}

impl<'a> Ctx<'a> {
    fn emit_block(&mut self, b: &Block) {
        match b {
            Block::Para(inlines) => self.emit_para_flow(inlines),
            Block::Head { level, inlines } => {
                let l = (*level).clamp(1, 6);
                self.out.push_str(&format!("<h{l}>"));
                // h1-h6 only allow phrasing content: a display formula degrades
                // to a span here
                self.phrasing += 1;
                // Formulas inside a heading are typeset at the heading's own
                // size (the SVG carries px, so it cannot inherit `font-size`).
                let saved = self.math.px;
                self.math.px = BODY_PX * HEAD_EM[l as usize - 1];
                self.emit_inlines(inlines);
                self.math.px = saved;
                self.phrasing -= 1;
                self.out.push_str(&format!("</h{l}>"));
            }
            Block::Code { lang, text } => {
                // Coloring hook (registered by the host): takes the raw code and
                // the foreground and returns HTML with <span>s; when not
                // registered, escape as plain text. The language name is only a
                // class and a hook argument; for an unrecognized language the
                // hook falls back to plain text internally.
                let body = match lang {
                    Some(l) => {
                        crate::hl::highlight(l, text, self.fg).unwrap_or_else(|| escape_html(text))
                    }
                    None => escape_html(text),
                };
                match lang {
                    Some(l) => {
                        let l = escape_html(l);
                        self.out
                            .push_str(&format!("<pre><code class=\"lang-{l}\">"));
                        self.out.push_str(&body);
                        self.out.push_str("</code></pre>");
                    }
                    None => {
                        self.out.push_str("<pre><code>");
                        self.out.push_str(&body);
                        self.out.push_str("</code></pre>");
                    }
                }
            }
            Block::Quote(inlines) => {
                // A display formula inside a quote also goes through segmenting
                // (a div is legal in a blockquote, but the old path of a div
                // nested in a p was illegal).
                self.out.push_str("<blockquote>");
                self.emit_para_flow(inlines);
                self.out.push_str("</blockquote>");
            }
            Block::Hr => self.out.push_str("<hr>"),
            // GFM HTML block: the raw HTML is **passed through verbatim** (GFM
            // semantics: block-level HTML is not escaped and is handed to the
            // renderer to execute). Inline HTML still goes through the Text branch
            // to be escaped.
            Block::Html { text } => self.out.push_str(text),
            Block::Li { inlines, .. } | Block::Td { inlines, .. } => {
                // Grouped blocks leaking into the ungrouped path: preview the content
                // as a paragraph rather than panicking.
                self.out.push_str("<p>");
                self.emit_inlines(inlines);
                self.out.push_str("</p>");
            }
            // Quote-ownership wrapper: emit the inner as-is (the blockquote is
            // on the grouping path)
            Block::Quoted { inner, .. } => self.emit_block(inner),
        }
    }

    /// Wrap the output starting at `mark` in `depth` layers of `<blockquote>`
    /// (a nested quote = a nested blockquote, semantically the same as
    /// Markdown's `> >`).
    fn wrap_blockquote(&mut self, mark: usize, depth: u8) {
        let open = "<blockquote>".repeat(depth as usize);
        let close = "</blockquote>".repeat(depth as usize);
        self.out.insert_str(mark, &open);
        self.out.push_str(&close);
    }

    /// Paragraph text flow (shared by Para and Quote). No display formula → a
    /// single `<p>`; mixed with display formulas → segment by formula: each
    /// text fragment is wrapped in `<p>`, and a formula becomes a block-level
    /// `<div>`. p>div is an illegal nesting that html5ever would split apart —
    /// the trailing text would be left outside the paragraph, and an empty `<p>`
    /// would add stray margin.
    fn emit_para_flow(&mut self, inlines: &[Inline]) {
        if !inlines
            .iter()
            .any(|i| matches!(i, Inline::Math { display: true, .. }))
        {
            self.emit_p(inlines);
            return;
        }
        let mut run = 0;
        for (idx, i) in inlines.iter().enumerate() {
            if let Inline::Math { display: true, src } = i {
                self.emit_p(&inlines[run..idx]);
                // After segmenting, the formula stands alone as a block (not inside
                // any p) — the normal div form
                match self.math.render(src, true, false) {
                    Some(html) => self.out.push_str(&html),
                    // A failed render's fallback text must also be wrapped in <p>
                    // (after segmenting there is no outer p anymore)
                    None => self
                        .out
                        .push_str(&format!("<p>$${}$$</p>", escape_html(src))),
                }
                run = idx + 1;
            }
        }
        self.emit_p(&inlines[run..]);
    }

    /// Emit one `<p>`; a whitespace-only fragment is skipped (an empty `<p>`
    /// has only margin and no content).
    fn emit_p(&mut self, inlines: &[Inline]) {
        let has_content = inlines
            .iter()
            .any(|i| !matches!(i, Inline::Text(t) if t.chars().all(char::is_whitespace)));
        if !has_content {
            return;
        }
        self.out.push_str("<p>");
        self.emit_inlines(inlines);
        self.out.push_str("</p>");
    }

    /// A list region: a level stack opens/closes nested `<ul>`/`<ol>` (the same
    /// algorithm as render::emit_list_region — orderedness may differ per
    /// level; when it changes at the same level, close first then open to keep
    /// the DOM legal).
    ///
    /// A list's `num` must go into the HTML: `<ol start>` handles a non-1
    /// start, and `<li value>` is added when a later item's number is
    /// discontinuous from the expected one — otherwise the preview's numbering
    /// would disagree with the edit view's marker (render emits `{num}. `
    /// directly).
    fn emit_list_region(&mut self, items: &[Block]) {
        // (level, ordered, next expected number)
        let mut stack: Vec<(u8, bool, u64)> = Vec::new();
        for b in items {
            let Block::Li {
                ordered,
                num,
                level,
                task,
                inlines,
            } = b.core()
            else {
                continue;
            };
            while let Some(&(tl, to, _)) = stack.last() {
                if tl > *level || (tl == *level && to != *ordered) {
                    let tag = if to { "ol" } else { "ul" };
                    self.out.push_str(&format!("</{tag}>"));
                    stack.pop();
                } else {
                    break;
                }
            }
            let need_open = match stack.last() {
                None => true,
                Some(&(tl, _, _)) => tl < *level,
            };
            if need_open {
                if *ordered {
                    // start=1 is the default and is not emitted extra (GitHub
                    // outputs the same shape)
                    if *num == 1 {
                        self.out.push_str("<ol>");
                    } else {
                        self.out.push_str(&format!("<ol start=\"{num}\">"));
                    }
                    // The first item is decided by start and does not count as
                    // a number jump (expected is set to num first)
                    stack.push((*level, true, *num));
                } else {
                    self.out.push_str("<ul>");
                    stack.push((*level, false, 0));
                }
            }
            let value = match stack.last_mut() {
                Some((_, true, expected)) => {
                    let v = (*num != *expected).then_some(*num);
                    *expected = num.saturating_add(1);
                    v
                }
                _ => None,
            };
            match value {
                Some(v) => self.out.push_str(&format!("<li value=\"{v}\">")),
                None => self.out.push_str("<li>"),
            }
            if let Some(checked) = task {
                // GFM task list: a ☐/☑ prefix (U+2610/U+2611 both have glyphs in
                // the CJK fallback font); the ordered number is preserved by
                // <ol start>/<li value>.
                let glyph = if *checked { "\u{2611}" } else { "\u{2610}" };
                self.out
                    .push_str(&format!("<span style=\"opacity:0.85;\">{glyph}</span> "));
            }
            self.emit_inlines(inlines);
            self.out.push_str("</li>");
        }
        while let Some((_, to, _)) = stack.pop() {
            let tag = if to { "ol" } else { "ul" };
            self.out.push_str(&format!("</{tag}>"));
        }
    }

    /// Table: row 0 is the header (thead/th), the rest tbody/td. Alignment
    /// follows the original implementation's `style="text-align:…"` marking
    /// (the host's styled_prefix depends on the md-tbl class name).
    fn emit_table(&mut self, items: &[Block]) {
        // Td's row values are not guaranteed monotonic (cross-table paste/edit
        // can disorder them). Group by row, emit in ascending order — row 0 is
        // the header, the rest tbody; within a group the appearance order
        // (column order) is kept. Same algorithm as render::emit_table (edit and
        // preview must be isomorphic).
        let mut rows: Vec<(usize, Vec<&Block>)> = Vec::new();
        for b in items {
            let Block::Td { row, .. } = b.core() else {
                continue;
            };
            match rows.iter_mut().find(|(r, _)| r == row) {
                Some((_, cells)) => cells.push(b.core()),
                None => rows.push((*row, vec![b.core()])),
            }
        }
        rows.sort_by_key(|(r, _)| *r);
        self.out.push_str("<table class=\"md-tbl\">");
        let mut thead_open = false;
        let mut tbody_open = false;
        for (row, cells) in &rows {
            if *row == 0 {
                self.out.push_str("<thead><tr>");
                thead_open = true;
            } else {
                if thead_open {
                    self.out.push_str("</thead>");
                    thead_open = false;
                }
                if !tbody_open {
                    self.out.push_str("<tbody>");
                    tbody_open = true;
                }
                self.out.push_str("<tr>");
            }
            for b in cells {
                let Block::Td {
                    row: _,
                    align,
                    inlines,
                    ..
                } = b
                else {
                    continue;
                };
                let al = align_name(*align);
                let tag = if *row == 0 { "th" } else { "td" };
                // Zebra striping: even data rows inside tbody (GitHub's
                // tbody tr:nth-child(2n) semantics)
                let zebra = match self.zebra {
                    Some(c) if *row >= 1 && *row % 2 == 0 => format!(";background:{c}"),
                    _ => String::new(),
                };
                self.out
                    .push_str(&format!("<{tag} style=\"text-align:{al}{zebra}\">"));
                self.emit_inlines(inlines);
                self.out.push_str(&format!("</{tag}>"));
            }
            self.out.push_str("</tr>");
        }
        if thead_open {
            self.out.push_str("</thead>");
        }
        if tbody_open {
            self.out.push_str("</tbody>");
        }
        self.out.push_str("</table>");
    }

    fn emit_inlines(&mut self, inlines: &[Inline]) {
        for i in inlines {
            match i {
                Inline::Text(t) => {
                    if self.link_depth > 0 {
                        // A bare URL inside link text no longer splits an <a>
                        // (a nested a is illegal; GitHub has the same semantics);
                        // the outer link's href is already available.
                        self.out.push_str(&escape_html(t));
                    } else {
                        push_text_autolink(&mut self.out, t);
                    }
                }
                Inline::Code(c) => {
                    self.out.push_str("<code>");
                    self.out.push_str(&escape_html(c));
                    self.out.push_str("</code>");
                }
                Inline::Strong(c) => {
                    // strong/em/del only allow phrasing content: a display
                    // formula degrades to a span
                    self.phrasing += 1;
                    self.out.push_str("<strong>");
                    self.emit_inlines(c);
                    self.out.push_str("</strong>");
                    self.phrasing -= 1;
                }
                Inline::Em(c) => {
                    self.phrasing += 1;
                    self.out.push_str("<em>");
                    self.emit_inlines(c);
                    self.out.push_str("</em>");
                    self.phrasing -= 1;
                }
                Inline::Del(c) => {
                    self.phrasing += 1;
                    self.out.push_str("<del>");
                    self.emit_inlines(c);
                    self.out.push_str("</del>");
                    self.phrasing -= 1;
                }
                Inline::Link { url, inlines: c } => {
                    // a's content model is transparent: inside a p it is the
                    // phrasing constraint, so handle it uniformly as phrasing
                    // (bold/italic/formula never nest block-level content)
                    self.link_depth += 1;
                    self.phrasing += 1;
                    if let Some(u) = safe_link_url(url) {
                        self.out
                            .push_str(&format!("<a href=\"{}\">", escape_html(&u)));
                        self.emit_inlines(c);
                        self.out.push_str("</a>");
                    } else {
                        // URL rejected: show only the text (GitHub semantics; a
                        // script URL is never emitted)
                        self.emit_inlines(c);
                    }
                    self.phrasing -= 1;
                    self.link_depth -= 1;
                }
                Inline::Image { url, alt } => {
                    if let Some(u) = safe_image_url(url, self.base) {
                        self.out.push_str(&format!(
                            "<img class=\"md-img\" src=\"{}\" alt=\"{}\">",
                            escape_html(&u),
                            escape_html(alt)
                        ));
                    } else {
                        // Image URL rejected: fall back to the alt text (the
                        // de-emphasized display is handled by CSS)
                        self.out.push_str(&format!(
                            "<span class=\"md-img-fb\">[{}]</span>",
                            escape_html(alt)
                        ));
                    }
                }
                Inline::Math { src, display } => {
                    // A formula that fails to render falls back to text as-is
                    // (delimiters included); content is never swallowed.
                    match self.math.render(src, *display, self.phrasing > 0) {
                        Some(svg) => self.out.push_str(&svg),
                        None => {
                            let d = if *display { "$$" } else { "$" };
                            self.out.push_str(&format!("{d}{}{d}", escape_html(src)));
                        }
                    }
                }
                // An in-block break (soft/hard): both become <br> in HTML
                Inline::SoftBreak | Inline::HardBreak => self.out.push_str("<br>"),
            }
        }
    }
}

fn align_name(align: u8) -> &'static str {
    match align {
        2 => "center",
        3 => "right",
        _ => "left",
    }
}

// ---- GitHub preview theme (aligned with github-markdown-css) ----------------
//
// The shape tokens are taken from github-markdown-css (GitHub's official
// rendering styles): h1/h2 bottom borders, rounded code-block backgrounds,
// 1px table borders + zebra striping, blockquote left border with muted text,
// blue links. The light/dark baselines are chosen automatically by the theme
// foreground's luminance (the host's menu_fg only has dark #c9d1d9 / light
// #333333, so the luminance judgment is stable). The output goes through the
// host's proven `<style>` prefix injection path (spliced by
// mdmap::styled_prefix); table zebra striping does not rely on `:nth-child` —
// `emit_table` inlines a background on the td of even data rows (the steady
// approach when blitz's selector support surface is unknown).

struct GhTheme {
    muted: &'static str,
    border: &'static str,
    code_bg: &'static str,
    link: &'static str,
}

/// GitHub light baseline (github-markdown-css v5.x).
const GH_LIGHT: GhTheme = GhTheme {
    muted: "#59636e",
    border: "#d1d9e0",
    code_bg: "#f6f8fa",
    link: "#0969da",
};
/// GitHub dark (default dark) baseline.
const GH_DARK: GhTheme = GhTheme {
    muted: "#9198a1",
    border: "#3d444d",
    code_bg: "#151b23",
    link: "#4493f8",
};

/// Perceived luminance of `#rrggbb` (Rec. 601 weights, 0..1). A parse failure
/// is treated as dark (returns 0 → picks the light baseline, prioritizing
/// light body-text readability).
fn hex_luma(s: &str) -> f32 {
    let hex = s.trim().trim_start_matches('#');
    if hex.len() != 6 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return 0.0;
    }
    let ch = |r: std::ops::Range<usize>| {
        u8::from_str_radix(&hex[r], 16)
            .map(|v| v as f32 / 255.0)
            .unwrap_or(0.0)
    };
    let (r, g, b) = (ch(0..2), ch(2..4), ch(4..6));
    0.299 * r + 0.587 * g + 0.114 * b
}

/// Pick the GitHub baseline by the theme foreground's luminance: a bright
/// foreground = a dark theme.
fn gh_theme(fg: &str) -> &'static GhTheme {
    if hex_luma(fg) > 0.5 {
        &GH_DARK
    } else {
        &GH_LIGHT
    }
}

/// GitHub-style markdown preview stylesheet (without the `<style>` tag; the
/// caller injects it).
///
/// Covers the semantic tags the preview emits (h1-h6/p/a/ul/ol/blockquote/
/// code/pre/hr/table/th/td/img) and the existing classes (md-img/md-img-fb);
/// `text-align` is carried by `emit_table`'s inline style and is not repeated
/// here.
pub fn github_css(fg: &str) -> String {
    let t = gh_theme(fg);
    let mono = "'Cascadia Mono',Consolas,'Courier New',monospace";
    // Heading font sizes come from [`HEAD_EM`], the same table the formula
    // typesetting uses — one list, so "heading text 2em / formula 16px" cannot
    // come back.
    let border = t.border;
    let mut heads = String::new();
    for (i, em) in HEAD_EM.iter().enumerate() {
        let extra = match i {
            0 | 1 => format!(" border-bottom:1px solid {border}; padding-bottom:.3em;"),
            5 => format!(" color:{};", t.muted),
            _ => String::new(),
        };
        heads.push_str(&format!("h{} {{ font-size:{em}em;{extra} }} ", i + 1));
    }
    format!(
        "h1,h2,h3,h4,h5,h6 {{ margin:24px 0 16px; font-weight:600; line-height:1.25; }} \
         {heads}\
         p {{ margin:0 0 16px; }} \
         a {{ color:{link}; text-decoration:none; }} \
         ul,ol {{ padding-left:2em; margin:0 0 16px; }} \
         li {{ margin:4px 0; }} \
         blockquote {{ margin:0 0 16px; padding:0 1em; color:{muted}; \
         border-left:.25em solid {border}; }} \
         code {{ padding:.2em .4em; background:{code_bg}; border-radius:6px; \
         font-size:85%; font-family:{mono}; }} \
         pre {{ padding:16px; background:{code_bg}; border-radius:6px; margin:0 0 16px; \
         line-height:1.45; overflow:auto; font-size:85%; font-family:{mono}; }} \
         pre > code {{ padding:0; background:transparent; font-size:100%; }} \
         hr {{ height:2px; border:0; background:{border}; margin:24px 0; }} \
         table {{ border-collapse:collapse; margin:0 0 16px; }} \
         th,td {{ border:1px solid {border}; padding:6px 13px; }} \
         th {{ font-weight:600; }} \
         img {{ max-width:100%; }} \
         img.md-img {{ display:block; max-width:100%; margin:10px 0; border-radius:4px; }} \
         .md-img-fb {{ opacity:.55; font-size:12px; }}",
        border = t.border,
        muted = t.muted,
        link = t.link,
        code_bg = t.code_bg,
        mono = mono,
    )
}

// ---- URL security policy (ported from infeditor's former
// format/markdown.rs; semantics unchanged) ------------------------------------

/// Image src resolution and whitelist. Returns a URL that can be placed
/// directly into `src="…"`.
///
/// * `http://` / `https://` — passed through as-is (network images);
/// * `file://` — normalized into a canonical file URL (percent-encoding done by
///   the url crate);
/// * a scheme-less path — an absolute path is used directly; a relative path is
///   resolved against the MD file's directory;
/// * any other scheme (`javascript:`/`data:`/`ftp:` …) — rejected; the caller
///   falls back to the alt text.
pub fn safe_image_url(raw: &str, base: Option<&Path>) -> Option<String> {
    let raw = raw.trim();
    // Bare control characters (tab/CR/LF) are rejected: HTML attribute
    // normalization strips them, meaning a concatenation like `java\tscript:`
    // can fool the scheme check; a valid URL has no bare control characters.
    if raw.is_empty() || raw.contains(['<', '>', '"']) || raw.contains(['\t', '\n', '\r']) {
        return None;
    }
    // A Windows drive path (C:\… / C:/…): a single letter + ':' would be
    // mistaken for a protocol by the scheme check ("c" is a legal scheme
    // character) and rejected, so peel it off first and take the Path branch.
    let win_drive = {
        let b = raw.as_bytes();
        b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':'
    };
    let scheme = if win_drive {
        None
    } else {
        raw.split_once(':')
            .filter(|(s, rest)| {
                !s.is_empty()
                    && !rest.is_empty()
                    && s.chars()
                        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
            })
            .map(|(s, _)| s.to_ascii_lowercase())
    };
    match scheme.as_deref() {
        Some("http") | Some("https") => Some(raw.to_string()),
        Some("file") => {
            let u = blitz_traits::net::Url::parse(raw).ok()?;
            let path = u.to_file_path().ok()?;
            blitz_traits::net::Url::from_file_path(path)
                .ok()
                .map(|u| u.to_string())
        }
        // A protocol-relative URL (//host/x.png) has no reasonable base under the
        // editor's offline semantics; reject
        Some(_) => None,
        None => {
            let p = Path::new(raw);
            let full = if p.is_absolute() {
                p.to_path_buf()
            } else {
                base?.join(p)
            };
            blitz_traits::net::Url::from_file_path(full)
                .ok()
                .map(|u| u.to_string())
        }
    }
}

/// Actual readability of a local image (used by editor rendering).
/// `safe_image_url` only does the URL whitelist and **does not stat the file** —
/// the preview and unit tests depend on its pure-policy semantics. Editor
/// rendering needs to fall back to the alt placeholder for a local image that
/// "resolved but whose file does not exist" (otherwise it renders as a blank
/// `<img>` and the user sees "nothing"), so this is used as a second filter.
/// http(s) cannot be pre-checked → always available.
pub fn image_file_available(u: &str) -> bool {
    if u.starts_with("http://") || u.starts_with("https://") {
        return true;
    }
    blitz_traits::net::Url::parse(u)
        .ok()
        .and_then(|x| x.to_file_path().ok())
        .is_none_or(|p| p.exists())
}

/// Link href whitelist: http/https/mailto and relative paths (the preview has
/// no navigation provider, so links are not clickable anyway; this guards
/// against script URLs once navigation is wired up).
/// `pub(crate)`: the edit side's Ctrl+click-to-open (ui.rs) must go through the
/// same whitelist — two exits with inconsistent policy is no defense at all.
pub fn safe_link_url(raw: &str) -> Option<String> {
    let raw = raw.trim();
    // Control characters rejected: see safe_image_url (`java\tscript:` bypassing
    // the scheme check).
    if raw.is_empty() || raw.contains(['<', '>', '"']) || raw.contains(['\t', '\n', '\r']) {
        return None;
    }
    // A Windows drive absolute path (C:\…) is also allowed (same semantics as
    // the image side).
    let win_drive = {
        let b = raw.as_bytes();
        b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':'
    };
    let scheme = if win_drive {
        None
    } else {
        raw.split_once(':')
            .filter(|(s, _)| {
                !s.is_empty()
                    && s.chars()
                        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
            })
            .map(|(s, _)| s.to_ascii_lowercase())
    };
    match scheme.as_deref() {
        None | Some("http") | Some("https") | Some("mailto") => Some(raw.to_string()),
        Some(_) => None,
    }
}

// ---- GFM extended autolinks --------------------------------------------------

const URL_TAIL_CHARS: &str = "-_/+&~%=#?!:.$'*@";

/// GFM extended autolinks: bare `http(s)://…` and `www.…` (an explicit
/// `<https://…>` is parsed into a Link by pulldown per CommonMark and does not
/// pass through here).
///
/// Split on the **raw text** (each fragment escaped independently); `<code>`
/// content does not pass through here. A bare URL inside link text is intercepted
/// by `Ctx::link_depth` on the Text branch (not called here) — a nested `<a>`
/// is illegal.
fn push_text_autolink(out: &mut String, s: &str) {
    let mut rest = s;
    while let Some((start, len)) = next_bare_url(rest) {
        out.push_str(&escape_html(&rest[..start]));
        let raw = &rest[start..start + len];
        let href = if raw.starts_with("www.") {
            format!("http://{raw}")
        } else {
            raw.to_string()
        };
        out.push_str(&format!(
            "<a href=\"{}\">{}</a>",
            escape_html(&href),
            escape_html(raw)
        ));
        rest = &rest[start + len..];
    }
    out.push_str(&escape_html(rest));
}

/// The `(start, byte length)` of the next bare URL. Scanning starts only at a
/// word start (the previous char is not alphanumeric/`&`); stepping is by
/// **character boundary** (slicing by byte in multibyte text would panic).
fn next_bare_url(s: &str) -> Option<(usize, usize)> {
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        let word_start = i == 0 || !(b[i - 1].is_ascii_alphanumeric() || b[i - 1] == b'&');
        if word_start {
            if let Some(len) = scan_bare_url(&s[i..]) {
                return Some((i, len));
            }
        }
        let ch_len = s[i..].chars().next().map(char::len_utf8).unwrap_or(1);
        i += ch_len;
    }
    None
}

/// GFM extended autolink tail collection: returns the bare URL's byte length
/// (0/None = not a URL). (Not split inside link text — guarded by
/// `Ctx::link_depth` on the Text branch; not handled here.)
fn scan_bare_url(rest: &str) -> Option<usize> {
    let scheme_len = if rest.starts_with("https://") {
        8
    } else if rest.starts_with("http://") {
        7
    } else if rest.starts_with("www.") {
        0 // no scheme; the href gets http:// prepended
    } else {
        return None;
    };
    let mut end = rest.len();
    for (idx, c) in rest.char_indices().skip(scheme_len) {
        // '.' and '/' are already included in URL_TAIL_CHARS
        if c.is_ascii_alphanumeric() || URL_TAIL_CHARS.contains(c) {
            continue;
        }
        end = idx;
        break;
    }
    if end <= scheme_len.max(4) {
        return None; // there must be at least the substance of "www.x" / "http://x"
    }
    // The www. form requires an actual dotted domain segment after it
    if scheme_len == 0 && !rest[4..end].contains('.') {
        return None;
    }
    // Strip trailing punctuation and unpaired right parens
    let mut bytes = &rest.as_bytes()[..end];
    while let Some(&last) = bytes.last() {
        let strip = matches!(
            last,
            b'?' | b'!' | b'.' | b',' | b':' | b';' | b'*' | b'_' | b'~' | b'\'' | b'"'
        ) || (last == b')' && !bytes.contains(&b'('));
        if !strip {
            break;
        }
        bytes = &bytes[..bytes.len() - 1];
    }
    if bytes.len() <= scheme_len.max(4) {
        return None;
    }
    Some(bytes.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::md;

    fn pv(src: &str) -> String {
        preview_html(&md::parse(src), math::DEFAULT_FG, None)
    }

    fn pv_base(src: &str, base: &Path) -> String {
        preview_html(&md::parse(src), math::DEFAULT_FG, Some(base))
    }

    // ---- Basic block-level ----

    /// A list's start number and number jumps must go into the HTML (the edit
    /// view's marker emits `{num}. ` directly; the preview needs `<ol start>` /
    /// `<li value>` to agree with it).
    #[test]
    fn ordered_list_numbers_preserved() {
        let html = pv("3. three\n4. four\n");
        assert!(html.contains("<ol start=\"3\">"), "{html}");
        assert!(html.contains("<li>three</li>"), "{html}");

        // A manual number jump (editing/pasting can produce non-contiguous nums):
        // add a value
        let doc = Doc::new(vec![
            Block::Li {
                ordered: true,
                num: 1,
                level: 0,
                task: None,
                inlines: vec![Inline::Text("a".into())],
            },
            Block::Li {
                ordered: true,
                num: 7,
                level: 0,
                task: None,
                inlines: vec![Inline::Text("b".into())],
            },
        ]);
        let html = preview_html(&doc, math::DEFAULT_FG, None);
        assert!(
            html.contains("<ol><li>a</li><li value=\"7\">b</li></ol>"),
            "{html}"
        );
    }

    /// When Td rows are out of order (cross-table paste/edit) the table
    /// structure must stay well-formed — only one thead for the header, data
    /// rows enter tbody in ascending row order.
    #[test]
    fn table_rows_out_of_order_stay_well_formed() {
        let cell = |row: usize, t: &str| Block::Td {
            table_seq: 0,
            row,
            align: 0,
            inlines: vec![Inline::Text(t.into())],
        };
        let doc = Doc::new(vec![cell(1, "b"), cell(0, "h"), cell(2, "c")]);
        let html = preview_html(&doc, math::DEFAULT_FG, None);
        assert_eq!(html.matches("<thead>").count(), 1, "{html}");
        assert_eq!(html.matches("<tbody>").count(), 1, "{html}");
        assert!(
            html.contains(
                "<thead><tr><th style=\"text-align:left\">h</th></tr></thead>\
                          <tbody><tr><td style=\"text-align:left\">b</td></tr>\
                          <tr><td style=\"text-align:left;background:#f6f8fa\">c</td></tr></tbody>"
            ),
            "{html}"
        );
    }

    #[test]
    fn code_fence_has_single_open_tag() {
        let html = pv("```\nlet x = 1;\n```\n");
        assert_eq!(html.matches("<pre><code>").count(), 1, "{html}");
        assert_eq!(html.matches("</code></pre>").count(), 1, "{html}");
        // An unclosed fence up to EOF: still only one open tag
        let unclosed = pv("```\nlet y = 2;\n");
        assert_eq!(unclosed.matches("<pre><code>").count(), 1, "{unclosed}");
        assert_eq!(unclosed.matches("</code></pre>").count(), 1, "{unclosed}");
        // With a language annotation
        let lang = pv("```rust\nfn main() {}\n```\n");
        assert_eq!(lang.matches("<pre><code").count(), 1, "{lang}");
        assert!(lang.contains("lang-rust"), "{lang}");
    }

    #[test]
    fn basics() {
        let html = pv("# 标题\n\n**bold** and *it* `code`\n\n- a\n- b\n\n1. x\n2. y");
        assert!(html.contains("<h1>标题</h1>"), "{html}");
        assert!(html.contains("<strong>bold</strong>"));
        assert!(html.contains("<em>it</em>"));
        assert!(html.contains("<code>code</code>"));
        assert!(html.contains("<ul>"), "{html}");
        assert!(html.contains("<ol>"), "{html}");
        assert!(html.contains("<li>a</li>"), "{html}");
    }

    #[test]
    fn code_fence_and_escape() {
        let html = pv("```rust\nlet a = \"<b>\";\n```\n");
        assert!(html.contains("<pre><code class=\"lang-rust\">"), "{html}");
        assert!(html.contains("&quot;&lt;b&gt;&quot;"), "{html}");
    }

    #[test]
    fn link_and_paragraph_break() {
        let html = pv("see [docs](https://x.io/a?b=1) now");
        assert!(
            html.contains("<a href=\"https://x.io/a?b=1\">docs</a>"),
            "{html}"
        );
        // Soft line breaks keep their newline semantics (<br>), consistent
        // with the wysiwyg edit view (they used to fold into a space)
        let html2 = pv("p1\np2\n\np3");
        assert!(html2.contains("<p>p1<br>p2</p>"), "{html2}");
        assert!(html2.contains("<p>p3</p>"));
    }

    #[test]
    fn setext_heading() {
        // pulldown (CommonMark) supports setext — the old line-based renderer
        // did not
        let html = pv("Title\n=====\n\nbody");
        assert!(html.contains("<h1>Title</h1>"), "{html}");
        assert!(html.contains("<p>body</p>"));
    }

    #[test]
    fn html_in_text_is_inert() {
        // **Inline** HTML does not execute but its content is kept (the md
        // layer keeps it as text → we escape it here).
        // Block-level HTML passes through untouched as GFM Block::Html (see the
        // gfm_html_block test).
        let html = pv("a <b>bold</b> c");
        assert!(!html.contains("<b>"), "{html}");
        assert!(html.contains("&lt;b&gt;"), "{html}");
        assert!(html.contains("bold"), "{html}");
    }

    #[test]
    fn gfm_html_block_passes_through() {
        // GFM html block: raw block-level HTML passes through untouched (the
        // user asked for support of the html block from
        // https://github.github.com/gfm/)
        let html = pv("<div class=\"box\">\n<p>hi</p>\n</div>");
        assert!(html.contains("<div class=\"box\">"), "{html}");
        assert!(html.contains("<p>hi</p>"), "{html}");
        // Other GFM syntax like tables is unaffected
        let html = pv("<table>\n<tr><td>x</td></tr>\n</table>\n\nafter");
        assert!(html.contains("<td>x</td>"), "{html}");
        assert!(html.contains("<p>after</p>"), "{html}");
    }

    #[test]
    fn hr_stays_hr() {
        let html = pv("---\n\ntext");
        assert!(html.contains("<hr>"), "{html}");
        assert!(!html.contains("<table"), "{html}");
    }

    #[test]
    fn blockquote_each_paragraph_own_block() {
        // Established semantics of the leaf block model: each Quote block
        // becomes its own blockquote (flattened, see the module docs'
        // "known boundaries"); no content is lost.
        let html = pv("> a\n>\n> b");
        assert_eq!(html.matches("<blockquote>").count(), 2, "{html}");
        assert!(html.contains("a") && html.contains("b"), "{html}");
    }

    /// Block-level content inside a quote (lists / nested quotes) is wrapped
    /// in a blockquote — a list becomes `<blockquote><ul>`, a nested quote
    /// becomes a nested blockquote.
    #[test]
    fn quoted_list_and_nested_quote_render_blockquote() {
        let html = pv("> - a\n> - b");
        assert_eq!(html.matches("<blockquote>").count(), 1, "{html}");
        assert!(html.contains("<li>") && html.contains("a"), "{html}");
        let html = pv("> > x");
        assert_eq!(html.matches("<blockquote>").count(), 2, "{html}");
        assert!(html.contains("x"), "{html}");
    }

    #[test]
    fn nested_list() {
        let html = pv("- a\n  - b\n- c\n");
        let inner = html
            .find("<ul><li>")
            .map(|_| ())
            .or_else(|| html.match_indices("<ul>").nth(1).map(|_| ()));
        assert!(inner.is_some(), "nested ul missing: {html}");
    }

    // ---- Math ----

    #[test]
    fn math_inline_renders_svg() {
        let html = pv("能量 $E=mc^2$ 守恒");
        assert!(html.contains("<svg"), "{html}");
        assert!(html.contains("能量") && html.contains("守恒"), "{html}");
    }

    #[test]
    fn math_block_multiline() {
        let html = pv("前文\n\n$$\nE=mc^2\n$$\n\n后文");
        assert!(html.contains("<div"), "{html}");
        assert!(html.contains("<svg"), "{html}");
        assert!(html.contains("前文") && html.contains("后文"), "{html}");
    }

    #[test]
    fn math_block_single_line() {
        let html = pv("$$a^2+b^2=c^2$$");
        assert!(html.contains("<div"), "{html}");
        assert!(html.contains("<svg"), "{html}");
    }

    #[test]
    fn math_cjk_flowchart_falls_back_to_literal() {
        // The CJK fallback face is gone with the vendor directory: a CJK
        // formula cannot typeset — the preview must show the LaTeX source as
        // literal text (in a code shell), never swallow it.
        let html = pv(&format!("$$ {} $$", crate::math::BOXED_GATHERED_FLOWCHART));
        assert!(
            html.contains("测量碱性岩的 F、Cl、C、S"),
            "the source must stay visible: {html}"
        );
        assert!(html.contains("<p>$$"), "{html}");
        assert!(!html.contains("<svg"), "{html}");
    }

    #[test]
    fn math_display_para_has_no_p_wrapper() {
        // A pure display-formula segment must not emit <p><div>…</div></p>
        // (illegal HTML nesting; after the parser splits it, an empty <p> is
        // left behind with stray 16px margin)
        let html = pv("$$\nE=mc^2\n$$");
        assert!(!html.contains("<p>") && !html.contains("</p>"), "{html}");
        assert!(html.starts_with("<div"), "{html}");
        // A multi-line block mixed with surrounding paragraphs is equally clean
        let html2 = pv("前文\n\n$$\nx\n$$\n\n后文");
        assert!(html2.contains("<p>前文</p>"), "{html2}");
        assert!(html2.contains("<div"), "{html2}");
        assert!(html2.contains("<p>后文</p>"), "{html2}");
    }

    #[test]
    fn math_display_mixed_with_text_splits_cleanly() {
        // Text mixed with a display formula: segment by the formula, wrap each
        // text run in <p>, the formula becomes a block-level div — the illegal
        // p>div nesting would be split by html5ever, leaving the trailing text
        // outside the paragraph
        let html = pv("前文 $$x$$ 后文");
        assert!(html.contains("<p>前文 </p>"), "{html}");
        assert!(html.contains("<div"), "{html}");
        assert!(html.contains("<p> 后文</p>"), "{html}");
        // A display formula that fails to render: the fallback text is also
        // wrapped in <p>, not left floating
        let bad = pv("前文 $$\\notacommand$$ 后文");
        assert!(bad.contains("<p>前文 </p>"), "{bad}");
        assert!(bad.contains("<p>$$\\notacommand$$</p>"), "{bad}");
        assert!(bad.contains("<p> 后文</p>"), "{bad}");
    }

    #[test]
    fn math_display_in_blockquote_is_valid() {
        // A display formula inside a quote: a div is legal in a blockquote, and
        // the text is wrapped in <p>
        let html = pv("> 引言 $$x$$ 尾注");
        assert!(html.contains("<blockquote>"), "{html}");
        assert!(html.contains("<p>引言 </p>"), "{html}");
        assert!(html.contains("<div"), "{html}");
        assert!(html.contains("<p> 尾注</p>"), "{html}");
    }

    #[test]
    fn math_display_in_heading_stays_phrasing() {
        // h1-h6 only allow phrasing content: a display formula uses a
        // span display:block; h1>div must not be emitted (a content-model
        // violation that would break the heading apart)
        let html = pv("# 公式 $$x$$ 结束");
        assert!(!html.contains("<h1><div"), "{html}");
        assert!(!html.contains("</div></h1>"), "{html}");
        assert!(html.contains("<h1>公式 "), "{html}");
        assert!(
            html.contains("<span style=\"display:block;margin:10px 0;\">"),
            "{html}"
        );
        assert!(html.contains("结束"), "{html}");
    }

    #[test]
    fn heading_formula_is_typeset_at_heading_size() {
        // The SVG carries its size in px (glyphs are paths), so it cannot
        // inherit the heading's `font-size`: before this, an h1's formula was
        // typeset at body size and looked a size or two small next to its own
        // heading text.
        let height = |html: &str| -> f32 {
            let i = html.find("height=\"").expect("svg height attribute") + 8;
            let j = html[i..].find('"').expect("closing quote") + i;
            html[i..j].parse().expect("numeric height")
        };
        let body = height(&pv("$x$"));
        let h1 = height(&pv("# $x$"));
        let h2 = height(&pv("## $x$"));
        assert!(h1 > h2, "h1 ({h1}) taller than h2 ({h2})");
        assert!(h2 > body, "h2 ({h2}) taller than body ({body})");
    }

    /// The stylesheet's heading sizes and the formula typesetting scale must
    /// come from one table ([`HEAD_EM`]) — two hand-kept lists is exactly how
    /// they drifted apart in the first place.
    #[test]
    fn github_css_heading_sizes_come_from_head_em() {
        let css = github_css(math::DEFAULT_FG);
        for (i, em) in HEAD_EM.iter().enumerate() {
            let rule = format!("h{} {{ font-size:{em}em;", i + 1);
            assert!(css.contains(&rule), "missing {rule:?} in {css}");
        }
    }

    #[test]
    fn math_display_in_strong_stays_phrasing() {
        // Same for strong/em/del: strong>div is illegal
        for (src, tag) in [
            ("**$$x$$**", "strong"),
            ("*$$x$$*", "em"),
            ("~~$$x$$~~", "del"),
        ] {
            let html = pv(src);
            assert!(!html.contains(&format!("<{tag}><div")), "{src}: {html}");
            assert!(html.contains(&format!("<{tag}><span")), "{src}: {html}");
            assert!(html.contains("<svg"), "{src}: {html}");
        }
    }

    #[test]
    fn math_not_triggered_in_code_span() {
        let html = pv("use `$x$` here");
        assert!(!html.contains("<svg"), "{html}");
        assert!(html.contains("<code>$x$</code>"), "{html}");
    }

    #[test]
    fn math_not_triggered_in_code_fence() {
        let html = pv("```\ninline $x$ math\n```\n");
        assert!(!html.contains("<svg"), "{html}");
        assert!(html.contains("$x$"), "{html}");
    }

    #[test]
    fn currency_not_math() {
        let src = "价格 $5 和 $10 之间，工资 $3,000";
        let html = pv(src);
        assert!(!html.contains("<svg"), "{html}");
        assert!(html.contains("$5"), "{html}");
    }

    #[test]
    fn math_parse_error_falls_back_to_text() {
        let src = "坏公式 $\\notacommand$ 在这里";
        let html = pv(src);
        assert!(!html.contains("<svg"), "{html}");
        assert!(html.contains("坏公式"), "{html}");
        assert!(html.contains("\\notacommand"), "{html}");
    }

    #[test]
    fn math_with_ampersand_align() {
        // The & of an alignment environment must not be broken by HTML escaping
        // (the formula source goes straight through latex)
        let src = "$$\n\\begin{aligned}a&=b\\\\c&=d\\end{aligned}\n$$";
        let html = pv(src);
        assert!(html.contains("<svg"), "{html}");
        assert!(!html.contains("&amp;"), "{html}");
    }

    #[test]
    fn math_in_bold() {
        let html = pv("**$x^2$ 加粗**");
        assert!(html.contains("<strong>"), "{html}");
        assert!(html.contains("<svg"), "{html}");
    }

    #[test]
    fn math_injection_is_inert() {
        let src = "$<script>alert(1)</script>$";
        let html = pv(src);
        assert!(!html.contains("<script>"), "{html}");
    }

    #[test]
    fn literal_fffc_is_not_a_placeholder() {
        // There is no placeholder mechanism on the tree (the SVG is inserted
        // directly); a literal U+FFFC is just ordinary text
        let html = pv("占位 \u{FFFC} 文本 $x^2$ 结束");
        assert!(html.contains("<svg"), "{html}");
        assert!(html.contains("\u{FFFC}"), "{html}");
    }

    #[test]
    fn escaped_dollar_is_not_math() {
        let html = pv("成本 \\$5 不是公式");
        assert!(!html.contains("<svg"), "{html}");
        assert!(html.contains("$5"), "{html}");
    }

    #[test]
    fn math_render_is_deterministic_across_calls() {
        // Formulas go through the process-level cache (MathSpec::cached): the
        // same parameters rendered repeatedly are byte-identical, and a
        // different theme only changes the fill, not the structure.
        let doc = md::parse("$E=mc^2$");
        let a = preview_html(&doc, "#333333", None);
        let b = preview_html(&doc, "#333333", None);
        assert_eq!(a, b);
        let dark = preview_html(&doc, "#c9d1d9", None);
        assert!(a.contains("333333") && dark.contains("c9d1d9"), "{a}");
    }

    // ---- GFM tables ----

    #[test]
    fn gfm_table_basic() {
        let html = pv("| a | b |\n| --- | ---: |\n| 1 | 2 |\n| 3 | 4 |\n\nafter");
        assert!(html.contains("<table class=\"md-tbl\">"), "{html}");
        assert!(
            html.contains("<th style=\"text-align:left\">a</th>"),
            "{html}"
        );
        assert!(
            html.contains("<th style=\"text-align:right\">b</th>"),
            "{html}"
        );
        assert!(
            html.contains("<td style=\"text-align:right\">2</td>"),
            "{html}"
        );
        assert!(html.contains("<p>after</p>"), "{html}");
    }

    #[test]
    fn gfm_table_interrupts_paragraph() {
        let html = pv("前言\n| a | b |\n| --- | --- |\n| 1 | 2 |");
        assert!(html.contains("<p>前言</p>"), "{html}");
        assert!(
            html.contains("<th style=\"text-align:left\">a</th>"),
            "{html}"
        );
    }

    #[test]
    fn gfm_table_inline_marks_and_escaped_pipe() {
        let html = pv("| 名 | 值 |\n| --- | --- |\n| **加** | x\\|y |");
        assert!(html.contains("<strong>加</strong>"), "{html}");
        assert!(html.contains(">x|y</td>"), "{html}");
    }

    #[test]
    fn gfm_table_extra_cells_dropped_and_missing_filled() {
        // pulldown semantics: extra columns are dropped, missing ones are padded
        // empty (row 2 is an even data row → it gets the zebra striping)
        let html = pv("| a | b |\n| --- | --- |\n| 1 | 2 | 3 |\n| 1 |");
        assert!(!html.contains(">3<"), "{html}");
        assert!(
            html.contains("<td style=\"text-align:left;background:#f6f8fa\"></td>"),
            "{html}"
        );
    }

    // ---- GFM emphasis / autolinks / reference links / task lists ----

    #[test]
    fn gfm_strikethrough() {
        let html = pv("~~划掉~~ 与 **加粗~~不删**");
        assert!(html.contains("<del>划掉</del>"), "{html}");
        assert!(html.contains("加粗~~不删"), "{html}");
    }

    #[test]
    fn gfm_bare_url_autolink() {
        let html = pv("看 https://example.com/a?b=1! 和 www.example.com/x。");
        assert!(
            html.contains("<a href=\"https://example.com/a?b=1\">https://example.com/a?b=1</a>"),
            "{html}"
        );
        assert!(
            html.contains("<a href=\"http://www.example.com/x\">www.example.com/x</a>"),
            "{html}"
        );
    }

    #[test]
    fn gfm_explicit_autolink() {
        let html = pv("<https://example.com/x>");
        assert!(
            html.contains("<a href=\"https://example.com/x\">https://example.com/x</a>"),
            "{html}"
        );
    }

    #[test]
    fn autolink_skips_code_and_existing_links() {
        let html = pv("`https://in.code` 与 [t](https://real.io) 与 https://out.io");
        assert!(html.contains("<code>https://in.code</code>"), "{html}");
        assert!(html.contains("<a href=\"https://real.io\">t</a>"), "{html}");
        assert!(
            html.contains("<a href=\"https://out.io\">https://out.io</a>"),
            "{html}"
        );
    }

    #[test]
    fn gfm_reference_links() {
        let src = "见 [文档][docs] 与 [官网][] 与 [快捷]。\n\n[docs]: https://docs.example.com\n[官网]: <https://example.com> \"标题\"\n[快捷]: https://q.io";
        let html = pv(src);
        assert!(
            html.contains("<a href=\"https://docs.example.com\">文档</a>"),
            "{html}"
        );
        assert!(
            html.contains("<a href=\"https://example.com\">官网</a>"),
            "{html}"
        );
        assert!(html.contains("<a href=\"https://q.io\">快捷</a>"), "{html}");
        // Definition lines do not enter the body
        assert!(!html.contains("docs.example.com</a> <"), "{html}");
    }

    #[test]
    fn gfm_task_list_still_works() {
        let html = pv("- [ ] 未完成\n- [x] 完成");
        assert!(html.contains("\u{2610}"), "{html}");
        assert!(html.contains("\u{2611}"), "{html}");
        assert!(html.contains("未完成") && html.contains("完成"), "{html}");
    }

    // ---- Link / image safety ----

    #[test]
    fn image_network_url() {
        let html = pv("![logo](https://x.io/a.png)");
        assert!(
            html.contains("<img class=\"md-img\" src=\"https://x.io/a.png\" alt=\"logo\">"),
            "{html}"
        );
    }

    #[test]
    fn image_relative_resolves_to_file_url() {
        // A `file://` URL is only produced from an **absolute** path, and what
        // counts as absolute differs by platform (`C:/docs` is relative on
        // POSIX). Use a native base so this pins the resolver, not Windows.
        #[cfg(windows)]
        let (base, prefix) = (Path::new("C:/docs"), "file:///C:/docs/img/");
        #[cfg(not(windows))]
        let (base, prefix) = (Path::new("/docs"), "file:///docs/img/");
        // CommonMark: a URL containing spaces must be wrapped in angle brackets
        let html = pv_base("![图](<img/pic a.png>)", base);
        assert!(html.contains(prefix), "{html}");
        assert!(html.contains("alt=\"图\""), "{html}");
        // The space is percent-encoded; the attribute value is safe
        assert!(html.contains("pic%20a.png"), "{html}");
    }

    #[test]
    fn image_without_base_falls_back_to_alt() {
        let html = pv("![alt text](local.png)");
        assert!(!html.contains("<img"), "{html}");
        assert!(html.contains("[alt text]"), "{html}");
    }

    #[test]
    fn image_dangerous_schemes_rejected() {
        for src in [
            "javascript:alert(1)",
            "data:text/html;base64,xxxx",
            "vbscript:x",
        ] {
            let html = pv(&format!("![x]({src})"));
            assert!(!html.contains("<img"), "{src}: {html}");
            assert!(!html.contains(src), "{src}: {html}");
        }
    }

    #[test]
    fn link_script_scheme_is_inert() {
        // A link href with a script scheme is likewise rejected (the text is
        // kept, no <a> is generated)
        let html = pv("[click](javascript:alert(1))");
        assert!(!html.contains("<a href=\"javascript"), "{html}");
        assert!(html.contains("click"), "{html}");
    }

    // ---- URL whitelist unit tests ----

    #[test]
    fn safe_image_url_absolute_and_file_scheme() {
        // What is absolute depends on the platform; `C:\…` is a plain relative
        // file name on POSIX (and must still not be mistaken for a `c:` URL
        // scheme — the `win_drive` check is platform-neutral).
        #[cfg(windows)]
        let (abs, base) = ("C:\\pics\\a.png", Path::new("C:/docs"));
        #[cfg(not(windows))]
        let (abs, base) = ("/pics/a.png", Path::new("/docs"));
        assert!(safe_image_url(abs, None).is_some());
        assert!(safe_image_url("C:\\pics\\a.png", Some(base)).is_some());
        assert!(safe_image_url("file:///C:/docs/a.png", None).is_some());
        assert!(safe_image_url("relative.png", None).is_none());
        assert!(safe_image_url("relative.png", Some(base)).is_some());
        assert!(safe_image_url("javascript:alert(1)", None).is_none());
        assert!(safe_image_url("", None).is_none());
        assert!(safe_image_url("https://x.io/a.png", None).is_some());
    }

    #[test]
    fn safe_link_url_allows_mailto_and_relative() {
        assert!(safe_link_url("mailto:a@b.c").is_some());
        assert!(safe_link_url("relative/page.html").is_some());
        assert!(safe_link_url("C:\\docs\\page.html").is_some());
        assert!(safe_link_url("ftp://x").is_none());
        assert!(safe_link_url("data:text/html,x").is_none());
    }

    #[test]
    fn autolink_word_boundary() {
        // A scan does not start when preceded by an alphanumeric (e.g.
        // anotherhttps://x must not be split into a link)
        let html = pv("xhttps://example.com");
        assert!(!html.contains("<a"), "{html}");
    }

    #[test]
    fn autolink_not_nested_in_link_text() {
        // A bare URL inside link text is not split into an <a> (a nested a is
        // illegal; html5ever would break the link apart)
        let html = pv("[see https://a.io](https://b.io)");
        assert_eq!(html.matches("<a ").count(), 1, "{html}");
        assert!(
            html.contains("<a href=\"https://b.io\">see https://a.io</a>"),
            "{html}"
        );
        // A link whose scheme is rejected likewise does not split (the guard is
        // in link_depth, not in URL validation)
        let bad = pv("[x javascript:alert(1)](javascript:alert(1))");
        assert!(!bad.contains("<a"), "{bad}");
        assert!(bad.contains("javascript:alert(1)"), "{bad}");
    }

    // ---- GitHub theme ----

    #[test]
    fn github_css_light_theme_tokens() {
        // A light-theme foreground (#333333, dark) → GitHub light baseline
        let css = github_css("#333333");
        assert!(css.contains("#0969da"), "link: {css}");
        assert!(css.contains("#f6f8fa"), "code bg: {css}");
        assert!(css.contains("#d1d9e0"), "border: {css}");
        // The h1/h2 bottom border is GitHub's signature shape
        assert!(
            css.matches("border-bottom:1px solid #d1d9e0").count() == 2,
            "{css}"
        );
    }

    #[test]
    fn github_css_dark_theme_tokens() {
        // A dark-theme foreground (#c9d1d9, bright) → GitHub dark baseline
        let css = github_css("#c9d1d9");
        assert!(css.contains("#4493f8"), "link: {css}");
        assert!(css.contains("#151b23"), "code bg: {css}");
        assert!(css.contains("#3d444d"), "border: {css}");
    }

    #[test]
    fn github_css_rejects_garbage_fg_as_light() {
        // A parse failure is treated as dark → light baseline (prioritizing
        // light body-text readability)
        let css = github_css("#gggggg");
        assert!(css.contains("#0969da"), "{css}");
    }

    #[test]
    fn table_zebra_stripes_on_even_data_rows() {
        let html = pv("| a | b |\n| --- | --- |\n| 1 | 2 |\n| 3 | 4 |");
        // Row 1 (the first data row) has no zebra
        assert!(
            html.contains("<td style=\"text-align:left\">1</td>"),
            "{html}"
        );
        // Row 2 (tbody's 2nd row) zebra = GitHub code_bg
        assert!(
            html.contains("<td style=\"text-align:left;background:#f6f8fa\">3</td>"),
            "{html}"
        );
        // The header row has no zebra
        assert!(
            !html.contains("<th style=\"text-align:left;background"),
            "{html}"
        );
    }

    #[test]
    fn hex_luma_boundaries() {
        assert!(hex_luma("#000000") < 0.5);
        assert!(hex_luma("#ffffff") > 0.5);
        assert!(
            hex_luma("#c9d1d9") > 0.5,
            "a dark-theme foreground should be bright"
        );
        assert!(
            hex_luma("#333333") < 0.5,
            "a light-theme foreground should be dark"
        );
        assert_eq!(hex_luma("#zzz"), 0.0);
        assert_eq!(hex_luma(""), 0.0);
    }
}
