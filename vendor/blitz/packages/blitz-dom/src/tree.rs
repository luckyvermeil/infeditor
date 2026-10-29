//! Versioned storage for the nodes of the DOM tree.

use std::collections::HashSet;
use std::ops::{Index, IndexMut};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use blitz_traits::node_id::NodeId;
use slotmap::{Key as _, KeyData, SlotMap};

use crate::Node;

slotmap::new_key_type! {
    /// The internal [`slotmap`] key for node storage. Only used at the
    /// storage boundary: all public APIs use [`NodeId`].
    struct NodeKey;
}

#[inline(always)]
fn to_key(id: NodeId) -> NodeKey {
    NodeKey::from(KeyData::from_ffi(id.as_u64()))
}

#[inline(always)]
fn to_id(key: NodeKey) -> NodeId {
    NodeId::from_u64(key.data().as_ffi())
}

/// Distinct parents remembered per batch before attribution is given up on
/// (see [`NodeTree::record_structure_change`]). A normal keystroke touches
/// one parent, so this only bites on document-wide edits, where a full
/// rebuild would happen anyway.
const STRUCTURE_CHANGE_LOG_CAP: usize = 1024;

/// Attribution state behind [`NodeTree::structure_dirty`].
struct StructureDirty {
    /// Structure changes at generations **greater** than this are all
    /// represented in `parents` (until a reset moves it forward).
    since: u64,
    /// Deduplicated parents whose child list changed since `since`.
    parents: HashSet<NodeId>,
}

impl StructureDirty {
    fn new(since: u64) -> Self {
        Self {
            since,
            parents: HashSet::new(),
        }
    }
}

/// The versioned map in which the nodes of the DOM tree are stored, backed by
/// a [`slotmap::SlotMap`].
///
/// Nodes are addressed by [`NodeId`], which carries the slot's version in
/// addition to its index: when a node is dropped and its slot reused, ids
/// referring to the dropped node no longer resolve ([`NodeTree::get`] returns
/// `None`, and indexing panics) instead of aliasing the new occupant.
pub struct NodeTree {
    map: SlotMap<NodeKey, Node>,
    /// Bumped whenever a node enters or leaves the document tree (attach,
    /// detach, or a drop of an *attached* node — see
    /// [`NodeTree::record_structure_change`]). Consumers that build `NodeId`
    /// indexes over the tree (e.g. wysiwyg's `data-bid` lookup) use this as
    /// their invalidation stamp: an unchanged generation guarantees every
    /// cached `NodeId` is still *in* the tree and resolves to the same node
    /// (slotmap ids carry a version, so a removed/reused slot never aliases).
    /// Attribute and text mutations do not bump it — they do not invalidate
    /// node identity or membership. Neither does creating a node before it is
    /// attached, nor dropping one that was already detached: neither changes
    /// what a walk from the root can reach.
    /// Starts at 1 so that a stamp of 0 is never current.
    structure_generation: AtomicU64,
    /// Attribution for the bumps above: *which* parent's child list changed,
    /// so a consumer can re-walk only the affected subtree instead of the
    /// whole tree. See [`NodeTree::structure_changes_since`].
    structure_dirty: Mutex<StructureDirty>,
    /// Bumped whenever box geometry may have moved (layout ran or a scroll
    /// offset changed). Values derived from `final_layout()` (e.g. hoisted
    /// paint child offsets) are memoised against it. Starts at 1 so that a
    /// stamp of 0 is never current.
    geometry_generation: AtomicU64,
    /// Scroll offset of the viewport (the root element scrolls the viewport,
    /// so its offset lives here rather than on the node).
    viewport_scroll: crate::Point<f64>,
}

impl NodeTree {
    pub(crate) fn new() -> Self {
        Self {
            map: SlotMap::with_key(),
            structure_generation: AtomicU64::new(1),
            structure_dirty: Mutex::new(StructureDirty::new(1)),
            geometry_generation: AtomicU64::new(1),
            viewport_scroll: crate::Point::ZERO,
        }
    }

    /// See the `structure_generation` field. Readers memoising `NodeId`
    /// indexes must rebuild when this changes.
    pub fn structure_generation(&self) -> u64 {
        self.structure_generation.load(Ordering::Relaxed)
    }

    /// Record one structure change whose effect is confined to `parent`'s
    /// child list (attach, detach, or dropping an attached node). Bumps the
    /// structure generation and remembers `parent` so
    /// [`Self::structure_changes_since`] can hand consumers the exact
    /// subtrees to re-walk.
    ///
    /// Callers must only use this for changes a walk from the root can
    /// observe. In particular the layout/construct machinery creates and
    /// drops *anonymous block / pseudo element* nodes directly in the tree
    /// without calling in: those never carry `data-bid`/`data-task-bid` /
    /// `data-math`, so no cached node-id index changes when they churn.
    pub(crate) fn record_structure_change(&self, parent: NodeId) {
        let mut dirty = self.structure_dirty.lock().unwrap_or_else(|e| e.into_inner());
        let current = self.structure_generation.load(Ordering::Relaxed);
        if dirty.parents.len() >= STRUCTURE_CHANGE_LOG_CAP {
            // Too many distinct parents in one batch (e.g. a fragment insert
            // in the middle of the document): forget the attribution and
            // start tracking from *this* generation. Consumers older than
            // that get `None` from `structure_changes_since` and rebuild
            // everything — correct, just not incremental.
            dirty.parents.clear();
            dirty.since = current + 1;
        }
        dirty.parents.insert(parent);
        self.structure_generation.fetch_add(1, Ordering::Relaxed);
    }

    /// All parents whose child list changed at a generation **greater** than
    /// `generation`, deduplicated. Returns `None` when the log no longer
    /// reaches back that far (overflow, or a change that could not be
    /// attributed to a parent): the caller must rebuild its index from the
    /// root instead of repairing it.
    pub fn structure_changes_since(&self, generation: u64) -> Option<Vec<NodeId>> {
        let dirty = self.structure_dirty.lock().unwrap_or_else(|e| e.into_inner());
        if generation < dirty.since {
            return None;
        }
        Some(dirty.parents.iter().copied().collect())
    }

    /// Tell the log that a consumer has an index of the tree as of
    /// `generation`: every change up to it is now reflected in that index, so
    /// older attribution can be dropped. Called by wysiwyg after a (re)build
    /// or a successful incremental repair.
    ///
    /// The DOM is only ever touched by the UI thread (`NodeHandle` is not
    /// `Send`), so there is no other index at an older generation to keep
    /// attribution for.
    pub fn note_structure_indexed(&self, generation: u64) {
        let mut dirty = self.structure_dirty.lock().unwrap_or_else(|e| e.into_inner());
        if generation >= dirty.since {
            dirty.since = generation;
            dirty.parents.clear();
        }
    }

    pub fn viewport_scroll(&self) -> crate::Point<f64> {
        self.viewport_scroll
    }

    /// Memoised paint geometry may depend on the viewport scroll, so this
    /// bumps the geometry generation like any other scroll write.
    pub(crate) fn set_viewport_scroll(&mut self, scroll: crate::Point<f64>) {
        self.viewport_scroll = scroll;
        self.bump_geometry_generation();
    }

    /// See the `geometry_generation` field.
    pub fn geometry_generation(&self) -> u64 {
        self.geometry_generation.load(Ordering::Relaxed)
    }

    pub(crate) fn bump_geometry_generation(&self) {
        self.geometry_generation.fetch_add(1, Ordering::Relaxed);
    }

    /// The number of live nodes in the map.
    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// Whether `id` resolves to a live node.
    pub fn contains_key(&self, id: NodeId) -> bool {
        self.map.contains_key(to_key(id))
    }

    /// Get a reference to the node with the given id, if it is still live.
    pub fn get(&self, id: NodeId) -> Option<&Node> {
        self.map.get(to_key(id))
    }

    /// Get a mutable reference to the node with the given id, if it is still live.
    pub fn get_mut(&mut self, id: NodeId) -> Option<&mut Node> {
        self.map.get_mut(to_key(id))
    }

    /// Insert a node constructed with knowledge of its own id.
    pub(crate) fn insert_with_key(&mut self, f: impl FnOnce(NodeId) -> Node) -> NodeId {
        // Creating a node is not a tree-shape change: until something attaches
        // it, a walk from the root cannot reach it, so no cached `NodeId`
        // index can be stale because of it. The attach that follows records
        // the change.
        to_id(self.map.insert_with_key(|key| f(to_id(key))))
    }

    /// Remove the node with the given id, returning it if it was still live.
    pub(crate) fn remove(&mut self, id: NodeId) -> Option<Node> {
        // Attribute the drop to the parent only while that parent is itself
        // still live: dropping nodes inside an already-removed subtree is
        // covered by the ancestor's own entry (`remove_and_drop_all_children`
        // recurses this way), and a drop of an already-detached node changes
        // nothing a walk from the root can see.
        let parent = self
            .map
            .get(to_key(id))
            .and_then(|node| node.parent)
            .filter(|&p| self.map.contains_key(to_key(p)));
        let removed = self.map.remove(to_key(id));
        if let (true, Some(parent)) = (removed.is_some(), parent) {
            self.record_structure_change(parent);
        }
        removed
    }

    /// Iterate over all live `(NodeId, &Node)` pairs.
    pub fn iter(&self) -> impl Iterator<Item = (NodeId, &Node)> {
        self.map.iter().map(|(key, node)| (to_id(key), node))
    }

    /// Iterate over all live `(NodeId, &mut Node)` pairs.
    pub fn iter_mut(&mut self) -> impl Iterator<Item = (NodeId, &mut Node)> {
        self.map.iter_mut().map(|(key, node)| (to_id(key), node))
    }
}

impl Index<NodeId> for NodeTree {
    type Output = Node;

    #[track_caller]
    #[inline]
    fn index(&self, id: NodeId) -> &Node {
        &self.map[to_key(id)]
    }
}

impl IndexMut<NodeId> for NodeTree {
    #[track_caller]
    #[inline]
    fn index_mut(&mut self, id: NodeId) -> &mut Node {
        &mut self.map[to_key(id)]
    }
}
