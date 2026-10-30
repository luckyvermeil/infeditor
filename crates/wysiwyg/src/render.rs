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
use crate::reveal::Reveal;

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
    region_fragments_revealed(doc, fg, img_base, math_px, &Reveal::none())
}

/// Same as [`region_fragments_sized`], with the **source expansion** applied
/// (see [`crate::reveal`]): the inline nodes the reveal covers emit their
/// Markdown delimiters, and the formula being edited emits its LaTeX source.
///
/// The delimiters are *virtual* spans (`data-vmark`): they take layout space but
/// no plain bytes, so `Pos.off` and every command riding on it see the document
/// exactly as before.
pub fn region_fragments_revealed(
    doc: &Doc,
    fg: &str,
    img_base: Option<&Path>,
    math_px: f32,
    reveal: &Reveal,
) -> Vec<String> {
    let e = Emit {
        fg,
        img_base,
        math_px,
        reveal,
    };
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
                emit_list_region(&doc.blocks[start..i], start, &mut s, &e);
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
                emit_table(&doc.blocks[start..i], start, &mut s, &e);
                frags.push(quote_wrap(&doc.blocks[start], s));
            }
            _ => {
                let mut s = String::new();
                // The **full** block, not the matched core: for a
                // `Quoted { depth, inner }` leaf (a code/HTML block or a rule inside
                // a quote) the core has already dropped the shell, and the source
                // form then lost every `> ` — the shell's own branch (which hands
                // `"> "×depth` down to the inner block's lines) never ran. The
                // non-source form renders the inner block identically either way.
                push_block(&mut s, i, &doc.blocks[i], &e);
                frags.push(quote_wrap(&doc.blocks[i], s));
                i += 1;
            }
        }
    }
    frags
}

/// Everything the emission of one fragment needs: theme colors, the image base,
/// the formula size — and the source reveal.
struct Emit<'a> {
    fg: &'a str,
    img_base: Option<&'a Path>,
    math_px: f32,
    reveal: &'a Reveal,
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
fn emit_list_region(items: &[Block], base: usize, s: &mut String, e: &Emit<'_>) {
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
            push_block(s, base + k, b, e);
            continue;
        };
        let bid = base + k;
        // Source form: the item's own marker in front of the line (`- ` / `1. ` /
        // `- [x] `), behind the quote shell it may sit in.
        let mut prefix = String::new();
        if e.reveal.contains(bid) {
            prefix.push_str(&quote_prefix(b));
            if *ordered {
                prefix.push_str(&format!("{num}. "));
            } else {
                prefix.push_str("- ");
            }
            if let Some(checked) = task {
                prefix.push_str(if *checked { "[x] " } else { "[ ] " });
            }
        }
        let quote = if prefix.is_empty() {
            String::new()
        } else {
            quote_prefix(b)
        };
        push_li(
            s, bid, *ordered, *num, *level, *task, inlines, &prefix, &quote, e,
        );
    }
    while let Some((_, to)) = stack.pop() {
        let tag = if to { "ol" } else { "ul" };
        s.push_str(&format!("</{tag}>"));
    }
}

fn push_block(s: &mut String, idx: usize, b: &Block, e: &Emit<'_>) {
    push_block_p(s, idx, b, "", e);
}

/// `quote`: the `> ` shell markers to print in front of the line when the block is
/// shown as **source** (one `> ` per nesting level). They have to sit inside the
/// block's own carrier — a marker outside a `display:block` element would land on a
/// line of its own, and outside the block's inline root the caret could not reach
/// it.
fn push_block_p(s: &mut String, idx: usize, b: &Block, quote: &str, e: &Emit<'_>) {
    let src = e.reveal.contains(idx);
    match b {
        Block::Para(inlines) => {
            // A paragraph that is exactly one display formula is a block of its own
            // (the preview segments display math the same way). Wrapping it in the
            // paragraph's `margin-bottom:16px; min-height:24px` made the spacing
            // around a formula differ from the preview. In source form it keeps the
            // paragraph shell instead: the `$$` markers come from the inline walk.
            if !src {
                if let [Inline::Math { src, display: true }] = inlines.as_slice() {
                    // The block keeps its measured typeset height while the source
                    // is on screen (`block_style` adds the floor): a formula SVG and
                    // its `$$…$$` line are not the same height, and the difference
                    // moved everything below the formula on every keystroke that
                    // stepped onto it.
                    s.push_str(&format!(
                        "<div data-bid=\"{idx}\" style=\"{}\">",
                        block_style(idx, "margin:10px 0;", e)
                    ));
                    if e.reveal.shows_atom(idx, 0..src.len()) {
                        push_math_source(s, src, true);
                    } else {
                        push_math(s, src, true, e.fg, e.math_px);
                    }
                    s.push_str("</div>");
                    return;
                }
            }
            s.push_str(&format!(
                "<div data-bid=\"{idx}\" style=\"{}\">",
                block_style(idx, PARA_STYLE, e)
            ));
            push_lead(s, quote, src);
            push_inlines(s, inlines, idx, 0, if src { quote } else { "" }, e);
            // Empty block: no text node added (convention 3), the height is
            // held up by min-height.
            s.push_str("</div>");
        }
        Block::Head { level, inlines } => {
            s.push_str(&format!(
                "<div data-bid=\"{idx}\" style=\"{}\">",
                block_style(idx, &head_style(*level), e)
            ));
            // A formula SVG carries its size in *pixels* (the glyphs are paths,
            // see math.rs), so it does not follow the heading's `font-size` the
            // way a text node does. Typeset it at the heading's own scale
            // instead — otherwise an h1 (1.75em) showed a body-sized formula,
            // visibly a size or two small next to its own heading text.
            let px = e.math_px * head_em(*level);
            let he = Emit {
                fg: e.fg,
                img_base: e.img_base,
                math_px: px,
                reveal: e.reveal,
            };
            if src {
                push_lead(s, quote, true);
                let empty = inlines.iter().all(|i| i.plain().trim().is_empty());
                push_marker(s, &crate::md::heading_marker(*level, empty), true);
            }
            push_inlines(s, inlines, idx, 0, if src { quote } else { "" }, &he);
            s.push_str("</div>");
        }
        Block::Code { lang, text } => {
            s.push_str(&format!(
                "<div data-bid=\"{idx}\" class=\"lang-{}\" style=\"{}\">",
                escape_attr(lang.as_deref().unwrap_or("text")),
                block_style(idx, CODE_STYLE, e)
            ));
            if src {
                push_code_source(s, lang.as_deref(), text, quote, e);
            } else {
                // Coloring hook (registered by the host, see `hl`); when not
                // registered, escape as plain text.
                match lang
                    .as_deref()
                    .and_then(|l| crate::hl::highlight(l, text, e.fg))
                {
                    Some(body) => s.push_str(&body),
                    None => push_code_text(s, text),
                }
            }
            s.push_str("</div>");
        }
        Block::Quote(inlines) => {
            s.push_str(&format!(
                "<div data-bid=\"{idx}\" style=\"{}\">",
                block_style(idx, QUOTE_STYLE, e)
            ));
            // A `Quote` is one quote level by definition; every line of it starts
            // with `> ` in the source.
            let lead = if src {
                let lead = format!("{quote}> ");
                push_marker(s, &lead, true);
                lead
            } else {
                String::new()
            };
            push_inlines(s, inlines, idx, 0, &lead, e);
            s.push_str("</div>");
        }
        Block::Html { text } => {
            s.push_str(&format!(
                "<div data-bid=\"{idx}\" style=\"{}\">",
                block_style(idx, HTML_BLOCK_STYLE, e)
            ));
            push_lead(s, quote, src);
            // Shown as source: escaped output (this is only the edit view; the
            // preview passes it through). Inside a quote the writer prefixes
            // **every** line with `> ` (see `push_prefixed_lines`), so the source
            // view repeats the shell after each real newline — the same treatment
            // the code fence gets; without it the copy's `> ` per line had no
            // on-screen counterpart past the first line.
            if src && !quote.is_empty() && text.contains('\n') {
                let lines: Vec<&str> = text.split('\n').collect();
                let last = lines.len().saturating_sub(1);
                for (i, line) in lines.iter().enumerate() {
                    if i > 0 {
                        // The break is a real byte of the HTML text; only the shell
                        // in front of the next line is virtual. A text ending in
                        // `\n` leaves a phantom last line — it gets no prefix (the
                        // writer skips it too).
                        s.push('\n');
                        if i < last || !line.is_empty() {
                            push_marker(s, quote, true);
                        }
                    }
                    push_code_text(s, line);
                }
            } else {
                push_code_text(s, text);
            }
            s.push_str("</div>");
        }
        Block::Hr => {
            if src {
                // A rule *is* its source: `---` (zero plain bytes — `Hr` carries no
                // text, so the marker is pure decoration for the ledger).
                s.push_str(&format!(
                    "<div data-bid=\"{idx}\" style=\"{}\">",
                    block_style(idx, HR_SOURCE_STYLE, e)
                ));
                push_lead(s, quote, true);
                push_marker(s, "---", true);
                s.push_str("</div>");
            } else {
                s.push_str(&format!(
                    "<div data-bid=\"{idx}\" style=\"{HR_STYLE}\"></div>"
                ));
            }
        }
        Block::Li { inlines, .. } | Block::Td { inlines, .. } => {
            // Grouped blocks normally go through push_list/push_table. If one leaks
            // into the ungrouped path, render its content as a paragraph instead of
            // killing the render.
            s.push_str(&format!(
                "<div data-bid=\"{idx}\" style=\"{}\">",
                block_style(idx, PARA_STYLE, e)
            ));
            push_lead(s, quote, src);
            push_inlines(s, inlines, idx, 0, if src { quote } else { "" }, e);
            s.push_str("</div>");
        }
        // Quote-ownership wrapper: the `> ` markers belong to the inner block's line,
        // so they are handed down rather than emitted here.
        Block::Quoted { depth, inner } => {
            if src {
                let q = "> ".repeat(usize::from(*depth));
                push_block_p(s, idx, inner, &q, e);
            } else {
                push_block_p(s, idx, inner, quote, e);
            }
        }
    }
}

/// A fenced code block in source form: the fence lines (virtual) around the code
/// text (real — it *is* the block's plain text).
///
/// Unquoted blocks keep their syntax colors (the fence wraps the whole text in one
/// piece); a block inside a quote has to repeat `> ` on every line, which needs the
/// text split — that path is plain, and it is the only one that pays for it.
fn push_code_source(s: &mut String, lang: Option<&str>, text: &str, quote: &str, e: &Emit<'_>) {
    let (open, close) = crate::md::code_fence_lines(lang, text);
    push_lead(s, quote, true);
    push_marker(s, &format!("{open}\n"), true);
    if quote.is_empty() {
        match lang.and_then(|l| crate::hl::highlight(l, text, e.fg)) {
            Some(body) => s.push_str(&body),
            None => push_code_text(s, text),
        }
    } else {
        for (i, line) in text.split('\n').enumerate() {
            if i > 0 {
                // The break is a **real** byte (it belongs to the code text); only
                // the `> ` in front of the next line is virtual.
                s.push('\n');
                push_marker(s, quote, true);
            }
            push_code_text(s, line);
        }
    }
    // The newline before the closing fence belongs to the fence, not to the code
    // text (whose plain bytes end at the last character), so it is part of the
    // virtual marker.
    push_marker(s, &format!("\n{quote}{close}"), false);
}

/// One list item. The marker and checkbox are absolutely positioned and hung
/// (convention 2), and data-bid lands on a display:block span (convention 1:
/// it is itself the inline root).
///
/// `prefix` is the item's **source** marker (`- `, `1. `, `- [x] `, with the quote
/// shell in front) — non-empty only while the item is expanded. Then the positioned
/// bullet/checkbox is dropped and the marker becomes part of the line's text
/// (virtual bytes), exactly as the writer spells it.
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
    prefix: &str,
    line: &str,
    e: &Emit<'_>,
) {
    let src = !prefix.is_empty();
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
    if src {
        s.push_str(&format!("<li style=\"{LI_STYLE}\">"));
    } else {
        s.push_str(&format!(
            "<li style=\"{LI_STYLE}\">\
             <span data-marker style=\"{MARKER_STYLE}\">{marker}</span>"
        ));
    }
    if let Some(checked) = task {
        if !src {
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
    }
    s.push_str(&format!(
        "<span data-bid=\"{idx}\" style=\"{}\">",
        block_style(
            idx,
            if task.is_some() {
                TASK_TEXT_STYLE
            } else {
                LI_TEXT_STYLE
            },
            e
        )
    ));
    if src {
        push_marker(s, prefix, true);
    }
    // Continuation lines of an item are indented, not re-marked, so `line` carries
    // only the quote shell.
    push_inlines(s, inlines, idx, 0, if src { line } else { "" }, e);
    s.push_str("</span></li>");
}

/// The `> ` shell a block sits in, in source form (one per quote nesting level).
fn quote_prefix(b: &Block) -> String {
    "> ".repeat(usize::from(b.quote_depth()))
}

fn emit_table(items: &[Block], base: usize, s: &mut String, e: &Emit<'_>) {
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
    let cols = rows.iter().map(|(_, c)| c.len()).max().unwrap_or(0);
    // Source form: for the reader a table is **one block** — a selection anywhere
    // in it (`Reveal::from_editor` marks every cell of the table) shows the whole
    // table as its Markdown: `|` between the cells, the delimiter row after the
    // header. The cells keep their own `data-bid`, so the caret still addresses
    // cells while the table reads as source.
    if items
        .iter()
        .enumerate()
        .any(|(k, _)| e.reveal.contains(base + k))
    {
        s.push_str(&format!("<table style=\"{TABLE_SRC_STYLE}\">"));
        // The quote shell is part of the source line (`> | a | b |`), and a `<table>`
        // has no place for text outside its cells — so each row carries the lead
        // inside its first cell, in front of the `|`. One `> ` per nesting level.
        let lead = items.first().map(quote_prefix).unwrap_or_default();
        let mut head_open = false;
        let mut body_open = false;
        for (ri, (row, cells)) in rows.iter().enumerate() {
            if *row == 0 {
                s.push_str("<thead><tr>");
                head_open = true;
            } else {
                if head_open {
                    s.push_str("</thead>");
                    head_open = false;
                }
                if !body_open {
                    s.push_str("<tbody>");
                    body_open = true;
                }
                s.push_str("<tr>");
            }
            let tag = if *row == 0 { "th" } else { "td" };
            for (ci, (bid, b)) in cells.iter().enumerate() {
                let last = ci + 1 == cells.len();
                s.push_str(&format!(
                    "<{tag} data-bid=\"{bid}\" style=\"{TD_SRC_STYLE}{}\">",
                    floor_css(*bid)
                ));
                if ci == 0 {
                    push_marker(s, &lead, true);
                }
                push_marker(s, "| ", true);
                match b {
                    Block::Td { inlines, .. } => push_inlines(s, inlines, *bid, 0, "", e),
                    other => push_code_text(s, &other.plain_text()),
                }
                push_marker(s, if last { " |" } else { " " }, false);
                s.push_str(&format!("</{tag}>"));
            }
            // A ragged row is padded so the row keeps its trailing pipe.
            if cells.len() < cols {
                s.push_str(&format!("<td style=\"{TD_SRC_STYLE}\">"));
                push_marker(s, "| ", true);
                push_marker(s, " |", false);
                s.push_str("</td>");
            }
            s.push_str("</tr>");
            // The delimiter row follows the header (GFM), alignment per column.
            if ri == 0 {
                s.push_str("<tr>");
                for ci in 0..cols {
                    let align = match cells.get(ci) {
                        Some((_, Block::Td { align: 1, .. })) => ":--",
                        Some((_, Block::Td { align: 2, .. })) => ":-:",
                        Some((_, Block::Td { align: 3, .. })) => "--:",
                        _ => "---",
                    };
                    s.push_str(&format!("<td style=\"{TD_SRC_DELIM_STYLE}\">"));
                    if ci == 0 {
                        push_marker(s, &lead, true);
                    }
                    push_marker(s, "| ", true);
                    push_marker(s, align, false);
                    push_marker(s, if ci + 1 == cols { " |" } else { " " }, false);
                    s.push_str("</td>");
                }
                s.push_str("</tr>");
            }
        }
        if head_open {
            s.push_str("</thead>");
        }
        if body_open {
            s.push_str("</tbody>");
        }
        s.push_str("</table>");
        return;
    }
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
                    // A cell whose formula is expanded to its LaTeX (the caret is in
                    // it) keeps its typeset height for the same reason a revealed
                    // table does: the row would otherwise shrink under the caret and
                    // the table below it would move.
                    let style = format!("{}{}", td_style(*align), reveal_floor(*bid, e));
                    s.push_str(&format!("<{tag} data-bid=\"{bid}\" style=\"{style}\">"));
                    push_inlines(s, inlines, *bid, 0, "", e);
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

/// Emit a block's per-line lead (`> `) when the block is shown as source. The lead
/// belongs to the *first* line too, so it is emitted before the inline walk, which
/// repeats it after every in-block break.
fn push_lead(s: &mut String, lead: &str, src: bool) {
    if src {
        push_marker(s, lead, true);
    }
}

/// Emit one inline list. `base` is the plain-byte offset the list starts at
/// inside its block (0 for the block's own inline root, the container's start
/// for a nested one — a container's plain text is exactly its children's, so
/// the recursion stays on the same ledger).
///
/// `bid` is the block number and decides whether the **source form** is used: a
/// block the reveal covers shows its Markdown for *every* inline node (the unit is
/// the block, not the node), plus the one atom expanded on its own (the formula at
/// the caret). The delimiters are virtual spans (`data-vmark`, zero plain bytes)
/// around the still-formatted content, so what the highlight shows is what a copy
/// produces ([`crate::edit::Editor::selected_markdown`] writes the same bytes).
///
/// `line` is the block's per-line prefix (`> ` for a quote) — it is repeated after
/// every in-block line break, which is what the writer does.
fn push_inlines(
    s: &mut String,
    inlines: &[Inline],
    bid: usize,
    base: usize,
    line: &str,
    e: &Emit<'_>,
) {
    let whole = e.reveal.contains(bid);
    let mut at = base;
    for i in inlines {
        let len = i.plain_len();
        let shown = whole || e.reveal.shows_atom(bid, at..at + len);
        push_inline(s, i, bid, at, shown, line, e);
        at += len;
    }
}

#[allow(clippy::too_many_arguments)]
fn push_inline(
    s: &mut String,
    i: &Inline,
    bid: usize,
    at: usize,
    shown: bool,
    line: &str,
    e: &Emit<'_>,
) {
    match i {
        Inline::Text(t) => push_text_html(s, t),
        Inline::Code(c) => {
            let body = |s: &mut String| push_code_span(s, c);
            if shown {
                push_source_wrap(s, i, body);
            } else {
                body(s);
            }
        }
        Inline::Strong(c) => {
            let body = |s: &mut String| push_container(s, "strong", c, bid, at, line, e);
            if shown {
                push_source_wrap(s, i, body);
            } else {
                body(s);
            }
        }
        Inline::Em(c) => {
            let body = |s: &mut String| push_container(s, "em", c, bid, at, line, e);
            if shown {
                push_source_wrap(s, i, body);
            } else {
                body(s);
            }
        }
        Inline::Del(c) => {
            let body = |s: &mut String| push_container(s, "del", c, bid, at, line, e);
            if shown {
                push_source_wrap(s, i, body);
            } else {
                body(s);
            }
        }
        Inline::Link { url, inlines: c } => {
            let body = |s: &mut String| {
                s.push_str(&format!(
                    "<a data-link=\"{}\" style=\"{LINK_STYLE}\">",
                    escape_attr(url)
                ));
                push_inlines(s, c, bid, at, line, e);
                s.push_str("</a>");
            };
            if shown {
                push_source_wrap(s, i, body);
            } else {
                body(s);
            }
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
            if shown {
                // Source form: `![alt](url)` — the alt is the real text (it *is*
                // the plain bytes), the delimiters are virtual.
                push_source_wrap(s, i, |s| {
                    s.push_str(&format!(
                        "<span data-img=\"{}\" alt=\"{}\" style=\"{IMG_STYLE}\">",
                        escape_attr(url),
                        escape_attr(alt)
                    ));
                    push_text_html(s, alt);
                    s.push_str("</span>");
                });
                return;
            }
            match crate::preview::safe_image_url(url, e.img_base).filter(|u| {
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
        Inline::Math { src, display } => {
            if shown {
                push_math_source(s, src, *display);
            } else {
                push_math(s, src, *display, e.fg, e.math_px);
            }
        }
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
            // Every line of a source block starts with its prefix again (`> ` for a
            // quote) — that is how the writer spells a multi-line block.
            push_marker(s, line, true);
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn push_container(
    s: &mut String,
    tag: &str,
    c: &[Inline],
    bid: usize,
    at: usize,
    line: &str,
    e: &Emit<'_>,
) {
    s.push_str(&format!("<{tag}>"));
    push_inlines(s, c, bid, at, line, e);
    s.push_str(&format!("</{tag}>"));
}

fn push_code_span(s: &mut String, c: &str) {
    s.push_str("<code style=\"");
    s.push_str(CODE_INLINE_STYLE);
    s.push_str("\">");
    push_text_html(s, c);
    s.push_str("</code>");
}

/// Emit `body` wrapped in the node's Markdown delimiters (the source form). The
/// delimiters come from [`crate::md::inline_delims`] — the **same** function the
/// Markdown writer uses, so what is shown is what a copy produces.
///
/// Delimiters carry `data-vmark` instead of being ordinary text: they occupy
/// layout bytes but no plain bytes (see `geom`'s virtual-byte ledger) — `Pos.off`
/// and every command riding on it stay untouched by this view.
fn push_source_wrap(s: &mut String, i: &Inline, body: impl FnOnce(&mut String)) {
    let Some((open, close)) = crate::md::inline_delims(i) else {
        body(s);
        return;
    };
    push_marker(s, &open, true);
    body(s);
    push_marker(s, &close, false);
}

/// One virtual Markdown delimiter (see [`push_source_wrap`]).
///
/// `open` marks the delimiter that *precedes* the content: a caret at the
/// content's first byte is drawn after it (`$|x$`), while at the last byte it
/// stays in front of the closing one (`$x|$`) — the pair reads as one monotonic
/// caret walk through the source instead of jumping over a delimiter.
fn push_marker(s: &mut String, text: &str, open: bool) {
    if text.is_empty() {
        return;
    }
    s.push_str(&format!(
        "<span data-vmark=\"{}\"{} style=\"{MARK_STYLE}\">",
        text.len(),
        if open { " data-vopen=\"1\"" } else { "" }
    ));
    push_text_html(s, text);
    s.push_str("</span>");
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

/// The **source** form of one formula (`$…$` / `$$…$$`), shown while the caret
/// sits in or next to it (see [`crate::reveal`]).
///
/// The LaTeX is a **real text node** with `data-math` — it holds exactly the
/// plain bytes `Pos.off` measures, so the caret can be placed inside it and
/// typing edits the source (`edit::inlines`). `data-math` keeps the one-element-
/// per-formula invariant geom's ordinal pairing relies on, and the extra
/// `data-math-src` tells the formula hit-test to stand aside: a click inside the
/// source must land on the exact character, not snap to the formula's edges.
///
/// The `$` delimiters are virtual markers (zero plain bytes). A whitespace-only
/// inline source gets none — `$ $` is not math (same rule as the Markdown
/// writer).
fn push_math_source(s: &mut String, src: &str, display: bool) {
    let inline = !display && !src.trim().is_empty();
    if display {
        push_marker(s, "$$", true);
    } else if inline {
        push_marker(s, "$", true);
    }
    s.push_str("<span data-math data-math-src style=\"");
    s.push_str(MATH_SRC_STYLE);
    s.push_str("\">");
    push_text_html(s, src);
    s.push_str("</span>");
    if display {
        push_marker(s, "$$", false);
    } else if inline {
        push_marker(s, "$", false);
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
/// A revealed Markdown delimiter (`**`, `*`, `~~`, `` ` ``, `[`, `](url)`, `$`).
/// Muted and monospaced so it reads as *notation*, not as content: the delimiter
/// is virtual (zero plain bytes, see `data-vmark` in geom), and it must never be
/// mistaken for text the caret can sit inside.
///
/// No color is declared — `opacity` dims whatever the theme's foreground is, so
/// one value works in light and dark themes alike (a fixed gray looked washed out
/// on one of them).
pub const MARK_STYLE: &str = "opacity:.55;font-family:monospace;font-size:88%;";
/// A table in source form: the `|` markers come from the cells themselves, so the
/// table reads as `| a | b |` instead of a grid.
///
/// The cells keep the **typeset** padding and border *width* (the border is
/// transparent, not absent): dropping them made every row 14px shorter, so
/// expanding a table pulled the whole document above/below it up by two rows — the
/// pointer then pointed at a different line mid-drag (the table flickered) and the
/// selection end jumped. Same box, different paint.
///
/// The **outer** box is kept too: the block's own `margin` (the typeset table's
/// 16px bottom gap) stays, or everything below the table would have moved up the
/// moment the sweep reached it — the height floors live on the cells and cannot
/// cover a margin.
pub const TABLE_SRC_STYLE: &str = "border-collapse:collapse;border:none;margin:0 0 16px;";
pub const TD_SRC_STYLE: &str = "border:1px solid transparent;padding:6px 13px;\
     text-align:left;line-height:1.5;vertical-align:top;font-weight:400;\
     background:var(--wys-tint, rgba(74,125,255,.07));";

/// The delimiter row of a source table: same columns, but one tight line. It is pure
/// notation, and every pixel it takes pushes the rest of the document down.
pub const TD_SRC_DELIM_STYLE: &str = "border:1px solid transparent;padding:0 13px;\
     text-align:left;line-height:1.1;vertical-align:top;background:var(--wys-tint, rgba(74,125,255,.07));";

/// The tint a block gets while it is expanded to source: the text selection only
/// covers the swept characters, so without it the reader cannot tell which *lines*
/// the sweep turned into source (the whole line is, including the parts past the
/// selection).
///
/// The host sets `--wys-tint` from the theme's accent (a 10% wash); the fallback is
/// the light theme's blue, so the editor's demo build looks right without a host.
pub const REVEAL_TINT: &str = "background:var(--wys-tint, rgba(74,125,255,.07));";

/// Per-block **height floor** while a block is expanded to source: the typeset
/// height measured right before the expansion (`Geom::block_heights`, taken by the
/// pointer handler). A block whose source is *shorter* than its typeset form — a
/// table, a tall formula — would otherwise collapse and pull the rest of the
/// document up, so the line under the pointer changed mid-drag. The floor keeps the
/// box; the leftover space simply stays blank.
///
/// Empty when nothing is being expanded (or before the first measurement).
static HEIGHT_FLOORS: OnceLock<RwLock<Vec<f32>>> = OnceLock::new();

fn height_floors() -> &'static RwLock<Vec<f32>> {
    HEIGHT_FLOORS.get_or_init(|| RwLock::new(Vec::new()))
}

/// Install the measured typeset heights (indexed by block number). Called by the
/// pointer handler on press, before the selection collapses and the expansion
/// changes the layout.
pub fn set_height_floors(floors: Vec<f32>) {
    if let Ok(mut w) = height_floors().write() {
        *w = floors;
    }
}

/// Drop the floors (nothing is expanded any more).
pub fn clear_height_floors() {
    if let Ok(mut w) = height_floors().write() {
        w.clear();
    }
}

/// `min-height` for a block that is being expanded, when its typeset height is known.
fn floor_css(bid: usize) -> String {
    let Ok(r) = height_floors().read() else {
        return String::new();
    };
    match r.get(bid) {
        Some(h) if *h > 0.0 => format!("min-height:{h}px;"),
        _ => String::new(),
    }
}

/// A block carrier's style, plus the "this line is shown as source" tint and (when
/// measured) the height floor.
fn block_style(bid: usize, base: &str, e: &Emit<'_>) -> String {
    if e.reveal.contains(bid) {
        // The tint goes **first**: a block that already paints its own background
        // (code / HTML: the gray that says "this is source anyway") keeps it — a
        // late `background` declaration would silently replace it and a revealed
        // fence would lose the code look for as long as the sweep lasts.
        format!("{REVEAL_TINT}{base}{}", floor_css(bid))
    } else {
        // No tint (only one atom is expanded, not the line), but the **box** can
        // still change: a typeset formula is not the same height as its LaTeX, so
        // the block keeps its measured typeset height while the source is on
        // screen — otherwise arrowing onto a tall formula made the rest of the
        // document jump on every keystroke.
        format!("{base}{}", reveal_floor(bid, e))
    }
}

/// The height floor a block needs because its **source is on screen**: the whole
/// block (tinted) or the single atom at the caret. Empty when neither, or before
/// the first measurement — see `REVEAL_TINT`'s note on `HEIGHT_FLOORS`.
fn reveal_floor(bid: usize, e: &Emit<'_>) -> String {
    if e.reveal.contains(bid) || e.reveal.atom_block() == Some(bid) {
        floor_css(bid)
    } else {
        String::new()
    }
}
/// A rule in source form: just the `---` line.
///
/// The vertical **margins are the typeset ones** (the rule is a 2px bar with 24px
/// above and below): a source line is taller than the bar, so the block grows —
/// which is the documented direction (`---` counts as "the source needs more
/// lines") — but shrinking the margins to `8px` made the block *collapse* by
/// 10px and pulled the text under the sweep up with it.
pub const HR_SOURCE_STYLE: &str = "margin:24px 0;min-height:1.5em;line-height:1.5;";
/// A formula's LaTeX source while it is expanded for editing: monospaced on a
/// faint background, the same visual language as inline code — you are looking
/// at (and editing) the source, not the typeset result.
pub const MATH_SRC_STYLE: &str = "font-family:monospace;font-size:88%;\
     background:rgba(128,128,128,.12);border-radius:3px;";
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
/* `user-select: none` switches blitz's **native drag selection** off: the widget owns the drag
   (see `ui.rs` onpointermove), which is what lets the source expansion change *while* the
   selection grows. `cursor: text` keeps the I-beam (blitz maps `user-select: none` to the
   default cursor, but a non-auto CSS cursor wins). */
.wys-content, .wys-content * { user-select: none; }
.wys-content { position: relative; padding: 16px 20px 40vh; color: var(--wys-fg, #1a1a1a); font-size: 16px; line-height: 1.5; white-space: pre-wrap; cursor: text; }
.wys-content a { cursor: pointer; }
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
        let rv = Reveal::none();
        let e = Emit {
            fg: "#000",
            img_base: None,
            math_px: 14.0,
            reveal: &rv,
        };
        let td = Block::Td {
            table_seq: 0,
            row: 0,
            align: 0,
            inlines: vec![Inline::Text("单元格".into())],
        };
        let mut html = String::new();
        push_block(&mut html, 0, &td, &e);
        assert!(html.contains("单元格"), "{html}");

        let li = Block::Li {
            ordered: false,
            num: 0,
            level: 0,
            task: None,
            inlines: vec![Inline::Text("条目".into())],
        };
        let mut html = String::new();
        push_block(&mut html, 1, &li, &e);
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

    /// The text stream **excluding** the source-expansion delimiters. Dropping
    /// them must leave exactly `plain_text()`: the markers are virtual (zero plain
    /// bytes), which is what keeps `Pos.off` and every command valid.
    fn text_without_markers(html: &str) -> String {
        let mut out = String::new();
        let mut rest = html;
        while let Some(p) = rest.find("<span data-vmark=") {
            out.push_str(&strip_tags(&rest[..p]));
            let after = &rest[p..];
            let close = after.find("</span>").map_or(after.len(), |i| i + 7);
            rest = &after[close..];
        }
        out.push_str(&strip_tags(rest));
        out
    }

    /// Select everything and render with the expansion applied. The source must hold
    /// **two blocks or more**: a sweep has to cross a line to expand anything.
    fn revealed_html(src: &str) -> (String, usize) {
        use crate::model::{Pos, Sel};
        let doc = md::parse(src);
        assert!(doc.blocks.len() >= 2, "the source must have two blocks");
        let mut e = crate::edit::Editor::new(doc.clone());
        let last = doc.blocks.len() - 1;
        let end = doc.blocks[last].plain_text().len();
        e.sel = Sel {
            anchor: Pos::new(0, 0),
            focus: Pos::new(last, end),
        };
        let html = region_fragments_revealed(&doc, DEFAULT_FG, None, 16.0, &e.reveal()).concat();
        let plain: String = doc.blocks.iter().map(|b| b.plain_text()).collect();
        (html, plain.len())
    }

    /// The reveal is looked up **by block number**, and a selection expands the
    /// **whole** block (the unit is the block, never a single inline):
    /// a wrong bid would expand a neighbour instead, which is silent and
    /// confusing, so the grouped renderers are pinned here.
    #[test]
    fn reveal_expands_whole_blocks_only() {
        use crate::model::{Pos, Sel};
        let frags = |src: &str, a: (usize, usize), b: (usize, usize)| {
            let doc = md::parse(src);
            let mut e = crate::edit::Editor::new(doc.clone());
            e.sel = Sel {
                anchor: Pos::new(a.0, a.1),
                focus: Pos::new(b.0, b.1),
            };
            region_fragments_revealed(&doc, DEFAULT_FG, None, 16.0, &e.reveal())
        };
        let two = "**a**\n\n**b**\n";
        // One block selected: nothing expands (that is a copy gesture, not a source
        // view).
        let f = frags(two, (0, 0), (0, 1));
        assert!(f.iter().all(|s| !s.contains("data-vmark")), "{f:?}");
        // Two blocks: both expand, each one whole.
        let f = frags(two, (0, 0), (1, 1));
        assert_eq!(f.len(), 2);
        assert_eq!(f[0].matches("data-vmark").count(), 2, "the first block");
        assert_eq!(f[1].matches("data-vmark").count(), 2, "the second one");
        // A partial sweep still expands a whole line: "b" of the first line is enough
        // for everything that line carries.
        let line = "**a** *b* `c` d\n\n尾\n";
        let f = frags(line, (0, 4), (1, 1));
        assert_eq!(f[0].matches("data-vmark").count(), 6, "{}", f[0]);
        // A sweep with no markup in it expands nothing there.
        let f = frags("plain\n\n**b**\n", (0, 0), (1, 1));
        assert!(!f[0].contains("data-vmark"), "nothing to show: {f:?}");
        assert!(f[1].contains("data-vmark"), "the marked block does: {f:?}");

        // List item / quote / table: one fragment each, and the touched one expands
        // its block prefix as well.
        let grouped = "- **item**\n\n> **quote**\n\n| **h** | p |\n|---|---|\n| $y$ | z |\n";
        let doc = md::parse(grouped);
        assert!(
            matches!(doc.blocks[0], Block::Li { .. }),
            "{:?}",
            doc.blocks
        );
        assert!(matches!(doc.blocks[1], Block::Quote(_)));
        assert!(doc.blocks[2].is_td() && doc.blocks[4].is_td());

        // The list item + the quote: `**` ×2 plus the item's own marker each.
        let f = frags(grouped, (0, 0), (1, 1));
        assert_eq!(f.len(), 3, "list / quote / table");
        assert_eq!(f[0].matches("data-vmark").count(), 3, "list item");
        assert!(f[0].contains(">\u{2d} </span>"), "the item marker");
        assert_eq!(f[1].matches("data-vmark").count(), 3, "quote");
        assert!(
            !f[2].contains("data-vmark"),
            "the table is outside the sweep"
        );
        // A sweep across two cells: the table shows its pipes + delimiter row and
        // keeps one `data-math-src` per formula.
        let f = frags(grouped, (3, 0), (4, 1));
        assert_eq!(f[2].matches("data-math-src").count(), 1, "{}", f[2]);
        assert_eq!(f[2].matches("data-bid=").count(), 4, "every cell: {}", f[2]);
        assert!(f[2].contains("---"), "the delimiter row: {}", f[2]);
        assert!(!f[0].contains("data-vmark") && !f[1].contains("data-vmark"));
    }

    /// A table inside a quote shows the `> ` shell on **every row** — the copy
    /// produces `> | a | b |`, so the source view must too (the shell used to be
    /// dropped for tables, the one block kind whose prefix lives per line).
    #[test]
    fn revealed_quoted_table_keeps_its_quote_prefix() {
        use crate::model::{Pos, Sel};
        let src = "> | a | b |\n> |---|---|\n> | c | d |\n";
        let doc = md::parse(src);
        let n = doc.blocks.len();
        assert_eq!(n, 4, "four cells: {:?}", doc.blocks);
        let mut e = crate::edit::Editor::new(doc.clone());
        e.sel = Sel {
            anchor: Pos::new(0, 0),
            focus: Pos::new(n - 1, doc.blocks[n - 1].plain_len()),
        };
        let html = region_fragments_revealed(&doc, DEFAULT_FG, None, 16.0, &e.reveal()).concat();
        // Header + delimiter + body row: one lead each.
        assert_eq!(html.matches("&gt; ").count(), 3, "{html}");
        assert_eq!(text_without_markers(&html), "abcd", "{html}");
    }

    /// Every block kind shows its own Markdown prefix, and — the invariant the
    /// whole design rests on — the text stream minus the markers is still exactly
    /// the block's `plain_text`.
    #[test]
    fn revealed_blocks_show_their_prefix() {
        use crate::model::{Pos, Sel};
        let one = |src: &str| {
            // A sweep has to cross a line, so every sample gets a trailing block.
            let src = format!("{src}\n\n尾\n");
            let doc = md::parse(&src);
            let mut e = crate::edit::Editor::new(doc.clone());
            let total: usize = doc.blocks.iter().map(|b| b.plain_len()).sum();
            // Select everything: every block expands.
            let last = doc.blocks.len() - 1;
            e.sel = Sel {
                anchor: Pos::new(0, 0),
                focus: Pos::new(last, doc.blocks[last].plain_len()),
            };
            assert!(total > 0 || doc.blocks.len() == 1);
            let html =
                region_fragments_revealed(&doc, DEFAULT_FG, None, 16.0, &e.reveal()).concat();
            let plain: String = doc.blocks.iter().map(|b| b.plain_text()).collect();
            assert_eq!(
                text_without_markers(&html),
                plain,
                "the delimiters are virtual: {html}"
            );
            html
        };
        assert!(one("# 标题 与 **粗**").contains("# "));
        assert!(one("## 二级").contains("## "));
        assert!(one("- 项目").contains("- "));
        assert!(one("1. 第一").contains("1. "));
        assert!(one("- [ ] 待办").contains("- [ ] "));
        assert!(one("- [x] 完成").contains("- [x] "));
        assert!(one("> 引用").contains("&gt; "), "the quote marker");
        // A multi-line quote repeats the prefix on every line (that is what the
        // writer produces), and the in-block break stays a single real byte.
        let quoted = one("> 一\n> 二");
        assert_eq!(quoted.matches("&gt; ").count(), 2, "{quoted}");
        // A multi-line **HTML block** inside a quote repeats the shell too (the
        // writer prefixes every line); it used to appear on the first line only.
        let html = one("> <div>\n> abc");
        assert_eq!(html.matches("&gt; ").count(), 2, "{html}");
        // A code block / rule inside a quote keeps shell **and** fence: the region
        // path used to hand the renderer the shell-stripped core, so the source
        // form lost every `> ` at once.
        let qcode = one("> ```rust\n> fn x() {}\n> ```");
        // One `> ` on the opening line and one inside the closing fence's
        // marker (the single code line needs no repeat); the multi-line count
        // is covered by the HTML block above.
        assert_eq!(qcode.matches("&gt; ").count(), 2, "{qcode}");
        assert!(qcode.contains("```rust"), "{qcode}");
        // The quoted rule: the shell and the `---` are two virtual markers. The
        // rule sits **between** two contributing blocks — a zero-byte rule as the
        // selection's first/last block is not part of the sweep (its plain text
        // contributes nothing), an interior one always is.
        let qhr = one("> x\n\n> ---");
        assert!(
            qhr.contains("&gt; </span>") && qhr.contains(">---</span>"),
            "the quoted rule: {qhr}"
        );
        // A list item's continuation line is indented, not re-marked: the `- ` must
        // appear exactly once.
        let item = one("- 一\n  二");
        assert_eq!(item.matches(">- </span>").count(), 1, "{item}");
        // A rule carries no text of its own, so it can only be selected as part of a
        // range passing through it — then its `---` belongs to the selected source.
        assert!(one("a\n\n---\n\nb").contains("---"), "the rule's `---`");
        let code = one("```rust\nfn x() {}\n```");
        assert!(code.contains("```rust") && code.contains("\n```"), "{code}");
        // A revealed fence keeps the code block's own background: the tint is emitted
        // *before* the base style, so the later `background` shorthand wins (appending
        // it replaced the gray and the fence flashed to the wash colour).
        let tint = code.find("--wys-tint").unwrap_or(usize::MAX);
        let gray = code.find("background:rgba(128,128,128,.14)").unwrap_or(0);
        assert!(
            tint < gray && gray > 0,
            "the code background must come last: {code}"
        );
        // A formula inside a block that is expanded shows its source too (the whole
        // line is source, so no `data-vlen` atom is left).
        let math = one("前 $x+y$ 后");
        assert!(math.contains("data-math-src") && !math.contains("data-vlen"));
        // A tilde fence for a language that contains a backtick (the writer's rule).
        let tilde = one("~~~a`b\ncode\n~~~");
        assert!(tilde.contains("~~~a`b"), "{tilde}");
    }

    /// A selection shows the Markdown its copy carries: `**bold**`, `` `code` ``,
    /// `[l](u)` and `$x$` all emit their delimiters — as `data-vmark` spans, so the
    /// text stream still matches `plain_text()` byte for byte.
    #[test]
    fn revealed_selection_emits_markdown_delimiters() {
        let (html, plain_len) =
            revealed_html("**bold** and `code` and [l](https://u) and $x$\n\nsecond");
        assert_eq!(
            html.matches("data-vmark=\"2\"").count(),
            2,
            "`**` twice: {html}"
        );
        // One `` ` `` on each side of the code span, one `$` on each side of the
        // formula, and the link's `[` — five one-byte markers.
        assert_eq!(html.matches("data-vmark=\"1\"").count(), 5, "{html}");
        assert_eq!(
            html.matches("data-vmark=\"12\"").count(),
            1,
            "the link's `](https://u)` closer: {html}"
        );
        assert!(html.contains("data-vopen=\"1\""), "{html}");
        assert!(html.contains("](https://u)"), "{html}");
        // The formatted content keeps its tags: the markers annotate it, they do
        // not flatten it.
        assert!(html.contains("<strong>bold</strong>"), "{html}");
        assert!(html.contains("<code"), "{html}");
        // Virtual: the plain text stream is untouched by the delimiters.
        assert_eq!(
            text_without_markers(&html),
            "bold and code and l and xsecond",
            "{html}"
        );
        assert_eq!(text_without_markers(&html).len(), plain_len);
        // Without a selection nothing expands.
        let doc = md::parse("**bold** and `code`");
        let plain_html =
            region_fragments_revealed(&doc, DEFAULT_FG, None, 16.0, &Reveal::none()).concat();
        assert!(!plain_html.contains("data-vmark"), "{plain_html}");
        assert_eq!(plain_html, content_html(&doc));
    }

    /// A formula at the caret renders its **LaTeX source** instead of the SVG: the
    /// source is real text (so the plain ledger matches and the caret can go inside
    /// it), the `$` are virtual, and exactly one element still carries `data-math`
    /// (geom pairs formulas with `math_spans()` by ordinal).
    #[test]
    fn revealed_formula_emits_its_source() {
        use crate::model::{Pos, Sel};
        let doc = md::parse("a $x+y$ b");
        let mut e = crate::edit::Editor::new(doc.clone());
        // "a " = 0..2, formula = 2..5, " b" = 5..7 → the caret at the right edge.
        e.sel = Sel::caret(Pos::new(0, 5));
        let rv = e.reveal();
        assert_eq!(rv.editing_span(0), Some((2, 5)));
        let html = region_fragments_revealed(&doc, DEFAULT_FG, None, 16.0, &rv).concat();
        assert!(html.contains("data-math-src"), "{html}");
        assert!(!html.contains("<svg"), "the SVG must be replaced: {html}");
        assert_eq!(
            html.matches("data-math ").count(),
            1,
            "one carrier per formula (geom's ordinal pairing): {html}"
        );
        assert!(
            !html.contains("data-vlen"),
            "the source is real text: {html}"
        );
        assert!(html.contains("x+y"), "{html}");
        assert_eq!(text_without_markers(&html), "a x+y b", "{html}");
        // Moving the caret away collapses it back to the typeset form.
        e.sel = Sel::caret(Pos::new(0, 0));
        let html = region_fragments_revealed(&doc, DEFAULT_FG, None, 16.0, &e.reveal()).concat();
        assert!(!html.contains("data-math-src"), "{html}");
    }

    /// A display formula revealed inside its own paragraph keeps the block shell
    /// and shows `$$…$$` around the source.
    #[test]
    fn revealed_display_formula_keeps_its_block() {
        use crate::model::{Pos, Sel};
        let doc = md::parse("$$\nx+y\n$$");
        let mut e = crate::edit::Editor::new(doc.clone());
        e.sel = Sel::caret(Pos::new(0, 2));
        let html = region_fragments_revealed(&doc, DEFAULT_FG, None, 16.0, &e.reveal()).concat();
        assert!(html.contains("data-math-src"), "{html}");
        assert_eq!(html.matches("data-vmark=\"2\"").count(), 2, "{html}");
        assert_eq!(text_without_markers(&html), doc.blocks[0].plain_text());
    }

    /// The **box**, not only the paint: a formula expanded to its LaTeX is not the
    /// same height as the typeset SVG, so its block keeps the height measured
    /// before the expansion (`HEIGHT_FLOORS`, installed by the press handler and by
    /// `apply` when the expansion starts). Without the floor, arrowing onto a tall
    /// display formula pulled everything below it up on that keystroke and pushed
    /// it back on the next one.
    #[test]
    fn a_shown_formula_keeps_its_typeset_height() {
        use crate::model::{Pos, Sel};
        let doc = md::parse("$$\nx+y\n$$\n\n尾");
        assert_eq!(doc.blocks.len(), 2, "{:?}", doc.blocks);
        let mut e = crate::edit::Editor::new(doc.clone());
        set_height_floors(vec![120.0, 0.0]);
        // The caret sits in the formula: only the atom is expanded, the block is not.
        e.sel = Sel::caret(Pos::new(0, 1));
        assert!(e.reveal().blocks().is_empty());
        let html = region_fragments_revealed(&doc, DEFAULT_FG, None, 16.0, &e.reveal()).concat();
        assert!(html.contains("data-math-src"), "{html}");
        assert!(html.contains("min-height:120px"), "{html}");
        // Away from it the typeset form needs none: its own height is the floor.
        e.sel = Sel::caret(Pos::new(1, 0));
        let html = region_fragments_revealed(&doc, DEFAULT_FG, None, 16.0, &e.reveal()).concat();
        assert!(!html.contains("min-height:120px"), "{html}");
        clear_height_floors();
    }

    /// Expanding a table must not move the document below it: the source form is
    /// still the same `<table>` block (transparent borders, same cell padding), so
    /// it keeps the typeset block's outer margin too — the height floors live on
    /// the cells and cannot cover a lost 16px margin.
    #[test]
    fn source_table_keeps_the_typeset_outer_box() {
        for decl in ["border-collapse:collapse;", "margin:0 0 16px;"] {
            assert!(TABLE_STYLE.contains(decl), "{decl}: {TABLE_STYLE}");
            assert!(TABLE_SRC_STYLE.contains(decl), "{decl}: {TABLE_SRC_STYLE}");
        }
    }
}
