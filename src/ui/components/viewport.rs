//! Editor viewport: virtualized line rendering with syntax highlighting,
//! search-match highlighting, selection, caret, mouse hit testing and scrolling.
//!
//! Geometry is expressed in explicit pixels derived from the window client
//! size — the Blitz layout engine does not resolve `bottom:` insets or
//! `height:100%` inside an auto-height container, which would otherwise
//! collapse the whole editing area. All styles are pre-computed strings
//! because `rsx!` cannot bind arbitrary values as element attributes.
//!
//! Keyboard input is *not* handled here: the single composer
//! ([`crate::ui::components::composer::Composer`]) owns DOM focus and is parked
//! over the caret.

use std::borrow::Cow;
use std::sync::OnceLock;

use dioxus_native::prelude::*;

use crate::highlight::{lexer::lex_line, Theme, TokenKind};
use crate::jsonl::Tok;
use crate::search::Match;
use crate::ui::design::FONT_MONO;
use crate::ui::state::jsonl::RowRef;
use crate::ui::state::{CaretStyle, EditorState, FindFocus, MARKER_W, PAD_LEFT, PAD_TOP, TAB_SIZE};
use crate::ui::Fmt;

/// Lines moved per wheel notch (`WheelDelta::Lines`).
const WHEEL_LINES: f32 = 3.0;

/// Lines longer than this are rendered without per-token splitting.
const MAX_HL_LINE: usize = 4096;

/// One rendered segment. `style` is the parsed final style string: the vast majority of segments
/// borrow a shared prebuilt stylesheet ([`Styles`], zero allocation); only segments hit by a find
/// match or a text mark build their own merged style (`Cow::Owned`). Each segment used to run one
/// `format!` — thousands of segments meant thousands of allocations, one of the repeat hotspots in
/// building a viewport at the ten-thousand-line scale.
struct Seg {
    text: String,
    style: Cow<'static, str>,
}

impl Seg {
    /// Dimmed rendering of a whitespace run (`Show whitespace`).
    ///
    /// Splitting the run into its own segment is what keeps the caret honest:
    /// the glyphs are the same width as the characters they replace, and the
    /// caret's x is computed from the *real* line text, so nothing moves.
    fn is_ws(text: &str) -> bool {
        !text.is_empty() && text.bytes().all(|b| b == b' ' || b == b'\t')
    }

    fn glyph(text: &str) -> String {
        text.chars()
            .map(|c| match c {
                '\t' => '\u{2192}', // →
                _ => '\u{00B7}',    // ·
            })
            .collect()
    }
}

struct RowView {
    /// DOM key: document rows are `d{ln}`, virtual rows `v{line}_{sub}`
    key: String,
    /// Gutter text: the line number for document rows, a dim `┆` for virtual rows
    num: String,
    number_style: String,
    row_style: String,
    /// Row-level class name (`jrow` = tree/preview virtual rows, hover highlight via the
    /// stylesheet; empty string for document rows)
    class: &'static str,
    segs: Vec<Seg>,
    /// `(glyph, x)` fold marker: drawn left of the opening bracket (x is the absolute offset from
    /// the viewport's left edge; absolutely positioned, moving with horizontal scroll). Color/hover
    /// state lives in the `.fold` class (`components::style`).
    marker: Option<(&'static str, f32)>,
}

struct ViewModel {
    rows: Vec<RowView>,
    /// Width of the line-number subcolumn (px): when the fold column sits right of the numbers,
    /// the numbers right-align inside `gutter - MARKER_W` alone, or they would collide with the
    /// fold button.
    num_w: f32,
    /// The viewport's left-edge client x (`view.x`). When the sidebar occupies a strip on the
    /// left the whole viewport shifts right and **must** be drawn from here: hit-testing, the
    /// scrollbar and the composer all convert with the same `view.x`, and a container that does
    /// not move with it produces "drawn on the left, clicked on the right" misalignment.
    vx: f32,
    content_top: f32,
    scroll_left: f32,
    caret_left: f32,
    caret_top: f32,
    caret_ok: bool,
    lh: f32,
    vw: f32,
    vh: f32,
    /// Viewport height minus the horizontal scrollbar: the clip height.
    text_h: f32,
    vtop: f32,
    gutter_w: f32,
    col_w: f32,
    /// Bracket-match highlight boxes `(x, y, w)` — one for the bracket under the caret and one
    /// for its partner
    brackets: Vec<(f32, f32, f32)>,
}

/// Whether highlighting should be produced for this document.
fn hl_enabled(spec: &'static crate::highlight::LangSpec, bytes: u64, forced: bool) -> bool {
    if spec.name == "Plain Text" {
        return false;
    }
    forced || bytes < crate::highlight::AUTO_DISABLE_BYTES
}

/// One highlighted/painted sub-range of a line: byte range + token class.
type Cover = Vec<(usize, usize, Option<TokenKind>)>;

/// Byte ranges of the search matches touching one line, plus `true` for the
/// current match.
type LineMatches = Vec<(usize, usize, bool)>;

/// Distribute the collected matches over the visible lines.
fn distribute_matches(
    doc: &crate::buffer::TextDocument,
    results: &[Match],
    cur: Option<usize>,
    first: u64,
    last: u64,
) -> Vec<LineMatches> {
    let n = (last.saturating_sub(first)) as usize;
    let mut per: Vec<LineMatches> = vec![Vec::new(); n];
    if results.is_empty() {
        return per;
    }
    let first_pos = doc.pos_of_line(first);
    let last_pos = if last >= doc.line_count() {
        doc.table.len()
    } else {
        doc.pos_of_line(last)
    };
    // results are sorted and non-overlapping -> binary search the entry point
    let mut i = results.partition_point(|m| m.end <= first_pos);
    while i < results.len() {
        let m = results[i];
        if m.start >= last_pos {
            break;
        }
        let l0 = doc.line_of_pos(m.start);
        let l1 = doc.line_of_pos(m.end.saturating_sub(1).max(m.start));
        let mut l = l0.max(first);
        while l <= l1.min(last.saturating_sub(1)) {
            let ls = doc.pos_of_line(l);
            let le = doc.line_span(l).end;
            let a = if l == l0 {
                m.start.saturating_sub(ls) as usize
            } else {
                0
            };
            let b = if l == l1 {
                (m.end.min(le)).saturating_sub(ls) as usize
            } else {
                (le.saturating_sub(ls)) as usize
            };
            if b > a && (l - first) < n as u64 {
                per[(l - first) as usize].push((a, b, cur == Some(i)));
            }
            l += 1;
        }
        i += 1;
    }
    per
}

/// `user-select` is *not* an inherited property in stylo, and blitz's drag
/// gate (`blitz-dom/src/events/pointer.rs`) only inspects the hit node and its
/// **immediate** parent. Setting `user-select:none` on the viewport container is
/// therefore useless — the hit node is a `<span>` two or three levels deeper and
/// still computes to `auto`, so blitz enters `DragMode::Selecting` and paints a
/// second, competing highlight in its own hardcoded colour (`#b4d5ff`). It must
/// be applied to every leaf that can be hit: the row `<div>` and each `<span>`.
const NO_SELECT: &str = "user-select:none;";

/// `cursor:text` on every text leaf, for the same reason as [`NO_SELECT`].
///
/// `Document::get_cursor()` resolves the pointer shape from the **hit node**'s
/// own computed `cursor`, and when that keyword is `auto` it falls back to
/// `UserSelect`: `UserSelect::None` on a text node yields `CursorIcon::Default`,
/// never `Text`. So the editor's `user-select:none` leaves — added above to stop
/// blitz's own selection from fighting ours — also cost us the I-beam, and
/// hovering over text showed an arrow instead of a caret.
///
/// Setting it on each leaf (rather than trusting inheritance on the container,
/// which stylo does not apply the way a browser would) makes the hit node
/// resolve to `CursorIcon::Text` directly, before the `user-select` fallback is
/// ever consulted. The gutter keeps `cursor:default`: an arrow
/// over the margin, and it is not a place text can be inserted.
const TEXT_CURSOR: &str = "cursor:text;";

/// Token style (color / weight / slant) for one token class.
fn token_style(t: &'static Theme, kind: Option<TokenKind>) -> String {
    let k = kind.unwrap_or(TokenKind::Plain);
    let mut s = format!("color:{};{NO_SELECT}{TEXT_CURSOR}", t.color(k));
    if t.bold(k) {
        s.push_str("font-weight:bold;");
    }
    if t.italic(k) {
        s.push_str("font-style:italic;");
    }
    s
}

/// `Tok` (parse-layer semantic class) → token slot. Colors vary by theme; this is only a mapping.
fn json_tok_slot(k: Tok) -> u16 {
    tok_slot(Some(match k {
        Tok::Key => TokenKind::AttrName,
        Tok::Str => TokenKind::String,
        Tok::Num => TokenKind::Number,
        Tok::Bool | Tok::Null => TokenKind::Keyword,
        Tok::Punct => TokenKind::Punct,
        Tok::Preview | Tok::Ellipsis => TokenKind::Comment,
    }))
}

/// Fold marker: `(glyph, x)`.
///
/// The shape is VS Code's chevron (`▾` expanded / `▸` collapsed, glyphs shipped with Consolas),
/// and the position is **fixed in the vertical slot right of the line numbers** (`x` supplied by
/// the caller = `gutter - MARKER_W`): the JSON tree, JSONL expanded blocks and JSONL document
/// rows share one column, aligning into a single vertical strip that neither moves right as
/// indentation deepens nor shifts with horizontal scrolling.
fn fold_marker(collapsed: bool, x: f32) -> (&'static str, f32) {
    let glyph = if collapsed { "\u{25B8}" } else { "\u{25BE}" };
    (glyph, x)
}

// ---- shared stylesheet -----------------------------------------------------
//
// A token segment's base style depends only on (theme, kind). Each segment used to run a
// `format!`; now "fixed class + column palette + special slot" is built once the first time a
// theme is used, and a segment only carries a slot index. There are only light/dark theme
// instances, so the table is cached globally once.

/// Number of fixed token-class slots (every `TokenKind` variant except `Col`).
const TOK_FIXED: usize = 30;
/// Number of column-palette slots (the length of `COL_*` in `theme.rs`; `Col(i)` takes
/// `i % 8`).
const COL_SLOTS: usize = 8;
const SLOT_SEL: usize = TOK_FIXED + COL_SLOTS;
const SLOT_SEL_INACTIVE: usize = SLOT_SEL + 1;
const SLOT_WS: usize = SLOT_SEL_INACTIVE + 1;
const SLOT_EOL: usize = SLOT_WS + 1;
const SLOT_JSON_ERR: usize = SLOT_EOL + 1;
const SLOT_COUNT: usize = SLOT_JSON_ERR + 1;

/// `tok_slot`'s numbering is exactly this array's indexing (a test keeps the two from drifting).
const TOK_FIXED_KINDS: [TokenKind; TOK_FIXED] = [
    TokenKind::Plain,
    TokenKind::Comment,
    TokenKind::DocComment,
    TokenKind::String,
    TokenKind::Char,
    TokenKind::Number,
    TokenKind::Keyword,
    TokenKind::Control,
    TokenKind::Type,
    TokenKind::Ident,
    TokenKind::Function,
    TokenKind::Operator,
    TokenKind::Punct,
    TokenKind::Preproc,
    TokenKind::Annotation,
    TokenKind::Tag,
    TokenKind::AttrName,
    TokenKind::AttrValue,
    TokenKind::Variable,
    TokenKind::Heading,
    TokenKind::Bold,
    TokenKind::Italic,
    TokenKind::Code,
    TokenKind::Link,
    TokenKind::Quote,
    TokenKind::ListMark,
    TokenKind::Rule,
    TokenKind::Added,
    TokenKind::Removed,
    TokenKind::Meta,
];

fn tok_slot(kind: Option<TokenKind>) -> u16 {
    use TokenKind::*;
    match kind.unwrap_or(Plain) {
        Plain => 0,
        Comment => 1,
        DocComment => 2,
        String => 3,
        Char => 4,
        Number => 5,
        Keyword => 6,
        Control => 7,
        Type => 8,
        Ident => 9,
        Function => 10,
        Operator => 11,
        Punct => 12,
        Preproc => 13,
        Annotation => 14,
        Tag => 15,
        AttrName => 16,
        AttrValue => 17,
        Variable => 18,
        Heading => 19,
        Bold => 20,
        Italic => 21,
        Code => 22,
        Link => 23,
        Quote => 24,
        ListMark => 25,
        Rule => 26,
        Added => 27,
        Removed => 28,
        Meta => 29,
        Col(i) => (TOK_FIXED + (i as usize % COL_SLOTS)) as u16,
    }
}

/// The prebuilt stylesheet (cached per theme, see [`styles_for`]).
struct Styles {
    all: Vec<String>,
}

impl Styles {
    fn build(theme: &'static Theme) -> Self {
        let cursor = |color: &str| format!("color:{color};{NO_SELECT}{TEXT_CURSOR}");
        let mut all: Vec<String> = Vec::with_capacity(SLOT_COUNT);
        for k in TOK_FIXED_KINDS {
            all.push(token_style(theme, Some(k)));
        }
        for i in 0..COL_SLOTS {
            all.push(token_style(theme, Some(TokenKind::Col(i as u8))));
        }
        // Byte-for-byte identical to the old `sel_style` (focused and unfocused variants)
        all.push(format!(
            "background:{}; color:{};{NO_SELECT}",
            theme.selection, theme.selection_fg
        ));
        all.push(format!(
            "background:{}; color:{};{NO_SELECT}",
            theme.sel_inactive, theme.sel_inactive_fg
        ));
        all.push(cursor(theme.gutter_fg)); // SLOT_WS
        all.push(cursor(theme.gutter_fg)); // SLOT_EOL
        all.push(cursor(theme.dirty_dot)); // SLOT_JSON_ERR
        debug_assert_eq!(all.len(), SLOT_COUNT);
        Styles { all }
    }

    fn slot(&'static self, i: u16) -> &'static str {
        &self.all[i as usize]
    }
}

fn styles_for(theme: &'static Theme) -> &'static Styles {
    static DARK: OnceLock<Styles> = OnceLock::new();
    static LIGHT: OnceLock<Styles> = OnceLock::new();
    if theme.dark {
        DARK.get_or_init(|| Styles::build(theme))
    } else {
        LIGHT.get_or_init(|| Styles::build(theme))
    }
}

/// Width, top offset and height of the caret box, in CSS px.
///
/// Pure so it can be tested without a live document: the three shapes differ
/// only in these numbers, and getting the underline's vertical offset wrong is
/// easy to miss by eye.
pub fn caret_geometry(style: CaretStyle, caret_w: f32, advance: f32, lh: f32) -> (f32, f32, f32) {
    match style {
        // a thin I-beam spanning the whole line box
        CaretStyle::Line => (caret_w.max(1.0), 0.0, lh),
        // a rectangle exactly one character wide
        CaretStyle::Block => (advance.max(1.0), 0.0, lh),
        // a 2px underline pinned to the bottom of the line box
        CaretStyle::Underline => (caret_w.max(1.0), (lh - 2.0).max(0.0), 2.0_f32.min(lh)),
    }
}

/// Build the view model: **pure reads**. All derived state (JSONL/JSON tree caches, hidden-row
/// maps, horizontal width scanning, caret clamping/fix-ups) is self-healed before rendering by
/// `state::heal`; this function only consumes — it takes `Signal::read`, not `write`, during
/// render, so the idle steady state produces no `mark_dirty` at all.
fn build(st: &EditorState) -> ViewModel {
    let lh = st.metrics.lh.max(1.0);
    let fs = st.font_size;
    let theme: &'static Theme = st.theme();
    let styles = styles_for(theme);
    let vw = st.view.w.max(200.0);
    // The scrollbars take space out of the viewport (see `ui::scrollbar`), so
    // both the clip height and the text column's width are the *text* area.
    let text_h = st.text_h().max(40.0);
    let vh = st.view.h.max(40.0);
    let vtop = st.view.y;

    let di = st.active;
    let sel = st.caret().selection();
    let caret_pos = st.caret().pos;
    let highlight_on = st.highlight_on;
    let all = st.highlight_all;
    let spec = st.spec_of(di);

    // ---- row space and selection/mark snapshots ------------------------------
    //
    // All taken before `&mut st.tabs[di]` is borrowed: the block below may not call any
    // `&self` method (tabs is mutably borrowed), while the screen-row map, marks and column
    // rectangles are all state-reading methods.
    let visual_rows = st.visual_rows();
    let marks_snap: Vec<crate::ui::state::Mark> = st.doc_marks().to_vec();
    // first/last are **visual row** coordinates (hidden rows are not in the row space; JSONL
    // virtual rows and soft-wrap sub-rows both are — `screen_of_visual` maps back to screen rows
    // first, then `jsonl_row_of` maps back to JSONL rows).
    let mut first_f = (st.scroll_top - PAD_TOP as f64) / lh as f64;
    if !first_f.is_finite() || first_f < 0.0 {
        first_f = 0.0;
    }
    let first = first_f.floor() as u64;
    let visible = ((text_h / lh).ceil() as u64).max(1) + 2;
    let last = (first + visible).min(visual_rows);
    let jsonl_refs: Vec<u64> = (first..last)
        .map(|r| {
            if st.wrap_enabled() {
                let (s, _) = st.screen_of_visual(r);
                st.jsonl_row_of(s)
            } else {
                st.jsonl_row_of(r)
            }
        })
        .collect();
    // The caret's visual row (a caret on a hidden row has already been moved by reveal_caret)
    let cur_vrow = st.visual_row_of_pos(caret_pos).0;

    let gutter_w = st.gutter_w();
    // Fold column: a vertical slot **right of** the line numbers (shared by the JSON tree and
    // JSONL). Every marker lands on the same x in this column, however deep the indentation —
    // VS Code's folding gutter is "one aligned column", so scanning does not require chasing the
    // tree shape to find the buttons.
    let fold_on = {
        let tab = &st.tabs[di];
        tab.json.enabled || tab.jsonl.enabled
    };
    let fold_x = gutter_w - MARKER_W;
    let num_w = if fold_on {
        gutter_w - MARKER_W
    } else {
        gutter_w
    };
    // the vertical scrollbar eats into the text column, not the gutter
    let col_w = (st.text_w()).max(50.0);

    let num_base = format!(
        "position:relative; height:{lh}px; line-height:{lh}px; font-family:{FONT_MONO}; font-size:{fs}px; \
         text-align:right; padding-right:8px; {NO_SELECT}"
    );
    let row_base = format!(
        "height:{lh}px; line-height:{lh}px; font-family:Consolas,'Cascadia Mono',monospace; \
         font-size:{fs}px; white-space:pre; padding-left:{PAD_LEFT}px; {NO_SELECT}{TEXT_CURSOR}"
    );
    // Row styles / line-number styles have only a handful of combinations: prebuild them once
    // per frame and make one short copy per row.
    let row_style_cur = format!("{row_base}background:{};", theme.cur_line);
    let row_style_plain = format!("{row_base}background:transparent;");
    let num_normal = format!("{num_base}color:{};", theme.gutter_fg);
    let num_cur = format!("{num_base}color:{};", theme.gutter_cur_fg);
    let num_marked = format!("{num_base}color:{};font-weight:bold;", theme.bookmark);

    // The selection is dimmed when the document does not own the keyboard
    // (a find box is focused), matching the greyed-out selection.
    let sel_slot: u16 = if st.doc_focused {
        SLOT_SEL as u16
    } else {
        SLOT_SEL_INACTIVE as u16
    };
    let match_bg = theme.match_hi_dim;
    let cur_bg = theme.match_hi;

    let mut rows: Vec<RowView> = Vec::new();
    let mut brackets: Vec<(f32, f32, f32)> = Vec::new();
    let (content_top, mut caret_ok, caret_left, caret_top);

    {
        let pair = st.match_bracket();
        let tab = &st.tabs[di];
        let doc = &tab.doc;
        // Whole-document JSON tree view: when on, the entire document parses into a collapsible
        // tree that directly replaces the textbox
        let json_preview = tab.json.enabled;
        // JSONL preview: the only standing per-frame cost = 2 u64 comparisons in refresh()
        // (cache sync happens before render in `state::heal`; this only consumes)
        let jsonl_on = tab.jsonl.enabled;
        let jv_open = !tab.jsonl.open.is_empty();
        // The document-line range covered by the visible interval (for matches / highlight
        // state); taken from the screen-row map rather than using first/last as line numbers, or
        // hidden line numbers would extend the range (the same computation is shared with the
        // self-heal probe)
        let (doc_first, doc_last) = st.visible_doc_range();
        // Column selections only take visible document lines: a whole rectangle can span hundreds
        // of thousands of lines, and reading every line's text each frame slows down linearly
        // with the selection's length (especially while scrolling).
        let col_rect: std::collections::HashMap<u64, (u64, u64)> = st
            .col_rect_range(doc_first, doc_last)
            .map(|v| v.into_iter().map(|(l, a, b)| (l, (a, b))).collect())
            .unwrap_or_default();
        let cur_line = doc.line_of_pos(caret_pos);
        let hl_on = highlight_on && hl_enabled(spec, doc.total_bytes(), all);

        // matches collected by Find All are painted until the doc changes
        let fresh = st.find.valid_for(doc.id, doc.gen);
        let per_line = if fresh {
            distribute_matches(doc, &st.find.results, st.find.cur, doc_first, doc_last)
        } else {
            vec![Vec::new(); (doc_last.saturating_sub(doc_first)) as usize]
        };

        let show_ws = st.show_ws;
        let show_eol = st.show_eol;

        // Generation advance + per-line invalidation: an edit voids only the checkpoints after
        // the first affected line (take() consumes and resets; with no edit the gen is unchanged
        // and sync_gen is a no-op)
        let edit_from = doc.take_edit_from_line();
        tab.hl.sync_gen(doc.gen, edit_from);

        let mut hl_state = if hl_on {
            tab.hl.state_at(doc_first, doc.gen, &|l| doc.line_text(l))
        } else {
            crate::highlight::LexState::normal()
        };

        let mut toks: Vec<crate::highlight::Token> = Vec::new();
        let mut cover: Cover = Vec::new();
        // Soft-wrapped visual rows share one document line's token cover: the lexer state may
        // advance only once per line, and continuation rows reuse it — otherwise the same line
        // would be lexed as several lines.
        let mut saved_cover: Cover = Vec::new();
        let mut saved_ln = u64::MAX;

        let marks = st.bookmarks.get(&doc.id);
        // Each visual row: `(RowRef, the soft-wrap sub-row within that screen row)`. When off,
        // the sub-row is always 0, byte-identical to the old behavior; when on,
        // `screen_of_visual` folds visual rows back to screen rows.
        let base: Vec<RowRef> = if json_preview {
            // Whole-document JSON tree view: every screen row is a virtual row (sub = the tree's
            // expanded row number)
            (first..last)
                .map(|r| RowRef::Virtual {
                    line: u64::MAX,
                    sub: r as u32,
                })
                .collect()
        } else if jv_open {
            // Sequential cursor: `row_at` rescans from the first expanded span each time, making
            // a full screen O(visible rows × expansions). `cursor_at` seeks once, then every row
            // is O(1).
            let mut cur = tab
                .jsonl
                .cursor_at(jsonl_refs.first().copied().unwrap_or(0));
            let mut out = Vec::with_capacity(jsonl_refs.len());
            for &r in &jsonl_refs {
                while cur.row() < r {
                    cur.advance();
                }
                out.push(cur.get());
            }
            out
        } else {
            jsonl_refs.iter().map(|&r| RowRef::Doc(r)).collect()
        };
        let subs: Vec<u64> = if st.wrap_enabled() {
            (first..last).map(|v| st.screen_of_visual(v).1).collect()
        } else {
            vec![0; (last.saturating_sub(first)) as usize]
        };
        let refs: Vec<(RowRef, u64)> = base.into_iter().zip(subs).collect();
        for (rr, sub) in refs.into_iter() {
            let RowRef::Doc(ln) = rr else {
                // ---- virtual rows: whole-document JSON tree or JSONL expanded block ----
                // The outer else already excluded Doc, so this must be Virtual; use `continue` as
                // the fallback instead of `unreachable!()` — an unreachable branch must not panic.
                let RowRef::Virtual { line, sub } = rr else {
                    continue;
                };
                if json_preview {
                    // Whole-document JSON tree: virtual rows come from tab.json.rows() and share
                    // the JSONL virtual rows' rendering (colored tokens + fold markers).
                    let r = &tab.json.rows()[sub as usize];
                    let ind = (PAD_LEFT - 2.0) + r.depth as f32 * st.metrics.adv * 2.0;
                    let segs = r
                        .segs
                        .iter()
                        .map(|(t, s)| Seg {
                            text: s.clone(),
                            style: Cow::Borrowed(styles.slot(json_tok_slot(*t))),
                        })
                        .collect();
                    let marker = if r.foldable {
                        // The fold button sits in the fixed column right of the numbers (same
                        // column as JSONL)
                        Some(fold_marker(r.collapsed, fold_x))
                    } else {
                        None
                    };
                    rows.push(RowView {
                        key: format!("p{sub}"),
                        // The gutter draws a dim ┆ to show this is a derived row, not a document
                        // row
                        num: "\u{2506}".to_string(),
                        number_style: num_normal.clone(),
                        // The background is left to the stylesheet (`.jrow:hover`) — an inline
                        // background would out-specify it
                        row_style: format!("{row_base}padding-left:{ind:.1}px;"),
                        class: "jrow",
                        segs,
                        marker,
                    });
                    continue;
                }
                let e = &tab.jsonl.open[&line];
                let indent = PAD_LEFT - 2.0; // border-left takes 2px
                let (num, marker, row_style, segs) = if let Some(m) = &e.err {
                    // Parse failure: one amber ⚠ line (the expanded block remains and can be
                    // clicked closed again)
                    (
                        "\u{2506}".to_string(), // ┆
                        None,
                        format!(
                            "{row_base}background:{};border-left:2px solid {};",
                            theme.panel_bg, theme.dirty_dot
                        ),
                        vec![Seg {
                            text: st.tr(Fmt::JsonlLineInvalid { err: m.clone() }).into_owned(),
                            style: Cow::Borrowed(styles.slot(SLOT_JSON_ERR as u16)),
                        }],
                    )
                } else {
                    let row = &e.rows[sub as usize];
                    let ind = indent + row.depth as f32 * st.metrics.adv * 2.0;
                    let segs = row
                        .segs
                        .iter()
                        .map(|(t, s)| Seg {
                            text: s.clone(),
                            style: Cow::Borrowed(styles.slot(json_tok_slot(*t))),
                        })
                        .collect();
                    let marker = if row.foldable {
                        Some(fold_marker(row.collapsed, fold_x))
                    } else {
                        None
                    };
                    (
                        "\u{2506}".to_string(), // ┆
                        marker,
                        format!(
                            "{row_base}background:{};border-left:2px solid {};padding-left:{ind:.1}px;",
                            theme.panel_bg, theme.accent
                        ),
                        segs,
                    )
                };
                rows.push(RowView {
                    key: format!("v{line}_{sub}"),
                    num,
                    number_style: num_normal.clone(),
                    row_style,
                    class: "jrow",
                    segs,
                    marker,
                });
                continue;
            };
            let text = doc.line_text(ln);
            let span = doc.line_span(ln);
            // Soft-wrap sub-row `sub`: draw only the [ra, rb) slice. When wrapping is off this is
            // always the whole line and the code below matches the old behavior.
            let (ra, rb) = if st.wrap_enabled() {
                let (a, b) = st.wrap_index.row_range(ln, sub, text.len());
                (a.min(text.len()), b.min(text.len()))
            } else {
                (0, text.len())
            };
            let sub_rows = if st.wrap_enabled() {
                st.wrap_index.rows_of(ln)
            } else {
                1
            };
            let last_sub = sub + 1 >= sub_rows;
            let first_sub = sub == 0;
            let cur = ln == cur_line;
            let marked = marks.is_some_and(|m| m.contains(&ln));
            let lm: &[(usize, usize, bool)] = per_line
                .get((ln - doc_first) as usize)
                .map(|v| v.as_slice())
                .unwrap_or(&[]);

            // Clip the selection to this line's span first: the offsets below
            // are then provably inside `[0, line_len)`.
            let sel_in_line = sel.as_ref().and_then(|r| {
                let s = doc.snap(r.start).clamp(span.start, span.end);
                let e = doc.snap(r.end).clamp(span.start, span.end);
                (s < e).then_some(((s - span.start) as usize, (e - span.start) as usize))
            });

            // Column-mode selection (this line's slice of the rectangle), same color and layer as
            // the linear selection
            let col_in_line = col_rect.get(&ln).map(|&(c0, c1)| {
                let l = text.len() as u64;
                (c0.min(l) as usize, c1.min(l) as usize)
            });

            // Text marks (right-click → stylize-as): checked against the snapshot one by one;
            // misaligned ones are not drawn (see the state::marks module comment — marks carry
            // their own evidence and vanish automatically after edits)
            let mut mk: Vec<(usize, usize, u8)> = Vec::new();
            for m in &marks_snap {
                if !m.still_valid(doc) || m.end <= span.start || m.start >= span.end {
                    continue;
                }
                let a = (m.start.max(span.start) - span.start) as usize;
                let b = (m.end.min(span.end) - span.start) as usize;
                if a < b {
                    mk.push((a, b, m.style));
                }
            }

            let row_style = if cur {
                row_style_cur.clone()
            } else {
                row_style_plain.clone()
            };
            let number_style = if marked {
                num_marked.clone()
            } else if cur {
                num_cur.clone()
            } else {
                num_normal.clone()
            };

            // token cover for this line (visual rows of one document row reuse it; scrolling in
            // from the middle still lexes the whole line once, advancing the lexer state only
            // once)
            if saved_ln != ln {
                cover.clear();
                if !hl_on || text.len() > MAX_HL_LINE {
                    cover.push((0, text.len(), None));
                } else {
                    toks.clear();
                    hl_state = lex_line(spec, &text, hl_state, &mut toks);
                    for t in toks.iter() {
                        let s = t.start.min(text.len());
                        let e = t.end.min(text.len());
                        if s >= e || !text.is_char_boundary(s) || !text.is_char_boundary(e) {
                            continue;
                        }
                        cover.push((s, e, Some(t.kind)));
                    }
                }
                saved_cover.clone_from(&cover);
                saved_ln = ln;
            } else {
                cover.clone_from(&saved_cover);
            }

            // split the cover at every selection / match boundary
            let mut segs: Vec<Seg> = Vec::with_capacity(cover.len() + lm.len() + 1);
            let mut buf = String::new();
            let mut buf_style: Cow<'static, str> = Cow::Borrowed("");
            let flush =
                |buf: &mut String, buf_style: &mut Cow<'static, str>, segs: &mut Vec<Seg>| {
                    if !buf.is_empty() {
                        segs.push(Seg {
                            text: std::mem::take(buf),
                            style: buf_style.clone(),
                        });
                    }
                };

            for (s, e, kind) in cover.iter().copied() {
                // clip to this visual row first, so fragments of the previous/next sub-row are
                // never drawn in
                let s = s.max(ra);
                let e = e.min(rb);
                if s >= e {
                    continue;
                }
                let mut cuts: Vec<usize> = vec![s];
                for (a, b) in [sel_in_line, col_in_line].into_iter().flatten() {
                    if a > s && a < e {
                        cuts.push(a);
                    }
                    if b > s && b < e {
                        cuts.push(b);
                    }
                }
                for (a, b, _) in lm.iter().copied() {
                    if a > s && a < e {
                        cuts.push(a);
                    }
                    if b > s && b < e {
                        cuts.push(b);
                    }
                }
                for (a, b, _) in mk.iter().copied() {
                    if a > s && a < e {
                        cuts.push(a);
                    }
                    if b > s && b < e {
                        cuts.push(b);
                    }
                }
                cuts.push(e);
                cuts.sort_unstable();

                let base = tok_slot(kind);
                let base_style = styles.slot(base);
                for w in cuts.windows(2) {
                    let (a, b) = (w[0], w[1]);
                    if b <= a || a >= text.len() {
                        continue;
                    }
                    let b = b.min(text.len());
                    let is_sel = sel_in_line.is_some_and(|(x, y)| a >= x && b <= y)
                        || col_in_line.is_some_and(|(x, y)| a >= x && b <= y);
                    let hit = lm.iter().find(|(x, y, _)| a >= *x && b <= *y);
                    let mark_hit = mk.iter().find(|(x, y, _)| a >= *x && b <= *y);
                    let style = if is_sel {
                        Cow::Borrowed(styles.slot(sel_slot))
                    } else if let Some((_, _, sty)) = mark_hit {
                        Cow::Owned(format!(
                            "{base_style}background:{};",
                            theme.mark[(*sty as usize).clamp(1, 5) - 1]
                        ))
                    } else {
                        match hit {
                            Some((_, _, is_cur)) => Cow::Owned(format!(
                                "{base_style}background:{};",
                                if *is_cur { cur_bg } else { match_bg }
                            )),
                            None => Cow::Borrowed(base_style),
                        }
                    };
                    if !buf.is_empty() && buf_style == style {
                        buf.push_str(&text[a..b]);
                        continue;
                    }
                    flush(&mut buf, &mut buf_style, &mut segs);
                    buf.push_str(&text[a..b]);
                    buf_style = style;
                }
            }
            flush(&mut buf, &mut buf_style, &mut segs);

            // `Show whitespace`: re-cut the finished segments so runs of spaces and
            // tabs become dimmed glyphs. Done *after* the token pass, so token
            // offsets and the caret's x are untouched.
            if show_ws {
                let mut cut: Vec<Seg> = Vec::with_capacity(segs.len() * 2);
                for seg in segs.into_iter() {
                    let bytes = seg.text.as_bytes();
                    let mut start = 0usize;
                    for i in 1..=bytes.len() {
                        let at_end = i == bytes.len();
                        let same = !at_end
                            && (bytes[i] == b' ' || bytes[i] == b'\t')
                                == (bytes[start] == b' ' || bytes[start] == b'\t');
                        if at_end || !same {
                            let piece = &seg.text[start..i];
                            if Seg::is_ws(piece) {
                                cut.push(Seg {
                                    text: Seg::glyph(piece),
                                    style: Cow::Borrowed(styles.slot(SLOT_WS as u16)),
                                });
                            } else {
                                cut.push(Seg {
                                    text: piece.to_string(),
                                    style: seg.style.clone(),
                                });
                            }
                            start = i;
                        }
                    }
                }
                segs = cut;
            }

            // `Show EOL`: pick the glyph per terminator type (more informative than a single
            // ¶ — the file's line-ending style is visible at a glance).
            // CR = `←` back to the line start, LF = `↓` next line (CR/LF chunk semantics, the
            // double glyph `←↓` is CRLF).
            // All glyphs come from the U+2190..=2195 arrow block — the same batch as `→` for
            // whitespace; the primary font (Consolas) always has them, metrics measure real
            // widths, and there is no fallback (⏎/↵ lack glyphs in the primary font and the
            // fallback renders small and dim, so they were dropped).
            if show_eol && last_sub {
                let term = doc.line_span(ln).term;
                let glyph = match term {
                    2 => "\u{2190}\u{2193}", // CRLF → ←↓ (CR + LF)
                    3 => "\u{2190}",         // CR → ← (back to the line start)
                    1 => "\u{2193}",         // LF → ↓ (next line)
                    _ => "",                 // 0 = EOF (no terminator, nothing shown)
                };
                if !glyph.is_empty() {
                    segs.push(Seg {
                        text: glyph.into(),
                        style: Cow::Borrowed(styles.slot(SLOT_EOL as u16)),
                    });
                }
            }

            // JSONL fold marker: peek at the first character while rendering the line (zero
            // extra IO); placed only when the JSONL view is on, in the fixed fold column right
            // of the line numbers.
            let marker = if jsonl_on && first_sub {
                text.as_bytes()
                    .iter()
                    .position(|b| !b.is_ascii_whitespace())
                    .filter(|&i| matches!(text.as_bytes()[i], b'{' | b'['))
                    .map(|_| fold_marker(!tab.jsonl.open.contains_key(&ln), fold_x))
            } else {
                None
            };

            rows.push(RowView {
                key: if st.wrap_enabled() {
                    format!("d{ln}_{sub}")
                } else {
                    format!("d{ln}")
                },
                // The first visual row shows the line number and continuations stay blank;
                // numbers up to 10 digits print as-is, longer ones in scientific notation (the
                // gutter shares the same formatting function with this, so the two always
                // agree).
                num: if first_sub {
                    crate::ui::state::format_line_no(ln + 1)
                } else {
                    String::new()
                },
                number_style,
                row_style,
                class: "",
                segs,
                marker,
            });
        }

        content_top = (PAD_TOP as f64 + first as f64 * lh as f64 - st.scroll_top) as f32;

        let caret_start = doc.pos_of_line(cur_line);
        let caret_text = doc.line_text(cur_line);
        let (_cv, _cl, _cs, ca, cb) = st.visual_row_of_pos(caret_pos);
        let cx = st.metrics.x_of_offset(
            &caret_text[ca.min(caret_text.len())..cb.min(caret_text.len())],
            (caret_pos.saturating_sub(caret_start) as usize).saturating_sub(ca),
            TAB_SIZE,
        );
        // The caret sits on document line cur_line, but the screen row number uses screen-row
        // coordinates (computed outside the block: JSONL expansions and hidden rows are both
        // folded in; with neither, it always equals the document line)
        caret_ok = cur_vrow >= first && cur_vrow < last;
        // The whole-document JSON tree view is a read-only preview: it draws no caret / selection
        if json_preview {
            caret_ok = false;
        }
        caret_left = gutter_w + PAD_LEFT + cx - st.scroll_left;
        caret_top = (PAD_TOP as f64 + cur_vrow as f64 * lh as f64 - st.scroll_top) as f32;

        // ---- bracket-match highlights ---------------------------------------
        // Two boxes: the bracket under the caret plus its partner. Computed with the caret's
        // same geometry (line-number column → x) so the two always align; no highlight when the
        // document is unfocused, for the same reason the selection greys out.
        // In the whole-document JSON tree view the caret lives in document coordinates and cannot
        // line up with tree rows, so it is skipped.
        brackets.clear();
        if st.doc_focused && !json_preview {
            let is_bracket = |c: char| matches!(c, '(' | ')' | '[' | ']' | '{' | '}');
            let here = if doc.char_at(caret_pos).is_some_and(is_bracket) {
                Some(caret_pos)
            } else if caret_pos > 0 && doc.char_at(doc.snap(caret_pos - 1)).is_some_and(is_bracket)
            {
                Some(doc.snap(caret_pos - 1))
            } else {
                None
            };
            for pos in [here, pair].into_iter().flatten() {
                let line = doc.line_of_pos(pos);
                let ls = doc.pos_of_line(line);
                let text = doc.line_text(line);
                let (bv, _bl, _bs, ba, bb) = st.visual_row_of_pos(pos);
                let off = (pos - ls) as usize;
                let cx = st.metrics.x_of_offset(
                    &text[ba.min(text.len())..bb.min(text.len())],
                    off.saturating_sub(ba),
                    TAB_SIZE,
                );
                let w = st.metrics.x_of_offset(
                    &text[ba.min(text.len())..bb.min(text.len())],
                    (off + 1).saturating_sub(ba),
                    TAB_SIZE,
                ) - cx;
                let x = gutter_w + PAD_LEFT + cx - st.scroll_left;
                let y = (PAD_TOP as f64 + bv as f64 * lh as f64 - st.scroll_top) as f32;
                brackets.push((x, y, w.max(1.0)));
            }
        }
    }

    // Folding visible rows into the horizontal width has moved before rendering to the self-heal
    // (`state::heal`, `measure_visible`) — this function is pure reads.

    ViewModel {
        rows,
        num_w,
        vx: st.view.x,
        content_top,
        scroll_left: st.scroll_left,
        caret_left,
        caret_top,
        caret_ok,
        lh,
        vw,
        vh,
        text_h,
        vtop,
        gutter_w,
        col_w,
        brackets,
    }
}

/// Wheel → body scroll queue.
///
/// **Shared with the composer**: the composer is a 320×20 transparent textarea sitting on top of
/// the caret, so with the mouse on the caret's line the wheel event's target is that textarea
/// (z-index 200), not the viewport — without this half, "the first wheel does nothing after
/// clicking into the body" reproduces every time (hit in practice and by the probes).
///
/// Sign convention: a positive `WheelDelta` means the **content** moves right/down (winit's
/// definition), the same axis as [`EditorState::scroll_by`]; one wheel notch down has negative dy
/// → content moves up → `scroll_top` grows. Nothing queues at a bound (nothing to scroll = no
/// frame scheduled).
pub(crate) fn wheel_scroll(
    mut state: Signal<EditorState>,
    d: dioxus_elements::geometry::WheelDelta,
    ctrl: bool,
    shift: bool,
) {
    let (dx, dy) = {
        let st = state.read();
        let lh = st.metrics.lh;
        let vh = st.view.h;
        match d {
            dioxus_elements::geometry::WheelDelta::Pixels(v) => (v.x as f32, v.y as f32),
            dioxus_elements::geometry::WheelDelta::Lines(v) => {
                (v.x as f32 * lh * WHEEL_LINES, v.y as f32 * lh * WHEEL_LINES)
            }
            dioxus_elements::geometry::WheelDelta::Pages(v) => (v.x as f32 * vh, v.y as f32 * vh),
        }
    };
    // Ctrl+wheel adjusts the font size.
    if ctrl {
        let dir = if dy > 0.0 {
            1.0
        } else if dy < 0.0 {
            -1.0
        } else {
            0.0
        };
        // A zero delta (synthesized events from some touchpads) never enters zoom: write()
        // would unconditionally mark dirty, and zoom(0) itself does nothing
        if dir != 0.0 {
            state.write().zoom(dir);
        }
        return;
    }
    if dx == 0.0 && dy == 0.0 {
        return; // empty wheel events schedule no frame
    }
    let (sdx, sdy) = if shift || (dx != 0.0 && dx.abs() > dy.abs()) {
        (dx as f64, 0.0)
    } else {
        (0.0, dy as f64)
    };
    let can = {
        let st = state.read();
        st.can_scroll(sdx, sdy)
    };
    if can && crate::ui::scroll::queue(crate::ui::scroll::Target::Doc, sdx, sdy) {
        state.read().request_frame();
    }
}

#[component]
pub fn EditorViewport() -> Element {
    let mut state = use_context::<Signal<EditorState>>();

    // ---- view-replacement routing ---------------------------------------------
    // With Markdown WYSIWYG / in-place preview / CSV table mode on, text rows are no longer
    // rendered here; the whole block is replaced by the corresponding view component (the
    // toolbar can switch back).
    {
        let st = state.read();
        if let Some(tab) = st.tabs.get(st.active) {
            if tab.md_wys {
                let id = tab.doc.id;
                drop(st);
                // Second layer of safety: key = the active tab's doc id — switching tabs forces
                // a remount and MdWys's on_ready consistency guard re-seeds with it (the primary
                // guard is `mdws::guard_seed`, already run during the set_active_tab event).
                return rsx! { super::mdws::MdWys { key: "{id}" } };
            }
            if tab.csv.enabled && tab.csv.table {
                drop(st);
                return rsx! { super::csvtable::CsvTableView {} };
            }
        }
    }

    // ---- self-heal before rendering (the only write timing on the render path) -----
    //
    // Dropping a `Signal::write()` guard unconditionally mark_dirties. The self-heal takes the
    // write lock only when it is actually needed; its probe condition matches the fix function's
    // early-exit branch word for word (see `state::heal`), so the probe after a heal is
    // necessarily false — in the idle steady state rendering does zero writes and zero dirty
    // marks, and event-driven frames no longer perpetuate themselves. An unfinished horizontal
    // width scan advances to done by design.
    if state.read().needs_heal() {
        state.write().self_heal();
    }
    let vm = {
        let st = state.read();
        build(&st)
    };
    let theme = { state.read().theme() };

    let lh = vm.lh;
    let content_top = vm.content_top;
    let neg_scroll = -vm.scroll_left;
    let row_w = vm.col_w + vm.scroll_left;
    let num_w = vm.num_w;
    let vw = vm.vw;
    let vh = vm.vh;

    let LayoutStyles {
        viewport: viewport_style,
        gutter: gutter_style,
        col: col_style,
    } = layout_styles(&vm, theme);
    // Fold-marker overlay data: `(key, glyph, x, y)`. y is an absolute viewport coordinate
    // (content_top + screen row number × line height); x is the left edge of the fixed fold
    // column right of the line numbers (viewport coordinates, unaffected by horizontal scroll).
    // Color and hover state live in the `.fold` class.
    let marker_els: Vec<(String, String, f32, f32)> = vm
        .rows
        .iter()
        .enumerate()
        .filter_map(|(i, r)| {
            r.marker.as_ref().map(|(g, mx)| {
                (
                    format!("m{}", r.key),
                    g.to_string(),
                    *mx,
                    content_top + i as f32 * lh,
                )
            })
        })
        .collect();
    let brk_border = theme.bracket_hi;
    let caret_style = caret_css(&vm, theme, &state.read());

    rsx! {
        div {
            id: "viewport",
            style: "{viewport_style}",

            // Pointer events (not mousedown): blitz only synthesizes the compatibility MouseDown
            // event for mouse input; touch/pen input dispatches pointerdown + touchstart only (see
            // blitz-dom events/driver.rs `handle_pointer_event`). Only the pointer family covers
            // mouse, finger and stylus at once.
            //
            // `touch-action:none` (viewport_style): on a finger drift >2px blitz enters Panning
            // scroll mode and **stops dispatching pointermove to the page** — finger-dragging to
            // select text would turn into scrolling the viewport and the selection could never
            // extend. With pan disabled, a finger drag has only this handler's own pointermove
            // left.
            onpointerdown: move |evt| {
                crate::ui::shell::debug_log(&format!(
                    "viewport pointerdown btn={:?} @ {:?}",
                    evt.data.trigger_button(),
                    evt.data.client_coordinates(),
                ));
                // Essential: blitz's pointer-down *default action* walks up from
                // the hit node and, finding nothing focusable, calls
                // `doc.clear_focus()` — which blurs the composer textarea that is
                // the editor's only keyboard target. Since focus is acquired once
                // through `autofocus` and can never be moved again from a task
                // (`MountedData::set_focus` panics inside dioxus), a blurred
                // composer means the keyboard is dead for the rest of the
                // session. `driver.rs` skips the default action for cancelled
                // events, so preventing default here is what keeps typing alive.
                evt.prevent_default();
                // A right press opens the context menu (on button-up) and must
                // leave the selection exactly as the user left it; only the
                // primary button moves the caret.
                if !matches!(evt.data.trigger_button(), Some(dioxus_elements::input_data::MouseButton::Primary)) {
                    return;
                }
                // **Only a "finger" counts as touch**: the pen follows the mouse's path — it is
                // accurate enough that point-and-click and press-and-drag selection are what users
                // expect; shoving it into touch gestures would make pen taps never place the
                // caret.
                let finger = evt.data.pointer_type() == "touch";
                let pid = evt.data.pointer_id();
                let cp = evt.data.client_coordinates();
                // Fold buttons: the JSON tree preview and JSONL run their own hit logic; a
                // consumed click does not move the caret (other clicks have no caret in a preview
                // and are intercepted by hit_test)
                let marker_consumed = {
                    let st = state.read();
                    let (preview, jsonl) = st
                        .tabs
                        .get(st.active)
                        .map(|t| (t.json.enabled, t.jsonl.enabled))
                        .unwrap_or((false, false));
                    // Coarse filter (read-only): fold buttons only draw in the marker slot right of
                    // the line numbers, and a click elsewhere is not worth one write() mark (the
                    // write guard mark_dirties unconditionally — a hit or miss cannot undo it).
                    let coarse = (preview || jsonl) && st.fold_marker_row_at(cp.x, cp.y).is_some();
                    drop(st);
                    if !coarse {
                        false
                    } else if preview {
                        state.write().json_marker_click(cp.x, cp.y)
                    } else {
                        state.write().jsonl_marker_click(cp.x, cp.y)
                    }
                };
                if marker_consumed {
                    return;
                }
                // Touch: on press **do not move the caret yet** — that press might turn into a
                // slide (scrolling the body). `ui::touch` accounts for the gesture, and only a lift
                // with no slide places the caret as a tap (the window-level `PointerButton` handles
                // it, see `ui::editor`). The mouse keeps its original semantics: press places the
                // caret, press-and-drag selects.
                if finger {
                    crate::ui::touch::begin(crate::ui::touch::PanTarget::Doc, pid, cp.x, cp.y);
                    return;
                }
                let shift = evt.data.modifiers().shift();
                let hit = state.read().hit_test(cp.x, cp.y);
                // Ctrl+click on a markdown link opens it outside the app and
                // consumes the click; off a link (or off the whitelist) the
                // click keeps its caret-placement semantics.
                if evt.data.modifiers().ctrl() {
                    if let Some(p) = hit {
                        if crate::ui::commands::markdown::open_link_at(&mut state.write(), p) {
                            return;
                        }
                    }
                }
                if let Some(p) = hit {
                    let mut st = state.write();
                    st.drag = true;
                    st.menu.open = None;
                    // Clicking the document also ends the Alt session: bare Alt → click → typing
                    // must not let the first letter trigger a mnemonic (same as the composer's
                    // document-click path).
                    st.menu.alt = false;
                    // clicking the text hands the keyboard back to the document
                    st.find.focus = FindFocus::Doc;
                    st.doc_focused = true;
                    // A single click places the caret; double-click word select and triple-click
                    // line select both fold in here (see editor_click).
                    st.editor_click(p, cp.x as f32, cp.y as f32, shift);
                }
            },
            onpointermove: move |evt| {
                if !state.read().drag {
                    return;
                }
                let cp = evt.data.client_coordinates();
                let hit = state.read().hit_test(cp.x, cp.y);
                if let Some(p) = hit {
                    state.write().set_caret(p, true);
                }
            },
            onpointerup: move |_evt| {
                // Conditional write (update_if): a lift with nothing dragged does not mark dirty,
                // or every mouse-up would trigger a whole-app re-render.
                crate::ui::state::update_if(&mut state, |st| st.drag, |st| st.drag = false);
            },
            onwheel: move |evt| {
                let mods = evt.data.modifiers();
                wheel_scroll(state, evt.data.delta(), mods.ctrl(), mods.shift());
            },

            // ---- line-number gutter ----
            // The numbers only occupy the left `num_w` (the right is given to the fold column),
            // and right-aligning keeps them clear of the fold button.
            div {
                style: "{gutter_style}",
                div {
                    style: "position:absolute; left:0; top:{content_top}px; width:{num_w}px;",
                    for r in vm.rows.iter() {
                        div {
                            key: "g{r.key}",
                            style: "{r.number_style}",
                            "{r.num}"
                        }
                    }
                }
            }

            // ---- text column ----
            div {
                style: "{col_style}",
                div {
                    // width is explicit so a row's background (current line,
                    // selection) spans the viewport instead of shrinking to the
                    // widest row — an absolutely positioned box is otherwise
                    // shrink-to-fit.
                    style: "position:absolute; top:{content_top}px; left:{neg_scroll}px;
                            width:{row_w}px;",
                    for r in vm.rows.iter() {
                        div {
                            key: "t{r.key}",
                            class: "{r.class}",
                            style: "{r.row_style}",
                            for (i, seg) in r.segs.iter().enumerate() {
                                span {
                                    key: "{i}",
                                    style: "{seg.style}",
                                    "{seg.text}"
                                }
                            }
                        }
                    }
                }
            }

            // ---- fold markers (a separate overlay) ----
            // They used to draw inside the gutter, but the gutter is overflow:hidden and only a
            // few dozen pixels wide, so any marker indented past ~14px (nested JSON tree rows,
            // hidden rows) was clipped away wholesale. The overlay is as wide as the viewport and
            // mx is an absolute viewport coordinate; clicks route by coordinate in the
            // viewport-level pointerdown (json_marker_click / jsonl_marker_click), and
            // pointer-events:none here never blocks the text.
            div {
                style: "position:absolute; left:0; top:0; width:{vw}px; height:{vh}px;
                        pointer-events:none; z-index:4;",
                for (key, glyph, mx, mtop) in marker_els.iter() {
                    span {
                        key: "{key}",
                        class: "fold",
                        style: "position:absolute; left:{mx}px; top:{mtop}px; \
                                height:{lh}px; line-height:{lh}px; {NO_SELECT}",
                        "{glyph}"
                    }
                }
            }

            // ---- bracket-match highlights ----
            // Drawn above the text and below the caret; thin borders (no fill), never stealing
            // the selection's visual weight.
            for (i, (bx, by, bw)) in vm.brackets.iter().enumerate() {
                div {
                    key: "brk{i}",
                    style: "position:absolute; left:{bx}px; top:{by}px; width:{bw}px; \
                            height:{lh}px; border:1px solid {brk_border}; border-radius:2px; \
                            z-index:4; pointer-events:none;",
                }
            }

            // ---- caret ----
            // Drawn as a direct child of the viewport, *not* inside the text
            // column: `caret_left` is an absolute viewport coordinate that
            // already includes the gutter width, so nesting it in the column
            // (which is itself offset by `left:{gutter}px`) would add the
            // gutter a second time and paint the caret ~64px too far right.
            if vm.caret_ok {
                div { style: "{caret_style}" }
            }

            // ---- minimap ----
            // Between the text column and the vertical bar. It reads its own
            // width out of `EditorState::mm_w`, which `text_w()` has already
            // subtracted, so the two cannot disagree about where the text ends.
            super::minimap::Minimap {}

            // ---- scrollbars ----
            // Last child and above everything else (z-index 20/21): they cover
            // the tail of a long line and the caret rather than being painted
            // under them.
            super::scrollbar::ScrollBars {}
        }
    }
}

/// The three layout strings of one viewport frame.
struct LayoutStyles {
    viewport: String,
    gutter: String,
    col: String,
}

/// `user-select:none` is essential, not cosmetic: blitz runs its own drag text
/// selection (`DragMode::Selecting` in blitz-dom/events/pointer.rs) which
/// (a) paints a second, competing highlight with its own colours and (b) walks
/// DOM nodes that this component remounts on every scroll, which is where
/// selection-time panics come from. The editor owns the selection.
fn layout_styles(vm: &ViewModel, theme: &Theme) -> LayoutStyles {
    LayoutStyles {
        viewport: format!(
            "position:absolute; top:{vtop}px; left:{vx}px; width:{vw}px; height:{vh}px; \
             overflow:hidden; background:{bg}; user-select:none; cursor:text; touch-action:none;",
            vtop = vm.vtop,
            vx = vm.vx,
            vw = vm.vw,
            vh = vm.vh,
            bg = theme.bg
        ),
        // The gutter and the edit area share **one color with no divider**: VS Code's gutter is
        // not an independent band, only the digits' brightness separates the two columns
        // (`editorLineNumber.foreground`). It also lets the current-line background read as one
        // unbroken horizontal band.
        gutter: format!(
            "position:absolute; left:0; top:0; width:{gutter}px; height:{h}px; \
             background:{bg}; overflow:hidden;",
            gutter = vm.gutter_w,
            h = vm.text_h,
            bg = theme.bg,
        ),
        col: format!(
            "position:absolute; left:{gutter}px; top:0; width:{col_w}px; height:{h}px; \
             overflow:hidden;",
            gutter = vm.gutter_w,
            col_w = vm.col_w,
            h = vm.text_h
        ),
    }
}

/// Caret css for this frame. Shape, width and blink are user settings; the caret
/// is hidden entirely when the document does not own the keyboard.
fn caret_css(vm: &ViewModel, theme: &Theme, st: &EditorState) -> String {
    let (w, top_off, h) = caret_geometry(st.caret_style, st.caret_w, st.metrics.adv, vm.lh);
    let color = if st.doc_focused {
        theme.caret
    } else {
        theme.caret_inactive
    };
    // Blinking rides the state phase (`caret_phase`, flipped by the BlinkTick thread every
    // 265ms, written back by the frame hook): a CSS `caret-blink` infinite animation is **not**
    // an option — blitz self-renews request_redraw every frame for windows with running
    // animations (the `is_animating` branch of `View::redraw`), pinning the whole app at
    // 40–60fps forever (measured in round seven, 2026-10). When unfocused the caret is hidden;
    // when focused but blink is off, it stays solid.
    let opacity = if !st.doc_focused || (st.caret_blink && !st.caret_phase) {
        "opacity:0;"
    } else {
        "opacity:1;"
    };
    format!(
        "position:absolute; left:{x}px; top:{y}px; width:{w}px; height:{h}px; \
         background:{c}; z-index:5; pointer-events:none; {op}",
        x = vm.caret_left,
        y = vm.caret_top + top_off,
        w = w,
        h = h,
        c = color,
        op = opacity
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::testutil::st_of;

    #[test]
    fn caret_shapes_differ_only_in_geometry() {
        let (lh, adv, cw) = (20.0, 8.4, 2.0);
        // line: thin, full height, flush with the top
        assert_eq!(
            caret_geometry(CaretStyle::Line, cw, adv, lh),
            (2.0, 0.0, 20.0)
        );
        // block: one character wide, full height
        assert_eq!(
            caret_geometry(CaretStyle::Block, cw, adv, lh),
            (8.4, 0.0, 20.0)
        );
        // underline: at the bottom, 2px tall
        assert_eq!(
            caret_geometry(CaretStyle::Underline, cw, adv, lh),
            (2.0, 18.0, 2.0)
        );
    }

    #[test]
    fn caret_geometry_never_produces_a_negative_box() {
        // a degenerate line height must not make the underline overflow or the
        // width collapse to nothing; a zero-height box is acceptable (it is
        // simply invisible)
        for lh in [0.0_f32, 1.0, 1.5, 2.0, 3.0] {
            for style in [CaretStyle::Line, CaretStyle::Block, CaretStyle::Underline] {
                let (w, top, h) = caret_geometry(style, 0.0, 0.0, lh);
                assert!(w >= 1.0, "width {w} for {style:?} at lh={lh}");
                assert!(h >= 0.0, "height {h} for {style:?} at lh={lh}");
                assert!(top >= 0.0, "top {top} for {style:?} at lh={lh}");
                assert!(
                    top + h <= lh.max(0.0) + 1e-3,
                    "box escapes line for {style:?} at lh={lh}"
                );
            }
        }
    }

    /// Oversized documents lose highlighting unless the user forced it on
    /// (View → Highlight Large Files); Plain Text never highlights at all.
    #[test]
    fn highlighting_yields_on_oversized_documents() {
        let spec = crate::highlight::by_name("Rust");
        let under = crate::highlight::AUTO_DISABLE_BYTES - 1;
        let over = crate::highlight::AUTO_DISABLE_BYTES;
        assert!(hl_enabled(spec, under, false), "small document highlights");
        assert!(
            !hl_enabled(spec, over, false),
            "past the byte limit highlighting yields"
        );
        assert!(
            hl_enabled(spec, over, true),
            "the View-menu force-on overrides the size limit"
        );
        assert!(
            !hl_enabled(crate::highlight::by_name("Plain Text"), under, false),
            "Plain Text never highlights"
        );
    }

    fn render(st: &mut EditorState, a: u64, b: u64) {
        let c = &mut st.tabs[0].caret;
        c.anchor = Some(a);
        c.pos = b;
        st.self_heal();
        let _ = build(st);
    }

    /// Every selection over every line of a mixed document must render.
    #[test]
    fn selection_rendering_is_exhaustively_safe() {
        let text = "alpha\nbeta\n\ngamma δ\nz";
        let n = text.len() as u64;
        let mut st = st_of(text);
        for a in 0..=n {
            for b in 0..=n {
                render(&mut st, a, b);
            }
        }
    }

    /// Same, but with the viewport scrolled and the language highlighted.
    #[test]
    fn selection_rendering_scrolled_and_highlighted() {
        let mut text = String::new();
        for i in 0..80 {
            text.push_str(&format!("line {i}: \"quoted\" // Comment ✓\n"));
        }
        let n = text.len() as u64;
        let mut st = st_of(&text);
        st.tabs[0].spec = crate::highlight::for_path(Some(std::path::Path::new("x.rs")));
        st.tabs[0].hl = crate::highlight::Highlighter::new(st.tabs[0].spec);
        st.scroll_top = 300.0;
        for a in (0..n).step_by(7) {
            for b in (0..n).step_by(11) {
                render(&mut st, a, b);
            }
        }
    }

    /// Stale "find all" offsets must not be used for rendering.
    #[test]
    fn stale_find_results_are_rejected() {
        let mut st = st_of("foo bar foo bar foo");
        st.open_find(false, true);
        st.find.pattern = "foo".into();
        st.find_all_matches();
        assert_eq!(st.find.results.len(), 3);
        assert!(
            st.find.valid_for(st.tabs[0].doc.id, st.tabs[0].doc.gen),
            "fresh results are valid"
        );

        // Any edit bumps the document generation, which must invalidate the
        // stored offsets: they are byte positions into the previous buffer.
        st.tabs[0].doc.insert_text(0, "x");
        assert_ne!(
            st.find.gen, st.tabs[0].doc.gen,
            "generation must change after an edit"
        );
        assert!(
            !st.find.valid_for(st.tabs[0].doc.id, st.tabs[0].doc.gen),
            "results must go stale after an edit"
        );

        // Rendering a document whose results are stale must not look them up.
        st.self_heal();
        let _ = build(&st);
    }

    /// A caret/anchor left beyond the end of the document (e.g. after an
    /// undo that shortened it) must not break rendering or hit testing.
    #[test]
    fn out_of_range_caret_is_tolerated() {
        let mut st = st_of("short");
        st.tabs[0].caret.anchor = Some(3);
        st.tabs[0].caret.pos = 9999;
        st.self_heal();
        let _ = build(&st);
        let _ = st.hit_test(200.0, 200.0);
        let _ = st.position_info();
    }

    /// The caret the viewport paints and the caret position used to park the
    /// IME composer (`caret_xy`) must describe the same point. They are
    /// computed in different places, and disagreement is exactly what makes a
    /// click look "offset" — the caret shows up beside where the user clicked.
    #[test]
    fn caret_paint_matches_composer_position() {
        let mut st = st_of("line one\nline two\nline three\n");
        st.view.x = 0.0;
        st.view.y = 52.0;
        st.view.w = 1280.0;
        st.view.h = 786.0;
        for pos in [0u64, 4, 9, 13, 18] {
            st.tabs[0].caret.anchor = None;
            st.tabs[0].caret.pos = pos;
            st.self_heal();
            let vm = build(&st);
            let (abs_x, abs_y, _lh) = st.caret_xy();
            assert!(
                (vm.caret_left - (abs_x - st.view.x)).abs() < 0.01,
                "caret x mismatch at pos {pos}: painted {} vs composer {}",
                vm.caret_left,
                abs_x - st.view.x
            );
            assert!(
                (vm.caret_top - (abs_y - st.view.y)).abs() < 0.01,
                "caret y mismatch at pos {pos}: painted {} vs composer {}",
                vm.caret_top,
                abs_y - st.view.y
            );
        }
    }

    /// The hit test must agree with the geometry the viewport renders: a click
    /// one line below the first text row lands on line 1, not line 0.
    #[test]
    fn hit_test_agrees_with_pixel_layout() {
        let mut st = st_of("line one\nline two\nline three\n");
        st.view.x = 0.0;
        st.view.y = 52.0;
        st.view.w = 1280.0;
        st.view.h = 786.0;
        // The gutter width adapts to the line-number digits (3-digit numbers, narrow default),
        // PAD_LEFT=PAD_TOP=6, lh=20. x takes the middle of column 5 (x_of_offset("line o") +
        // half a character width), not depending on any particular gutter width.
        // rel_y = 88 - 52 - 6 = 30 -> line 1
        let line_text = st
            .active_doc()
            .line_text(1)
            .trim_end_matches(['\n', '\r'])
            .to_string();
        let col5_x = st.metrics.x_of_offset(&line_text, 5, TAB_SIZE) as f64;
        // A quarter into the column: offset_of_x rounds to the nearest glyph boundary, so
        // midpoints would tie
        let x = st.gutter_w() as f64 + PAD_LEFT as f64 + col5_x + 0.25 * st.metrics.adv as f64;
        let p = st.hit_test(x, 88.0).unwrap();
        let doc = st.active_doc();
        let line = doc.line_of_pos(p);
        let col = p - doc.pos_of_line(line);
        assert_eq!(line, 1, "click at y=88 must be on the second text row");
        // hit_test's column must agree with the render geometry (x_of_offset): find between
        // which two glyph xs the rel_x falls
        let rel_x = x - st.gutter_w() as f64 - PAD_LEFT as f64;
        let exp_col = (0..=line_text.len())
            .take_while(|&i| st.metrics.x_of_offset(&line_text, i, TAB_SIZE) as f64 <= rel_x)
            .count()
            - 1;
        assert_eq!(
            col as usize, exp_col,
            "hit_test must match x_of_offset geometry"
        );
    }

    /// JSONL preview: with the preview on but nothing expanded the row set is unchanged; after
    /// expanding, virtual rows appear while the expanded document rows remain (virtual rows are
    /// inserted beneath them).
    #[test]
    fn jsonl_rows_render_identity_then_virtual() {
        let mut st = st_of("plain\n{\"a\":[1,2]}\ntail\n");
        st.view.x = 0.0;
        st.view.y = 52.0;
        st.view.w = 1280.0;
        st.view.h = 786.0;
        st.tabs[0].jsonl.enabled = true;
        st.self_heal();
        let vm0 = build(&st);
        assert_eq!(vm0.rows.len(), 3, "unexpanded: identity mapping");
        // Line 1 starts with `{` ⇒ it has a fold marker
        assert!(
            vm0.rows
                .iter()
                .find(|r| r.key == "d1")
                .is_some_and(|r| r.marker.is_some()),
            "a foldable row should carry a marker"
        );
        assert!(vm0
            .rows
            .iter()
            .find(|r| r.key == "d0")
            .is_some_and(|r| r.marker.is_none()));

        let doc = st.tabs[0].doc.clone();
        st.tabs[0].jsonl.toggle_line(&doc, 1, 1);
        st.self_heal();
        let vm1 = build(&st);
        // Row set = d1 + virtual rows ({, "a": [, 1, 2, ], }) + d0 + d2
        let vkeys: Vec<&String> = vm1.rows.iter().map(|r| &r.key).collect();
        assert!(
            vkeys.iter().any(|k| k.starts_with('v')),
            "virtual rows should appear: {vkeys:?}"
        );
        assert!(
            vkeys.contains(&&"d1".to_string()),
            "the expanded document row itself still renders"
        );
        // Virtual row styles carry a panel_bg background and an accent left border
        let vrow = vm1.rows.iter().find(|r| r.key.starts_with('v')).unwrap();
        assert!(vrow.row_style.contains(theme_panel_of(&st).as_str()));
        // The caret still points at document row 0 (the default position); the virtual
        // coordinates should keep it visible
        assert!(vm1.caret_ok);
    }

    fn theme_panel_of(st: &EditorState) -> String {
        st.theme().panel_bg.to_string()
    }

    /// Whole-document JSON tree view: once on, the screen has no document rows (d*), only tree
    /// virtual rows (p*), and the virtual rows carry fold markers (the root container folds).
    #[test]
    fn json_preview_renders_tree_rows_instead_of_document() {
        let mut st = st_of(r#"{"a":[1,2],"b":{"c":3}}"#);
        st.view.x = 0.0;
        st.view.y = 52.0;
        st.view.w = 1280.0;
        st.view.h = 786.0;
        st.tabs[0].json.enabled = true;
        let doc = st.tabs[0].doc.clone();
        let loc = st.locale;
        st.tabs[0].json.refresh(&doc, loc);
        st.self_heal();
        let vm = build(&st);
        // no document rows should remain
        assert!(
            !vm.rows.iter().any(|r| r.key.starts_with('d')),
            "preview mode must not render document rows: {:?}",
            vm.rows.iter().map(|r| r.key.clone()).collect::<Vec<_>>()
        );
        // tree virtual rows should appear
        assert!(
            vm.rows.iter().any(|r| r.key.starts_with('p')),
            "tree virtual rows should appear"
        );
        // The first row is the root container and should carry a fold marker
        let first = vm.rows.first().expect("at least one tree row");
        assert!(
            first.marker.is_some(),
            "the root container row should carry a fold marker"
        );
        // the caret is not drawn in the preview
        assert!(!vm.caret_ok);
    }

    /// Clicking a fold marker in the preview collapses / expands the subtree (via
    /// `json_marker_click`).
    #[test]
    fn json_preview_marker_click_folds_and_unfolds() {
        let mut st = st_of(r#"{"a":[1,2],"b":3}"#);
        st.view.x = 0.0;
        st.view.y = 52.0;
        st.view.w = 1280.0;
        st.view.h = 786.0;
        st.tabs[0].json.enabled = true;
        let doc = st.tabs[0].doc.clone();
        let loc = st.locale;
        st.tabs[0].json.refresh(&doc, loc);
        let full = st.tabs[0].json.rows().len();
        // The first row's (root `{`) marker lands at y ≈ PAD_TOP
        let lh = st.metrics.lh.max(1.0) as f64;
        let y = 52.0 + crate::ui::state::PAD_TOP as f64 + lh * 0.5;
        // The fold button sits in the fixed column right of the numbers (viewport coordinates
        // + view.x)
        let gw = st.gutter_w();
        let x = st.view.x as f64 + gw as f64 - MARKER_W as f64 / 2.0;
        assert!(
            st.json_marker_click(x, y),
            "clicking the root marker should be consumed"
        );
        let less = st.tabs[0].json.rows().len();
        assert!(less < full, "collapsing by click reduces the row count");
        // clicking again expands it back
        assert!(
            st.json_marker_click(x, y),
            "the second click should be consumed too"
        );
        assert_eq!(
            st.tabs[0].json.rows().len(),
            full,
            "clicking again restores the expansion"
        );
    }

    /// Viewport render snapshot: with a given document and viewport size, the row structure
    /// (key / line number / segment text) must be stable item by item. Layout/interaction
    /// regressions no longer rely on the naked eye — this test is the guardrail when changing the
    /// render pipeline.
    #[test]
    fn viewport_row_snapshot_for_plain_text() {
        let mut st = st_of("alpha\nbeta\n\ngamma δ\nz");
        st.view.x = 0.0;
        st.view.y = 52.0;
        st.view.w = 800.0;
        st.view.h = 200.0;
        st.self_heal();
        let vm = build(&st);
        let snap: Vec<(String, String, Vec<String>)> = vm
            .rows
            .iter()
            .map(|r| {
                (
                    r.key.clone(),
                    r.num.clone(),
                    r.segs.iter().map(|s| s.text.clone()).collect(),
                )
            })
            .collect();
        assert_eq!(
            snap,
            vec![
                ("d0".into(), "1".into(), vec!["alpha".into()]),
                ("d1".into(), "2".into(), vec!["beta".into()]),
                // an empty line has no segments (all-empty text is skipped by the boundary check)
                ("d2".into(), "3".into(), vec![]),
                ("d3".into(), "4".into(), vec!["gamma δ".into()]),
                ("d4".into(), "5".into(), vec!["z".into()]),
            ]
        );
    }

    /// Soft wrap: one document line splits into several visual rows, continuations repeat no
    /// line number, and hit-testing and up/down movement land on the right sub-row.
    #[test]
    fn wrap_splits_rows_and_maps_hit_test_and_caret() {
        use crate::ui::testutil::st_of_view;
        let mut st = st_of_view("aaa bbb ccc ddd eee\ntail\n");
        st.view.w = 150.0;
        st.wrap = true;
        st.self_heal();
        assert!(
            st.visual_rows() > 2,
            "a long line should fold into several visual rows"
        );
        let vm = build(&st);
        let first: Vec<_> = vm.rows.iter().take(2).collect();
        assert_eq!(first[0].key, "d0_0");
        assert_eq!(first[1].key, "d0_1");
        assert_eq!(first[0].num, "1");
        assert_eq!(first[1].num, "", "continuations repeat no line number");
        let joined: String = first
            .iter()
            .flat_map(|r| r.segs.iter().map(|s| s.text.as_str()))
            .collect();
        assert!(
            joined.starts_with("aaa bbb"),
            "the first segment comes from the line start: {joined:?}"
        );
        let row0: String = first[0].segs.iter().map(|s| s.text.as_str()).collect();
        assert!(
            row0.len() < "aaa bbb ccc ddd eee".len(),
            "the first visual row draws only part: {row0:?}"
        );

        // The second visual row's midpoint should hit document row 0, with an offset inside
        // that sub-row's range
        let lh = st.metrics.lh as f64;
        let vrow = st.visual_start_of_screen(0) + 1;
        let y = st.view.y as f64 + PAD_TOP as f64 + vrow as f64 * lh + lh * 0.5;
        let x = st.view.x as f64 + st.gutter_w() as f64 + PAD_LEFT as f64 + 1.0;
        let pos = st.hit_test(x, y).expect("the sub-row should be hittable");
        assert_eq!(st.active_doc().line_of_pos(pos), 0);
        let (_, _, _, a, b) = st.visual_row_of_pos(pos);
        assert!(a < b, "the hit sub-row should contain text");

        // Down from the line start: the same document row's next visual row, not the next
        // document row
        st.set_caret(0, false);
        st.move_down(false);
        assert_eq!(st.active_doc().line_of_pos(st.caret().pos), 0);
        assert!(st.visual_row_of_pos(st.caret().pos).2 >= 1);
    }

    /// JSONL virtual rows and soft-wrap sub-rows can coexist: the expanded block is still 1 row
    /// while document rows split by visual rows, and the mapping must stay invertible.
    #[test]
    fn wrap_and_jsonl_virtual_rows_coexist() {
        use crate::ui::testutil::st_of_view;
        let mut st =
            st_of_view("{\"a\":[1,2,3]}\ntail with enough words to wrap around the narrow pane\n");
        st.view.w = 140.0;
        st.wrap = true;
        st.tabs[0].jsonl.enabled = true;
        st.self_heal();
        let doc = st.tabs[0].doc.clone();
        st.tabs[0].jsonl.toggle_line(&doc, 0, 1);
        st.self_heal();
        let vm = build(&st);
        assert!(
            vm.rows.iter().any(|r| r.key.starts_with("v0_")),
            "the expanded block's virtual rows should survive"
        );
        assert!(
            vm.rows.iter().any(|r| r.key.starts_with("d1_")),
            "document rows should render per visual row"
        );
        for v in 0..st.visual_rows() {
            let (s, sub) = st.screen_of_visual(v);
            assert_eq!(st.visual_start_of_screen(s) + sub, v);
        }
    }

    /// Any selection must render under soft wrapping (sub-row slicing, style splitting — no
    /// out-of-bounds).
    #[test]
    fn selection_rendering_is_safe_with_wrap() {
        let text = "alpha beta gamma delta epsilon\nsecond line with more words\n";
        let n = text.len() as u64;
        let mut st = st_of(text);
        st.view.x = 0.0;
        st.view.y = 52.0;
        st.view.w = 150.0;
        st.view.h = 120.0;
        st.wrap = true;
        for a in (0..=n).step_by(3) {
            for b in (0..=n).step_by(5) {
                render(&mut st, a, b);
                let vm = build(&st);
                assert!(!vm.rows.is_empty());
            }
        }
    }

    /// After wrapping, caret scrolling follows visual rows, not document rows: the end of a long
    /// line must scroll into view.
    #[test]
    fn wrap_scrolls_caret_by_visual_row() {
        use crate::ui::testutil::st_of_view;
        let mut st = st_of_view(&format!("{}\ntail\n", "word ".repeat(20)));
        st.view.w = 150.0;
        st.view.h = 70.0;
        st.wrap = true;
        st.self_heal();
        let end_of_long_line = st.active_doc().line_range(0).end;
        st.set_caret(end_of_long_line, false);
        let (vrow, _, _, _, _) = st.visual_row_of_pos(end_of_long_line);
        assert!(
            vrow >= 3,
            "a long line should fold into several visual rows: {vrow}"
        );
        let lh = st.metrics.lh as f64;
        let top = (st.scroll_top / lh).floor() as u64;
        let bottom = ((st.scroll_top + st.text_h() as f64) / lh).ceil() as u64;
        assert!(
            vrow >= top && vrow < bottom,
            "the caret's visual row {vrow} should be inside [{top}, {bottom})"
        );
    }

    /// The selection splits segments: the split positions and "selected segment has the
    /// background, unselected does not" must be stable.
    #[test]
    fn viewport_selection_snapshot_cuts_segments() {
        let mut st = st_of("hello world\nsecond");
        st.view.x = 0.0;
        st.view.y = 52.0;
        st.view.w = 800.0;
        st.view.h = 200.0;
        st.tabs[0].caret.anchor = Some(0);
        st.tabs[0].caret.pos = 5;
        st.self_heal();
        let vm = build(&st);
        let row0 = &vm.rows[0];
        let texts: Vec<&str> = row0.segs.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(texts, vec!["hello", " world"]);
        assert!(
            row0.segs[0].style.contains("background:"),
            "the selected segment should carry the selection background: {}",
            row0.segs[0].style
        );
        assert!(
            !row0.segs[1].style.contains("background:"),
            "the unselected segment must not carry a background: {}",
            row0.segs[1].style
        );
        // The segment style on the selection's other side shares the source with the
        // "no selection" token style (after slotting it is still the same shared string)
        let plain = {
            let mut st2 = st_of("hello world\nsecond");
            st2.view = st.view;
            st2.self_heal();
            build(&st2)
        };
        assert_eq!(
            plain.rows[0].segs[0].style.as_ref(),
            row0.segs[1].style.as_ref(),
            "unselected text must keep the no-selection style"
        );
    }

    /// `tok_slot`'s numbering must pair one-to-one with `TOK_FIXED_KINDS` — if someone later
    /// adds a `TokenKind` variant and forgets to sync the slots, this test fails first.
    #[test]
    fn tok_slot_matches_the_fixed_table() {
        for (i, k) in TOK_FIXED_KINDS.iter().enumerate() {
            assert_eq!(tok_slot(Some(*k)) as usize, i, "slot drift: {k:?}");
        }
        assert_eq!(
            tok_slot(None) as usize,
            tok_slot(Some(TokenKind::Plain)) as usize
        );
        // Column slots are taken modulo the palette length
        assert_eq!(tok_slot(Some(TokenKind::Col(0))) as usize, TOK_FIXED);
        assert_eq!(tok_slot(Some(TokenKind::Col(9))) as usize, TOK_FIXED + 1);
        assert_eq!(
            tok_slot(Some(TokenKind::Col(COL_SLOTS as u8))) as usize,
            TOK_FIXED
        );
        assert_eq!(SLOT_COUNT, TOK_FIXED + COL_SLOTS + 5);
    }

    /// The strings the shared stylesheet returns per slot must match the original `token_style` /
    /// special-slot assembly one by one — the cache optimization may not change a single byte of
    /// the output.
    #[test]
    fn shared_styles_match_direct_construction() {
        for dark in [true, false] {
            let theme = Theme::of(dark);
            let s = styles_for(theme);
            for (i, k) in TOK_FIXED_KINDS.iter().enumerate() {
                assert_eq!(s.slot(i as u16), token_style(theme, Some(*k)), "slot {i}");
            }
            for i in 0..COL_SLOTS {
                assert_eq!(
                    s.slot((TOK_FIXED + i) as u16),
                    token_style(theme, Some(TokenKind::Col(i as u8)))
                );
            }
            assert_eq!(
                s.slot(SLOT_SEL as u16),
                format!(
                    "background:{}; color:{};{NO_SELECT}",
                    theme.selection, theme.selection_fg
                )
            );
            assert_eq!(
                s.slot(SLOT_JSON_ERR as u16),
                format!("color:{};{NO_SELECT}{TEXT_CURSOR}", theme.dirty_dot)
            );
            assert_eq!(
                s.slot(SLOT_WS as u16),
                format!("color:{};{NO_SELECT}{TEXT_CURSOR}", theme.gutter_fg)
            );
        }
        // The light/dark tables are cached separately
        let d = styles_for(Theme::of(true));
        let l = styles_for(Theme::of(false));
        assert_ne!(
            d.slot(0),
            l.slot(0),
            "the light and dark base styles must differ"
        );
    }

    /// Manual benchmark (ignored by default): large-document viewport build time.
    /// `cargo test --lib bench_viewport_build -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn bench_viewport_build() {
        let mut text = String::new();
        for i in 0..100_000 {
            text.push_str(&format!(
                "line {i}: some code with \"strings\" and 中文注释\n"
            ));
        }
        let mut st = st_of(&text);
        st.view.x = 0.0;
        st.view.y = 52.0;
        st.view.w = 1280.0;
        st.view.h = 786.0;
        st.tabs[0].spec = crate::highlight::for_path(Some(std::path::Path::new("x.rs")));
        st.tabs[0].hl = crate::highlight::Highlighter::new(st.tabs[0].spec);
        st.self_heal();
        for _ in 0..20 {
            let _ = build(&st);
        }
        let n = 300;
        let t0 = std::time::Instant::now();
        for _ in 0..n {
            let _ = build(&st);
        }
        let per = t0.elapsed().as_secs_f64() * 1000.0 / n as f64;
        println!("viewport build: {per:.3} ms/frame (100k-line document, viewport 1280x786)");
    }

    /// Manual benchmark (ignored by default): minimap ink-model time.
    /// `cargo test --lib bench_minimap_model -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn bench_minimap_model() {
        let mut text = String::new();
        for i in 0..100_000 {
            text.push_str(&format!(
                "line {i}: some code with \"strings\" and 中文注释\n"
            ));
        }
        let mut st = st_of(&text);
        st.view.x = 0.0;
        st.view.y = 52.0;
        st.view.w = 1280.0;
        st.view.h = 786.0;
        st.minimap = true;
        st.scroll_top = 400_000.0;
        st.self_heal();
        let n = 300;
        let t0 = std::time::Instant::now();
        for _ in 0..n {
            let _ = st.mm_model();
        }
        let per = t0.elapsed().as_secs_f64() * 1000.0 / n as f64;
        println!("minimap model: {per:.3} ms/frame");
    }
}
