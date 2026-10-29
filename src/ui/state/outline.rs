//! Markdown document outline (data and navigation for the sidebar "Outline" panel).
//!
//! # Data source
//!
//! [`build_outline`] uses **Head spans from `wysiwyg::md::block_line_ranges`**:
//! line ranges come for free (text-mode line jumps and preview scrolling both
//! need "which line a heading is on"), and setext (`text\n---`) and ATX are
//! treated alike. Level and title text come from **real parsing** — the span's
//! source fragment is fed back into `wysiwyg::md::parse`; `Block::Head`'s
//! `level` and `plain_text()` (inline markers stripped, same source as the
//! preview) provide the item content; if parsing unexpectedly fails, fall
//! back to counting ATX markers on the source line so no item is lost.
//!
//! The Head-block route via `wysiwyg::parse_doc` was rejected: the block model
//! has no line numbers, and back-deriving them requires line scanning again —
//! the line-range scheme needs a single parse pass (per the task spec: "a
//! line-range solution is enough").
//!
//! # Refresh (gen-keyed cache)
//!
//! The cache hangs off [`Tab::outline`](crate::ui::state::Tab::outline), keyed
//! by document `gen`: probe [`EditorState::outline_build_needed`] and repair
//! [`EditorState::outline_rebuild`] form a two-phase conditional write pair
//! (heal rule: the repair's early return matches the probe verbatim); the
//! component rebuilds at most once per render, only when the cache is stale.
//! Large documents (> [`OUTLINE_MAX_BYTES`]) are marked `too_large` and not
//! parsed — the outline is a navigation aid, not worth a full pulldown re-run
//! on huge files (the preview's mdmap cache is likewise invalidated by gen;
//! the only extra here is the size gate).
//!
//! # Navigation (two views)
//!
//! [`EditorState::outline_click`] moves the piece-table caret to the heading
//! line (text/wys modes show the same status-bar line number), then
//! dispatches by view:
//!
//! * **Text**: `set_caret` already includes `scroll_caret_into_view`;
//! * **wys**: place the wys editor's selection (`mdws::WYS`) at the start of
//!   the matching heading block — block index = item index (the outline only
//!   collects top-level Head spans; `Doc::blocks` flattens leaf blocks, so
//!   heading block order equals source order; headings inside quotes carry a
//!   `Quoted` shell and are excluded, consistent with the span criterion)
//!   **and then the visible half: overlay sync, native-selection push and
//!   scroll-into-view** (`mdws::goto_block` — writing the model alone left the
//!   viewport where it was and the click looked dead).
//!   Known boundary: titles within list items (`- # h`) are flat Head blocks in the block model but **do not** occupy
//!   Head span, will squeeze the line number—for such documents, wys fallback takes "nearest title block".
//!   Visual scrolling of the wys viewport depends on mdws private HANDLE/CARET signals, the host side cannot drive it,
//!   After model cursor is positioned, it follows on the next component interaction scroll (verified on real machine, see component comment).

use std::rc::Rc;

use super::{EditorState, Fmt, Msg, SidePanel};

/// Document size cap for outline parsing (bytes). Above it, only
/// [`OutlineCache::too_large`] is flagged; the panel shows a notice instead of
/// re-parsing the whole huge file.
pub const OUTLINE_MAX_BYTES: u64 = 4 * 1024 * 1024;

/// One outline item = one top-level heading.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutlineItem {
    /// Heading level 1..=6.
    pub level: u8,
    /// Plain Text title (inline markers stripped; empty title displayed by component fallback).
    pub title: String,
    /// Heading's starting source line (0-based) — the line number for text-mode jumps / preview scrolling.
    pub line: u64,
    /// Tree indent level (H1→0 … H6→5); the panel indents by it.
    pub depth: usize,
}

/// Outline cache (hangs off [`Tab::outline`](crate::ui::state::Tab::outline), keyed by document gen; see module docs).
pub struct OutlineCache {
    /// Document generation when the cache was built.
    pub gen: u64,
    /// Heading items (source order).
    pub items: Rc<Vec<OutlineItem>>,
    /// Document too large, parsing skipped (the panel shows a notice).
    pub too_large: bool,
}

/// Source → outline items (pure function, main unit-test entry).///
/// One top-level Head span per item; **item index = the k-th top-level
/// heading in order** (the wys jump block-index convention, see module
/// docs). The span range is split back into source lines and fed to real
/// parsing for level/title; if fragment parsing fails (theoretically
/// unreachable), fall back to ATX counting so no item is lost.
pub fn build_outline(src: &str) -> Vec<OutlineItem> {
    let lines: Vec<&str> = src.lines().collect();
    let mut items = Vec::new();
    for span in wysiwyg::md::block_line_ranges(src) {
        if span.kind != wysiwyg::md::SpanKind::Head {
            continue;
        }
        // Defensive: clamp the span's line range to the actual line count (push_span already clamps; this guards an off-by-one in lines())
        let end = span.end.min(lines.len().saturating_sub(1));
        if span.start >= lines.len() {
            continue;
        }
        let frag = lines[span.start..=end].join("\n");
        let (level, title) = heading_of(&frag);
        items.push(OutlineItem {
            level,
            title,
            line: span.start as u64,
            depth: level.saturating_sub(1) as usize,
        });
    }
    items
}

/// Title source code fragment → (level, Plain Text title).
///
/// Real parsing first: `md::parse`'s Head block provides level and
/// `plain_text()` (links take text, images take alt, emphasis/code markers
/// stripped — same source as wys/preview). The fallback branch only covers
/// the theoretical failure of "parse did not recognize a Head": count `#`
/// for ATX, check the underline line for setext.
fn heading_of(frag: &str) -> (u8, String) {
    if let Some(block) = wysiwyg::md::parse(frag).blocks.first() {
        if let wysiwyg::Block::Head { level, .. } = block.core() {
            return (*level, block.core().plain_text());
        }
    }
    // Fallback: count ATX markers on the first line; for setext check the last line's underline (`=`→1, `-`→2).
    let mut it = frag.lines();
    let first = it.next().unwrap_or("");
    let t = first.trim_start();
    let hashes = t.len() - t.trim_start_matches('#').len();
    if (1..=6).contains(&hashes) {
        let title = t[hashes..].trim().trim_end_matches('#').trim().to_string();
        return (hashes as u8, title);
    }
    let underline = frag.lines().nth(1).map(str::trim).unwrap_or("");
    let level = if underline.chars().all(|c| c == '=') && !underline.is_empty() {
        1
    } else if underline.chars().all(|c| c == '-') && !underline.is_empty() {
        2
    } else {
        1
    };
    (level, first.trim().to_string())
}

/// Caret line → highlighted item index: the last heading whose start line is
/// ≤ the caret line (the "current section"; None when the caret is before
/// the first heading).
pub fn outline_active_index(items: &[OutlineItem], caret_line: u64) -> Option<usize> {
    let mut best = None;
    for (i, it) in items.iter().enumerate() {
        if it.line <= caret_line {
            best = Some(i);
        } else {
            break;
        }
    }
    best
}

/// The wys block index of outline item `head_seq`: the outline collects
/// top-level Head spans and `Doc::blocks` flattens leaf blocks, so
/// heading-block order equals source order — except that headings inside
/// quotes (`Quoted` shell) are excluded on the same basis as the span walk,
/// and headings inside list items shift the index. `head_seq` past the last
/// heading (content drifted from the piece table) falls back to the nearest
/// heading block; a document without headings returns None.
fn wys_heading_block(doc: &wysiwyg::Doc, head_seq: usize) -> Option<usize> {
    let mut seen = 0usize;
    let mut nearest: Option<usize> = None;
    for (i, b) in doc.blocks.iter().enumerate() {
        if matches!(b.core(), wysiwyg::Block::Head { .. }) {
            nearest = Some(i);
            if b.quote_depth() == 0 {
                if seen == head_seq {
                    return Some(i);
                }
                seen += 1;
            }
        }
    }
    nearest
}

impl EditorState {
    /// The caret's current source line (0-based) — the outline panel's
    /// active-row highlight (was `mdmap::md_caret_line`; the piece table's
    /// line index is the estimate in every view, not a preview concept).
    pub fn md_caret_line(&self) -> u64 {
        self.active_doc().line_of_pos(self.caret().pos)
    }

    /// Whether the active document supports an outline (**Markdown only**).
    ///
    /// The status bar's "Outline" icon only appears when supported — a button
    /// that does nothing on non-Markdown documents is worse than no button.
    pub fn outline_supported(&self) -> bool {
        self.spec().name == "Markdown"
    }

    /// Whether the outline panel is currently shown (for menu check / jump dispatch).
    pub fn outline_panel_shown(&self) -> bool {
        self.ws_show && self.side_panel == SidePanel::Outline && self.outline_supported()
    }

    /// The active document's outline items (empty when the cache is missing or not Markdown).
    pub fn outline_items(&self) -> Rc<Vec<OutlineItem>> {
        self.tabs
            .get(self.active)
            .and_then(|t| t.outline.as_ref())
            .map(|c| c.items.clone())
            .unwrap_or_else(|| Rc::new(Vec::new()))
    }

    /// Whether the active document disabled outline parsing due to size.
    pub fn outline_too_large(&self) -> bool {
        self.tabs
            .get(self.active)
            .and_then(|t| t.outline.as_ref())
            .is_some_and(|c| c.too_large)
    }

    /// Pure-read probe: whether the outline cache needs (re)building. The
    /// condition matches [`Self::outline_rebuild`]'s early return verbatim
    /// (heal rule).
    ///
    /// Always false for non-Markdown documents — the panel shows an empty
    /// state and never parses.
    pub fn outline_build_needed(&self) -> bool {
        let Some(tab) = self.tabs.get(self.active) else {
            return false;
        };
        if tab.spec.name != "Markdown" {
            return false;
        }
        match &tab.outline {
            Some(c) => c.gen != tab.doc.gen,
            None => true,
        }
    }

    /// Pre-render repair: rebuild the outline cache when gen is stale (oversized documents are flagged, not parsed).
    pub fn outline_rebuild(&mut self) {
        if !self.outline_build_needed() {
            return;
        }
        let (gen, too_large, items) = {
            let tab = self.tab();
            let gen = tab.doc.gen;
            if tab.doc.total_bytes() > OUTLINE_MAX_BYTES {
                (gen, true, Rc::new(Vec::new()))
            } else {
                let src = tab.doc.text_range(0..tab.doc.total_bytes());
                (gen, false, Rc::new(build_outline(&src)))
            }
        };
        self.tab_mut().outline = Some(OutlineCache {
            gen,
            items,
            too_large,
        });
    }

    /// Click outline item `index`: move the caret to the heading's source
    /// line and scroll per view.
    ///
    /// Out-of-bounds is normal (rows render from the previous frame's cache,
    /// the document may have changed before the click) — rebuild the cache
    /// first, then look up the index; if still out of bounds, silently
    /// ignore, never panic.
    pub fn outline_click(&mut self, index: usize) {
        if self.outline_build_needed() {
            self.outline_rebuild();
        }
        let (line, level) = {
            let items = self.outline_items();
            let Some(item) = items.get(index) else {
                return;
            };
            (item.line, item.level)
        };
        // Heading line start (line number clamped to the last line — the document may be shorter than the cache)
        let doc = self.active_doc();
        let line = line.min(doc.line_count().saturating_sub(1));
        let pos = doc.pos_of_line(line);
        self.set_caret(pos, false);
        self.doc_focused = true;
        // Dispatch by view (module docs "Navigation (two views)")
        if self.tab().md_wys {
            self.outline_wys_goto(index);
        }
        self.status = Some(
            Fmt::StatusOutlineGoto {
                line: (line + 1) as usize,
                level: level as usize,
            }
            .into(),
        );
    }

    /// wys-mode jump: place the wys selection at the start of the
    /// `head_seq`-th top-level heading block.
    ///
    /// The block-index convention is in the module docs: headings inside
    /// quotes (`Quoted` shell) are excluded on the same basis as Head spans;
    /// when headings inside list items shift the index, fall back to the
    /// nearest heading block. The *visible* half of the jump (overlay sync,
    /// native selection, scrolling into view) lives in
    /// [`crate::ui::components::mdws::goto_block`] — writing the model cursor
    /// alone used to be the whole function, and the click looked dead.
    fn outline_wys_goto(&mut self, head_seq: usize) {
        use crate::ui::components::mdws;
        use dioxus_native::prelude::ReadableExt;
        // Resolve against the wys model itself: in this view its content is
        // the authority (the piece table may lag until the next sync).
        let block = {
            let w = mdws::WYS.read();
            wys_heading_block(&w.doc, head_seq)
        };
        let Some(block) = block else {
            return; // no heading block in the document (extreme case of wys content drifting from the piece table)
        };
        if mdws::goto_block(block) {
            // The host caret follows the wys selection (status line/col and
            // the sync flows read it), same post-move step as a keyboard move.
            mdws::align_status(self);
        }
    }

    /// View → Document outline (panel toggle): open/collapse the sidebar's
    /// outline panel.
    ///
    /// Triggering again (panel shown) = collapse; otherwise switch to the
    /// outline panel and expand the sidebar (workspace state untouched — the
    /// folder sidebar's loading/expansion stays as-is).
    /// Status bar bottom-left / "View → Document outline": toggle the outline panel.
    ///
    /// Only Markdown documents can open it ([`Self::outline_supported`]) —
    /// non-Markdown documents get a notice instead of an empty panel.
    pub fn toggle_outline(&mut self) {
        if !self.outline_supported() {
            self.status = Some(Msg::StatusOutlineUnsupported.into());
            return;
        }
        if self.ws_show && self.side_panel == SidePanel::Outline {
            self.ws_show = false;
            self.status = Some(Msg::StatusOutlineOff.into());
        } else {
            self.side_panel = SidePanel::Outline;
            self.ws_show = true;
            self.status = Some(Msg::StatusOutlineOn.into());
        }
        self.relayout();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn titles(items: &[OutlineItem]) -> Vec<(u8, &str, u64, usize)> {
        items
            .iter()
            .map(|i| (i.level, i.title.as_str(), i.line, i.depth))
            .collect()
    }

    /// Heading extraction/level tree/line numbers: ATX all levels + setext two levels + source order.
    #[test]
    fn build_outline_extracts_levels_titles_and_lines() {
        // 0: # 一        ─┐ H1
        // 1: (empty)     ─┘
        // 2: ## 二       ─┐ H2
        // 3: (empty)     ─┘
        // 4: setext 标题 ─┐ setext H2 (two lines, one block; line = first line 4)
        // 5: ---         ─┘
        // 6: (empty)
        // 7: ### 三      H3
        let src = "# 一\n\n## 二\n\nsetext 标题\n---\n\n### 三\n";
        let items = build_outline(src);
        assert_eq!(
            titles(&items),
            vec![
                (1, "一", 0, 0),
                (2, "二", 2, 1),
                (2, "setext 标题", 4, 1),
                (3, "三", 7, 2),
            ]
        );
    }

    /// Plain Text title same source as preview: links take text, images take alt, emphasis/code stripped markers.
    #[test]
    fn build_outline_strips_inline_markup_like_the_renderer() {
        let src = "## **粗** [链接](u) `码` ~~删~~\n\n# ![图](img.png) 尾\n";
        let items = build_outline(src);
        assert_eq!(items[0].title, "粗 链接 码 删");
        assert_eq!(items[1].title, "图 尾");
        assert_eq!(items[1].level, 1);
    }

    /// Only top-level headings: `#` in code fences, headings inside quotes,
    /// and headings inside list items are all excluded from the outline
    /// (the first two are not Head spans; list-item headings occupy no span,
    /// see the module docs' known boundary).
    #[test]
    fn build_outline_ignores_non_top_level_headings() {
        let src = "# 真\n\n```\n# 假的\n```\n\n> # 引用内\n\n- # 列表内\n\n## 尾\n";
        let items = build_outline(src);
        assert_eq!(
            titles(&items),
            vec![(1, "真", 0, 0), (2, "尾", 10, 1)],
            "{:?}",
            items
        );
    }

    /// Empty/no-heading documents → empty outline.
    #[test]
    fn build_outline_empty_documents() {
        assert!(build_outline("").is_empty());
        assert!(build_outline("段落一\n\n段落二\n").is_empty());
    }

    /// Empty headings (`#` with no text) keep the item; the empty title is shown via the component's fallback.
    #[test]
    fn build_outline_keeps_empty_heading() {
        let items = build_outline("#\n\n## \n");
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].title, "");
        assert_eq!(items[1].level, 2);
    }

    /// Active item = the section the caret is in (the last heading starting at or before the caret line).
    #[test]
    fn active_index_follows_the_caret_line() {
        // lines: 0 "# a" / 1 empty / 2 "## b" / 3 empty / 4 "## c"
        let items = build_outline("# a\n\n## b\n\n## c\n");
        // caret line → item: 0..1→0 (section a), 2..3→1 (section b), 4 and later→2 (section c)
        assert_eq!(outline_active_index(&items, 0), Some(0));
        assert_eq!(
            outline_active_index(&items, 1),
            Some(0),
            "still in section a before line 2 of b"
        );
        assert_eq!(outline_active_index(&items, 2), Some(1));
        assert_eq!(
            outline_active_index(&items, 3),
            Some(1),
            "still in section b before line 4 of c"
        );
        assert_eq!(outline_active_index(&items, 4), Some(2));
        assert_eq!(outline_active_index(&items, 999), Some(2));
    }

    /// Caret before the first heading → no highlight.
    #[test]
    fn active_index_is_none_before_the_first_heading() {
        let items = build_outline("前言\n\n# a\n");
        assert_eq!(outline_active_index(&items, 0), None);
        assert_eq!(outline_active_index(&items, 1), None);
        assert_eq!(outline_active_index(&items, 2), Some(0));
        assert_eq!(outline_active_index(&[], 0), None);
    }

    /// Click-jump line number computation: the caret lands at the heading
    /// line's start (text-mode jump semantics).
    #[test]
    fn outline_click_places_the_caret_at_the_heading_line() {
        // lines: 0 "# 一" / 1 empty / 2 "## 二" / 3 empty / 4 "正文"
        let mut st = EditorState::minimal();
        st.tabs[0].doc = crate::buffer::TextDocument::from("# 一\n\n## 二\n\n正文\n");
        st.set_language_by_name("Markdown");
        st.outline_rebuild();
        let want = st.active_doc().pos_of_line(2);

        st.outline_click(1);
        assert_eq!(
            st.caret().pos,
            want,
            "clicking item 2 → caret at the start of the ## 二 line"
        );
        assert_eq!(st.caret().anchor, None, "jump carries no selection");
        assert_eq!(st.active_doc().line_of_pos(st.caret().pos), 2);
        assert!(
            st.doc_focused,
            "keyboard focus returns to the document after the jump"
        );

        // out-of-bounds clicks are silently ignored (caret unmoved)
        let before = st.caret().pos;
        st.outline_click(99);
        assert_eq!(st.caret().pos, before);
    }

    /// Document-view jump block resolution: outline item `n` maps to the n-th
    /// top-level heading block; headings inside quotes are skipped (they are
    /// not outline items) and a drifted seq falls back to the nearest heading.
    ///
    /// Note the block indices are *not* the source line numbers: empty lines
    /// are empty blocks in the wys model, so the assertions are structural
    /// (which block, which heading text) instead of hand-counted indices.
    #[test]
    fn wys_heading_block_maps_items_to_blocks() {
        // blocks: H1 "一" / para / H2 "二" / para / H3 "三" (inside a quote —
        // not an outline item) / list items (Li blocks, no Head)
        let doc = wysiwyg::parse_doc("# 一\n\ntext\n\n## 二\n\ntext\n\n> ### 三\n\n- item\n");
        let quoted = doc
            .blocks
            .iter()
            .position(|b| b.plain_text().contains("三"))
            .expect("the quoted heading block exists");

        // item 0 → the H1 block; item 1 → the H2 block
        let b0 = super::wys_heading_block(&doc, 0).expect("item 0 maps");
        assert!(matches!(doc.blocks[b0].core(), wysiwyg::Block::Head { .. }));
        assert!(doc.blocks[b0].plain_text().starts_with("一"));
        let b1 = super::wys_heading_block(&doc, 1).expect("item 1 maps");
        assert!(doc.blocks[b1].plain_text().starts_with("二"));
        // the quoted heading is never a jump target of a real outline item
        assert_ne!(b0, quoted);
        assert_ne!(b1, quoted);

        // drifted seq → the nearest heading block (still a Head)
        let near = super::wys_heading_block(&doc, 99).expect("fallback");
        assert!(matches!(
            doc.blocks[near].core(),
            wysiwyg::Block::Head { .. }
        ));

        // a document without headings: no jump target at all
        let plain = wysiwyg::parse_doc("just text\n");
        assert_eq!(super::wys_heading_block(&plain, 0), None);
    }

    /// Cache invalidated by gen: rebuild needed after edits, back to steady
    /// state after rebuilding (zero-write render premise).
    #[test]
    fn outline_cache_refreshes_on_edit_and_converges() {
        let mut st = EditorState::minimal();
        st.tabs[0].doc = crate::buffer::TextDocument::from("# a\n");
        st.set_language_by_name("Markdown");
        assert!(st.outline_build_needed(), "no cache → needs building");
        st.outline_rebuild();
        assert!(
            !st.outline_build_needed(),
            "back to steady state after building"
        );
        assert_eq!(st.outline_items().len(), 1);

        let end = st.active_doc().total_bytes();
        st.active_doc_mut().insert_text(end, "\n## b\n");
        assert!(st.outline_build_needed(), "gen changed → rebuild needed");
        st.outline_rebuild();
        assert!(!st.outline_build_needed());
        assert_eq!(st.outline_items().len(), 2, "new heading appears");
    }

    /// Non-Markdown documents never parse (the panel shows an empty state); only Markdown builds a cache.
    #[test]
    fn non_markdown_documents_never_build_an_outline() {
        let mut st = EditorState::minimal();
        st.tabs[0].doc = crate::buffer::TextDocument::from("# 不是 md\n");
        assert!(!st.outline_build_needed(), "Plain Text does not parse");
        st.outline_rebuild();
        assert!(st.outline_items().is_empty());

        st.set_language_by_name("Markdown");
        assert!(st.outline_build_needed());
        st.outline_rebuild();
        assert_eq!(st.outline_items().len(), 1);
    }

    /// Oversized documents skip parsing and are only flagged (the panel shows a notice); the cache likewise reaches gen-keyed steady state.
    #[test]
    fn oversized_documents_are_marked_not_parsed() {
        let mut st = EditorState::minimal();
        let big = "# 头\n".to_string() + &"x\n".repeat((OUTLINE_MAX_BYTES / 2) as usize + 2);
        st.tabs[0].doc = crate::buffer::TextDocument::from(big.as_str());
        st.set_language_by_name("Markdown");
        st.outline_rebuild();
        assert!(
            st.outline_too_large(),
            "oversized documents should be flagged"
        );
        assert!(st.outline_items().is_empty());
        assert!(
            !st.outline_build_needed(),
            "back to steady state after flagging"
        );
    }

    /// Panel toggle: toggle_outline expands the sidebar and switches to the
    /// outline panel; triggering again collapses.
    #[test]
    fn toggle_outline_shows_and_hides_the_panel() {
        let mut st = EditorState::minimal();
        st.set_window_size(1000.0, 800.0);
        st.set_language_by_name("Markdown");
        assert_eq!(st.sidebar_w(), 0.0);

        st.toggle_outline();
        assert!(st.outline_panel_shown());
        assert!(
            st.sidebar_w() > 0.0,
            "the outline panel occupies sidebar width"
        );
        assert!(
            (st.view.x - st.sidebar_w()).abs() < 0.01,
            "the viewport yields space"
        );

        st.toggle_outline();
        assert!(!st.outline_panel_shown());
        assert_eq!(
            st.sidebar_w(),
            0.0,
            "no longer takes width after collapsing"
        );
    }

    /// Outline is Markdown-only: non-Markdown documents only show a notice, no empty panel.
    #[test]
    fn toggle_outline_is_markdown_only() {
        let mut st = EditorState::minimal();
        st.set_window_size(1000.0, 800.0);
        assert!(!st.outline_supported());
        st.toggle_outline();
        assert!(!st.outline_panel_shown(), "no panel for non-Markdown");
        assert_eq!(st.sidebar_w(), 0.0);
        let key = st.status.as_ref().expect("status set").key();
        assert!(
            key.contains("StatusOutlineUnsupported"),
            "should state Markdown-only support: {key}"
        );

        // switch to a Markdown document: the toggle takes effect (the panel state stayed "off" within the same status)
        st.set_language_by_name("Markdown");
        st.toggle_outline();
        assert!(st.outline_panel_shown());
    }
}
