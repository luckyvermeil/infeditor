//! Bidirectional coordinate queries: screen point → `Pos`; `Pos` → caret/
//! selection rect.
//!
//! Everything accesses `BaseDocument` synchronously via `NodeHandle` (the
//! front door of `dioxus-native-dom`, see design doc §1). All reads go through
//! `try_doc()` — during rendering blitz may hold a mutable borrow, and when
//! `try_doc()` returns `None` the caller skips this query (the next frame
//! naturally retries).
//!
//! ## Coordinate-system notes (all confirmed by observation; read before
//! changing)
//!
//! - `find_text_position(x, y)`: **document viewport coordinates** (the
//!   window's content-area CSS px), returning `(inline_root_id, byte offset
//!   within that inline root's text)`.
//! - parley `Cursor::geometry(layout, w)`: **relative to the inline root's
//!   content-box top-left**, with y already including the line position; `w` is
//!   the caret width.
//! - `TextLayout.block_offset`: the block-wise offset caused by
//!   `align-content`; it must be added.
//! - This module's `doc_position`: **document content space** coordinates —
//!   accumulated along the `containing_block` chain by adding
//!   `unrounded_layout().location`, **without subtracting scroll** (copied from
//!   blitz's `physical_unrounded_geometry`; it is `pub(crate)` and unavailable,
//!   so the ~10 lines are written by hand). The caret is absolutely positioned
//!   inside the scrolling container, so the document coordinates scroll with
//!   the content ✓.
//! - A block's content-box origin = border-box origin + `border` + `padding`
//!   (taffy 0.14's `Layout` fields are directly readable).
//!
//! ## Virtual bytes (the exception to convention 3: images / math)
//!
//! The plain text that `Pos.off` measures includes Image.alt / Math source
//! bytes, but the DOM's `<img>`/`<svg>` have no text node — so layout bytes and
//! plain bytes drift by `vlen` after an atom node. render writes `data-vlen`
//! on atom elements, and this module's two mapping functions
//! (`layout_to_plain` / `plain_to_layout`) reconcile the two byte ledgers for
//! text nodes and atom nodes along the pre-order traversal of data-bid
//! elements. When the caret lands inside an atom's range it snaps to its front
//! edge (the semantics of `plain_to_layout`).

use std::rc::Rc;

use blitz_dom::{BaseDocument, ElementData};
use dioxus_native::NodeHandle;
use parley::{Affinity, BreakReason, Cursor};

use crate::dom_index::attr_usize;
use crate::model::{Block, Doc, Pos, Sel};

/// Caret rect (document content space, CSS px).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CaretRect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

/// Overlay rect (**relative to `.wys-root`'s content coordinates**, CSS px):
/// shared by the caret and the focused-cell highlight. The position is
/// structured data rather than a pre-spliced CSS string — the IME anchor /
/// floating panels read the fields directly, and only the render layer calls
/// [`css`](Self::css) once.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct OverlayRect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl OverlayRect {
    /// Inline style (position and size; `position`/colors etc. come from the
    /// CSS class).
    pub fn css(&self) -> String {
        format!(
            "left:{:.1}px;top:{:.1}px;width:{:.1}px;height:{:.1}px;",
            self.x, self.y, self.w, self.h
        )
    }
}

/// Caret width (CSS px). Also passed to parley's `Cursor::geometry`.
const CARET_W: f32 = 2.0;
/// Minimum caret height: an empty line / zero-line layout can yield height 0.
const MIN_CARET_H: f32 = 16.0;
/// Paragraph gap used by the empty-block fallback extrapolation (same order as
/// .wys-content's paragraph margin).
const EMPTY_GAP: f32 = 16.0;
/// Single-line height used by the empty-block fallback extrapolation
/// (min-height 22 + line height).
const EMPTY_LINE_H: f32 = 24.0;

/// Geometry-query facade for a Blitz document. Cheap to clone (internal `Rc`).
#[derive(Clone)]
pub struct Geom {
    handle: Rc<NodeHandle>,
}

impl Geom {
    pub fn new(handle: impl Into<Rc<NodeHandle>>) -> Self {
        Self {
            handle: handle.into(),
        }
    }

    /// Screen point → logical position. `x/y` are the window content area's CSS
    /// px (the semantics of dioxus's `client_coordinates()`).
    pub fn pos_at(&self, x: f32, y: f32, model: &Doc) -> Option<Pos> {
        let doc = self.handle.try_doc()?;
        let (nid, byte) = doc.find_text_position(x, y)?;
        // The inline root normally carries data-bid; walking up is defensive
        // against "an inline root with no marker" (convention 1 guarantees this
        // should not happen).
        let mut cur = Some(nid);
        while let Some(id) = cur {
            let node = doc.get_node(id)?;
            if let Some(el) = node.data.downcast_element() {
                if let Some(bid) = attr_usize(el, "data-bid") {
                    // Layout bytes → plain bytes (the atom node's virtual-byte
                    // mapping)
                    let plain = layout_to_plain(&doc, id, byte)
                        .unwrap_or_else(|| block_plain_len(&doc, id));
                    return Some(Pos::new(bid, plain));
                }
            }
            cur = node.parent;
        }
        // We hit an inline root but no ancestor in the whole chain carries
        // data-bid — a hanging span like a list marker / checkbox is its own
        // inline root (convention 2); clicking it used to return None and the
        // ui layer's fallback would throw the caret to the end of the document.
        // Instead land on the nearest block start by geometry. When
        // find_text_position itself missed (blank content area) it still returns
        // None — the `?` above already lets that through; this only covers the
        // "hit but no block number" case.
        nearest_block_at(&doc, model, self.handle.node_id(), x, y).map(|bid| Pos::new(bid, 0))
    }

    /// Every block's current **content height**, indexed by block number (0 for a
    /// block the DOM does not know). Measured just before the source expansion
    /// changes the shapes, see `render::set_height_floors`.
    pub fn block_heights(&self, model: &Doc) -> Vec<f32> {
        let mut out = vec![0.0; model.blocks.len()];
        let Some(doc) = self.handle.try_doc() else {
            return out;
        };
        let container = fragment_container(&doc, self.handle.node_id());
        crate::dom_index::with_index(&doc, container, |idx| {
            for (nid, bid) in idx.order() {
                if let Some(&(_, _, _, h)) = bid_content_rect(&doc, nid).as_ref() {
                    if let Some(slot) = out.get_mut(bid) {
                        *slot = h;
                    }
                }
            }
        });
        out
    }

    /// Drag-time mapping: the block under the pointer's **row**, then the offset
    /// inside it — never "the text nearest to this point".
    ///
    /// [`Geom::pos_at`] answers "which text is closest", and that answer flips when
    /// the **source expansion** changes a block's shape: dragging into the blank
    /// area beside a table alternated between the table's cell and the paragraph
    /// above (the bordered table is wide, the source table is narrow), so the table
    /// flickered between typeset and source on every pointer move. Here the block is
    /// chosen by the pointer's vertical band (ties resolved by horizontal distance,
    /// so a table row's *last* cell wins), and a point past that block's right edge
    /// means "end of the block" — the same answer in both shapes, so the sweep can
    /// not oscillate.
    ///
    /// A table is one unit at its **outer edges** (see [`table_edge_pos`]): a point
    /// past the top/bottom of the table belongs to the table as a whole — its first
    /// cell's start / last cell's end — whatever column the pointer is over. The
    /// per-cell band would otherwise stop the sweep at the cell under the pointer's
    /// x, and a table that ends the document (no block below to reach) could not be
    /// selected past that cell: the last character was unreachable.
    pub fn drag_pos_at(&self, x: f32, y: f32, model: &Doc) -> Option<Pos> {
        let doc = self.handle.try_doc()?;
        let (dx, dy) = client_to_content(&doc, self.handle.node_id(), x, y);
        let container = fragment_container(&doc, self.handle.node_id());
        // A point **over a typeset formula's box** is answered by the atom itself:
        // the box has no text, and the nearest-text rule flips between the texts on
        // either side of it as the pointer crosses (`math_front_at`).
        if let Some(p) = Self::math_front_at(&doc, model, container, dx, dy) {
            return Some(p);
        }
        // Candidates: caret-addressable blocks, keyed by (vertical distance,
        // horizontal distance) — containment on both axes wins over "near in y".
        let hit = crate::dom_index::with_index(&doc, container, |idx| {
            let mut best: Option<DragHit> = None;
            for (nid, bid) in idx.order() {
                // `caret_addressable` also filters Code blocks (they are reached
                // through the text hit instead of the empty-block fallback), but a
                // drag over a code block must extend the selection: only a rule,
                // which has no text at all, is skipped here.
                let Some(b) = model.blocks.get(bid) else {
                    continue;
                };
                if matches!(b.core(), Block::Hr) {
                    continue;
                }
                let Some((x0, y0, w, h)) = bid_content_rect(&doc, nid) else {
                    continue;
                };
                let ddy = (y0 - dy).max(0.0).max(dy - (y0 + h));
                let ddx = (x0 - dx).max(0.0).max(dx - (x0 + w));
                if best.is_none_or(|(by, bx, ..)| (ddy, ddx) < (by, bx)) {
                    best = Some((ddy, ddx, bid, nid, x0, y0, x0 + w, y0 + h));
                }
            }
            best
        });
        let (_, _, bid, nid, left, top, right, bottom) = hit?;
        // Past the table's top/bottom the table wins as a whole — checked before
        // `pos_at`, which answers "the text nearest the point" and would stop at the
        // cell under the pointer's column (a mid-table position).
        if let Some((t_top, t_bottom, first, last)) = table_of(&doc, model, nid, bid) {
            let in_last_row = model
                .blocks
                .get(bid)
                .zip(model.blocks.get(last))
                .is_some_and(|(at, end)| same_row(at, end));
            if let Some(p) = table_edge_pos(
                model,
                first,
                last,
                (t_top, t_bottom),
                bottom,
                in_last_row,
                dy,
            ) {
                return Some(p);
            }
        }
        // A point inside the block: the exact character under the pointer (this is
        // the normal "dragging over text" case, unchanged).
        if let Some(p) = self.pos_at(x, y, model) {
            if p.block == bid {
                return Some(p);
            }
        }
        // Otherwise clamp to the block: above it → its start, below it → its end,
        // past its right edge → its end, before it → its start; a point inside the
        // box but on blank space (cell padding) takes the nearer end by x.
        let len = model.blocks.get(bid)?.plain_len();
        let off = if dy < top || dx < left {
            0
        } else if dy > bottom || dx >= (left + right) / 2.0 {
            // Below the block, past its right edge, or inside it past the halfway
            // point: the end.
            len
        } else {
            0
        };
        Some(Pos::new(bid, off))
    }

    /// Screen point → empty-text-block hit (returns the block number). An empty
    /// paragraph / list item / table cell has no text node, so
    /// `find_text_position` (vendor patch four's nearest_text_descendant
    /// fallback) would land the click on a neighboring paragraph's text and
    /// misplace the caret — clicking an empty block must hit directly by
    /// geometry.
    ///
    /// The set of addressable blocks is decided by **model block type**
    /// (`caret_addressable`) — it used to be filtered by the presence of
    /// `inline_layout_data`, but after a rebuild whether an inline layout is
    /// built is unreliable, and a text-less shell like Hr would be
    /// mis-hit (the caret could neither enter nor be drawn).
    pub fn empty_block_at(&self, x: f32, y: f32, model: &Doc) -> Option<usize> {
        let doc = self.handle.try_doc()?;
        // Convert once (the old implementation was inside the per-block loop,
        // re-walking the ancestor chain for every candidate block).
        let (dx, dy) = client_to_content(&doc, self.handle.node_id(), x, y);
        let container = fragment_container(&doc, self.handle.node_id());
        crate::dom_index::with_index(&doc, container, |idx| {
            for (nid, bid) in idx.order() {
                if !caret_addressable(model.blocks.get(bid)) {
                    continue;
                }
                if has_text_node(&doc, nid) {
                    continue;
                }
                let Some((x0, y0, w, h)) = bid_content_rect(&doc, nid) else {
                    continue;
                };
                // Widen by 4px above and below: a click in the paragraph-margin
                // gap also counts as a hit (a click between two paragraphs should
                // land on the nearer block, not on nearest_text_descendant's
                // neighbor).
                if dx >= x0 && dx <= x0 + w && dy >= y0 - 4.0 && dy <= y0 + h + 4.0 {
                    return Some(bid);
                }
            }
            None
        })
    }

    /// Screen point → **math formula** hit (inline and block-level unified).
    ///
    /// A formula is an atom node (`<span data-math data-vlen>`) with no text
    /// node, so `find_text_position` lands on neighboring text/paragraphs
    /// (observed: clicking a block formula jumps the caret to the end of the
    /// next paragraph). Here we hit-test the `data-math` element's geometric
    /// rectangle: a click on the left half → the formula source start, on the
    /// right half → the source end (inserting on either side does not break the
    /// formula — emit's `X$…$` / `$…$X` both re-parse).
    ///
    /// DOM order matches the in-block [`Block::math_spans`] order, so pairing by
    /// ordinal works — **multiple formulas in one block** (inline formulas
    /// included) all hit; the old implementation only recognized "a single
    /// display Math occupying its own line", so clicking an inline formula's
    /// right side could only land on its left.
    pub fn math_at(&self, x: f32, y: f32, model: &Doc) -> Option<Pos> {
        let doc = self.handle.try_doc()?;
        // Fast short-circuit for a document without formulas: a click is a hot
        // path; do not walk the whole tree for ordinary text.
        if !model.blocks.iter().any(Block::has_math) {
            return None;
        }
        let (dx, dy) = client_to_content(&doc, self.handle.node_id(), x, y);
        let container = fragment_container(&doc, self.handle.node_id());
        crate::dom_index::with_index(&doc, container, |idx| {
            // Walk blocks in DOM pre-order; in-block formulas are in render order
            // (= Block::math_spans order).
            for (_, bid) in idx.order() {
                let Some(nodes) = idx.math(bid) else { continue };
                let Some(b) = model.blocks.get(bid) else {
                    continue;
                };
                let spans = b.math_spans();
                for (ordinal, &nid) in nodes.iter().enumerate() {
                    let Some(&(start, end)) = spans.get(ordinal) else {
                        continue;
                    };
                    // Only a **typeset** formula is an object. A formula whose
                    // source is shown (`data-math-src`) and one that failed to
                    // typeset (`<code data-math>`, no `data-vlen`) carry real text:
                    // the click must land on the character under the pointer, not
                    // snap to the formula's edges — that is the whole point of
                    // expanding it, and `pos_at` is exact there.
                    if !math_is_atom(&doc, nid) {
                        continue;
                    }
                    let Some((x0, y0, w, h)) = math_rect(&doc, nid) else {
                        continue;
                    };
                    const PAD: f32 = 2.0;
                    if dx >= x0 - PAD && dx <= x0 + w + PAD && dy >= y0 - PAD && dy <= y0 + h + PAD
                    {
                        return Some(Pos::new(bid, if dx < x0 + w / 2.0 { start } else { end }));
                    }
                }
            }
            None
        })
    }

    /// Drag-time mapping over a **typeset formula**: a point over its box — the
    /// carrier's box spans the whole line, not just the ink — → the caret just
    /// **before** the formula (its front edge).
    ///
    /// The box holds no text (the glyphs are an SVG), so `find_text_position`
    /// answers with a *neighbouring* text's edge — and which neighbour wins flips
    /// as the pointer crosses the box. That is the "the caret walks back while I
    /// drag across a formula" report: coming from the right the caret sat past the
    /// formula, and one step into its box it jumped back in front of it.
    ///
    /// A formula is an object: it takes no caret of its own, so while the pointer
    /// is anywhere over it the caret stays after the last thing **before** it, and
    /// only crossing the box's far edge moves it past. The box never changes shape
    /// (the atom stays typeset), so the answer cannot oscillate — the same property
    /// the row-band mapping gives tables.
    ///
    /// A formula **shown as source** (and one that failed to typeset: `<code
    /// data-math>`) holds real text and is left to `pos_at`, and an atom that
    /// covers its whole block (a display formula) is left to the band clamp, which
    /// already answers its start/end by half.
    fn math_front_at(
        doc: &BaseDocument,
        model: &Doc,
        container: blitz_dom::NodeId,
        dx: f32,
        dy: f32,
    ) -> Option<Pos> {
        // A click is a hot path, and so is every pointer move: no formula in the
        // document means no walk.
        if !model.blocks.iter().any(Block::has_math) {
            return None;
        }
        crate::dom_index::with_index(doc, container, |idx| {
            for (_, bid) in idx.order() {
                let Some(nodes) = idx.math(bid) else { continue };
                for (ordinal, &nid) in nodes.iter().enumerate() {
                    // Geometry first: only a formula whose box holds the pointer
                    // gets its spans built (a drag move sweeps every formula in
                    // the document, and `math_spans` allocates).
                    let Some((x0, y0, w, h)) = math_rect(doc, nid) else {
                        continue;
                    };
                    // The hit box is the **carrier's**, not the SVG's: an inline
                    // element's layout box carries the line's height (measured: a
                    // 15px `x+y` inks ~12px inside a ~24px box, ±6px of margin
                    // still hits), so a pointer a few pixels above/below the glyphs
                    // lands here too.
                    if w <= 0.0 || dx < x0 || dx > x0 + w || dy < y0 || dy > y0 + h {
                        continue;
                    }
                    // Only a typeset formula is an object: it holds no text, so
                    // the whole box answers with one position. A formula shown as
                    // source (`data-math-src`) and one that failed to typeset
                    // (`<code data-math>`) are real text — left to `pos_at`, and
                    // not an answer that ends the scan: a later formula may follow.
                    if !math_is_atom(doc, nid) {
                        continue;
                    }
                    let Some(b) = model.blocks.get(bid) else {
                        continue;
                    };
                    let Some(&(start, end)) = b.math_spans().get(ordinal) else {
                        continue;
                    };
                    if end <= start {
                        continue;
                    }
                    // A display formula is its block: the band clamp below already
                    // answers its start/end by half, and those are the only two
                    // positions the block has.
                    if start == 0 && end >= b.plain_len() {
                        continue;
                    }
                    return Some(Pos::new(bid, start));
                }
            }
            None
        })
    }

    /// Screen point → task checkbox hit (returns the block number).
    ///
    /// The checkbox is an absolutely positioned span outside data-bid
    /// (convention 2), and when unchecked its content is an NBSP (zero advance
    /// width) — `find_text_position`'s cluster hit often lands directly on the
    /// adjacent body text, and the hit chain naturally has no `data-task-bid`
    /// (observed: clicking the box never checks it). Here we instead do
    /// **geometric hit-testing**: iterate every `data-task-bid` element and do a
    /// rectangle-containment test on its border box (2px of slack on each side,
    /// same order as the empty-block tolerance).
    pub fn task_at(&self, x: f32, y: f32) -> Option<usize> {
        let doc = self.handle.try_doc()?;
        let (dx, dy) = client_to_content(&doc, self.handle.node_id(), x, y);
        let container = fragment_container(&doc, self.handle.node_id());
        crate::dom_index::with_index(&doc, container, |idx| {
            for (nid, bid) in idx.tasks() {
                let Some((x0, y0, w, h)) = content_box(&doc, nid) else {
                    continue;
                };
                const PAD: f32 = 2.0;
                if dx >= x0 - PAD && dx <= x0 + w + PAD && dy >= y0 - PAD && dy <= y0 + h + PAD {
                    return Some(bid);
                }
            }
            None
        })
    }

    /// Screen point → link URL (Ctrl+click-to-open depends on it).
    pub fn link_at(&self, x: f32, y: f32) -> Option<String> {
        let doc = self.handle.try_doc()?;
        let (nid, _) = doc.find_text_position(x, y)?;
        let mut cur = Some(nid);
        while let Some(id) = cur {
            let node = doc.get_node(id)?;
            if let Some(el) = node.data.downcast_element() {
                if let Some(u) = attr_str(el, "data-link") {
                    return Some(u);
                }
            }
            cur = node.parent;
        }
        None
    }

    /// Logical position → caret rect (document content space).
    ///
    /// Computed from "the previous frame's layout" (design doc §4.3): the old
    /// layout is exact when a click/move does not change the text; after a large
    /// replacement the caller recomputes next frame. An empty text block has no
    /// parley lines, and `geometry` falls back to a default box (h=0) — here we
    /// fall back to 16px.
    ///
    /// **Coordinate system (stepped on in practice)**: blitz's inline parley
    /// layout runs in **physical pixel** space (font size / line height /
    /// coordinates are all multiplied by `viewport.scale()`, and blitz multiplies
    /// back when painting text), while taffy's document coordinates
    /// (`doc_position`) are CSS px — coordinates derived from parley must be
    /// divided back by the scale, otherwise on a high-DPI / scaled window the
    /// caret is `scale` times larger than the text (demo scale=1 can never
    /// expose this).
    pub fn caret_rect(&self, pos: Pos) -> Option<CaretRect> {
        self.caret_rect_edge(pos, CaretEdge::Plain)
    }

    /// The caret rect of a **selection's focus**.
    ///
    /// A range paints the block's layout edge when it reaches the block's
    /// first/last plain byte (see [`selection_layout_span`]) — the virtual source
    /// markers at that edge are part of the copied text. The focus caret must sit
    /// on the **same** edge: a selection ending at a table's last cell showed the
    /// highlight past the trailing ` |` while the caret bar stopped in front of
    /// it, so the caret "could not reach the far side of the last character".
    /// A collapsed caret keeps the plain mapping (inside the markers — the
    /// `**bold|**` / `$x|$` convention).
    pub fn caret_rect_sel(&self, sel: Sel) -> Option<CaretRect> {
        let edge = {
            let doc = self.handle.try_doc()?;
            let end_len = self
                .block_node(&doc, sel.range().1.block)
                .map(|nid| block_plain_len(&doc, nid))
                .unwrap_or(usize::MAX);
            focus_caret_edge(sel, end_len)
        };
        self.caret_rect_edge(sel.focus, edge)
    }

    fn caret_rect_edge(&self, pos: Pos, edge: CaretEdge) -> Option<CaretRect> {
        let doc = self.handle.try_doc()?;
        let nid = self.block_node(&doc, pos.block)?;
        let node = doc.get_node(nid)?;
        let el = node.data.downcast_element()?;
        // Empty-text-block fallback: after blitz rebuilds the DOM it **may not
        // build inline_layout_data** for an empty paragraph (observed: Enter
        // makes an empty line → the fragment rebuilds → the empty block's
        // inline_layout=None), so every parley path is None — the caret is then
        // synthesized from the taffy box (the line height held up by min-height,
        // still valid after a rebuild), otherwise the caret CSS stays at its
        // previous position and ↑/↓ looks like it "skips" the empty line.
        // An empty paragraph may also have an inline_layout (an empty Layout,
        // zero lines). For a zero-line layout parley's cursor rect has a
        // **negative offset** (observed y=-40), so the computed caret would land
        // on the previous line. A zero-line layout always goes through geometric
        // synthesis ([`inline_caret`] returns None).
        let Some(c) = inline_caret(&doc, nid, el, pos.off, doc.viewport().scale(), edge) else {
            let container = fragment_container(&doc, self.handle.node_id());
            return empty_block_caret(&doc, container, nid, pos.block, pos.off);
        };
        let (bx, by) = doc_position(&doc, nid);
        let lay = node.unrounded_layout();
        Some(CaretRect {
            x: bx + lay.border.left + lay.padding.left + c.x,
            y: by + lay.border.top + lay.padding.top + c.y,
            w: CARET_W,
            h: c.h,
        })
    }

    /// Diagnostics (for INF_WYS_LOG): a full breakdown of the caret geometry —
    /// cursor rect, block document coordinates, padding, block_offset, per-line
    /// metrics. Same source as [`Self::caret_rect`] (the same `inline_caret`,
    /// otherwise the debug data would drift from the actual caret).
    pub fn caret_debug(&self, pos: Pos) -> Option<String> {
        let doc = self.handle.try_doc()?;
        let scale = doc.viewport().scale();
        let nid = self.block_node(&doc, pos.block)?;
        let node = doc.get_node(nid)?;
        let el = node.data.downcast_element()?;
        let (bx, by) = doc_position(&doc, nid);
        let lay = node.unrounded_layout();
        let Some(c) = inline_caret(&doc, nid, el, pos.off, scale, CaretEdge::Plain) else {
            return Some(format!(
                "scale={scale:.4} bid={} EMPTY-FALLBACK blkpos=({bx:.1},{by:.1}) \
                 box={:.1}x{:.1} pad=({:.1},{:.1}) border=({:.1},{:.1})",
                pos.block,
                lay.size.width,
                lay.size.height,
                lay.padding.left,
                lay.padding.top,
                lay.border.left,
                lay.border.top,
            ));
        };
        let layout = &el.inline_layout_data.as_ref()?.layout;
        let lines: Vec<String> = layout
            .lines()
            .map(|l| {
                let m = l.metrics();
                format!(
                    "[y={:.1} lh={:.1} blk={:.1}..{:.1} content={:.1}..{:.1} base={:.1}]",
                    m.offset / scale,
                    m.line_height / scale,
                    m.block_min_coord / scale,
                    m.block_max_coord / scale,
                    m.content_block_min_coord / scale,
                    m.content_block_max_coord / scale,
                    m.baseline / scale
                )
            })
            .collect();
        let runs: Vec<String> = layout
            .lines()
            .flat_map(|l| l.items())
            .filter_map(|item| match item {
                parley::PositionedLayoutItem::GlyphRun(gr) => {
                    let r = gr.run();
                    Some(format!(
                        "[fs={:.1} asc={:.1} desc={:.1}]",
                        r.font_size() / scale,
                        r.font_metrics().ascent / scale,
                        r.font_metrics().descent / scale
                    ))
                }
                _ => None,
            })
            .collect();
        Some(format!(
            "scale={scale:.4} bid={} lbyte={} caret=({:.1},{:.1}) h={:.1} \
             atomic_dx={:.1} blkpos=({:.1},{:.1}) pad=({:.1},{:.1}) border=({:.1},{:.1}) \
             blkoff={:.1} layh={:.1} lines={} runs={}",
            pos.block,
            plain_to_layout(&doc, nid, pos.off).unwrap_or(pos.off),
            c.x,
            c.y,
            c.h,
            c.atomic_dx,
            bx,
            by,
            lay.padding.left,
            lay.padding.top,
            lay.border.left,
            lay.border.top,
            c.block_offset,
            layout.height() / scale,
            lines.join(""),
            runs.join("")
        ))
    }

    /// In-block visual-line movement (the geometric implementation of Up/Down;
    /// a cross-block move returns None and the edit layer falls back).
    /// `up=false` moves down. `h_pos` is the vertical target column (content-box
    /// coordinate x, physical pixels, remembered in Editor's vertical chain) —
    /// when None the current caret x is used.
    ///
    /// Note that parley's `Cursor::previous_visual/next_visual` are the
    /// **previous/next character cluster in bidi visual order** (left/right
    /// movement), not the previous/next line — misusing this pair once made
    /// ↑/↓ move one cell horizontally (stepped on in practice).
    ///
    /// `Selection::previous_line/next_line` is also avoided: a newly created
    /// Selection's private `h_pos` is always None, and `move_to_line` falls back
    /// to the **current x** as the target column — after passing a line shorter
    /// than the target column or an empty paragraph, the target column is reset
    /// to the landing x, and further ↑/↓ drifts the column (most visible between
    /// empty paragraphs). Here the target column is self-hosted: line locating
    /// copies parley's `move_to_line` (line midpoint y + `Cursor::from_point`),
    /// and x is decided by the caller's chained h_pos.
    pub fn move_visual(&self, pos: Pos, up: bool, h_pos: Option<f32>) -> Option<Pos> {
        let doc = self.handle.try_doc()?;
        let nid = self.block_node(&doc, pos.block)?;
        let node = doc.get_node(nid)?;
        let el = node.data.downcast_element()?;
        let tl = el.inline_layout_data.as_ref()?;
        let layout = &tl.layout;
        let lbyte = plain_to_layout(&doc, nid, pos.off)?;
        // Same source as caret_rect: the same helper resolves affinity.
        let cur = Cursor::from_byte_index(layout, lbyte, resolved_affinity(layout, lbyte));
        // The visual line the focus is on (cursor geometry is f64 physical
        // pixels while line metrics are f32; unify to f64 before comparing)
        let geo = cur.geometry(layout, 0.0);
        let y_mid = (geo.y0 + geo.y1) / 2.0;
        let line_count = layout.lines().count();
        let last = line_count.saturating_sub(1);
        let line_ix = layout
            .lines()
            .position(|l| {
                let m = l.metrics();
                let (lo, hi) = (f64::from(m.block_min_coord), f64::from(m.block_max_coord));
                y_mid >= lo && y_mid < hi
            })
            .unwrap_or(last);
        if (up && line_ix == 0) || (!up && line_ix == last) {
            return None; // reached the top/bottom of the block → cross-block fallback
        }
        let target_ix = if up { line_ix - 1 } else { line_ix + 1 };
        let ty = {
            let mut lines = layout.lines();
            let line = lines.nth(target_ix)?;
            let m = line.metrics();
            (f64::from(m.block_min_coord) + f64::from(m.block_max_coord)) / 2.0
        };
        let x = h_pos.unwrap_or(geo.x0 as f32);
        let moved = Cursor::from_point(layout, x, ty as f32);
        let nbyte = moved.index();
        if nbyte == lbyte {
            // When the target line has no hit cluster (an empty line), parley's
            // `from_point` returns the whole layout's text_len (the headless test
            // `from_point_on_empty_line_falls_back_to_text_len` locks this
            // behavior), and at a fold point it equals the current byte — a
            // `return None` used to trigger the ui layer's cross-block fallback,
            // so ↑/↓ got thrown out of the block at an empty line in a code
            // block. Instead return the target line's endpoint (going up takes the
            // line end, going down the line start); when the endpoint equals the
            // current byte the ui layer treats it as not moved and the caret stays
            // in the block.
            let tr = layout.lines().nth(target_ix)?.text_range();
            let nbyte = if up { tr.end } else { tr.start };
            let plain = layout_to_plain(&doc, nid, nbyte)?;
            return Some(Pos::new(pos.block, plain));
        }
        let plain = layout_to_plain(&doc, nid, nbyte)?;
        Some(Pos::new(pos.block, plain))
    }

    /// The current caret's h_pos (content-box coordinate x, physical pixels) —
    /// used for same-column cross-block movement.
    pub fn h_pos(&self, pos: Pos) -> Option<f32> {
        let doc = self.handle.try_doc()?;
        let nid = self.block_node(&doc, pos.block)?;
        let node = doc.get_node(nid)?;
        let el = node.data.downcast_element()?;
        let tl = el.inline_layout_data.as_ref()?;
        let layout = &tl.layout;
        let lbyte = plain_to_layout(&doc, nid, pos.off)?;
        // Same source as caret_rect: the same helper resolves affinity.
        let cur = Cursor::from_byte_index(layout, lbyte, resolved_affinity(layout, lbyte));
        Some(cur.geometry(layout, 0.0).x0 as f32)
    }

    /// Cross-block visual-line movement: land on `to_block`'s target visual line
    /// (↑ = last line, ↓ = first line) at the position where x ≈ `h_pos`.
    /// `h_pos` is the x in the current block's content-box coordinates (physical
    /// pixels); across blocks it assumes each block's content-box left edges are
    /// aligned (paragraph padding is 0; a list indent introduces a one-indent
    /// — an acceptable approximation). Returns None when the target block has
    /// no lines (an empty block).
    pub fn move_visual_cross(&self, to_block: usize, up: bool, h_pos: f32) -> Option<Pos> {
        let doc = self.handle.try_doc()?;
        let nid = self.block_node(&doc, to_block)?;
        let node = doc.get_node(nid)?;
        let el = node.data.downcast_element()?;
        let tl = el.inline_layout_data.as_ref()?;
        let layout = &tl.layout;
        // The target line's vertical midpoint (line metrics and cursor geometry
        // are both physical pixels). The line iterator borrows the layout, so the
        // temporaries from last()/next() must live until the metrics are read.
        let y_mid = {
            let mut lines = layout.lines();
            let line = if up { lines.last()? } else { lines.next()? };
            let m = line.metrics();
            (f64::from(m.block_min_coord) + f64::from(m.block_max_coord)) / 2.0
        };
        let cur = Cursor::from_point(layout, h_pos, y_mid as f32);
        let plain = layout_to_plain(&doc, nid, cur.index())?;
        Some(Pos::new(to_block, plain))
    }

    /// The current visual line's start/end (the browser semantics of Home/End).
    ///
    /// Consistent with browsers/editors: on an explicit line break End lands
    /// **before the break character** (not at the next line's start); on a soft
    /// break End lands at the fold point (the next line's start, no character
    /// skipped). An empty block / zero-line layout returns `None` (the caller
    /// falls back to the block start/end).
    ///
    /// `model` is needed only to get the block's plain text (to decide whether
    /// the line ends with `\n`; layout bytes carry no character content); the
    /// byte conversion still goes through `layout_to_plain` (an atom node's
    /// virtual-byte ledger is undisturbed).
    pub fn line_edge(&self, pos: Pos, model: &Doc, to_start: bool) -> Option<Pos> {
        let doc = self.handle.try_doc()?;
        let nid = self.block_node(&doc, pos.block)?;
        let node = doc.get_node(nid)?;
        let el = node.data.downcast_element()?;
        let layout = &el.inline_layout_data.as_ref()?.layout;
        let lbyte = plain_to_layout(&doc, nid, pos.off)?;
        let range = visual_line_range(layout, lbyte)?;
        let target = if to_start { range.start } else { range.end };
        let mut plain = layout_to_plain(&doc, nid, target)?;
        if !to_start {
            // Explicit line break: text_range.end includes the `\n`, and End
            // should stop before it. Only when that `\n` belongs to *this*
            // visual line — on an empty line right after a break the `\n` is the
            // previous line's, and stepping over it moved the caret up a line
            // instead of leaving it in place.
            let line_start = layout_to_plain(&doc, nid, range.start)?;
            let text = model
                .blocks
                .get(pos.block)
                .map(|b| b.plain_text())
                .unwrap_or_default();
            if plain > line_start && text.as_bytes().get(plain - 1) == Some(&b'\n') {
                plain -= 1;
            }
        }
        Some(Pos::new(pos.block, plain))
    }

    /// The document-coordinate origin of the scrolling container's root (the
    /// element holding the `NodeHandle`). The caret's CSS coordinates =
    /// document coordinates − this origin (the root has no padding, see
    /// SHELL_CSS).
    pub fn root_origin(&self) -> Option<(f32, f32)> {
        let doc = self.handle.try_doc()?;
        Some(doc_position(&doc, self.handle.node_id()))
    }

    /// A block element's **border-box** rect (document content space, CSS px):
    /// `(x, y, w, h)`. Used by the table-cell highlight overlay (the td's whole
    /// cell rect). taffy layout is CSS px, in the same unit as `doc_position` and
    /// **not divided by scale** (only parley-derived coordinates are). Returns
    /// None on out-of-range / missing node.
    pub fn block_border_rect(&self, bid: usize) -> Option<(f32, f32, f32, f32)> {
        let doc = self.handle.try_doc()?;
        let nid = self.block_node(&doc, bid)?;
        let node = doc.get_node(nid)?;
        let lay = node.unrounded_layout();
        let (x, y) = doc_position(&doc, nid);
        Some((x, y, lay.size.width, lay.size.height))
    }

    fn block_node(&self, doc: &BaseDocument, bid: usize) -> Option<blitz_dom::NodeId> {
        node_for_bid(doc, fragment_container(doc, self.handle.node_id()), bid)
    }
}

/// Block number → DOM node id (shared by selection push-back, caret geometry,
/// and host diagnostics).
///
/// Goes through `crate::dom_index`'s segmented incremental index: after a
/// structural change only the segment containing the dirty parent is rebuilt
/// first (one keystroke = one segment); only when that fails is one full rebuild
/// done, after which the hot path is O(1). The old implementation did a
/// full-tree DFS on every call — on a large document every keystroke paid a
/// full traversal.
pub fn node_for_bid(
    doc: &BaseDocument,
    container: blitz_dom::NodeId,
    bid: usize,
) -> Option<blitz_dom::NodeId> {
    crate::dom_index::with_index(doc, container, |idx| idx.bid(bid))
}

/// The fragment container: the child element under the handle (`.wys-root`)
/// whose class contains `wys-content` — the render layer's `display:contents`
/// fragment wrappers all hang under it (the **segmentation boundary** of the
/// segmented incremental index). On the first frame, before it is mounted / on
/// an abnormal structure, fall back to the handle itself: the index semantics
/// are unchanged, the whole tree is just treated as one coarse segment.
pub fn fragment_container(doc: &BaseDocument, handle: blitz_dom::NodeId) -> blitz_dom::NodeId {
    doc.get_node(handle)
        .and_then(|node| {
            node.children.iter().copied().find(|&cid| {
                doc.get_node(cid)
                    .and_then(|child| child.data.downcast_element())
                    .is_some_and(|el| {
                        el.attrs().iter().any(|a| {
                            &*a.name.local == "class"
                                && a.value.split_whitespace().any(|c| c == "wys-content")
                        })
                    })
            })
        })
        .unwrap_or(handle)
}

/// A formula element's hit rect (document content space, CSS px). The atom
/// span's own taffy box may be zero-sized (inline elements are typeset by
/// parley), so fall back to its inner `<svg>`'s box.
fn math_rect(doc: &BaseDocument, nid: blitz_dom::NodeId) -> Option<(f32, f32, f32, f32)> {
    if let Some(r) = bid_content_rect(doc, nid) {
        return Some(r);
    }
    let sid = first_svg(doc, nid)?;
    let node = doc.get_node(sid)?;
    let lay = node.unrounded_layout();
    if lay.size.width <= 0.0 || lay.size.height <= 0.0 {
        return None;
    }
    let (x, y) = doc_position(doc, sid);
    Some((x, y, lay.size.width, lay.size.height))
}

/// Whether a `data-math` node is a **typeset atom**: the carrier span written by
/// [`crate::render`] for a formula that rendered (`data-vlen`, no text of its
/// own). The source form (`data-math-src`) and the failed-parse `<code
/// data-math>` fallback hold real text, so the formula hit-tests must stand aside
/// for them and let the text mapping answer.
fn math_is_atom(doc: &BaseDocument, nid: blitz_dom::NodeId) -> bool {
    doc.get_node(nid)
        .and_then(|n| n.data.downcast_element().and_then(atomic_vlen))
        .is_some()
}

/// Whether a model block type can accept the caret: `Hr` is a text-less shell
/// (the caret can neither enter nor be drawn), and an empty `Code` block does
/// not participate in geometric clicking (excluded per the task agreement; to
/// allow it, delete this branch). `None` (bid out of range / DOM out of sync
/// with the model) is never addressable.
fn caret_addressable(b: Option<&Block>) -> bool {
    !matches!(b, None | Some(Block::Hr | Block::Code { .. }))
}

/// Client coordinates → document content space (the shared conversion for hit
/// testing).
///
/// The only relationship between the two coordinate sets is scrolling:
/// `doc_position` accumulates **document coordinates** (already including
/// `.wys-root`'s position in the window, e.g. y=96), while an event's
/// `client_coordinates()` are **window client coordinates** — the two are
/// pixel-identical when unscrolled. So the conversion only adds the scrolling
/// container offsets along the way (scrolling changes the viewport, not the
/// document coordinates).
///
/// The old implementation also added the root origin `doc_position(root)`,
/// effectively counting y=96 twice: empty-block clicks / list-marker landings
/// (`nearest_block_at`) and task-checkbox hits were all offset 96px downward,
/// so clicking a checkbox never hit (observed: the box would not toggle; the
/// geometric rect was at y=117 but the converted point was y=218).
/// scroll_offset is CSS px; taffy sizes are also CSS px and are **not divided
/// by scale** (only parley-derived coordinates are).
fn client_to_content(doc: &BaseDocument, root: blitz_dom::NodeId, x: f32, y: f32) -> (f32, f32) {
    let mut sx = 0.0f32;
    let mut sy = 0.0f32;
    let mut cur = Some(root);
    while let Some(id) = cur {
        let Some(n) = doc.get_node(id) else { break };
        sx += n.scroll_offset().x as f32;
        sy += n.scroll_offset().y as f32;
        cur = n.parent;
    }
    (x + sx, y + sy)
}

/// An element's content-box rect (document content space, `(x0, y0, w, h)`)
/// — sign is not judged; the caller decides usability as needed (hit testing
/// requires > 0, the empty-block fallback just needs the position).
fn content_box(doc: &BaseDocument, nid: blitz_dom::NodeId) -> Option<(f32, f32, f32, f32)> {
    let node = doc.get_node(nid)?;
    let (bx, by) = doc_position(doc, nid);
    let lay = node.unrounded_layout();
    Some((
        bx + lay.border.left + lay.padding.left,
        by + lay.border.top + lay.padding.top,
        lay.size.width - lay.border.left - lay.border.right - lay.padding.left - lay.padding.right,
        lay.size.height - lay.border.top - lay.border.bottom - lay.padding.top - lay.padding.bottom,
    ))
}

/// A candidate for [`Geom::drag_pos_at`]: `(row gap, column gap, block number,
/// DOM node, content x0, y0, x1, y1)` — the gaps are the ordering key.
type DragHit = (f32, f32, usize, blitz_dom::NodeId, f32, f32, f32, f32);

/// The **layout byte range** a model selection paints between two block nodes.
///
/// `plain_to_layout` is the caret mapping: a plain `0` is drawn *inside* the
/// opening source marker (`**|bold**`) and a plain `len` sits *before* the
/// closing one (`**bold|**`). For a **range** those two edges are wrong: the
/// markers are part of the copied text (`selected_markdown` writes them), so a
/// selection that reaches a block's first/last plain byte must paint the block's
/// layout edge — otherwise the marker at the edge is highlighted by nothing:
/// with a table ending the document, "select past it" stopped in front of the
/// trailing `|`, its last visible character.
///
/// A **collapsed** selection keeps the plain mapping — a caret has to stay
/// inside the markers, and widening it would paint a sliver at the caret.
pub(crate) fn selection_layout_span(
    doc: &BaseDocument,
    sn: blitz_dom::NodeId,
    en: blitz_dom::NodeId,
    start: usize,
    end: usize,
    collapsed: bool,
) -> Option<(usize, usize)> {
    if collapsed {
        return Some((
            plain_to_layout(doc, sn, start)?,
            plain_to_layout(doc, en, end)?,
        ));
    }
    let s = if start == 0 {
        0
    } else {
        plain_to_layout(doc, sn, start)?
    };
    let e = if end >= block_plain_len(doc, en) {
        // `usize::MAX` = the block's total layout length, i.e. past the closing
        // markers (see `plain_to_layout`).
        plain_to_layout(doc, en, usize::MAX)?
    } else {
        plain_to_layout(doc, en, end)?
    };
    Some((s, e))
}

/// A block element's content-box rect; returns None when the box is not laid
/// out yet (width/height ≤ 0) — hit testing would rather miss than
/// mis-hit.
fn bid_content_rect(doc: &BaseDocument, nid: blitz_dom::NodeId) -> Option<(f32, f32, f32, f32)> {
    content_box(doc, nid).filter(|&(_, _, w, h)| w > 0.0 && h > 0.0)
}

/// The `<table>` ancestor of a node, with its content box (the table's box). A
/// drag past this box belongs to the table as a whole — see [`table_edge_pos`].
/// `None` when the node is not inside a laid-out table.
fn table_box(doc: &BaseDocument, mut nid: blitz_dom::NodeId) -> Option<(f32, f32, f32, f32)> {
    for _ in 0..8 {
        let node = doc.get_node(nid)?;
        if let Some(el) = node.data.downcast_element() {
            if &*el.name.local == "table" {
                return content_box(doc, nid).filter(|&(_, _, w, h)| w > 0.0 && h > 0.0);
            }
        }
        nid = node.parent?;
    }
    None
}

/// The table a cell belongs to: its vertical band `(top, bottom)` plus the block
/// numbers of its first and last cell. `None` for a block that is not a cell (or
/// a cell with no laid-out `<table>` ancestor — an ungrouped `Td` rendered as a
/// paragraph).
fn table_of(
    doc: &BaseDocument,
    model: &Doc,
    nid: blitz_dom::NodeId,
    bid: usize,
) -> Option<(f32, f32, usize, usize)> {
    let seq = model.blocks.get(bid)?.table_seq()?;
    let (_, y, _, h) = table_box(doc, nid)?;
    // A table's cells are contiguous in the model (parse/emit order), so the ends
    // are the run of blocks sharing the `table_seq`.
    let mut first = bid;
    while first > 0 && model.blocks.get(first - 1).and_then(Block::table_seq) == Some(seq) {
        first -= 1;
    }
    let mut last = bid;
    while last + 1 < model.blocks.len()
        && model.blocks.get(last + 1).and_then(Block::table_seq) == Some(seq)
    {
        last += 1;
    }
    Some((y, y + h, first, last))
}

/// Whether two blocks are cells of the **same table row**.
fn same_row(a: &Block, b: &Block) -> bool {
    match (a.core(), b.core()) {
        (Block::Td { row: ra, .. }, Block::Td { row: rb, .. }) => ra == rb,
        _ => false,
    }
}

/// The model position a drag takes when the pointer is **outside the table's
/// rows**: past its top, past its bottom/right (the drag left the block below the
/// last row), or below the last row's content box (a cell's bottom padding, the
/// source form's delimiter row).
///
/// A table is one unit for the reader, so the table's edge wins over the cell
/// under the pointer's column: the per-cell band used to stop the sweep in the
/// cell whose x the pointer was over, and a table that **ends the document** (no
/// block below ever being reached) could not be selected past that cell — its
/// last character was unreachable. Symmetrically a drag above a leading table now
/// reaches its first cell's start.
///
/// `None` = the pointer is inside the table's rows; the ordinary per-cell mapping
/// applies (the cell under the pointer, its nearer end by x).
fn table_edge_pos(
    model: &Doc,
    first: usize,
    last: usize,
    bounds: (f32, f32),
    row_bottom: f32,
    in_last_row: bool,
    dy: f32,
) -> Option<Pos> {
    let (top, bottom) = bounds;
    if dy < top {
        return Some(Pos::new(first, 0));
    }
    if dy > bottom || (in_last_row && dy > row_bottom) {
        return Some(Pos::new(last, model.blocks.get(last)?.plain_len()));
    }
    None
}

/// The nearest caret-addressable block to a point (the geometric fallback when
/// `pos_at` yields no block number). Among candidate blocks' content-box
/// rects, take the one with the **smallest point-to-rect distance** (0 when the
/// point is inside; in y, `empty_block_at`'s ±4px widening also applies), and
/// land the caret at the block start.
fn nearest_block_at(
    doc: &BaseDocument,
    model: &Doc,
    root: blitz_dom::NodeId,
    x: f32,
    y: f32,
) -> Option<usize> {
    let (dx, dy) = client_to_content(doc, root, x, y);
    let container = fragment_container(doc, root);
    crate::dom_index::with_index(doc, container, |idx| {
        let mut best: Option<(f32, usize)> = None;
        for (nid, bid) in idx.order() {
            if !caret_addressable(model.blocks.get(bid)) {
                continue;
            }
            let Some((x0, y0, w, h)) = bid_content_rect(doc, nid) else {
                continue;
            };
            let ddx = (x0 - dx).max(0.0).max(dx - (x0 + w));
            let ddy = (y0 - 4.0 - dy).max(0.0).max(dy - (y0 + h + 4.0));
            let d2 = ddx * ddx + ddy * ddy;
            if best.is_none_or(|(bd, _)| d2 < bd) {
                best = Some((d2, bid));
            }
        }
        best.map(|(_, bid)| bid)
    })
}

/// The layout byte range of the **visual line** the caret is on. Applies to
/// both soft and explicit line breaks; when `lbyte` falls on a line boundary,
/// the line is chosen by affinity (same source as [`Geom::caret_rect`]).
/// Returns `None` for a zero-line layout / out of range.
fn visual_line_range<B: parley::style::Brush>(
    layout: &parley::Layout<B>,
    lbyte: usize,
) -> Option<std::ops::Range<usize>> {
    layout.lines().next()?;
    let cur = Cursor::from_byte_index(layout, lbyte, resolved_affinity(layout, lbyte));
    let geo = cur.geometry(layout, 0.0);
    let y_mid = (geo.y0 + geo.y1) / 2.0;
    let mut last = None;
    for line in layout.lines() {
        let m = line.metrics();
        let (lo, hi) = (f64::from(m.block_min_coord), f64::from(m.block_max_coord));
        last = Some(line.text_range());
        if y_mid >= lo && y_mid < hi {
            return last;
        }
    }
    // The caret is on a trailing empty line (no items; its geometry falls
    // beyond the last line): fall back to the last line.
    last
}

/// The caret's affinity at `lbyte`. A soft-break fold point (the previous
/// line's `text_range().end == lbyte` with break Regular/Emergency) takes
/// [`Affinity::Upstream`] (line-end wins — a caret at a fold point's line end
/// must not be drawn at the next line's start); a hard break (Explicit, where
/// the caret only feels right at the next line's start) and ordinary positions
/// take Downstream.
/// `caret_rect`/`caret_debug`/`move_visual`/`h_pos` all share this function —
/// caret display and ↑/↓ line judgment must use the same affinity, otherwise
/// pressing ↑ would "skip a line".
fn resolved_affinity<B: parley::style::Brush>(
    layout: &parley::Layout<B>,
    lbyte: usize,
) -> Affinity {
    for line in layout.lines() {
        let tr = line.text_range();
        if tr.end == lbyte
            && matches!(
                line.break_reason(),
                BreakReason::Regular | BreakReason::Emergency
            )
        {
            return Affinity::Upstream;
        }
    }
    Affinity::Downstream
}

/// The parley geometry of the in-block caret (the block content-box
/// coordinate system, CSS px). Shared by `caret_rect` and `caret_debug` —
/// computing it twice in two places will drift sooner or later (the old
/// implementation was copy-pasted).
struct InlineCaret {
    x: f32,
    y: f32,
    h: f32,
    /// The atom's rightward push (CSS px, diagnostics; already included in
    /// `x`).
    atomic_dx: f32,
    /// The block-wise offset caused by `align-content` (CSS px, already
    /// included in `y`).
    block_offset: f32,
}

/// Compute the in-block caret from **the previous frame's layout** (design doc
/// §4.3): the old layout is exact when a click/move does not change the text;
/// after a large replacement the caller recomputes next frame.
///
/// Returns None when there is no inline layout or there are zero lines (an
/// empty block) — for a zero-line layout parley's cursor rect has a
/// **negative offset** (observed y=-40), so it is always handed to
/// [`empty_block_caret`] to be synthesized from the taffy box.
///
/// **Coordinate system (stepped on in practice)**: blitz's inline parley layout
/// runs in **physical pixel** space (font size / line height / coordinates are
/// all multiplied by `viewport.scale()`), while taffy's document coordinates
/// are CSS px — coordinates derived from parley must be divided back by the
/// scale, otherwise on high DPI the caret is `scale` times larger than the text
/// (demo scale=1 can never expose this).
/// How a caret's plain offset maps to a layout byte. A **selection's** focus at
/// a block edge sits on the block's layout edge (the virtual source markers there
/// are painted by the selection, so the caret marks the far side of them); a
/// plain caret sits *inside* the markers (`**|bold**` / `$x|$`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum CaretEdge {
    Plain,
    /// Layout 0 — before the opening markers.
    BlockStart,
    /// The block's total layout length — after the closing markers.
    BlockEnd,
}

/// Which layout edge a **selection's focus caret** sits on, given the end
/// block's plain length. Pure so the rule can be pinned without a layout: a range
/// ending at a block's last plain byte marks the far side of the trailing markers
/// (`| 丙 | 丁 |` → after the last `|`), a range starting at plain 0 the near side
/// of the opening ones; a collapsed caret stays inside the markers.
fn focus_caret_edge(sel: Sel, end_block_plain_len: usize) -> CaretEdge {
    if sel.collapsed() {
        return CaretEdge::Plain;
    }
    let (start, end) = sel.range();
    if sel.focus == end {
        if end.off >= end_block_plain_len {
            CaretEdge::BlockEnd
        } else {
            CaretEdge::Plain
        }
    } else if sel.focus == start && start.off == 0 {
        CaretEdge::BlockStart
    } else {
        CaretEdge::Plain
    }
}

fn inline_caret(
    doc: &BaseDocument,
    nid: blitz_dom::NodeId,
    el: &ElementData,
    off: usize,
    scale: f32,
    edge: CaretEdge,
) -> Option<InlineCaret> {
    let tl = el.inline_layout_data.as_ref()?;
    let layout = &tl.layout;
    // Zero lines = an empty paragraph (parley's cursor rect for a zero-line
    // layout has a negative offset; observed y=-40).
    layout.lines().next()?;
    // plain bytes → layout bytes (the atom node's mapping)
    let mapped = match edge {
        CaretEdge::Plain => plain_to_layout(doc, nid, off),
        CaretEdge::BlockStart => Some(0),
        CaretEdge::BlockEnd => plain_to_layout(doc, nid, usize::MAX),
    };
    let lbyte = mapped.unwrap_or(off);
    // A soft-break fold point must use Upstream (line-end wins), otherwise a
    // caret at a fold point's line end is drawn at the next line's start.
    let cur = Cursor::from_byte_index(layout, lbyte, resolved_affinity(layout, lbyte));
    let bb = cur.geometry(layout, CARET_W);
    // When the caret lands at the end of an atom node (image/formula), parley
    // draws it at the atom's front edge (the atom has no text bytes, so the
    // layout byte does not advance) — visually pressed against the SVG/img left
    // edge. Push x to the atom box's right edge (observed: typing after a
    // formula showed no caret). `atomic_end_offset` returns CSS px (taffy's
    // convention) while `bb` is physical px — convert each and then add.
    let atomic_dx = atomic_end_offset(doc, nid, off).unwrap_or(0.0);
    Some(InlineCaret {
        x: bb.x0 as f32 / scale + atomic_dx,
        y: bb.y0 as f32 / scale + tl.block_offset,
        h: ((bb.y1 - bb.y0) as f32 / scale).max(MIN_CARET_H),
        atomic_dx,
        block_offset: tl.block_offset,
    })
}

/// The fallback caret rect for an empty text block (document content space):
/// the content-box top-left, with height taken from the content-box height (the
/// line height held up by min-height). Only used when the inline layout is
/// missing / zero-line; see [`Geom::caret_rect`].
fn empty_block_caret(
    doc: &BaseDocument,
    container: blitz_dom::NodeId,
    nid: blitz_dom::NodeId,
    bid: usize,
    off: usize,
) -> Option<CaretRect> {
    // A block-level formula (a display:block span + a centered SVG) has no text
    // lines, so go through the geometric fallback. The caret must hug the
    // **formula's** left/right edge: off=0 takes the SVG's left edge, off>0 the
    // right edge; otherwise it is drawn at the line's far left (observed:
    // clicking a block formula sent the caret to the screen's left edge).
    if let Some(sid) = first_svg(doc, nid) {
        let svg = doc.get_node(sid)?;
        let sl = svg.unrounded_layout();
        let (sx, sy) = doc_position(doc, sid);
        return Some(CaretRect {
            x: if off == 0 { sx } else { sx + sl.size.width },
            y: sy,
            w: CARET_W,
            h: sl.size.height.max(MIN_CARET_H),
        });
    }
    let (x0, y0, w, h) = content_box(doc, nid)?;
    if w <= 0.0 || h <= 0.0 {
        // The first frame of a newly created empty block: the taffy box may
        // still be 0×0 (min-height waits for the next layout round), but the
        // **position** direction is already fixed. We can neither use its own
        // position (that would throw the caret to the top of the document) nor
        // return None (the host keeps the previous line's old caret — observed:
        // after Enter the caret stayed on the previous line and only jumped when
        // typing). Walk backwards for the nearest **already laid out** block and
        // extrapolate by "paragraph gap + each empty block's line height +
        // paragraph gap"; the first block uses the content area's top-left. Do
        // not just look at bid-1: on consecutive Enters the previous empty block
        // may itself be unlaid-out.
        let anchor = (0..bid).rev().find_map(|prev| {
            let (px, py, _, ph) =
                node_for_bid(doc, container, prev).and_then(|n| bid_content_rect(doc, n))?;
            let missing = (bid - prev - 1) as f32;
            Some((
                px,
                py + ph + EMPTY_GAP + missing * (EMPTY_LINE_H + EMPTY_GAP),
            ))
        });
        let (bx, by) = doc_position(doc, nid);
        // The first block (or when no preceding block is laid out): start from
        // `.wys-content`'s padding (20,16).
        let (px, py) = anchor.unwrap_or((bx + 20.0, by + 16.0));
        return Some(CaretRect {
            x: px,
            y: py,
            w: CARET_W,
            h: EMPTY_LINE_H,
        });
    }
    Some(CaretRect {
        x: x0,
        y: y0,
        w: CARET_W,
        h: h.max(MIN_CARET_H),
    })
}

/// The first `<svg>` in a block (an element node, a block-level formula);
/// returns None if there is none.
fn first_svg(doc: &BaseDocument, id: blitz_dom::NodeId) -> Option<blitz_dom::NodeId> {
    let node = doc.get_node(id)?;
    for &c in &node.children {
        let Some(child) = doc.get_node(c) else {
            continue;
        };
        if let Some(el) = child.data.downcast_element() {
            if el.name.local.to_string() == "svg" {
                return Some(c);
            }
            if let Some(found) = first_svg(doc, c) {
                return Some(found);
            }
        }
    }
    None
}

/// Document content-space coordinates: accumulate location along the containing
/// block chain (without subtracting scroll).
fn doc_position(doc: &BaseDocument, nid: blitz_dom::NodeId) -> (f32, f32) {
    let mut x = 0.0_f32;
    let mut y = 0.0_f32;
    let mut cur = Some(nid);
    while let Some(id) = cur {
        let Some(n) = doc.get_node(id) else { break };
        x += n.unrounded_layout().location.x;
        y += n.unrounded_layout().location.y;
        cur = n.containing_block();
    }
    (x, y)
}

// ---------------------------------------------------------------------------
// Virtual-byte mapping: plain (includes Image.alt / Math source) ↔ layout
// (pure text nodes)
// ---------------------------------------------------------------------------

/// Whether an element is an atom node, and its plain byte count (data-vlen,
/// written by render).
fn atomic_vlen(el: &ElementData) -> Option<usize> {
    attr_usize(el, "data-vlen")
}

/// A revealed Markdown delimiter (`data-vmark`, written by [`crate::render`] when
/// the source expansion is on): it occupies that many **layout** bytes and
/// **zero** plain bytes. Both directions skip it, so `Pos.off` keeps measuring
/// the model's `plain_text` — a click inside `**bold**`'s delimiter snaps to the
/// boundary it decorates instead of counting the marker as text.
fn marker_bytes(el: &ElementData) -> Option<usize> {
    attr_usize(el, "data-vmark")
}

/// Pre-order traverse a data-bid element's subtree, mapping layout bytes to
/// plain bytes. Returns None when `byte` is past the text (the caller clamps to
/// the block end).
pub(crate) fn layout_to_plain(
    doc: &BaseDocument,
    bid_node: blitz_dom::NodeId,
    byte: usize,
) -> Option<usize> {
    let node = doc.get_node(bid_node)?;
    // The block element itself carries no text (its children/text are the
    // content)
    let mut acc_plain = 0usize;
    let mut acc_layout = 0usize;
    for &c in &node.children {
        if let Some(p) = walk_l2p(doc, c, byte, &mut acc_plain, &mut acc_layout) {
            return Some(p);
        }
    }
    // No hit (byte is after all the text): plain = the block end
    Some(acc_plain)
}

fn walk_l2p(
    doc: &BaseDocument,
    id: blitz_dom::NodeId,
    byte: usize,
    acc_plain: &mut usize,
    acc_layout: &mut usize,
) -> Option<usize> {
    let node = doc.get_node(id)?;
    match &node.data {
        blitz_dom::NodeData::Text(t) => {
            let len = t.content.len();
            // `byte` may be *past* this text — `usize::MAX` is the "block end"
            // query the diagnostics use — and `acc_plain + (byte - acc_layout)`
            // wrapped there (a debug-build panic inside the log path, which is
            // exactly when someone is trying to debug something else). The offset
            // only means anything when the byte lands in this node anyway.
            let within = byte.saturating_sub(*acc_layout).min(len);
            let hit = *acc_layout + len >= byte;
            let p = *acc_plain + within;
            *acc_plain += len;
            *acc_layout += len;
            if hit {
                Some(p.min(*acc_plain))
            } else {
                None
            }
        }
        blitz_dom::NodeData::Element(el) => {
            if let Some(v) = marker_bytes(el) {
                // Source-expansion delimiter: layout bytes, no plain bytes. A hit
                // inside it is the plain boundary it decorates.
                if byte <= *acc_layout + v {
                    return Some(*acc_plain);
                }
                *acc_layout += v;
                return None;
            }
            if let Some(v) = atomic_vlen(el) {
                // Atom node: occupies no layout bytes, but occupies plain
                // bytes. When byte lands on (or before) the atom's front edge →
                // snap to the atom's plain front edge; when already past the atom
                // → account for it and hand off to later nodes. It used to return
                // early unconditionally, so every click past an image/formula at
                // the block end landed in the middle of the alt text (with
                // layout="abc"+img+"def", a click at byte=6 used to return 5;
                // the correct answer is 8).
                if byte <= *acc_layout {
                    return Some(*acc_plain);
                }
                *acc_plain += v;
                return None;
            }
            for &c in &node.children {
                if let Some(p) = walk_l2p(doc, c, byte, acc_plain, acc_layout) {
                    return Some(p);
                }
            }
            None
        }
        _ => None,
    }
}

/// Pre-order traverse mapping plain bytes to layout bytes. `off == usize::MAX`
/// returns the block's total layout byte count (used for a selection to the
/// block end). When plain lands inside an atom's range it snaps to its front
/// edge.
pub(crate) fn plain_to_layout(
    doc: &BaseDocument,
    bid_node: blitz_dom::NodeId,
    off: usize,
) -> Option<usize> {
    let node = doc.get_node(bid_node)?;
    let mut acc_plain = 0usize;
    let mut acc_layout = 0usize;
    let mut i = 0;
    while i < node.children.len() {
        let c = node.children[i];
        // Is the next sibling an opening delimiter? (see the text branch of walk_p2l)
        let opens_next = node
            .children
            .get(i + 1)
            .and_then(|&n| doc.get_node(n))
            .and_then(|n| n.data.downcast_element().map(is_opening_marker))
            .unwrap_or(false);
        if let Some(l) = walk_p2l(doc, c, off, &mut acc_plain, &mut acc_layout, opens_next) {
            return Some(l);
        }
        i += 1;
    }
    // off == usize::MAX (a selection to the block end) or off past all the
    // plain: both land at the block end.
    Some(acc_layout)
}

fn walk_p2l(
    doc: &BaseDocument,
    id: blitz_dom::NodeId,
    off: usize,
    acc_plain: &mut usize,
    acc_layout: &mut usize,
    next_opens: bool,
) -> Option<usize> {
    let node = doc.get_node(id)?;
    match &node.data {
        blitz_dom::NodeData::Text(t) => {
            let len = t.content.len();
            // The end of a text node is also the plain boundary of whatever follows
            // it. When that is an **opening delimiter** the caret belongs *after*
            // the delimiter (`**|bold**`), so the boundary is left to the marker
            // branch below instead of being claimed here — otherwise → at the end of
            // "abc" in `abc**bold**` visually stepped over `**b`.
            let hit = if next_opens {
                off < *acc_plain + len
            } else {
                off <= *acc_plain + len
            };
            let l = *acc_layout + (off.saturating_sub(*acc_plain)).min(len);
            *acc_plain += len;
            *acc_layout += len;
            if hit {
                Some(l)
            } else {
                None
            }
        }
        blitz_dom::NodeData::Element(el) => {
            if let Some(v) = marker_bytes(el) {
                // Source-expansion delimiter: the caret at this plain boundary is
                // drawn **inside** the source when the marker opens it (`$|x$`) and
                // **before** it when the marker closes it (`$x|$`) — one monotonic
                // walk through the visible source, no delimiter stepped over.
                if off <= *acc_plain {
                    return Some(if is_opening_marker(el) {
                        *acc_layout + v
                    } else {
                        *acc_layout
                    });
                }
                *acc_layout += v;
                return None;
            }
            if let Some(v) = atomic_vlen(el) {
                let hit = off <= *acc_plain + v;
                let l = *acc_layout; // snap to the atom's front edge
                *acc_plain += v;
                // `off == usize::MAX` must fall through to the block end: a
                // short-circuit here returned the *first* atom's front edge
                // instead of the total layout byte count the function is
                // contracted to return.
                if hit {
                    return Some(l);
                }
                return None;
            }
            let mut i = 0;
            while i < node.children.len() {
                let c = node.children[i];
                // Is the **next** sibling an opening delimiter? (see the text branch)
                let opens_next = node
                    .children
                    .get(i + 1)
                    .and_then(|&n| doc.get_node(n))
                    .and_then(|n| n.data.downcast_element().map(is_opening_marker))
                    .unwrap_or(false);
                if let Some(l) = walk_p2l(doc, c, off, acc_plain, acc_layout, opens_next) {
                    return Some(l);
                }
                i += 1;
            }
            None
        }
        _ => None,
    }
}

fn is_opening_marker(el: &ElementData) -> bool {
    marker_bytes(el).is_some() && el.attrs().iter().any(|a| &*a.name.local == "data-vopen")
}

/// The block's total layout byte count (= the plain length when there are no
/// atoms).
fn block_plain_len(doc: &BaseDocument, bid_node: blitz_dom::NodeId) -> usize {
    let mut acc = 0usize;
    sum_plain(doc, bid_node, &mut acc);
    acc
}

/// Whether the subtree holds any text node at all.
///
/// This — not "plain length is zero" — is the condition the empty-block hit
/// test needs: an atom (image/formula) contributes `data-vlen` to the plain
/// length but has no text node, so a block holding only an image with a
/// non-empty alt used to be skipped and the click fell through to
/// `find_text_position`, which lands on a neighbouring paragraph's text.
fn has_text_node(doc: &BaseDocument, id: blitz_dom::NodeId) -> bool {
    let Some(node) = doc.get_node(id) else {
        return false;
    };
    match &node.data {
        blitz_dom::NodeData::Text(_) => true,
        blitz_dom::NodeData::Element(el) => {
            // An atom is a leaf for the text search too (its alt/src is an
            // attribute, not a text node); a source-expansion delimiter is
            // notation, not text (a block must not look non-empty because a
            // revealed `**` sits in it).
            if atomic_vlen(el).is_some() || marker_bytes(el).is_some() {
                return false;
            }
            node.children.iter().any(|&c| has_text_node(doc, c))
        }
        _ => false,
    }
}

/// If `off` lands exactly at the plain end of some atom node (img/math, carrying
/// `data-vlen`), return that atom box's width (**CSS px**, taffy's
/// `unrounded_layout` convention). Used to push the caret from the atom's front
/// edge to its right edge — the atom has no text bytes, so parley does not
/// advance the caret's x automatically.
///
/// A correction is only needed when there is no text node after the atom: with
/// text, the layout byte lands at the text start and the caret is naturally
/// after the atom. The caller ([`inline_caret`])'s parley geometry is physical
/// px while this function is CSS px — convert each and then add; do not
/// uniformly divide by the scale again.
fn atomic_end_offset(doc: &BaseDocument, bid_node: blitz_dom::NodeId, off: usize) -> Option<f32> {
    let node = doc.get_node(bid_node)?;
    let mut acc_plain = 0usize;
    let mut result: Option<f32> = None;
    for &c in &node.children {
        if let Some(w) = walk_atomic_end(doc, c, off, &mut acc_plain) {
            result = Some(w);
        }
    }
    result
}

fn walk_atomic_end(
    doc: &BaseDocument,
    id: blitz_dom::NodeId,
    off: usize,
    acc_plain: &mut usize,
) -> Option<f32> {
    let node = doc.get_node(id)?;
    match &node.data {
        blitz_dom::NodeData::Text(t) => {
            *acc_plain += t.content.len();
            None
        }
        blitz_dom::NodeData::Element(el) => {
            // A source-expansion delimiter has layout bytes but no plain bytes, so
            // it can never be the atom a caret sits at the end of.
            if marker_bytes(el).is_some() {
                return None;
            }
            if let Some(v) = atomic_vlen(el) {
                let end = *acc_plain + v;
                // The caret is exactly at the atom's end, and there is no text
                // after it (the layout byte will not advance)
                if off == end {
                    // Take the atom box's width (CSS px, taffy's convention):
                    // prefer its own layout (a block-level img's border box); an
                    // inline span is typeset by parley and its taffy box is always
                    // zero-sized (observed: after a formula closed, the caret
                    // jumped back to the formula's left edge), so fall back to its
                    // inner `<svg>`/`<img>` box (valid after resolve; the caller
                    // retries next frame). Only parley's inline box is physical px
                    // (×scale); a node's unrounded_layout is always CSS px — the
                    // two conventions must not be mixed.
                    let lay = node.unrounded_layout();
                    let w = if lay.size.width > 0.0 {
                        Some(lay.size.width)
                    } else {
                        replaced_box_width(doc, id)
                    };
                    if let Some(w) = w {
                        *acc_plain = end;
                        return Some(w);
                    }
                }
                *acc_plain = end;
                return None;
            }
            for &c in &node.children {
                if let Some(w) = walk_atomic_end(doc, c, off, acc_plain) {
                    return Some(w);
                }
            }
            None
        }
        _ => None,
    }
}

/// The border-box width (CSS px) of the first replaced element (`<svg>`/`<img>`)
/// in a node's subtree. Taken when an inline atom carrier (a `data-math` span)
/// is itself zero-sized.
fn replaced_box_width(doc: &BaseDocument, id: blitz_dom::NodeId) -> Option<f32> {
    let node = doc.get_node(id)?;
    if let Some(el) = node.data.downcast_element() {
        let tag = el.name.local.to_string();
        if tag == "svg" || tag == "img" {
            let lay = node.unrounded_layout();
            if lay.size.width > 0.0 {
                return Some(lay.size.width);
            }
        }
    }
    for &c in &node.children {
        if let Some(w) = replaced_box_width(doc, c) {
            return Some(w);
        }
    }
    None
}

fn sum_plain(doc: &BaseDocument, id: blitz_dom::NodeId, acc: &mut usize) {
    let Some(node) = doc.get_node(id) else { return };
    match &node.data {
        blitz_dom::NodeData::Text(t) => *acc += t.content.len(),
        blitz_dom::NodeData::Element(el) => {
            if let Some(v) = atomic_vlen(el) {
                *acc += v;
                return;
            }
            // A delimiter contributes layout bytes only (see `marker_bytes`).
            if marker_bytes(el).is_some() {
                return;
            }
            for &c in &node.children {
                sum_plain(doc, c, acc);
            }
        }
        _ => {}
    }
}

fn attr_str(el: &ElementData, name: &str) -> Option<String> {
    let a = el.attrs().iter().find(|a| &*a.name.local == name)?;
    Some(a.value.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use parley::{FontContext, LayoutContext};

    // ---- Drag mapping across a table's edges -------------------------------
    //
    // The geometry (band distances) needs a real layout, but the **decision**
    // `table_edge_pos` is pure — that is where the "table ends the document"
    // rule lives, so it is pinned here.

    /// A point past the table's top/bottom lands on the table's first/last cell,
    /// whatever column the pointer is over; a point inside the rows keeps the
    /// per-cell mapping. Below the **last row's content** (cell padding, the
    /// source table's delimiter row) the table's end wins too — that is the shape
    /// a table ending the document has, where nothing below can rescue the sweep.
    #[test]
    fn table_edges_own_the_drag_past_them() {
        let doc = crate::md::parse("| a | b |\n|---|---|\n| c | d |");
        assert_eq!(doc.blocks.len(), 4, "{:?}", doc.blocks);
        let (first, last) = (0usize, 3usize);
        // Geometry: the table spans 100..160, the winning row's content ends at 154.
        let band = (first, last, (100.0, 160.0), 154.0);
        let at = |dy: f32, in_last_row: bool| {
            table_edge_pos(&doc, band.0, band.1, band.2, band.3, in_last_row, dy)
        };
        // Inside the rows: the cell under the pointer decides (no override).
        assert_eq!(at(130.0, false), None);
        assert_eq!(at(130.0, true), None);
        // Past the top: the table's start.
        assert_eq!(at(90.0, false), Some(Pos::new(first, 0)));
        // Past the bottom: the table's end — the last character of the last cell
        // is inside the selection now, not just the cell under the pointer's x.
        assert_eq!(at(170.0, true), Some(Pos::new(last, 1)));
        // Inside the table box but below the last row's content: the end as well,
        // but only when the winning row IS the last one (between rows, the cell
        // under the pointer stays the answer).
        assert_eq!(at(158.0, true), Some(Pos::new(last, 1)));
        assert_eq!(at(158.0, false), None);
    }

    /// `same_row` reads the model's `row` field (the flag `table_edge_pos` needs
    /// to know whether "below the row" means "below the table").
    #[test]
    fn same_row_reads_the_row_field() {
        let doc = crate::md::parse("| a | b |\n|---|---|\n| c | d |");
        assert!(same_row(&doc.blocks[0], &doc.blocks[1]), "header cells");
        assert!(!same_row(&doc.blocks[0], &doc.blocks[2]), "header vs body");
        assert!(same_row(&doc.blocks[2], &doc.blocks[3]), "body cells");
    }

    // ---- The virtual-byte ledger of a revealed source ----------------------
    //
    // `layout_to_plain` / `plain_to_layout` only read node data and attributes,
    // so the real renderer's structure can be built headlessly (the same
    // `DocumentMutator` harness `dom_index` uses).

    use blitz_dom::{Attribute, DocumentConfig, LocalName, QualName};

    fn qname(name: &str) -> QualName {
        QualName::new(None, blitz_dom::ns!(html), LocalName::from(name))
    }

    fn attr(name: &str, value: &str) -> Attribute {
        Attribute {
            name: qname(name),
            value: value.to_string(),
        }
    }

    /// `<html><body><div data-bid="0">…children…</div></body></html>`, returning
    /// the document and the block node.
    fn block_doc(
        build: impl FnOnce(&mut blitz_dom::DocumentMutator) -> Vec<blitz_dom::NodeId>,
    ) -> (BaseDocument, blitz_dom::NodeId) {
        let mut doc = BaseDocument::new(DocumentConfig {
            viewport: Some(blitz_traits::shell::Viewport::new(
                400,
                300,
                1.0,
                blitz_traits::shell::ColorScheme::Light,
            )),
            ..Default::default()
        });
        let root = doc.root_node().id;
        let (html, block) = {
            let mut m = doc.mutate();
            let html = m.create_element(qname("html"), vec![]);
            let body = m.create_element(qname("body"), vec![]);
            let block = m.create_element(qname("div"), vec![attr("data-bid", "0")]);
            let kids = build(&mut m);
            m.append_children(block, &kids);
            m.append_children(body, &[block]);
            m.append_children(html, &[body]);
            m.append_children(root, &[html]);
            (html, block)
        };
        let _ = html;
        (doc, block)
    }

    /// `<span data-vmark=..>`: a revealed Markdown delimiter.
    fn marker(m: &mut blitz_dom::DocumentMutator, text: &str, open: bool) -> blitz_dom::NodeId {
        let attrs = if open {
            vec![
                attr("data-vmark", &text.len().to_string()),
                attr("data-vopen", "1"),
            ]
        } else {
            vec![attr("data-vmark", &text.len().to_string())]
        };
        let span = m.create_element(qname("span"), attrs);
        let t = m.create_text_node(text);
        m.append_children(span, &[t]);
        span
    }

    /// The caret of a **selection's focus** follows the selection's edge: a range
    /// ending at the table's last cell puts the caret after the trailing ` |`
    /// (the painted range covers it), not between `丁` and the pipe.
    #[test]
    fn a_selection_focus_caret_sits_on_the_painted_edge() {
        let sel = |a: (usize, usize), b: (usize, usize)| crate::model::Sel {
            anchor: Pos::new(a.0, a.1),
            focus: Pos::new(b.0, b.1),
        };
        // Selecting down to the table's end (focus = the last cell's end): after
        // the trailing markers.
        assert_eq!(
            focus_caret_edge(sel((0, 0), (3, 1)), 1),
            CaretEdge::BlockEnd
        );
        // Dragging **upward** from inside the table: the focus is the range's
        // start at plain 0 → before the opening markers.
        assert_eq!(
            focus_caret_edge(sel((3, 1), (0, 0)), 1),
            CaretEdge::BlockStart
        );
        // A focus inside a block keeps the plain mapping.
        assert_eq!(focus_caret_edge(sel((0, 0), (3, 0)), 1), CaretEdge::Plain);
        assert_eq!(focus_caret_edge(sel((3, 0), (0, 3)), 1), CaretEdge::Plain);
        // A collapsed caret stays inside the markers.
        assert_eq!(focus_caret_edge(sel((3, 1), (3, 1)), 1), CaretEdge::Plain);
    }

    /// A **range** reaching a block's first/last plain byte paints the block's
    /// layout edges — the virtual markers at the edges are part of the copied
    /// text, so the highlight must cover them. A caret keeps the plain mapping
    /// (it belongs *inside* the markers).
    #[test]
    fn a_range_paints_the_virtual_markers_at_its_edges() {
        let (doc, block) = block_doc(|m| {
            let open = marker(m, "**", true);
            let strong = m.create_element(qname("strong"), vec![]);
            let text = m.create_text_node("bold");
            m.append_children(strong, &[text]);
            let close = marker(m, "**", false);
            vec![open, strong, close]
        });
        let span = |a: usize, b: usize, collapsed: bool| {
            selection_layout_span(&doc, block, block, a, b, collapsed)
        };
        // The whole block: from layout 0 (before `**`) to layout 8 (after the
        // closing `**`) — what a copy of the full block writes.
        assert_eq!(span(0, 4, false), Some((0, 8)));
        // A range from the first character to the block end: the opening marker is
        // covered too (start == plain 0), the closing one follows the end.
        assert_eq!(span(0, 4, false), Some((0, 8)));
        // Mid-range: plain → layout as usual.
        assert_eq!(span(0, 2, false), Some((0, 4)));
        assert_eq!(span(1, 3, false), Some((3, 5)));
        // A caret stays inside the markers (`**|bold**`).
        assert_eq!(span(0, 0, true), Some((2, 2)));
        assert_eq!(span(4, 4, true), Some((6, 6)));
    }

    /// A revealed `**bold**`: delimiters are layout bytes but **not** plain
    /// bytes, so the two ledgers must both agree with the model's `plain_text`.
    #[test]
    fn revealed_markers_are_virtual_in_both_directions() {
        let (doc, block) = block_doc(|m| {
            let open = marker(m, "**", true);
            let strong = m.create_element(qname("strong"), vec![]);
            let text = m.create_text_node("bold");
            m.append_children(strong, &[text]);
            let close = marker(m, "**", false);
            vec![open, strong, close]
        });
        // Plain text is "bold" (4 bytes); the layout stream is `**bold**` (8).
        assert_eq!(block_plain_len(&doc, block), 4);
        assert_eq!(plain_to_layout(&doc, block, usize::MAX), Some(8));
        // layout → plain: a hit inside either delimiter snaps to the boundary it
        // decorates, text bytes map one to one.
        for (byte, plain) in [(0, 0), (1, 0), (2, 0), (4, 2), (6, 4), (8, 4)] {
            assert_eq!(
                layout_to_plain(&doc, block, byte),
                Some(plain),
                "layout {byte} → plain"
            );
        }
        // plain → layout: at the content start the caret is drawn **inside** the
        // opening delimiter, at the content end it stays in front of the closing
        // one (one monotonic walk through the source).
        assert_eq!(plain_to_layout(&doc, block, 0), Some(2));
        assert_eq!(plain_to_layout(&doc, block, 2), Some(4));
        assert_eq!(plain_to_layout(&doc, block, 4), Some(6));
    }

    /// The plain boundary at the end of a text run is also the front edge of the
    /// delimiter that follows it — and the caret belongs **after** an opening
    /// delimiter, so `abc|**bold**` must draw after `**` (not before it, which made
    /// one → press look like it stepped over `**b`).
    #[test]
    fn boundary_before_an_opening_marker_draws_inside_it() {
        let (doc, block) = block_doc(|m| {
            let text = m.create_text_node("abc");
            let open = marker(m, "**", true);
            let inner = m.create_text_node("bold");
            let close = marker(m, "**", false);
            vec![text, open, inner, close]
        });
        // plain "abcbold"; the caret at the end of "abc" (off 3) sits after `**`.
        assert_eq!(plain_to_layout(&doc, block, 3), Some(5));
        assert_eq!(plain_to_layout(&doc, block, 2), Some(2));
        // The closing delimiter keeps the content end in front of itself.
        assert_eq!(plain_to_layout(&doc, block, 7), Some(9));
    }

    /// A revealed formula: the LaTeX is real text (identity mapping), the `$`
    /// delimiters are virtual, and the carrier is not an atom any more (no
    /// `data-vlen`) — that is what lets the caret live inside the source.
    #[test]
    fn revealed_formula_source_maps_identically() {
        let (doc, block) = block_doc(|m| {
            let open = marker(m, "$", true);
            let src = m.create_element(qname("span"), vec![attr("data-math", "")]);
            let text = m.create_text_node("x+y");
            m.append_children(src, &[text]);
            let close = marker(m, "$", false);
            vec![open, src, close]
        });
        assert_eq!(block_plain_len(&doc, block), 3);
        assert_eq!(plain_to_layout(&doc, block, usize::MAX), Some(5));
        for off in 0..=3 {
            assert_eq!(
                layout_to_plain(&doc, block, off + 1),
                Some(off),
                "the source maps 1:1"
            );
        }
        assert_eq!(plain_to_layout(&doc, block, 1), Some(2));
    }

    /// An atom (a typeset formula / an image) is still walked as an opaque block:
    /// the virtual-byte branch must not shadow `data-vlen`.
    #[test]
    fn atom_vlen_still_wins_over_plain_walk() {
        let (doc, block) = block_doc(|m| {
            let a = m.create_text_node("ab");
            let atom = m.create_element(
                qname("span"),
                vec![attr("data-math", ""), attr("data-vlen", "3")],
            );
            let b = m.create_text_node("cd");
            vec![a, atom, b]
        });
        assert_eq!(block_plain_len(&doc, block), 7);
        // Layout bytes: "ab" (0..2) + atom (0) + "cd" (2..4). A click on the `d`
        // (layout 3) is plain 6 — the atom's virtual bytes sit between.
        assert_eq!(layout_to_plain(&doc, block, 3), Some(6));
        // …and the caret inside the atom's range snaps to its front edge.
        assert_eq!(plain_to_layout(&doc, block, 4), Some(2));
    }

    /// The formula hit-tests must answer only for a **typeset** carrier. The
    /// source span and the failed-parse `<code>` fallback hold real text, so
    /// `math_is_atom` says no and the text mapping places the caret on the exact
    /// character instead of snapping to the formula's edges.
    #[test]
    fn only_a_typeset_formula_is_an_atom() {
        let (doc, block) = block_doc(|m| {
            let atom = m.create_element(
                qname("span"),
                vec![attr("data-math", ""), attr("data-vlen", "3")],
            );
            let src = m.create_element(
                qname("span"),
                vec![attr("data-math", ""), attr("data-math-src", "")],
            );
            let fallback = m.create_element(qname("code"), vec![attr("data-math", "")]);
            vec![atom, src, fallback]
        });
        let kids = doc.get_node(block).expect("block").children.clone();
        assert!(math_is_atom(&doc, kids[0]), "typeset carrier is an atom");
        assert!(!math_is_atom(&doc, kids[1]), "source on screen is text");
        assert!(!math_is_atom(&doc, kids[2]), "parse fallback is text");
    }

    /// `usize::MAX` is the "block end" query (`ui`'s resync diagnostic asks it):
    /// it must answer the block's plain length. The text branch computed
    /// `acc_plain + (byte - acc_layout)` *before* the hit test, so `MAX` wrapped
    /// the addition and the debug build panicked — inside the very log path
    /// someone uses to debug.
    #[test]
    fn the_block_end_query_does_not_overflow() {
        let (doc, block) = block_doc(|m| {
            let a = m.create_text_node("ab");
            let atom = m.create_element(
                qname("span"),
                vec![attr("data-math", ""), attr("data-vlen", "3")],
            );
            let b = m.create_text_node("cd");
            vec![a, atom, b]
        });
        assert_eq!(block_plain_len(&doc, block), 7);
        assert_eq!(layout_to_plain(&doc, block, usize::MAX), Some(7));
        // Any byte past the text is the same answer, not a panic.
        assert_eq!(layout_to_plain(&doc, block, 99), Some(7));
    }

    /// Headlessly construct a parley layout (same engine as blitz's inline
    /// layout; tests only line breaking and the Cursor API, so no real DOM is
    /// needed — the verifiable part of geometry-class fixes).
    fn layout_of(text: &str, max_advance: Option<f32>) -> parley::Layout<()> {
        let mut fcx = FontContext::new();
        let mut lcx = LayoutContext::<()>::new();
        let rb = lcx.ranged_builder(&mut fcx, text, 1.0, false);
        let mut layout = rb.build(text);
        layout.break_all_lines(max_advance);
        layout
    }

    /// Premise lock (verify parley's behavior headlessly; run this test before
    /// changing move_visual): the trailing empty line has no items, and a
    /// `Cursor::from_point` landing on it returns the **whole layout's
    /// text_len** (not a separate "empty-line endpoint" answer) — the trailing
    /// empty line of `"ab\n"` has range 3..3, exactly equal to text_len, so
    /// `nbyte == lbyte` must hold when ↑/↓ enters an empty line; the old
    /// implementation's `return None` triggered the cross-block fallback and
    /// threw the caret out of the block. This is the basis for Home/End's
    /// visual-line positioning: line ranges cover their own line and butt end to
    /// end; a hard break's line-end range includes the `\n` (the caller
    /// `Geom::line_edge` then steps back to before the break).
    #[test]
    fn visual_line_range_covers_each_visual_line() {
        let l = layout_of("aa\nbb\ncc", None);
        assert_eq!(l.lines().count(), 3);
        let r0 = visual_line_range(&l, 0).expect("line0");
        assert_eq!(r0.start, 0);
        assert!(r0.end >= 2, "first line's range {r0:?}");
        let r1 = visual_line_range(&l, r0.end).expect("line1");
        assert_eq!(r1.start, r0.end, "line ranges butt end to end");
        let r2 = visual_line_range(&l, l.text_len()).expect("line2");
        assert_eq!(r2.end, l.text_len());
        assert_eq!(r2.start, r1.end);
    }

    #[test]
    fn from_point_on_empty_line_falls_back_to_text_len() {
        let text = "ab\n";
        let layout = layout_of(text, Some(1000.0));
        let lines: Vec<_> = layout.lines().collect();
        assert_eq!(
            lines.len(),
            2,
            "ab\\n should split into 2 lines: content + trailing empty"
        );
        assert_eq!(lines[0].text_range(), 0..3);
        assert_eq!(lines[1].text_range(), 3..3, "trailing empty line's range");
        let m = lines[1].metrics();
        let y = (f64::from(m.block_min_coord) + f64::from(m.block_max_coord)) / 2.0;
        let cur = Cursor::from_point(&layout, 0.0, y as f32);
        assert_eq!(
            cur.index(),
            text.len(),
            "empty line from_point falls back to text_len"
        );
    }

    /// A soft-wrap break resolves to Upstream (prefer line end), a hard break /
    /// ordinary position to Downstream; the cursor geometry y differs between the
    /// two affinities (Upstream draws at the previous line's end).
    #[test]
    fn affinity_resolves_soft_wrap_to_upstream() {
        // "aaaa aaaa" at a narrow width always produces a soft wrap
        // (Regular/Emergency)
        let layout = layout_of("aaaa aaaa", Some(40.0));
        let soft_end = layout
            .lines()
            .find(|l| {
                matches!(
                    l.break_reason(),
                    BreakReason::Regular | BreakReason::Emergency
                )
            })
            .map(|l| l.text_range().end);
        let e = soft_end.expect("narrow width should produce a soft wrap");
        assert_eq!(resolved_affinity(&layout, e), Affinity::Upstream);
        assert_eq!(resolved_affinity(&layout, 0), Affinity::Downstream);
        assert_eq!(
            resolved_affinity(&layout, layout.lines().last().unwrap().text_range().end),
            Affinity::Downstream,
            "end of text (break None) is not a soft break point"
        );
        // The cursor geometry really differs between the two affinities (y0
        // lives on different lines)
        let up = Cursor::from_byte_index(&layout, e, Affinity::Upstream).geometry(&layout, 0.0);
        let down = Cursor::from_byte_index(&layout, e, Affinity::Downstream).geometry(&layout, 0.0);
        assert_ne!(
            up.y0, down.y0,
            "Upstream should draw at the previous line's end"
        );

        // A hard break (Explicit) resolves to Downstream: the cursor sits at the
        // start of the next line
        let hard = layout_of("ab\ncd", Some(1000.0));
        let brk = hard
            .lines()
            .find(|l| l.break_reason() == BreakReason::Explicit)
            .map(|l| l.text_range().end)
            .expect("there should be a hard break");
        assert_eq!(resolved_affinity(&hard, brk), Affinity::Downstream);
    }

    /// Caret-addressability is decided by the model block type (Hr/Code
    /// excluded, None excluded).
    #[test]
    fn caret_addressable_excludes_hr_code_and_none() {
        use crate::model::Block;
        assert!(!caret_addressable(None));
        assert!(!caret_addressable(Some(&Block::Hr)));
        assert!(!caret_addressable(Some(&Block::Code {
            lang: None,
            text: String::new()
        })));
        assert!(caret_addressable(Some(&Block::Para(vec![]))));
    }
}
