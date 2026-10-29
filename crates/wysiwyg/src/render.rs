//! Doc → content-area HTML string (injected via `dangerous_inner_html`, the
//! proven pattern from infeditor's `mdpreview`/`mdmap`; needs dioxus-native's
//! `html` feature).
//!
//! Hard DOM conventions (`geom` depends on them — read before touching):
//! 1. **One block = one `display:block` element carrying `data-bid="N"`**, whose
//!    direct content is inline HTML — so that element is itself the inline
//!    root: the inline root returned by `find_text_position` is it, and
//!    `attr("data-bid")` directly yields the block number.
//!    (A Td block's carrier is `<td data-bid>`; a table cell's inline root =
//!    the td.)
//! 2. List markers and task checkboxes are hung on the li's left side with
//!    `position:absolute` and **never enter the text flow of a data-bid
//!    element**, otherwise `Pos.off` would be polluted. A checkbox is a span
//!    with `data-task` (checked state) + `data-task-bid` (block number)
//!    containing one text character (geom::task_at hit-tests it back through
//!    the inline root).
//! 3. `Pos.off` is a plain-text byte offset within the block; inline HTML must
//!    be in the same order and quantity as `plain_text`. **Do not** insert
//!    extra visible text nodes (an empty block's placeholder uses CSS
//!    min-height, not a ZWSP). Exception: a Math's HTML is an SVG (no text
//!    node), its source exists only in plain_text — the caret inside a math
//!    range is snapped to the nearest position by parley, which is acceptable.
//! 4. A nested list = adjacent Li layered by `level`, rendered as nested
//!    `<ul>`/`<ol>`; orderedness may differ per level (the stack opens/closes
//!    level by level).
//!
//! Styling is mostly inline `style` (the established conclusion given Blitz's
//! limited CSS support); the global table [`SHELL_CSS`] only carries the
//! container and caret-overlay rules.

use std::collections::HashSet;
use std::path::Path;
use std::sync::{OnceLock, RwLock};

use crate::model::{Block, Doc, Inline};

/// Image URLs known to have failed loading (synced by the host from blitz's
/// `failed_images`, see `mdws::overlays_frame`). The render layer falls back to
/// the alt placeholder accordingly — blitz has no image-load event, so the
/// host can only "emit the `<img>` first, then re-render as a placeholder
/// after a failure".
static FAILED_IMAGES: OnceLock<RwLock<HashSet<String>>> = OnceLock::new();

fn failed_images() -> &'static RwLock<HashSet<String>> {
    FAILED_IMAGES.get_or_init(|| RwLock::new(HashSet::new()))
}

/// Called during the host's event phase: replace the failed set wholesale (the
/// set only grows and stays small; a wholesale replace avoids locking per
/// entry). The host also bumps a signal to trigger a re-render.
pub fn set_failed_images(urls: Vec<String>) {
    let next: HashSet<String> = urls.into_iter().collect();
    if let Ok(mut w) = failed_images().write() {
        *w = next;
    }
}

/// Whether this URL's image is known to have failed loading.
pub fn is_image_failed(url: &str) -> bool {
    failed_images()
        .read()
        .map(|s| s.contains(url))
        .unwrap_or(false)
}

/// Default foreground (light theme). The SVG fill is baked in and must be
/// passed explicitly — see math.rs constraint 1.
pub const DEFAULT_FG: &str = "#333333";

/// HTML inside the content-area root container (`.wys-content`) (default
/// foreground, images without a base).
pub fn content_html(doc: &Doc) -> String {
    content_html_with(doc, DEFAULT_FG, None)
}

/// Content-area root container HTML. `fg`: theme foreground (`#rrggbb`) for
/// the formula SVG fill; `img_base`: the document's directory — relative-path
/// images are resolved against it into file:// URLs, using the same
/// `safe_image_url` whitelist as the preview; a failed resolution / no base /
/// a rejection falls back to the alt placeholder.
pub fn content_html_with(doc: &Doc, fg: &str, img_base: Option<&Path>) -> String {
    region_fragments_sized(doc, fg, img_base, 16.0).concat()
}

/// Same as [`content_html_with`], but formulas are typeset at the current
/// body font size (px).
pub fn content_html_sized(doc: &Doc, fg: &str, img_base: Option<&Path>, math_px: f32) -> String {
    region_fragments_sized(doc, fg, img_base, math_px).concat()
}

/// HTML fragments **split by region**: a single-block region
/// (Para/Head/Code/Quote/Hr) gets one fragment each, consecutive Li one
/// fragment, consecutive same-seq Td one fragment. Fragments concatenated in
/// order are byte-identical to [`content_html_with`] (a test reconciles this).
///
/// Purpose: the component renders one `display:contents`-wrapped
/// `dangerous_inner_html` node per fragment — with dioxus positional
/// diffing, an unchanged fragment costs zero re-parse / zero re-layout, so a
/// keystroke in a large document only re-lays-out the current block
/// (eliminating the O(whole) HTML re-parse).
///
/// Images without a base (the compatibility entry of
/// [`region_fragments_with`]; the host component needs no change).
pub fn region_fragments(doc: &Doc, fg: &str) -> Vec<String> {
    region_fragments_with(doc, fg, None)
}

/// The base-taking version of [`region_fragments`] (for resolving
/// relative-path images; see [`content_html_with`]).
pub fn region_fragments_with(doc: &Doc, fg: &str, img_base: Option<&Path>) -> Vec<String> {
    region_fragments_sized(doc, fg, img_base, 16.0)
}

/// Same as [`region_fragments_with`], but formulas render at the body font
/// size `math_px` (the host passes the user's font size in; otherwise the
/// default 16 would be inconsistent with 14px body text).
pub fn region_fragments_sized(
    doc: &Doc,
    fg: &str,
    img_base: Option<&Path>,
    math_px: f32,
) -> Vec<String> {
    let mut frags: Vec<String> = Vec::new();
    let mut i = 0;
    while i < doc.blocks.len() {
        // Grouping does not cross quote-depth boundaries; a Quoted group is
        // wrapped in a quote-styled container
        let depth = doc.blocks[i].quote_depth();
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
                let mut s = String::new();
                emit_list_region(&doc.blocks[start..i], start, &mut s, fg, img_base, math_px);
                frags.push(quote_wrap(&doc.blocks[start], s));
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
                let mut s = String::new();
                emit_table(&doc.blocks[start..i], start, &mut s, fg, img_base, math_px);
                frags.push(quote_wrap(&doc.blocks[start], s));
            }
            b => {
                let mut s = String::new();
                push_block(&mut s, i, b, fg, img_base, math_px);
                frags.push(quote_wrap(&doc.blocks[i], s));
                i += 1;
            }
        }
    }
    frags
}

/// Wrap a block/region inside a quote with a quote-styled container (left
/// border + indent, indent stacking with depth). The [`Block::Quote`]
/// compatible shape already carries QUOTE_STYLE and is not wrapped again
/// (avoiding a double border). The container has **no text node** — geom's
/// text stream is byte-identical to `plain_text` (convention 3).
fn quote_wrap(b: &Block, s: String) -> String {
    match b {
        Block::Quoted { depth, .. } => format!(
            "<div style=\"{QUOTE_NEST_STYLE}padding-left:{}em;\">{s}</div>",
            usize::from(*depth)
        ),
        _ => s,
    }
}

/// A list region: a level stack opens/closes nested `<ul>`/`<ol>`.
/// Orderedness may differ per level: when it changes at the same level, close
/// first then open (keeping the DOM legal).
/// `base`: the first block's bid in the region (data-bid = base + ordinal).
fn emit_list_region(
    items: &[Block],
    base: usize,
    s: &mut String,
    fg: &str,
    img_base: Option<&Path>,
    math_px: f32,
) {
    // Stack element: (level, ordered).
    let mut stack: Vec<(u8, bool)> = Vec::new();
    for (k, b) in items.iter().enumerate() {
        let Block::Li { ordered, level, .. } = b.core() else {
            continue;
        };
        // Close: the stack top is deeper than the current level, or the same
        // level but with different orderedness
        while let Some(&(tl, to)) = stack.last() {
            if tl > *level || (tl == *level && to != *ordered) {
                let tag = if to { "ol" } else { "ul" };
                s.push_str(&format!("</{tag}>"));
                stack.pop();
            } else {
                break;
            }
        }
        // Open: the stack is empty, or the stack top is shallower than the
        // current level
        let need_open = match stack.last() {
            None => true,
            Some(&(tl, _)) => tl < *level,
        };
        if need_open {
            // Open a new level: indent = depth × 2em (the indent of an inner
            // list hanging inside its own li is dug out by the outer ul's
            // padding-left; here a uniform margin-left prevents stacking).
            let indent = *level as usize * 2;
            let tag = if *ordered { "ol" } else { "ul" };
            s.push_str(&format!(
                "<{tag} style=\"{}margin-left:{indent}em;\">",
                LIST_STYLE
            ));
            stack.push((*level, *ordered));
        }
        let Block::Li {
            num, task, inlines, ..
        } = b.core()
        else {
            // Not a list item after all (the grouping invariant broke): render it as
            // a plain paragraph so the text stays visible instead of panicking.
            push_block(s, base + k, b, fg, img_base, math_px);
            continue;
        };
        push_li(
            s,
            base + k,
            *ordered,
            *num,
            *level,
            *task,
            inlines,
            fg,
            img_base,
            math_px,
        );
    }
    while let Some((_, to)) = stack.pop() {
        let tag = if to { "ol" } else { "ul" };
        s.push_str(&format!("</{tag}>"));
    }
}

fn push_block(
    s: &mut String,
    idx: usize,
    b: &Block,
    fg: &str,
    img_base: Option<&Path>,
    math_px: f32,
) {
    match b {
        Block::Para(inlines) => {
            // A paragraph that is exactly one display formula is a block of its own
            // (the preview segments display math the same way). Wrapping it in the
            // paragraph's `margin-bottom:16px; min-height:24px` made the spacing
            // around a formula differ from the preview.
            if let [Inline::Math { src, display: true }] = inlines.as_slice() {
                s.push_str(&format!(
                    "<div data-bid=\"{idx}\" style=\"margin:10px 0;\">"
                ));
                push_math(s, src, true, fg, math_px);
                s.push_str("</div>");
                return;
            }
            s.push_str(&format!("<div data-bid=\"{idx}\" style=\"{PARA_STYLE}\">"));
            push_inlines(s, inlines, fg, img_base, math_px);
            // Empty block: no text node added (convention 3), the height is
            // held up by min-height.
            s.push_str("</div>");
        }
        Block::Head { level, inlines } => {
            s.push_str(&format!(
                "<div data-bid=\"{idx}\" style=\"{}\">",
                head_style(*level)
            ));
            // A formula SVG carries its size in *pixels* (the glyphs are paths,
            // see math.rs), so it does not follow the heading's `font-size` the
            // way a text node does. Typeset it at the heading's own scale
            // instead — otherwise an h1 (1.75em) showed a body-sized formula,
            // visibly a size or two small next to its own heading text.
            let px = math_px * head_em(*level);
            push_inlines(s, inlines, fg, img_base, px);
            s.push_str("</div>");
        }
        Block::Code { lang, text } => {
            s.push_str(&format!(
                "<div data-bid=\"{idx}\" class=\"lang-{}\" style=\"{CODE_STYLE}\">",
                escape_attr(lang.as_deref().unwrap_or("text"))
            ));
            // Coloring hook (registered by the host, see `hl`); when not
            // registered, escape as plain text.
            match lang
                .as_deref()
                .and_then(|l| crate::hl::highlight(l, text, fg))
            {
                Some(body) => s.push_str(&body),
                None => push_code_text(s, text),
            }
            s.push_str("</div>");
        }
        Block::Quote(inlines) => {
            s.push_str(&format!("<div data-bid=\"{idx}\" style=\"{QUOTE_STYLE}\">"));
            push_inlines(s, inlines, fg, img_base, math_px);
            s.push_str("</div>");
        }
        Block::Html { text } => {
            s.push_str(&format!(
                "<div data-bid=\"{idx}\" style=\"{HTML_BLOCK_STYLE}\">"
            ));
            // Shown as source: escaped output (this is only the edit view; the
            // preview passes it through)
            push_code_text(s, text);
            s.push_str("</div>");
        }
        Block::Hr => {
            s.push_str(&format!(
                "<div data-bid=\"{idx}\" style=\"{HR_STYLE}\"></div>"
            ));
        }
        Block::Li { inlines, .. } | Block::Td { inlines, .. } => {
            // Grouped blocks normally go through push_list/push_table. If one leaks
            // into the ungrouped path, render its content as a paragraph instead of
            // killing the render.
            s.push_str(&format!("<div data-bid=\"{idx}\" style=\"{PARA_STYLE}\">"));
            push_inlines(s, inlines, fg, img_base, math_px);
            s.push_str("</div>");
        }
        // Quote-ownership wrapper: emit the inner first (the quote-styled
        // container is on the grouping path)
        Block::Quoted { inner, .. } => push_block(s, idx, inner, fg, img_base, math_px),
    }
}

/// One list item. The marker and checkbox are absolutely positioned and hung
/// (convention 2), and data-bid lands on a display:block span (convention 1:
/// it is itself the inline root).
///
/// An unordered marker uses an **ASCII hyphen**: `•` (U+2022) is rendered by
/// blitz's font fallback as a weird "θ"-like glyph (observed); any display
/// glyph beyond ASCII is looked up in the font cmap first.
#[allow(clippy::too_many_arguments)]
fn push_li(
    s: &mut String,
    idx: usize,
    ordered: bool,
    num: u64,
    level: u8,
    task: Option<bool>,
    inlines: &[Inline],
    fg: &str,
    img_base: Option<&Path>,
    math_px: f32,
) {
    // A task item draws no bullet: a task list has only a checkbox, no list
    // symbol (the hyphen in "- ☐ task" is redundant visual noise). An ordered
    // task item keeps its number.
    let marker = if ordered {
        format!("{num}. ")
    } else if task.is_some() {
        String::new()
    } else {
        "- ".to_string()
    };
    let _ = level;
    s.push_str(&format!(
        "<li style=\"{LI_STYLE}\">\
         <span data-marker style=\"{MARKER_STYLE}\">{marker}</span>"
    ));
    if let Some(checked) = task {
        // Checkbox: carries one text character (X when checked / NBSP when
        // unchecked — an empty span collapses to zero size), and geom::task_at
        // hit-tests data-task-bid by geometric rectangle.
        // **No tabindex**: in host mode patch six pins the DOM focus to this
        // span, and the composer's onblur→autofocus is then overridden by the
        // click's default action, so after clicking the box the whole keyboard
        // goes dead (observed: after checking, Ctrl+S/typing do nothing). Without
        // it, a click goes through clear_focus → the host's autofocus reclaims
        // focus; in demo mode the mousedown branch's focus_root() re-focuses the
        // root.
        let glyph = if checked { "X" } else { "\u{a0}" };
        let box_style = if checked {
            TASK_BOX_ON_STYLE
        } else {
            TASK_BOX_OFF_STYLE
        };
        s.push_str(&format!(
            "<span data-task=\"{}\" data-task-bid=\"{}\" style=\"{box_style}\">{glyph}</span>",
            if checked { "1" } else { "0" },
            idx
        ));
    }
    s.push_str(&format!(
        "<span data-bid=\"{idx}\" style=\"{}\">",
        if task.is_some() {
            TASK_TEXT_STYLE
        } else {
            LI_TEXT_STYLE
        }
    ));
    push_inlines(s, inlines, fg, img_base, math_px);
    s.push_str("</span></li>");
}

fn emit_table(
    items: &[Block],
    base: usize,
    s: &mut String,
    fg: &str,
    img_base: Option<&Path>,
    math_px: f32,
) {
    // Td's row values are **not guaranteed monotonic** (cross-table paste/
    // editing can disorder them) — the old implementation assumed they were
    // non-decreasing, and when disordered the <tr> open/close went wrong (a
    // header row opened inside tbody, row count doubled). Now group by row
    // value and emit in ascending order: row 0 is the header (thead/th), the
    // rest tbody/td; within a group the appearance order (column order) is
    // kept, and data-bid = base + the original ordinal (the block number does
    // not change with emission order, so geom's reverse lookup is unaffected).
    let mut rows: Vec<(usize, Vec<(usize, &Block)>)> = Vec::new();
    for (k, b) in items.iter().enumerate() {
        let Block::Td { row, .. } = b.core() else {
            continue;
        };
        match rows.iter_mut().find(|(r, _)| *r == *row) {
            Some((_, cells)) => cells.push((base + k, b.core())),
            None => rows.push((*row, vec![(base + k, b.core())])),
        }
    }
    rows.sort_by_key(|(r, _)| *r);
    s.push_str(&format!("<table style=\"{TABLE_STYLE}\">"));
    let mut thead_open = false;
    let mut tbody_open = false;
    for (row, cells) in &rows {
        if *row == 0 {
            s.push_str("<thead><tr>");
            thead_open = true;
        } else {
            if thead_open {
                s.push_str("</thead>");
                thead_open = false;
            }
            if !tbody_open {
                s.push_str("<tbody>");
                tbody_open = true;
            }
            s.push_str("<tr>");
        }
        for (bid, b) in cells {
            let tag = if *row == 0 { "th" } else { "td" };
            match b {
                Block::Td { align, inlines, .. } => {
                    let style = td_style(*align);
                    s.push_str(&format!("<{tag} data-bid=\"{bid}\" style=\"{style}\">"));
                    push_inlines(s, inlines, fg, img_base, math_px);
                    s.push_str(&format!("</{tag}>"));
                }
                // The row map should only ever hold cells. A malformed tree must not
                // take down the whole frame: render the block's text as a plain cell.
                other => {
                    let style = td_style(0);
                    s.push_str(&format!("<{tag} data-bid=\"{bid}\" style=\"{style}\">"));
                    push_code_text(s, &other.plain_text());
                    s.push_str(&format!("</{tag}>"));
                }
            }
        }
        s.push_str("</tr>");
    }
    if thead_open {
        s.push_str("</thead>");
    }
    if tbody_open {
        s.push_str("</tbody>");
    }
    s.push_str("</table>");
}

fn td_style(align: u8) -> String {
    let a = match align {
        1 => "left",
        2 => "center",
        3 => "right",
        _ => "left",
    };
    format!(
        "border:1px solid rgba(128,128,128,.35);padding:6px 13px;text-align:{a};\
         line-height:1.5;"
    )
}

fn push_inlines(
    s: &mut String,
    inlines: &[Inline],
    fg: &str,
    img_base: Option<&Path>,
    math_px: f32,
) {
    for i in inlines {
        match i {
            Inline::Text(t) => push_text_html(s, t),
            Inline::Code(c) => {
                s.push_str("<code style=\"");
                s.push_str(CODE_INLINE_STYLE);
                s.push_str("\">");
                push_text_html(s, c);
                s.push_str("</code>");
            }
            Inline::Strong(c) => {
                s.push_str("<strong>");
                push_inlines(s, c, fg, img_base, math_px);
                s.push_str("</strong>");
            }
            Inline::Em(c) => {
                s.push_str("<em>");
                push_inlines(s, c, fg, img_base, math_px);
                s.push_str("</em>");
            }
            Inline::Del(c) => {
                s.push_str("<del>");
                push_inlines(s, c, fg, img_base, math_px);
                s.push_str("</del>");
            }
            Inline::Link { url, inlines: c } => {
                s.push_str(&format!(
                    "<a data-link=\"{}\" style=\"{LINK_STYLE}\">",
                    escape_attr(url)
                ));
                push_inlines(s, c, fg, img_base, math_px);
                s.push_str("</a>");
            }
            Inline::Image { url, alt } => {
                // Image: alt participates in plain (the caret can land on both
                // sides), but <img> has no text node — data-vlen bookkeeping
                // (geom's virtual-byte mapping depends on it, see geom.rs).
                // src is mandatory — blitz kicks off resource loading via src
                // (data-img is only a marker).
                //
                // The same URL whitelist as the preview:
                // `safe_image_url(url, img_base)` — relative paths are resolved
                // against img_base into file:// URLs; with no base / rejected
                // (javascript:/data: etc.) it falls back to the alt placeholder.
                // The placeholder span is a **real text node** (the alt text) and
                // must **not carry data-vlen** — geom treats a data-vlen element
                // as an atom and does not recurse, while parley counts the alt
                // text in the layout bytes, so the two ledgers would be
                // off by alt.len() after the alt.
                match crate::preview::safe_image_url(url, img_base).filter(|u| {
                    crate::preview::image_file_available(u)
                        && !is_image_failed(u)
                        && !is_image_failed(url)
                }) {
                    Some(u) => {
                        s.push_str(&format!(
                            "<img src=\"{}\" data-img=\"{}\" data-vlen=\"{}\" alt=\"{}\" style=\"{IMG_STYLE}\">",
                            escape_attr(&u),
                            escape_attr(url),
                            alt.len(),
                            escape_attr(alt)
                        ));
                    }
                    None => {
                        s.push_str(&format!(
                            "<span data-img=\"{}\" alt=\"{}\" style=\"{IMG_STYLE}\">",
                            escape_attr(url),
                            escape_attr(alt)
                        ));
                        push_text_html(s, alt);
                        s.push_str("</span>");
                    }
                }
            }
            Inline::Math { src, display } => push_math(s, src, *display, fg, math_px),
            // In-block line break. `\n` must stay inside a **text node** (same
            // convention as a Code block: layout bytes are identical to plain
            // bytes, zero disturbance to geom's byte ledger); the outer
            // white-space:pre-wrap makes parley keep the `\n` and break lines.
            // Soft/hard breaks look the same in the view (the difference is only
            // in emit's md form). **Must not use `<br>`**: blitz expands br into
            // a parley text `\n` but the DOM has no text node, so the two
            // ledgers drift by 1 byte after every break.
            Inline::SoftBreak | Inline::HardBreak => {
                s.push_str("<span style=\"white-space:pre-wrap\">");
                s.push('\n');
                s.push_str("</span>");
            }
        }
    }
}

/// The **edit-view** HTML of one formula: the SVG is wrapped in a carrier
/// carrying `data-vlen` (geom's virtual-byte ledger uses it to skip the `<svg>`
/// as an atom). `px` is the body font size — the block-level scaling is
/// handled in one place by [`MathSpec`], not pre-multiplied here.
///
/// A failure falls back to `<code>`: the content stays visible, and the
/// formula source is never swallowed.
fn push_math(s: &mut String, src: &str, display: bool, fg: &str, px: f32) {
    let spec = if display {
        crate::math::MathSpec::block(src, fg, px)
    } else {
        crate::math::MathSpec::inline(src, fg, px)
    };
    match spec.cached() {
        Some(svg) => {
            let style = if display {
                MATH_DISPLAY_STYLE
            } else {
                MATH_STYLE
            };
            s.push_str(&format!(
                "<span data-math data-vlen=\"{}\" style=\"{style}\">{svg}</span>",
                src.len()
            ));
        }
        None => {
            // `data-math` is kept even on the fallback: geom pairs the DOM's
            // formula nodes with `Block::math_spans()` **by ordinal**, and a
            // fallback without it shifted every later formula onto the wrong
            // source range (clicking a good formula put the caret inside a
            // failed one). The src stays visible as real text — no `data-vlen`,
            // so it is walked as text and the plain ledger still matches.
            s.push_str("<code data-math style=\"");
            s.push_str(CODE_INLINE_STYLE);
            s.push_str("\">");
            push_text_html(s, src);
            s.push_str("</code>");
        }
    }
}

/// Attribute-value escaping (quotes and structural characters). Shares the
/// same character set / entities as the preview.
fn escape_attr(t: &str) -> String {
    let mut o = String::with_capacity(t.len());
    crate::preview::push_escape_html(&mut o, t);
    o
}

/// Text HTML escaping. **Consistency with `plain_text`**: the entity decodes
/// back to the same character sequence, so offsets are unaffected. Shares the
/// implementation with the preview (`preview::push_escape_html`).
fn push_text_html(s: &mut String, t: &str) {
    crate::preview::push_escape_html(s, t);
}

/// Code-block text: emitted as-is (including `\n`); line breaks are handled by
/// `white-space:pre-wrap` (CODE_STYLE). **No `<br>`** — the `\n` must stay in
/// the text node, otherwise layout bytes and `Pos.off` drift inside the code
/// block (the premise of the virtual-byte mapping: non-atom offsets are
/// identity).
fn push_code_text(s: &mut String, text: &str) {
    // Chunked escaping (same character set as push_text_html): only break the
    // chunk at chars that need escaping, avoiding a per-char String allocation
    // (a 10KB code block once caused 10 thousand small allocations).
    let mut chunk = 0usize;
    for (i, c) in text.char_indices() {
        let esc = match c {
            '&' => "&amp;",
            '<' => "&lt;",
            '>' => "&gt;",
            '"' => "&quot;",
            _ => continue,
        };
        s.push_str(&text[chunk..i]);
        s.push_str(esc);
        chunk = i + c.len_utf8();
    }
    s.push_str(&text[chunk..]);
}

// ---------------------------------------------------------------------------
// Styles. Calibrated value by value
// (line-height 1.5, headings 1.75/1.55/1.38/1.25/1.13/1.0 em, blockquote 0.25em
// left border, table 6px 13px cells, inline code 85%…), trimmed to Blitz's
// support surface (no ::marker/::before etc.): grays are uniformly
// rgba(128,128,128,α) — readable following the light/dark theme.
// ---------------------------------------------------------------------------

pub const PARA_STYLE: &str = "margin:0 0 16px;min-height:24px;line-height:1.5;";
pub const QUOTE_STYLE: &str =
    "margin:0 0 16px;min-height:24px;padding:0 1em;color:var(--wys-second,#6a737d);\
     border-left:0.25em solid rgba(128,128,128,.35);line-height:1.5;";
/// Container style for a block inside a quote (`Block::Quoted`) — a left
/// border + depth-stacked indent (the padding-left is appended by
/// `quote_wrap`). No text node (geom convention 3).
pub const QUOTE_NEST_STYLE: &str = "margin:0 0 16px;padding:0 1em;color:var(--wys-second,#6a737d);\
     border-left:0.25em solid rgba(128,128,128,.35);";
pub const HR_STYLE: &str =
    "margin:24px 0;height:2px;padding:0;border:0;background:rgba(128,128,128,.35);";
pub const CODE_STYLE: &str =
    "margin:0 0 16px;padding:0.5em;background:rgba(128,128,128,.14);border-radius:5px;\
     font-family:monospace;font-size:85%;line-height:1.5;white-space:pre-wrap;";
/// The edit view of a GFM HTML block: shown as **source text** (a monospace
/// block), with the same background as a code block plus a more solid gray
/// border on the left to distinguish it. The text = the raw HTML source (same
/// origin as `plain_text`, so Pos mapping and editing work as usual); only the
/// preview exit actually passes it through to execute.
pub const HTML_BLOCK_STYLE: &str =
    "margin:0 0 16px;padding:0.5em;background:rgba(128,128,128,.14);border-radius:5px;\
     border-left:3px solid rgba(128,128,128,.45);\
     font-family:monospace;font-size:85%;line-height:1.5;white-space:pre-wrap;";
pub const CODE_INLINE_STYLE: &str =
    "padding:.2em .4em;background:rgba(128,128,128,.16);border-radius:3px;font-family:monospace;font-size:85%;";
pub const LIST_STYLE: &str = "list-style:none;padding-left:2em;margin:0 0 16px;";
pub const LI_STYLE: &str = "list-style:none;position:relative;display:block;margin:0 0 4px;";
/// The marker hangs in the 2em pit dug by the ul; neither user-select nor
/// clicks affect the body text.
/// A single span (a second nested absolute would offset again relative to the
/// outer one and run off screen — stepped on in practice).
pub const MARKER_STYLE: &str =
    "position:absolute;left:-2em;width:1.75em;text-align:right;opacity:.65;user-select:none;";
/// data-bid carrier: only display:block can become an inline root
/// (convention 1).
pub const LI_TEXT_STYLE: &str = "display:block;min-height:22px;line-height:1.5;";
/// Task-item text: digs a 1.6em pit for the checkbox.
pub const TASK_TEXT_STYLE: &str = "display:block;min-height:22px;line-height:1.5;";
/// The checkbox hangs 1.5em to the left of the body text: the body stays in
/// the same column as an ordinary list item, and the box lands in the ul's 2em
/// indent pit.
pub const TASK_BOX_OFF_STYLE: &str =
    "position:absolute;left:-1.5em;top:4px;width:.9em;height:.9em;border:1.5px solid rgba(128,128,128,.8);\
     border-radius:2px;font-size:11px;line-height:1.1;text-align:center;user-select:none;";
pub const TASK_BOX_ON_STYLE: &str =
    "position:absolute;left:-1.5em;top:4px;width:.9em;height:.9em;border:1.5px solid #4a7dff;\
     background:rgba(74,125,255,.15);border-radius:2px;color:#4a7dff;font-size:11px;\
     line-height:1.1;text-align:center;font-weight:700;user-select:none;";
pub const TABLE_STYLE: &str =
    "border-collapse:collapse;margin:0 0 16px;width:100%;empty-cells:show;";
pub const LINK_STYLE: &str = "color:#4a7dff;text-decoration:underline;cursor:pointer;";
pub const IMG_STYLE: &str = "max-width:100%;vertical-align:middle;";
pub const MATH_STYLE: &str = "vertical-align:middle;";
/// A block-level formula: occupies its own line, horizontally centered, with
/// vertical margin (MathJax's display look).
/// `text-align` works in blitz's inline layout (table right-alignment is
/// verified).
pub const MATH_DISPLAY_STYLE: &str =
    "display:block;text-align:center;margin:12px 0;line-height:1.2;";

/// Heading text size, relative to the body size: h1..h6 =
/// 1.75/1.55/1.38/1.25/1.13/1.0 em.
///
/// The **single source** for the heading scale: [`head_style`] writes it as
/// the CSS `font-size`, and [`push_block`] multiplies a heading's formula
/// typesetting size by it (a formula SVG is sized in px, so it has to be told
/// the heading's scale explicitly — CSS font-size does not reach it).
fn head_em(level: u8) -> f32 {
    match level {
        1 => 1.75,
        2 => 1.55,
        3 => 1.38,
        4 => 1.25,
        5 => 1.13,
        _ => 1.0,
    }
}

fn head_style(level: u8) -> String {
    // h1..h6 em sizes from [`head_em`], margin-top 24px / margin-bottom 16px,
    // line-height inherited 1.5.
    let size = head_em(level);
    format!(
        "margin:24px 0 16px;min-height:22px;font-size:{size}em;font-weight:600;line-height:1.5;"
    )
}

/// Global CSS (injected once via an rsx `<style>`): container + caret overlay.
/// Content colors go through CSS variables (the demo/host can override
/// `--wys-fg` to implement a light/dark theme).
///
/// `.wys-content` must carry `white-space:pre-wrap`: the caret/selection
/// mapping's byte ledger (geom) treats every non-atom text offset as
/// identity — layout bytes = plain bytes. Under the default `normal`, runs of
/// spaces (and leading/trailing ones) collapse *visually only*, so typing a
/// few spaces leaves them in the text node but out of the layout: the caret
/// and click hit-testing drift left by the swallowed width (the "typing
/// spaces misplaces the caret" report). `pre-wrap` keeps every space a real
/// glyph, and the emitter's side of the contract is already enforced by a
/// test: no whitespace-only text may sit between tags (see
/// `content_html_never_has_whitespace_between_tags`).
///
/// Caret blinking uses the variable `--wys-caret-op` (1/0, updated by the host
/// every half-period according to the BlinkTick phase): a CSS infinite
/// animation **must not** be used — blitz self-sustains `request_redraw` every
/// frame for a window with a running animation (the `is_animating` branch of
/// `View::redraw`), so the whole app would redraw forever at 40–60fps
/// (observed in the 2026-10 seventh round). Unset it defaults to 1 = always
/// on.
pub const SHELL_CSS: &str = "
.wys-root { position: relative; overflow: auto; height: 100%; }
.wys-content { position: relative; padding: 16px 20px 40vh; color: var(--wys-fg, #1a1a1a); font-size: 16px; line-height: 1.5; white-space: pre-wrap; }
.wys-content th { font-weight: 600; }
.wys-caret { position: absolute; width: 2px; background: var(--wys-caret, #4a7dff); pointer-events: none; opacity: var(--wys-caret-op, 1); }
.wys-cell { position: absolute; box-sizing: border-box; border-radius: 2px; pointer-events: none; background: var(--wys-cell, rgba(74,125,255,.16)); border: 1px solid rgba(74,125,255,.45); }
";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::md;

    /// The container keeps every space a real glyph (see the `SHELL_CSS` doc:
    /// geom's byte ledger assumes layout bytes = plain bytes; collapsing runs
    /// of spaces breaks it and typing spaces misplaces the caret).
    #[test]
    fn content_css_preserves_spaces() {
        let rule = SHELL_CSS
            .lines()
            .find(|l| l.trim_start().starts_with(".wys-content {"))
            .expect(".wys-content rule missing");
        assert!(
            rule.contains("white-space:pre-wrap") || rule.contains("white-space: pre-wrap"),
            ".wys-content must set white-space:pre-wrap: {rule}"
        );
    }

    /// Is there a whitespace-only run between two tags? Under `pre-wrap` such a
    /// run would *render* (a stray gap / line), and it would also desync geom's
    /// byte ledger — the emitter must never produce one.
    fn inter_tag_whitespace(html: &str) -> Option<usize> {
        let b = html.as_bytes();
        for i in 0..b.len() {
            if b[i] != b'>' {
                continue;
            }
            let mut j = i + 1;
            while j < b.len() && matches!(b[j], b' ' | b'\n' | b'\t' | b'\r') {
                j += 1;
            }
            if j > i + 1 && j < b.len() && b[j] == b'<' {
                return Some(i);
            }
        }
        None
    }

    /// Every block kind, emitted with no whitespace-only text between tags
    /// (spaces may only appear inside real text runs — `>a  b<` is fine,
    /// `>\n<` is not).
    #[test]
    fn content_html_never_has_whitespace_between_tags() {
        let doc = md::parse(
            "# 标题\n\n段落 with  multiple   spaces\n\n- 条目 一\n- 条目 二\n\
             \n> 引用  文本\n\n| a | b |\n|---|---|\n| 1 | 2 |\n\n```rust\nlet a = 1;\n```\n\n---\n",
        );
        let html = content_html(&doc);
        assert_eq!(
            inter_tag_whitespace(&html),
            None,
            "whitespace-only text between tags: {html}"
        );
    }

    /// End to end: the boxed gathered CJK flowchart renders through the
    /// WYSIWYG content path as an inline SVG math atom (no LaTeX leakage).
    #[test]
    fn boxed_gathered_cjk_flowchart_falls_back_to_code() {
        // No CJK fallback face (vendor removed): the formula cannot typeset;
        // the content view shows the source in the <code> fallback — content
        // is never swallowed.
        let doc = md::parse(&format!("$$ {} $$", crate::math::BOXED_GATHERED_FLOWCHART));
        let html = content_html(&doc);
        assert!(html.contains("<code"), "fallback shell: {html}");
        assert!(
            html.contains("讨论卤素向深部地幔的再循环"),
            "the source must stay visible: {html}"
        );
        assert!(!html.contains("<svg"), "{html}");
    }

    /// A formula inside a heading must be typeset at the heading's size.
    ///
    /// The formula SVG carries its size in px (the glyphs are paths, see
    /// math.rs), so it does not follow the surrounding `font-size` — before this
    /// fix an h1 (1.75em) drew a body-sized formula next to its own heading
    /// text. `head_em` is the shared table for both the CSS size and this
    /// scale.
    #[test]
    fn heading_formula_is_typeset_at_heading_size() {
        let height = |html: &str| -> f32 {
            let i = html.find("height=\"").expect("svg height attribute") + 8;
            let j = html[i..].find('"').expect("closing quote") + i;
            html[i..j].parse().expect("numeric height")
        };
        let body = height(&content_html(&md::parse("$x$")));
        let h1 = height(&content_html(&md::parse("# $x$")));
        let h2 = height(&content_html(&md::parse("## $x$")));
        assert!(h1 > h2, "h1 ({h1}) must be taller than h2 ({h2})");
        assert!(h2 > body, "h2 ({h2}) must be taller than body ({body})");
        // The CSS side comes from the same table.
        assert!(
            head_style(1).contains("font-size:1.75em"),
            "{}",
            head_style(1)
        );
        assert!(head_style(6).contains("font-size:1em"), "{}", head_style(6));
    }

    /// data-bid appears exactly once per block (geom's reverse lookup depends
    /// on it).
    #[test]
    fn every_block_has_unique_bid() {
        let doc = md::parse(
            "# H\n\npara one\n\n- l1\n- l2\n\n> q\n\n```\ncode\n```\n\n---\n\n| a | b |\n|---|---|\n| 1 | 2 |\n",
        );
        let html = content_html(&doc);
        let mut counts = std::collections::HashMap::new();
        let mut rest = html.as_str();
        while let Some(p) = rest.find("data-bid=\"") {
            let after = &rest[p + 10..];
            let end = after.find('"').unwrap();
            *counts.entry(after[..end].to_string()).or_insert(0) += 1;
            rest = &after[end..];
        }
        for i in 0..doc.blocks.len() {
            assert_eq!(counts.get(&i.to_string()), Some(&1), "block {i} bid count");
        }
    }

    /// A block inside a quote (`Block::Quoted`) is wrapped in a quote-styled
    /// container (left border + indent); the container has no text node, and
    /// each block's data-bid still appears exactly once (geom's reverse lookup
    /// depends on it).
    #[test]
    fn quoted_block_wrapped_in_quote_container() {
        let doc = md::parse("> - a\n> - b\n");
        let html = content_html(&doc);
        assert!(html.contains(QUOTE_NEST_STYLE), "{html}");
        assert!(html.contains("border-left"), "{html}");
        for i in 0..doc.blocks.len() {
            assert_eq!(
                html.matches(&format!("data-bid=\"{i}\"")).count(),
                1,
                "block {i} bid count: {html}"
            );
        }
        // Fragment concatenation matches the whole segment (sharded rendering
        // also holds for Quoted)
        assert_eq!(region_fragments(&doc, DEFAULT_FG).concat(), html);
    }

    /// List markers/checkboxes must not appear inside a data-bid element's text
    /// flow (convention 2).
    #[test]
    fn list_marker_outside_bid_element() {
        let doc = md::parse("- l1\n- [x] t2\n");
        let html = content_html(&doc);
        let p = html.find("data-bid=\"0\"").unwrap();
        let inner_start = p + "data-bid=\"0\"".len();
        let inner_end = html[inner_start..].find("</span>").unwrap() + inner_start;
        let inner = &html[inner_start..inner_end];
        assert!(
            !inner.contains("•") && !inner.contains("1. "),
            "marker leaked: {inner}"
        );
        assert!(html.contains("data-marker"), "marker missing");
        assert!(html.contains("data-task-bid=\"1\""), "checkbox missing");
    }

    /// A nested list renders as nested ul/ol.
    #[test]
    fn nested_list_dom_structure() {
        let doc = md::parse("- a\n  - b\n- c\n");
        let html = content_html(&doc);
        // The inner ul appears before the outer ul closes
        let inner = html.find(
            "<ul style=\"list-style:none;padding-left:2em;margin:0 0 16px;margin-left:2em;\">",
        );
        assert!(inner.is_some(), "nested ul missing: {html}");
        let l1 = html.find("data-bid=\"1\"").unwrap();
        let l2 = html.find("data-bid=\"2\"").unwrap();
        assert!(inner.unwrap() < l1 && l1 < l2);
    }

    /// Inline HTML escaping does not change the plain-text order. The image is
    /// asserted as the convention-3 exception (<img> has no text node; the alt
    /// exists only in plain_text — render-layer offsets are snapped to the
    /// nearest position by parley).
    #[test]
    fn inline_html_preserves_plain_text_order() {
        let doc = md::parse("a & b <tag> \"q\" **bold** `c` [l](u)");
        let html = content_html(&doc);
        let plain = strip_tags(&html);
        let expected = doc.blocks[0].plain_text();
        assert!(
            plain.contains(&expected),
            "html={html} plain={plain} expected={expected}"
        );
        let doc2 = md::parse("x ![i](p.png) y");
        let html2 = content_html(&doc2);
        assert!(html2.contains("data-img=\"p.png\""), "img missing: {html2}");
        assert!(html2.contains("alt=\"i\""));
    }

    /// A link carries a data-link attribute (Ctrl+click-to-open depends on it).
    #[test]
    fn link_has_data_attr() {
        let doc = md::parse("[txt](https://x.y)");
        let html = content_html(&doc);
        assert!(html.contains("data-link=\"https://x.y\""), "got: {html}");
    }

    /// Math: renders an SVG or falls back to code (one of the two).
    #[test]
    fn math_renders_svg_or_fallback() {
        let doc = md::parse("a $x$ b");
        let html = content_html(&doc);
        assert!(
            html.contains("data-math") || html.contains("<code"),
            "got: {html}"
        );
    }

    /// Fragment concatenation is byte-identical to the whole HTML (the premise
    /// of sharded rendering).
    #[test]
    fn fragments_concat_matches_content_html() {
        let md = "# T\n\npara one\n\n- l1\n- l2\n  - l3\n\n> q\n\n---\n\n```\ncode\n```\n\n| a | b |\n|---|---|\n| 1 | 2 |\n\nlast para\n";
        let doc = md::parse(md);
        assert_eq!(
            region_fragments(&doc, DEFAULT_FG).concat(),
            content_html(&doc)
        );
        // Empty document: parses to one empty Para block → a single fragment,
        // still consistent with content_html.
        let empty = md::parse("");
        assert_eq!(
            region_fragments(&empty, DEFAULT_FG).concat(),
            content_html(&empty)
        );
    }

    /// The placeholder span for a rejected URL is a **real text node** (the alt)
    /// and must not carry data-vlen — geom treats a data-vlen element as an
    /// atom and does not recurse its subtree, but the alt text enters parley's
    /// layout, so the two ledgers would be off by alt.len() bytes after the
    /// alt.
    #[test]
    fn image_fallback_span_has_no_data_vlen() {
        let doc = md::parse("x ![i](javascript:alert) y");
        let html = content_html(&doc);
        assert!(
            !html.contains("<img"),
            "rejected url must not emit <img>: {html}"
        );
        let p = html.find("<span data-img=").expect("fallback span missing");
        let tag_end = html[p..].find('>').unwrap() + p;
        let tag = &html[p..tag_end];
        assert!(
            !tag.contains("data-vlen"),
            "fallback span must not carry data-vlen: {tag}"
        );
        assert!(tag.contains("alt=\"i\""), "alt attr missing: {tag}");
        // The alt text is output as a text node (the caret can land on it), and
        // the plain-text order is unchanged.
        assert!(html.contains(">i</span>"), "alt text node missing: {html}");
    }

    /// Td's row values are not guaranteed monotonic (paste/editing can disorder
    /// them) — group by row, emit in ascending order: row 0 goes to thead/th,
    /// the rest to tbody/td; within a row the appearance order (column order) is
    /// kept; data-bid keeps the original block number (geom's reverse lookup is
    /// unaffected by emission order).
    #[test]
    fn table_rows_grouped_by_row_value() {
        let td = |row: usize, txt: &str| Block::Td {
            table_seq: 0,
            row,
            align: 0,
            inlines: vec![Inline::Text(txt.to_string())],
        };
        let doc = Doc::new(vec![td(1, "r1c1"), td(0, "h1"), td(1, "r1c2"), td(0, "h2")]);
        let html = content_html(&doc);
        let i_h1 = html.find("<th data-bid=\"1\"").expect("h1 th missing");
        let i_h2 = html.find("<th data-bid=\"3\"").expect("h2 th missing");
        let i_r1 = html.find("<td data-bid=\"0\"").expect("r1c1 td missing");
        let i_r2 = html.find("<td data-bid=\"2\"").expect("r1c2 td missing");
        // The two header columns come first (column order h1→h2), the data row
        // after (r1c1→r1c2)
        assert!(
            i_h1 < i_h2 && i_h2 < i_r1 && i_r1 < i_r2,
            "row grouping order wrong: {html}"
        );
        assert!(
            html.find("<thead><tr>").unwrap() < i_h1,
            "thead must wrap row 0: {html}"
        );
        assert!(
            html.contains("</thead><tbody>"),
            "tbody must follow thead: {html}"
        );
    }

    /// A relative-path image is resolved against the base into a file:// URL
    /// (the same `safe_image_url` whitelist as the preview); with no base it
    /// cannot resolve → alt placeholder, no <img>.
    #[test]
    fn image_base_resolves_relative_path() {
        let doc = md::parse("![i](p.png)");
        // A local image is stat'ed for existence before rendering (missing → alt
        // placeholder), so create a real file
        let dir = std::env::temp_dir().join(format!("wys_img_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("p.png");
        std::fs::write(&file, b"x").unwrap();
        let html = content_html_with(&doc, DEFAULT_FG, Some(dir.as_path()));
        let expected = crate::preview::safe_image_url("p.png", Some(dir.as_path()))
            .expect("base-relative path must resolve");
        assert!(
            expected.starts_with("file:"),
            "base-resolved url should be file://: {expected}"
        );
        assert!(
            html.contains(&format!("<img src=\"{expected}\"")),
            "resolved url missing: {html}"
        );
        // <img> has no text node → data-vlen records the alt's bytes (the two
        // ledgers work as usual)
        assert!(html.contains("data-vlen=\"1\""), "img vlen missing: {html}");
        let _ = std::fs::remove_file(&file);
        // No base: the relative path fails to resolve → alt placeholder
        let html2 = content_html_with(&doc, DEFAULT_FG, None);
        assert!(
            !html2.contains("<img"),
            "no-base must fall back to placeholder: {html2}"
        );
        assert!(html2.contains(">i</span>"), "alt text missing: {html2}");
        // Resolution succeeded but the file does not exist: also falls back to the
        // alt placeholder (a blank <img> is imperceptible to the user)
        let missing = md::parse("![i](missing.png)");
        let html3 = content_html_with(&missing, DEFAULT_FG, Some(dir.as_path()));
        assert!(
            !html3.contains("<img"),
            "missing local file must fall back to placeholder: {html3}"
        );
        assert!(html3.contains(">i</span>"), "alt text missing: {html3}");
    }

    /// An in-block line break renders as a **text node** `\n` (wrapped in
    /// white-space:pre-wrap) — layout bytes are identical to plain bytes (geom's
    /// two ledgers); **no `<br>`** (blitz expands br into a parley `\n` but the
    /// DOM has no text node, so every break drifts by 1 byte).
    #[test]
    fn breaks_render_as_text_node_newline() {
        // One paragraph containing a hard break (a  \nb) and two soft breaks
        // (b\nc\nd)
        let doc = md::parse("a  \nb\nc\nd");
        let html = content_html(&doc);
        assert!(
            html.contains("white-space:pre-wrap"),
            "pre-wrap span missing: {html}"
        );
        assert!(!html.contains("<br"), "must not use <br>: {html}");
        // The text stream = plain (a break counts 1 byte)
        assert_eq!(doc.blocks[0].plain_text(), "a\nb\nc\nd");
        assert_eq!(strip_tags(&html), "a\nb\nc\nd");
    }

    /// Grouped blocks (list items / cells) normally go through the list/table
    /// builders; a malformed tree that leaks one into the ungrouped path must render
    /// its text instead of panicking (the old code had `unreachable!` there).
    #[test]
    fn ungrouped_cell_and_item_render_as_paragraphs() {
        let td = Block::Td {
            table_seq: 0,
            row: 0,
            align: 0,
            inlines: vec![Inline::Text("单元格".into())],
        };
        let mut html = String::new();
        push_block(&mut html, 0, &td, "#000", None, 14.0);
        assert!(html.contains("单元格"), "{html}");

        let li = Block::Li {
            ordered: false,
            num: 0,
            level: 0,
            task: None,
            inlines: vec![Inline::Text("条目".into())],
        };
        let mut html = String::new();
        push_block(&mut html, 1, &li, "#000", None, 14.0);
        assert!(html.contains("条目"), "{html}");
    }

    fn strip_tags(html: &str) -> String {
        let mut out = String::new();
        let mut depth: i32 = 0;
        for c in html.chars() {
            match c {
                '<' => depth += 1,
                '>' => depth = depth.saturating_sub(1),
                _ if depth == 0 => out.push(c),
                _ => {}
            }
        }
        out.replace("&amp;", "&")
            .replace("&lt;", "<")
            .replace("&gt;", ">")
            .replace("&quot;", "\"")
    }
}
