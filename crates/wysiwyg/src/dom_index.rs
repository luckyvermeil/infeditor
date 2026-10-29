//! `data-bid` node index: reduces "block number → DOM node" from a full-tree
//! DFS per lookup to O(1), and after a structural change **rebuilds only the
//! affected segments**.
//!
//! Caret / click / selection push-back is a **per-keystroke, per-frame** hot
//! path; the old implementation (`Geom::block_node`, `collect_bids`,
//! `collect_tasks`, `collect_math`) did a pre-order walk from the root every
//! time — on large documents the keystroke cost grew linearly with block
//! count. This module collects three kinds of nodes (block roots, task
//! checkboxes, formulas) into the index, after which lookups are direct by
//! block number / ordinal.
//!
//! ## Segments
//!
//! The edit view's DOM shape is a "fragment container" (`.wys-content`, see
//! [`crate::geom::fragment_container`]) holding a series of `display:contents`
//! wrappers (one fragment per region, see
//! `render::region_fragments_sized`). A keystroke only `set_inner_html`s
//! **one** wrapper. So the index is segmented by "the container's direct
//! subtrees": each segment stores its own pre-order block table / task
//! checkboxes / formulas, and the flat tables (bid→node, bid→formula) are
//! added/removed per segment. `order()`/`tasks()` iterate by concatenating
//! segments, which naturally preserves DOM order.
//!
//! ## Invalidation semantics (bound to the render generation)
//!
//! - `structure_generation` unchanged → the cache is directly valid
//!   (in-place attribute/text edits do not bump it; neither does creating an
//!   unattached node or dropping a detached one).
//! - Changed → the vendor's `structure_changes_since` yields the list of
//!   **dirty parent nodes**:
//!   - dirty parent == the container itself (a wrapper added/removed/moved) →
//!     one sync of the container's child table: a new child segment takes one
//!     small DFS, a vanished segment is removed from the reverse index, other
//!     segments are reused as-is;
//!   - the dirty parent is some segment root (most common: the target of
//!     `set_inner_html` is the wrapper) or a node inside a segment → rebuild
//!     just that one fragment;
//!   - the dirty parent is outside the container (overlay sibling add/remove,
//!     etc.) → skip it entirely; such nodes carry no `data-bid` and do not
//!     affect index contents;
//!   - the log does not cover it (one batch of dirty parents exceeds the
//!     vendor limit of 1024) → fall back to one full DFS.
//! - After each successful rebuild / segment repair, `note_structure_indexed`
//!   advances the vendor log to the current generation (the DOM is only
//!   touched on the UI thread; `NodeHandle` is not `Send`, so a single
//!   consumer is safe).
//!
//! Cache key = document address + document root + container node + structural
//! generation; a thread-local single slot, so alternating access across
//! documents is also safe (each document's change log lives in its own
//! `NodeTree`).

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};

use blitz_dom::{BaseDocument, ElementData, NodeId};

/// A direct subtree of the root (a real document = one `display:contents`
/// fragment wrapper).
struct Segment {
    /// Segment root. The wrapper itself is never replaced (its contents are);
    /// a replacement/removal is handled by the container-level sync, so the
    /// root is stable during segment repair.
    root: NodeId,
    /// Pre-order `(data-bid element, block number)`.
    order: Vec<(NodeId, usize)>,
    /// `data-task-bid` elements `(element, block number)`, in DOM order.
    tasks: Vec<(NodeId, usize)>,
    /// Block number → `data-math` elements within the segment (DOM order =
    /// [`crate::model::Block::math_spans`] order).
    math: HashMap<usize, Vec<NodeId>>,
}

impl Segment {
    fn new(root: NodeId) -> Self {
        Self {
            root,
            order: Vec::new(),
            tasks: Vec::new(),
            math: HashMap::new(),
        }
    }
}

/// The index maintained by one (incremental) pass.
pub(crate) struct DomIndex {
    /// `BaseDocument` address (a document instance swap must rebuild, even if
    /// the generation value happens to be identical).
    doc: usize,
    root: NodeId,
    /// The fragment container (segmentation boundary, see the module docs).
    /// Recorded separately from root: when the container is replaced the
    /// host's `Geom` resolves a new id → different cache key → rebuild.
    container: NodeId,
    generation: u64,
    /// Block number → the element carrying `data-bid`. Exactly one per block
    /// (render's hard convention 1).
    bids: HashMap<usize, NodeId>,
    /// Block number → `data-math` elements inside the block (DOM order).
    math: HashMap<usize, Vec<NodeId>>,
    /// Segments, in the container's child-table order (= DOM order).
    segments: Vec<Segment>,
    /// Segment root → segment index (a dirty parent is often exactly a
    /// segment root, an O(1) hit).
    seg_pos: HashMap<NodeId, usize>,
}

impl DomIndex {
    fn build(doc: &BaseDocument, container: NodeId) -> Self {
        #[cfg(test)]
        TESTS_BUILDS.with(|c| c.set(c.get() + 1));
        let mut idx = Self {
            doc: doc_key(doc),
            root: doc.root_node().id,
            container,
            generation: doc.structure_generation(),
            bids: HashMap::new(),
            math: HashMap::new(),
            segments: Vec::new(),
            seg_pos: HashMap::new(),
        };
        idx.resync_children(doc);
        idx
    }

    /// Whether the cache still matches the current structure of this
    /// document/container.
    fn current(&self, doc: &BaseDocument, container: NodeId) -> bool {
        self.same_origin(doc, container) && self.generation == doc.structure_generation()
    }

    /// Cache ownership (document/root/container) is unchanged, only the
    /// generation is stale — try an incremental repair.
    fn same_origin(&self, doc: &BaseDocument, container: NodeId) -> bool {
        self.doc == doc_key(doc) && self.root == doc.root_node().id && self.container == container
    }

    /// Container child-table sync: a new child segment takes one small DFS, a
    /// vanished segment is removed from the reverse index, order is re-laid
    /// out. The insides of other segments are not examined (their changes are
    /// handled by their own dirty entries).
    fn resync_children(&mut self, doc: &BaseDocument) {
        let children: Vec<NodeId> = doc
            .get_node(self.container)
            .map(|n| n.children.iter().copied().collect())
            .unwrap_or_default();
        let child_set: HashSet<NodeId> = children.iter().copied().collect();
        let mut reuse: HashMap<NodeId, Segment> = HashMap::new();
        for seg in std::mem::take(&mut self.segments) {
            if child_set.contains(&seg.root) {
                reuse.insert(seg.root, seg);
            } else {
                self.unindex(&seg);
            }
        }
        self.segments.clear();
        self.seg_pos.clear();
        for (i, &c) in children.iter().enumerate() {
            let seg = match reuse.remove(&c) {
                Some(seg) => seg,
                None => {
                    let seg = Self::walk_segment(doc, c);
                    self.absorb(&seg);
                    seg
                }
            };
            self.seg_pos.insert(c, i);
            self.segments.push(seg);
        }
    }

    /// Incremental repair: rebuild only the segment containing the dirty
    /// parent. Returns false = cannot increment (caller should fully
    /// rebuild).
    fn repair(&mut self, doc: &BaseDocument, dirty: &[NodeId]) -> bool {
        let mut targets: Vec<NodeId> = Vec::new();
        let mut resync = false;
        for &d in dirty {
            if d == self.container {
                resync = true;
                continue;
            }
            if self.seg_pos.contains_key(&d) {
                // The segment root itself was changed (the innerHTML target)
                // — the most common path, O(1).
                if !targets.contains(&d) {
                    targets.push(d);
                }
                continue;
            }
            match self.segment_root_of(doc, d) {
                Some(seg_root) => {
                    if self.seg_pos.contains_key(&seg_root) {
                        if !targets.contains(&seg_root) {
                            targets.push(seg_root);
                        }
                    } else {
                        // A new child of the container (its attach entry is
                        // necessarily in the list too; idempotent).
                        resync = true;
                    }
                }
                None => {
                    // d is dead, or outside the container (overlay add/remove,
                    // etc.): changes outside the container carry no data-bid
                    // and do not affect the index; a dead node's coverage is
                    // guaranteed by its live ancestor / segment root / container
                    // entry (the vendor only records parents that were still
                    // alive at record time, so there is always a live one on the
                    // chain).
                    continue;
                }
            }
        }
        if resync {
            self.resync_children(doc);
        }
        for root in targets {
            if let Some(&i) = self.seg_pos.get(&root) {
                self.rebuild_segment(doc, i);
            }
            // The segment already vanished with the container sync → skip.
        }
        true
    }

    /// d → the segment root it lives in (a direct child of the container).
    /// Returns None if d is dead or outside the container.
    fn segment_root_of(&self, doc: &BaseDocument, mut id: NodeId) -> Option<NodeId> {
        loop {
            let node = doc.get_node(id)?;
            let parent = node.parent?;
            if parent == self.container {
                return Some(id);
            }
            id = parent;
        }
    }

    /// Rebuild segment i: un-index the old contents, re-walk the segment
    /// root's subtree.
    fn rebuild_segment(&mut self, doc: &BaseDocument, i: usize) {
        let seg_root = self.segments[i].root;
        let old = std::mem::replace(&mut self.segments[i], Segment::new(seg_root));
        self.unindex(&old);
        let fresh = Self::walk_segment(doc, seg_root);
        self.absorb(&fresh);
        self.segments[i] = fresh;
    }

    /// Remove this segment's contribution from the flat tables. Decided by
    /// **node identity**, to avoid mistakenly deleting a same-numbered entry
    /// another segment registered later (a duplicate block number is
    /// pathological input; the first one wins).
    fn unindex(&mut self, seg: &Segment) {
        let mut mine: HashMap<usize, NodeId> = HashMap::new();
        for &(nid, bid) in &seg.order {
            mine.entry(bid).or_insert(nid);
        }
        for (bid, nid) in mine {
            if self.bids.get(&bid) == Some(&nid) {
                self.bids.remove(&bid);
            }
        }
        for (bid, nodes) in &seg.math {
            if self.math.get(bid) == Some(nodes) {
                self.math.remove(bid);
            }
        }
    }

    /// Merge a segment's contribution into the flat tables (first come first
    /// served, consistent with global pre-order semantics).
    fn absorb(&mut self, seg: &Segment) {
        for &(nid, bid) in &seg.order {
            self.bids.entry(bid).or_insert(nid);
        }
        for (bid, nodes) in &seg.math {
            self.math.entry(*bid).or_insert_with(|| nodes.clone());
        }
    }

    /// Pre-order DFS of a segment subtree. `scope` = the current owning block
    /// number (a `data-bid` element opens a new scope).
    fn walk(doc: &BaseDocument, id: NodeId, scope: Option<usize>, seg: &mut Segment) {
        #[cfg(test)]
        TESTS_WALKED.with(|c| c.set(c.get() + 1));
        let Some(node) = doc.get_node(id) else { return };
        let mut scope = scope;
        if let Some(el) = node.data.downcast_element() {
            if let Some(bid) = attr_usize(el, "data-bid") {
                // One element per block; on a duplicate within the segment the
                // first in DOM order wins.
                if !seg.order.iter().any(|&(_, b)| b == bid) {
                    seg.order.push((id, bid));
                }
                scope = Some(bid);
            }
            if let Some(bid) = attr_usize(el, "data-task-bid") {
                seg.tasks.push((id, bid));
            }
            if has_attr(el, "data-math") {
                if let Some(bid) = scope {
                    seg.math.entry(bid).or_default().push(id);
                }
            }
        }
        for &c in &node.children {
            Self::walk(doc, c, scope, seg);
        }
    }

    fn walk_segment(doc: &BaseDocument, seg_root: NodeId) -> Segment {
        let mut seg = Segment::new(seg_root);
        Self::walk(doc, seg_root, None, &mut seg);
        seg
    }

    pub(crate) fn bid(&self, bid: usize) -> Option<NodeId> {
        self.bids.get(&bid).copied()
    }

    /// Pre-order DOM iteration of `(data-bid element, block number)`: segments
    /// are concatenated in the container's child-table order.
    pub(crate) fn order(&self) -> impl Iterator<Item = (NodeId, usize)> + '_ {
        self.segments
            .iter()
            .flat_map(|seg| seg.order.iter().copied())
    }

    pub(crate) fn tasks(&self) -> impl Iterator<Item = (NodeId, usize)> + '_ {
        self.segments
            .iter()
            .flat_map(|seg| seg.tasks.iter().copied())
    }

    pub(crate) fn math(&self, bid: usize) -> Option<&[NodeId]> {
        self.math.get(&bid).map(Vec::as_slice)
    }
}

fn doc_key(doc: &BaseDocument) -> usize {
    doc as *const BaseDocument as usize
}

// Test-only: the actual number of full index rebuilds / nodes visited by walk
// (proves the hot path has no per-lookup DFS and that segment repair does not
// grow with document size). Thread-local — with parallel tests, another
// thread's counts must not be attributed to the current case.
#[cfg(test)]
thread_local! {
    static TESTS_BUILDS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static TESTS_WALKED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

thread_local! {
    /// Single-slot cache: while editing, the vast majority of queries hit the
    /// same document, so per-document slots are unnecessary.
    static CACHE: RefCell<Option<DomIndex>> = const { RefCell::new(None) };
}

/// Run one query on the index; when the index is stale (structural change /
/// document or container swap) try an incremental repair first, and fully
/// rebuild if that fails.
///
/// `container` = the fragment container (segmentation boundary, see
/// [`crate::geom::fragment_container`]).
///
/// Do **not** call `with_index` again inside the closure (re-entry falls back
/// to a temporary index, see below), and do not carry a `&DomIndex` out of
/// the closure — it borrows the thread-local cache slot.
pub(crate) fn with_index<R>(
    doc: &BaseDocument,
    container: NodeId,
    f: impl FnOnce(&DomIndex) -> R,
) -> R {
    CACHE.with(|cell| match cell.try_borrow_mut() {
        Ok(mut slot) => {
            refresh(&mut slot, doc, container);
            // `slot`'s RefMut lives until f returns; a re-entrant
            // try_borrow_mut fails and takes the temporary-index branch below,
            // so it does not panic.
            f(slot.as_ref().expect("filled just above"))
        }
        // Re-entry (theoretically impossible): do not reuse the cache, build a
        // fresh one just for this query. Note this does **not** advance the
        // vendor log — the old index in the cache slot is still waiting for
        // those entries.
        Err(_) => f(&DomIndex::build(doc, container)),
    })
}

/// Bring the cache slot up to the document's current state: if same-origin
/// and the generation is stale, repair incrementally first, otherwise fully
/// rebuild.
fn refresh(slot: &mut Option<DomIndex>, doc: &BaseDocument, container: NodeId) {
    let stale = match slot {
        None => true,
        Some(idx) => !idx.current(doc, container),
    };
    if !stale {
        return;
    }
    let pending = match slot.as_ref() {
        Some(idx) if idx.same_origin(doc, container) => doc.structure_changes_since(idx.generation),
        _ => None,
    };
    if let Some(dirty) = pending {
        let idx = slot.as_mut().expect("Some taken just above");
        if idx.repair(doc, &dirty) {
            idx.generation = doc.structure_generation();
            doc.note_structure_indexed(idx.generation);
            return;
        }
        // Repair failed (the log does not cover it) → fall through to the full
        // rebuild below.
    }
    *slot = Some(DomIndex::build(doc, container));
    let generation = slot.as_ref().expect("filled just now").generation;
    doc.note_structure_indexed(generation);
}

/// Parse an element attribute into a block number; missing/non-numeric returns
/// None.
pub(crate) fn attr_usize(el: &ElementData, name: &str) -> Option<usize> {
    // LocalName is a string_cache Atom (Deref to str); bypass the local_name!
    // macro and compare directly against &str — the macro's exported form is
    // unstable across the two blitz versions.
    let a = el.attrs().iter().find(|a| &*a.name.local == name)?;
    a.value.parse().ok()
}

/// Whether the element carries some attribute (any value, e.g. `data-math`).
fn has_attr(el: &ElementData, name: &str) -> bool {
    el.attrs().iter().any(|a| &*a.name.local == name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use blitz_dom::{Attribute, DocumentConfig, LocalName, QualName};
    use blitz_traits::shell::Viewport;

    fn qname(name: &str) -> QualName {
        QualName::new(None, blitz_dom::ns!(html), LocalName::from(name))
    }

    fn attr(name: &str, value: &str) -> Attribute {
        Attribute {
            name: qname(name),
            value: value.to_string(),
        }
    }

    /// In-test query entry: the container is the document root (the wrapper
    /// structure built by `doc_with` hangs directly under the root).
    fn query<R>(doc: &BaseDocument, f: impl FnOnce(&DomIndex) -> R) -> R {
        let container = doc.root_node().id;
        with_index(doc, container, f)
    }

    /// Build `<html><body><div data-bid=0>…</div><div data-bid=1>…</div></body></html>`.
    /// `build` receives the mutator and the root node and attaches children
    /// itself.
    fn doc_with(
        build: impl FnOnce(&mut blitz_dom::DocumentMutator, NodeId) -> Vec<NodeId>,
    ) -> BaseDocument {
        let mut doc = BaseDocument::new(DocumentConfig {
            viewport: Some(Viewport::new(
                400,
                300,
                1.0,
                blitz_traits::shell::ColorScheme::Light,
            )),
            ..Default::default()
        });
        let root = doc.root_node().id;
        let mut m = doc.mutate();
        let html = m.create_element(qname("html"), vec![]);
        let body = m.create_element(qname("body"), vec![]);
        let children = build(&mut m, root);
        m.append_children(body, &children);
        m.append_children(html, &[body]);
        m.append_children(root, &[html]);
        drop(m);
        doc
    }

    /// Mimic the real render structure: n fragment wrappers hung directly
    /// under the container (here the root), each holding one `data-bid` block
    /// + one text child (3 nodes per segment).
    fn doc_wrapped(n: usize) -> (BaseDocument, Vec<NodeId>) {
        let mut doc = BaseDocument::new(DocumentConfig {
            viewport: Some(Viewport::new(
                400,
                300,
                1.0,
                blitz_traits::shell::ColorScheme::Light,
            )),
            ..Default::default()
        });
        let root = doc.root_node().id;
        let wraps = {
            let mut m = doc.mutate();
            let wraps: Vec<NodeId> = (0..n)
                .map(|i| {
                    let w = m.create_element(qname("div"), vec![]);
                    let b = m.create_element(qname("div"), vec![attr("data-bid", &i.to_string())]);
                    let t = m.create_text_node("x");
                    m.append_children(b, &[t]);
                    m.append_children(w, &[b]);
                    w
                })
                .collect();
            m.append_children(root, &wraps);
            wraps
        };
        (doc, wraps)
    }

    fn element(m: &mut blitz_dom::DocumentMutator, name: &str, attrs: Vec<Attribute>) -> NodeId {
        m.create_element(qname(name), attrs)
    }

    #[test]
    fn indexes_bids_tasks_and_math_in_dom_order() {
        let doc = doc_with(|m, _| {
            let b0 = element(m, "div", vec![attr("data-bid", "0")]);
            let math0 = element(
                m,
                "span",
                vec![attr("data-math", ""), attr("data-vlen", "3")],
            );
            let task = element(
                m,
                "span",
                vec![attr("data-task-bid", "0"), attr("data-task", "1")],
            );
            m.append_children(b0, &[task, math0]);

            let b1 = element(m, "div", vec![attr("data-bid", "1")]);
            let math1 = element(m, "span", vec![attr("data-math", "")]);
            let nested = element(m, "td", vec![attr("data-bid", "2")]);
            let math2 = element(m, "span", vec![attr("data-math", "")]);
            m.append_children(b1, &[math1, nested]);
            m.append_children(nested, &[math2]);
            vec![b0, b1]
        });

        query(&doc, |idx| {
            let bid_attr = |bid: usize| {
                let nid = idx.bid(bid)?;
                let el = doc.get_node(nid)?.data.downcast_element()?;
                attr_usize(el, "data-bid")
            };
            assert_eq!(bid_attr(0), Some(0));
            assert_eq!(bid_attr(1), Some(1));
            assert_eq!(bid_attr(2), Some(2));
            assert_eq!(idx.bid(3), None);

            let order: Vec<usize> = idx.order().map(|(_, b)| b).collect();
            assert_eq!(
                order,
                vec![0, 1, 2],
                "pre-order: the outer block precedes the inner one"
            );

            let tasks: Vec<usize> = idx.tasks().map(|(_, b)| b).collect();
            assert_eq!(tasks, vec![0]);

            assert_eq!(idx.math(0).map(|v| v.len()), Some(1));
            // A nested block is its own scope: b1 only gets its own formula, the
            // td's formula belongs to the td.
            assert_eq!(idx.math(1).map(|v| v.len()), Some(1));
            assert_eq!(idx.math(2).map(|v| v.len()), Some(1));
        });
    }

    #[test]
    fn index_rebuilds_after_structural_change_and_reuses_otherwise() {
        let mut doc = doc_with(|m, root| {
            let b0 = element(m, "div", vec![attr("data-bid", "0")]);
            let text = m.create_text_node("a");
            m.append_children(b0, &[text]);
            let holder = element(m, "div", vec![attr("data-holder", "1")]);
            let _ = root;
            vec![b0, holder]
        });

        let (stamp, first) = query(&doc, |idx| (doc.structure_generation(), idx.bid(0)));
        assert_eq!(
            first,
            query(&doc, |idx| idx.bid(0)),
            "reuse the cache within one generation"
        );

        // Structural change: a new block → generation bumps → the index's
        // incremental repair sees the new block.
        let new_bid = {
            let root = doc_root(&doc);
            let mut m = doc.mutate();
            let b1 = element(&mut m, "div", vec![attr("data-bid", "1")]);
            let text = m.create_text_node("b");
            m.append_children(b1, &[text]);
            // Attaching under the document root is enough (the index only looks
            // at shape, it does not require an html/body structure).
            m.append_children(root, &[b1]);
            b1
        };
        let after = doc.structure_generation();
        assert!(
            after > stamp,
            "adding/removing a node must bump the structural generation"
        );
        query(&doc, |idx| {
            assert_eq!(idx.bid(1), Some(new_bid));
            assert_eq!(idx.generation, after);
        });
    }

    /// `doc` has already been wrapped in html/body by `doc_with`; the root node
    /// id comes from `root_node()` (borrow is held for the duration of
    /// `mutate()`, so fetch it first).
    fn doc_root(doc: &BaseDocument) -> NodeId {
        doc.root_node().id
    }

    #[test]
    fn dropping_children_invalidates() {
        let mut doc = doc_with(|m, _| {
            let b0 = element(m, "div", vec![attr("data-bid", "0")]);
            vec![b0]
        });
        assert!(query(&doc, |idx| idx.bid(0)).is_some());
        let before = doc.structure_generation();

        let root = doc_root(&doc);
        let mut m = doc.mutate();
        // This is exactly the path an innerHTML replacement takes: drop the old
        // subtree, then re-parse.
        m.remove_and_drop_all_children(root);
        drop(m);

        assert!(doc.structure_generation() > before);
        query(&doc, |idx| {
            assert_eq!(
                idx.bid(0),
                None,
                "the block vanished with its subtree; the index must rebuild"
            );
            assert_eq!(idx.order().next(), None);
        });
    }

    /// Detaching without freeing (`remove_node`) must also invalidate the index:
    /// dioxus keeps node references until the slot is reused, so the node is
    /// still in the slotmap and `get_node` still finds it, but it is no longer
    /// in the document tree — returning it would point the caret at an
    /// invisible element.
    #[test]
    fn detaching_a_node_invalidates() {
        let mut doc = doc_with(|m, _| {
            let b0 = element(m, "div", vec![attr("data-bid", "0")]);
            vec![b0]
        });
        let node = query(&doc, |idx| idx.bid(0)).expect("the block is indexed");
        let before = doc.structure_generation();

        let mut m = doc.mutate();
        m.remove_node(node);
        drop(m);

        assert!(
            doc.structure_generation() > before,
            "detaching is also a structural change"
        );
        assert!(
            doc.get_node(node).is_some(),
            "the node is still in the slotmap (not freed)"
        );
        query(&doc, |idx| assert_eq!(idx.bid(0), None));
    }

    /// The hot path does not DFS per lookup: after one index build, consecutive
    /// queries never rebuild.
    ///
    /// Also takes one magnitude measurement (no timing assertion, to avoid CI
    /// jitter): 2000 blocks × 2000 queries vs a full-tree DFS at the same
    /// scale, printed for manual comparison.
    #[test]
    fn lookups_reuse_one_index_instead_of_walking_the_tree() {
        const BLOCKS: usize = 2000;
        let doc = doc_with(|m, _| {
            (0..BLOCKS)
                .map(|i| {
                    let b = element(m, "div", vec![attr("data-bid", &i.to_string())]);
                    let t = m.create_text_node("x");
                    m.append_children(b, &[t]);
                    b
                })
                .collect()
        });

        TESTS_BUILDS.with(|c| c.set(0));
        for i in 0..BLOCKS {
            assert!(query(&doc, |idx| idx.bid(i)).is_some());
        }
        assert_eq!(
            TESTS_BUILDS.with(|c| c.get()),
            1,
            "within one generation, {BLOCKS} queries must build the index only once"
        );

        // Same-scale baseline: one pre-order DFS from the root per query.
        let root = doc.root_node().id;
        let t0 = std::time::Instant::now();
        for i in 0..BLOCKS {
            assert!(dfs_find(&doc, root, i).is_some());
        }
        let dfs = t0.elapsed();
        let t1 = std::time::Instant::now();
        for i in 0..BLOCKS {
            assert!(query(&doc, |idx| idx.bid(i)).is_some());
        }
        let indexed = t1.elapsed();
        eprintln!("data-bid reverse lookup, {BLOCKS} blocks × {BLOCKS} queries: DFS {dfs:?} / indexed {indexed:?}");
    }

    /// In-test DFS baseline (only for the magnitude comparison above).
    fn dfs_find(doc: &BaseDocument, id: NodeId, bid: usize) -> Option<NodeId> {
        let node = doc.get_node(id)?;
        if let Some(el) = node.data.downcast_element() {
            if attr_usize(el, "data-bid") == Some(bid) {
                return Some(id);
            }
        }
        node.children.iter().find_map(|&c| dfs_find(doc, c, bid))
    }

    #[test]
    fn text_mutation_keeps_generation_and_cached_ids() {
        let mut doc = doc_with(|m, _| {
            let b0 = element(m, "div", vec![attr("data-bid", "0")]);
            let t = m.create_text_node("a");
            m.append_children(b0, &[t]);
            vec![b0]
        });
        let before = doc.structure_generation();
        let node = query(&doc, |idx| idx.bid(0)).expect("the block is indexed");

        let text_id = doc.get_node(node).and_then(|n| n.children.first().copied());
        let mut m = doc.mutate();
        let t = text_id.expect("has a text child");
        m.set_node_text(t, "abc");
        drop(m);

        assert_eq!(
            doc.structure_generation(),
            before,
            "a pure text edit is not a structural change"
        );
        query(&doc, |idx| assert_eq!(idx.bid(0), Some(node)));
    }

    /// A keystroke (one fragment's innerHTML rebuild) only re-walks that one
    /// fragment: the number of visited nodes is independent of the document's
    /// total size, and it does not fall back to a full rebuild.
    #[test]
    fn fragment_edit_only_rewalks_that_fragment() {
        const WRAPPERS: usize = 2000;
        let (mut doc, wraps) = doc_wrapped(WRAPPERS);
        let old_node = query(&doc, |idx| idx.bid(1000)).expect("indexed");
        assert_eq!(query(&doc, |idx| idx.order().count()), WRAPPERS);

        TESTS_BUILDS.with(|c| c.set(0));
        TESTS_WALKED.with(|c| c.set(0));
        // Simulate set_inner_html: clear the wrapper's contents, then attach two
        // blocks again.
        {
            let mut m = doc.mutate();
            m.remove_and_drop_all_children(wraps[1000]);
            let b = element(&mut m, "div", vec![attr("data-bid", "1000")]);
            let t = m.create_text_node("y");
            m.append_children(b, &[t]);
            let b2 = element(&mut m, "div", vec![attr("data-bid", "5000")]);
            m.append_children(wraps[1000], &[b, b2]);
        }
        let t0 = std::time::Instant::now();
        let (new_node, total) = query(&doc, |idx| (idx.bid(1000), idx.order().count()));
        let repair = t0.elapsed();
        assert_ne!(
            new_node,
            Some(old_node),
            "the block element must be a fresh node after re-parse"
        );
        assert!(new_node.is_some());
        assert!(
            idx_bid5000(&doc),
            "a new block inside the segment enters the index"
        );
        assert_eq!(total, WRAPPERS + 1);

        let builds = TESTS_BUILDS.with(|c| c.get());
        let walked = TESTS_WALKED.with(|c| c.get());
        assert_eq!(
            builds, 0,
            "a single-fragment edit must not fall back to a full rebuild"
        );
        assert!(
            walked <= 32,
            "segment-level rebuild should only walk the current segment (walk visited {walked} nodes)"
        );

        // Magnitude printout: full rebuild vs segment repair (no timing
        // assertion).
        let t1 = std::time::Instant::now();
        let full = DomIndex::build(&doc, doc.root_node().id);
        let full_t = t1.elapsed();
        assert_eq!(full.order().count(), WRAPPERS + 1);
        eprintln!("first query after a keystroke ({WRAPPERS} fragments): segment repair {repair:?} / full rebuild {full_t:?}");
    }

    fn idx_bid5000(doc: &BaseDocument) -> bool {
        query(doc, |idx| idx.bid(5000).is_some())
    }

    /// A container child-table change (a wrapper added/removed) goes through the
    /// sync instead of a full DFS: only the new subtree is walked.
    #[test]
    fn wrapper_insert_resyncs_without_full_walk() {
        const WRAPPERS: usize = 200;
        let (mut doc, mut wraps) = doc_wrapped(WRAPPERS);
        assert!(query(&doc, |idx| idx.bid(0)).is_some());

        TESTS_BUILDS.with(|c| c.set(0));
        TESTS_WALKED.with(|c| c.set(0));
        let new_wrapper = {
            let root = doc_root(&doc);
            let mut m = doc.mutate();
            let w = m.create_element(qname("div"), vec![]);
            let b = m.create_element(qname("div"), vec![attr("data-bid", "9000")]);
            let t = m.create_text_node("z");
            m.append_children(b, &[t]);
            m.append_children(w, &[b]);
            m.append_children(root, &[w]);
            wraps.push(w);
            w
        };
        query(&doc, |idx| {
            assert!(idx.bid(9000).is_some());
            assert_eq!(idx.order().count(), WRAPPERS + 1);
        });
        assert_eq!(TESTS_BUILDS.with(|c| c.get()), 0);
        assert!(
            TESTS_WALKED.with(|c| c.get()) <= 16,
            "a new wrapper only walks its new subtree (walk visited {} nodes)",
            TESTS_WALKED.with(|c| c.get())
        );
        let _ = new_wrapper;

        // Remove one more wrapper: the sync should drop it from the index.
        {
            let mut m = doc.mutate();
            m.remove_and_drop_node(wraps[7]);
        }
        query(&doc, |idx| {
            assert_eq!(
                idx.bid(7),
                None,
                "the segment vanished with its wrapper's deletion"
            );
            assert_eq!(idx.order().count(), WRAPPERS);
        });
        assert_eq!(
            TESTS_BUILDS.with(|c| c.get()),
            0,
            "a deletion should not fully rebuild either"
        );
    }

    /// A structural change outside the container (an overlay sibling added or
    /// removed) does not affect index contents: skipped outright, no rebuild.
    #[test]
    fn changes_outside_container_are_ignored() {
        // root → holder (container) → wrappers; the root also has an overlay
        // sibling node.
        let mut doc = doc_with(|m, _| {
            let b0 = element(m, "div", vec![attr("data-bid", "0")]);
            vec![b0]
        });
        // doc_with's structure is root→html→body→b0; set the container to body.
        let container = {
            let html = doc
                .get_node(doc_root(&doc))
                .and_then(|n| n.children.first().copied());
            let body = html
                .and_then(|h| doc.get_node(h))
                .and_then(|n| n.children.first().copied())
                .expect("body");
            body
        };
        assert!(with_index(&doc, container, |idx| idx.bid(0)).is_some());

        TESTS_BUILDS.with(|c| c.set(0));
        TESTS_WALKED.with(|c| c.set(0));
        // Attach an overlay node under html (outside the container).
        {
            let html = doc
                .get_node(doc_root(&doc))
                .and_then(|n| n.children.first().copied())
                .expect("html");
            let mut m = doc.mutate();
            let ov = m.create_element(qname("div"), vec![attr("class", "wys-caret")]);
            m.append_children(html, &[ov]);
        }
        assert!(with_index(&doc, container, |idx| idx.bid(0)).is_some());
        assert_eq!(
            TESTS_BUILDS.with(|c| c.get()),
            0,
            "a change outside the container must not trigger a rebuild"
        );
        assert_eq!(
            TESTS_WALKED.with(|c| c.get()),
            0,
            "a change outside the container must not walk a single DFS"
        );
    }

    /// One batch of dirty parents exceeding the vendor log limit → attribution
    /// lost → fall back to one full rebuild (correctness first; incremental is
    /// only the common-case optimization).
    #[test]
    fn attribution_overflow_falls_back_to_full_rebuild() {
        const WRAPPERS: usize = 1200; // > vendor log limit of 1024
        let (mut doc, wraps) = doc_wrapped(WRAPPERS);
        assert!(query(&doc, |idx| idx.bid(0)).is_some());

        TESTS_BUILDS.with(|c| c.set(0));
        {
            let mut m = doc.mutate();
            for &w in &wraps[..1100] {
                m.remove_and_drop_all_children(w);
            }
        }
        query(&doc, |idx| {
            assert_eq!(idx.order().count(), WRAPPERS - 1100);
            assert_eq!(idx.bid(0), None);
            assert!(idx.bid(1199).is_some());
        });
        assert_eq!(
            TESTS_BUILDS.with(|c| c.get()),
            1,
            "after the log overflows there must be exactly one full rebuild"
        );
    }
}
