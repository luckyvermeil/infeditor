//! View state of the whole-document JSON preview (tree view).
//!
//! Reuses [`crate::jsonl`]'s parsing and flattening layers (sharing the same
//! `Node` / `flatten` as the JSONL preview) to parse **the entire document**
//! as one JSON value into a collapsible tree that **directly replaces the
//! text box** (modeled on Firefox's built-in JSON viewer: colored types,
//! collapsible containers, `{…} N items` summary when collapsed).
//!
//! Design points:
//! - while the preview is on, there are no "document lines" on screen, only
//!   virtual rows expanded from the tree;
//! - collapsing/expanding a container node only touches the `collapsed` set
//!   and rebuilds `rows`, **never called per frame**;
//! - `refresh` runs every frame, but in the common case (`gen` unchanged) it
//!   does two `u64` comparisons and returns.

use std::collections::BTreeSet;

use crate::buffer::doc::TextDocument;
use crate::jsonl::{self, Node, Row};

/// Byte cap for whole-document preview (past it the preview shows a notice
/// instead of hitting a crash path).
pub const MAX_DOC_BYTES: usize = 8 << 20;

#[derive(Clone, Debug, Default)]
pub struct JsonView {
    /// View toggle (tree view ↔ text). **On by default for a valid, in-cap
    /// .json document** (visual-first default, see [`JsonView::for_doc`]);
    /// oversized or unparseable documents fall back to the text view.
    /// Toggled from the toolbar.
    pub enabled: bool,
    tree: Option<Tree>,
    cache: Option<Cache>,
    /// Dedup flag for the "preview unavailable" notice on oversized
    /// documents: one dialog per document.
    pub(crate) notified_too_large: bool,
}

#[derive(Clone, Debug)]
struct Tree {
    root: Option<Node>,
    rows: Vec<Row>,
    collapsed: BTreeSet<u32>,
    err: Option<String>,
}

#[derive(Clone, Debug)]
struct Cache {
    gen: u64,
    lines: u64,
    total: u64,
    /// Language the error row was rendered in, `None` for a tree that carries no
    /// text. A valid tree is language-neutral, so a switch never reparses it; an
    /// error row would otherwise keep the language it was parsed under.
    err_locale: Option<i18n::Locale>,
}

impl JsonView {
    /// Per-language-spec initialization at document open (called from
    /// [`crate::ui::state::Tab::with_spec`]): a **valid, in-cap** JSON
    /// document opens straight into the tree view — the visual-first default,
    /// the same spirit as csv/tsv → table and JSON Lines → preview. The tree
    /// is built eagerly here and the cache pre-populated, so the first frame
    /// renders from it instead of reparsing.
    ///
    /// Two deliberate fallbacks to the text view:
    /// - **oversized** documents ([`Self::is_too_large`]): the tree could only
    ///   show the "too large" notice — text is the useful view;
    /// - **unparseable** documents: the tree view is *read-only* (the composer
    ///   blocks editing keys while it is on), so dropping a broken file into a
    ///   one-line error row it cannot leave would be hostile — fixing broken
    ///   JSON needs the text view. The toolbar switch remains either way.
    pub fn for_doc(name: &str, doc: &TextDocument) -> JsonView {
        if name != "JSON" || Self::is_too_large(doc) {
            return JsonView::default();
        }
        let full = doc_full_text(doc);
        let Ok(root) = jsonl::parse_line(&full) else {
            return JsonView::default();
        };
        let (rows, _) = jsonl::flatten(&root, &BTreeSet::new());
        let total = rows.len() as u64;
        JsonView {
            enabled: true,
            tree: Some(Tree {
                root: Some(root),
                rows,
                collapsed: BTreeSet::new(),
                err: None,
            }),
            cache: Some(Cache {
                gen: doc.gen,
                lines: doc.line_count(),
                total,
                err_locale: None,
            }),
            notified_too_large: false,
        }
    }

    /// Called once per frame at the start of `viewport::build`.
    ///
    /// Fast path (`gen`, `line_count` unchanged): two `u64` comparisons and
    /// return; `gen` changed (edit/undo/reload) ⇒ reparse the whole document
    /// (≤ 8 MiB, click cadence).
    ///
    /// `loc` is the language of the document's state, used for the one row this
    /// view ever renders text in ([`i18n::Fmt::JsonTreeTooLarge`] /
    /// [`i18n::Fmt::JsonTreeInvalid`]). It is an argument rather than
    /// `i18n::current()`: the preview must follow the state (README §3), and a
    /// language switch re-renders that row instead of leaving it in the old
    /// language. Trees without text are language-neutral — they keep their cache.
    pub fn refresh(&mut self, doc: &TextDocument, loc: i18n::Locale) {
        if !self.enabled {
            self.cache = None;
            self.tree = None;
            return;
        }
        let key = (doc.gen, doc.line_count());
        if let Some(c) = &self.cache {
            let text_current = c.err_locale.is_none_or(|l| l == loc);
            if (c.gen, c.lines) == key && text_current && self.tree.is_some() {
                return;
            }
        }
        // Cheap "is it too large" check first (reads `total_bytes` only, no
        // full-text join); past the cap just show a notice, skipping parse /
        // flatten, so an oversized document never allocates the whole text.
        if doc.total_bytes() as usize > MAX_DOC_BYTES {
            let tree = Tree::err(
                loc.tr(i18n::Fmt::JsonTreeTooLarge {
                    mib: doc.total_bytes() >> 20,
                })
                .into_owned(),
            );
            let total = tree.rows.len() as u64;
            self.tree = Some(tree);
            self.cache = Some(Cache {
                gen: key.0,
                lines: key.1,
                total,
                err_locale: Some(loc),
            });
            return;
        }
        let full = doc_full_text(doc);
        let tree = match jsonl::parse_line(&full) {
            Ok(root) => {
                let (rows, _) = jsonl::flatten(&root, &BTreeSet::new());
                Tree {
                    root: Some(root),
                    rows,
                    collapsed: BTreeSet::new(),
                    err: None,
                }
            }
            Err(m) => Tree::err(loc.tr(i18n::Fmt::JsonTreeInvalid { err: m }).into_owned()),
        };
        let total = tree.rows.len() as u64;
        let err_locale = tree.err.as_ref().map(|_| loc);
        self.tree = Some(tree);
        self.cache = Some(Cache {
            gen: key.0,
            lines: key.1,
            total,
            err_locale,
        });
    }

    /// Read-only probe of whether [`Self::refresh`] would do real work (for
    /// pre-render self-healing). The condition must match this method's early
    /// return verbatim — **including `loc`**, since a language switch makes
    /// `refresh` re-render the error row — or self-healing will not converge.
    pub fn stale(&self, doc: &TextDocument, loc: i18n::Locale) -> bool {
        if !self.enabled {
            return self.cache.is_some() || self.tree.is_some();
        }
        let key = (doc.gen, doc.line_count());
        !(self
            .cache
            .as_ref()
            .is_some_and(|c| (c.gen, c.lines) == key && c.err_locale.is_none_or(|l| l == loc))
            && self.tree.is_some())
    }

    /// Whether the document exceeds the tree preview's byte/char cap (cheap:
    /// reads `total_bytes` only, no full-text join). Used for a quick check
    /// before toggling the preview and for per-frame auto-notices.
    pub fn is_too_large(doc: &TextDocument) -> bool {
        doc.total_bytes() as usize > MAX_DOC_BYTES
    }

    /// Total virtual row count (scroll bound). When off, it is the document
    /// line count (identity fast path).
    pub fn total_rows(&self, doc_lines: u64) -> u64 {
        if !self.enabled {
            return doc_lines;
        }
        self.cache.as_ref().map(|c| c.total).unwrap_or(doc_lines)
    }

    /// Current tree's expanded rows (collapsed state applied).
    pub fn rows(&self) -> &[Row] {
        self.tree.as_ref().map(|t| t.rows.as_slice()).unwrap_or(&[])
    }

    /// Parse error message (if any).
    pub fn error(&self) -> Option<&str> {
        self.tree.as_ref().and_then(|t| t.err.as_deref())
    }

    /// Collapse/expand a container node inside the tree (rebuilds the tree's
    /// `rows`). Returns whether anything changed; a nonexistent `node` id is
    /// a no-op (does not pollute the collapsed set).
    pub fn toggle_node(&mut self, node: u32) -> bool {
        let Some(t) = self.tree.as_mut() else {
            return false;
        };
        let Some(root) = t.root.as_ref() else {
            return false;
        };
        if t.collapsed.contains(&node) {
            t.collapsed.remove(&node);
        } else if !tree_has_id(root, node) {
            return false;
        } else {
            t.collapsed.insert(node);
        }
        let (rows, _) = jsonl::flatten(root, &t.collapsed);
        t.rows = rows;
        if let Some(c) = self.cache.as_mut() {
            c.total = t.rows.len() as u64;
        }
        true
    }

    /// Collapse all containers (root only remains).
    pub fn collapse_all(&mut self) {
        let Some(t) = self.tree.as_mut() else {
            return;
        };
        let Some(root) = t.root.as_ref() else {
            return;
        };
        let mut ids = BTreeSet::new();
        collect_containers(root, &mut ids);
        t.collapsed = ids;
        let (rows, _) = jsonl::flatten(root, &t.collapsed);
        t.rows = rows;
        if let Some(c) = self.cache.as_mut() {
            c.total = t.rows.len() as u64;
        }
    }

    /// Expand all containers (full tree).
    pub fn expand_all(&mut self) {
        let Some(t) = self.tree.as_mut() else {
            return;
        };
        let Some(root) = t.root.as_ref() else {
            return;
        };
        t.collapsed.clear();
        let (rows, _) = jsonl::flatten(root, &t.collapsed);
        t.rows = rows;
        if let Some(c) = self.cache.as_mut() {
            c.total = t.rows.len() as u64;
        }
    }

    /// Total node count in the tree (for status bar / toolbar summaries).
    pub fn node_count(&self) -> Option<usize> {
        self.tree
            .as_ref()
            .and_then(|t| t.root.as_ref())
            .map(count_nodes)
    }
}

impl Tree {
    fn err(msg: String) -> Tree {
        Tree {
            root: None,
            rows: vec![Row {
                node: 0,
                depth: 0,
                segs: vec![(crate::jsonl::Tok::Ellipsis, format!("⚠ {msg}"))],
                foldable: false,
                collapsed: false,
            }],
            collapsed: BTreeSet::new(),
            err: Some(msg),
        }
    }
}

/// Join the whole document text (for preview parsing; [`MAX_DOC_BYTES`]
/// gates the cap).
fn doc_full_text(doc: &TextDocument) -> String {
    let n = doc.line_count();
    let cap = (doc.table.len() as usize).saturating_add(1);
    let mut s = String::with_capacity(cap);
    for l in 0..n {
        s.push_str(&doc.line_text(l));
    }
    s
}

fn tree_has_id(root: &Node, id: u32) -> bool {
    if root.id == id {
        return true;
    }
    root.children.iter().any(|c| tree_has_id(c, id))
}

fn collect_containers(root: &Node, ids: &mut BTreeSet<u32>) {
    if root.is_container() {
        ids.insert(root.id);
        for c in &root.children {
            collect_containers(c, ids);
        }
    }
}

fn count_nodes(root: &Node) -> usize {
    1 + root.children.iter().map(count_nodes).sum::<usize>()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buffer::TextDocument;
    use crate::jsonl::Tok;

    fn view_of(text: &str) -> JsonView {
        let doc = TextDocument::from(text.to_string());
        let mut v = JsonView {
            enabled: true,
            ..Default::default()
        };
        // An explicit language, never the process mirror: the mirror is written
        // by whichever test restored settings last, so asserting on it makes the
        // result depend on test order.
        v.refresh(&doc, i18n::Locale::ZhCn);
        v
    }

    #[test]
    fn preview_parses_whole_document_as_one_tree() {
        let v = view_of(r#"{"id":7,"user":{"name":"阿白","tags":["a","b"]},"ok":true}"#);
        assert!(v.error().is_none(), "should parse: {:?}", v.error());
        // root { / id / user{ / name / tags[ / a / b / ] / } / ok / } ≈ 11 rows
        let rows = v.rows();
        assert!(
            rows.len() > 5,
            "the tree should expand to multiple rows: {}",
            rows.len()
        );
        assert_eq!(rows[0].segs, vec![(Tok::Punct, "{".into())]);
        assert!(rows[0].foldable);
    }

    #[test]
    fn preview_reports_parse_error_as_single_row() {
        let v = view_of("{not valid");
        assert!(v.error().is_some());
        assert_eq!(v.total_rows(1), 1);
        assert!(v.rows()[0].segs[0].1.contains("JSON 无效"));
    }

    /// The error row is the only text this view renders, and it is rendered at
    /// `refresh` time: a language switch must re-render it, while a tree without
    /// text (valid JSON) is language-neutral and keeps its cache.
    #[test]
    fn language_switch_rerenders_the_error_row() {
        let doc = TextDocument::from("{not valid".to_string());
        let mut v = JsonView {
            enabled: true,
            ..Default::default()
        };
        v.refresh(&doc, i18n::Locale::ZhCn);
        assert!(v.rows()[0].segs[0].1.contains("JSON 无效"));
        // self-heal must report the switch as work, otherwise the frame loop
        // never calls refresh and the row keeps the old language
        assert!(v.stale(&doc, i18n::Locale::EnUs));
        v.refresh(&doc, i18n::Locale::EnUs);
        // same document, no reparse key change: only the language did
        assert!(
            v.rows()[0].segs[0].1.contains("Invalid JSON"),
            "{:?}",
            v.rows()[0].segs[0].1
        );

        let ok = TextDocument::from(r#"{"a":1}"#.to_string());
        let mut v = JsonView {
            enabled: true,
            ..Default::default()
        };
        v.refresh(&ok, i18n::Locale::ZhCn);
        let rows = v.rows().len();
        v.refresh(&ok, i18n::Locale::EnUs);
        assert_eq!(v.rows().len(), rows, "a valid tree is not re-rendered");
    }

    #[test]
    fn preview_collapse_and_expand_all() {
        let mut v = view_of(r#"{"a":[1,2,3],"b":{"c":1}}"#);
        assert!(v.error().is_none());
        let full = v.rows().len();
        assert!(full > 3);
        v.collapse_all();
        let collapsed = v.rows().len();
        assert!(
            collapsed < full,
            "collapse-all should significantly reduce rows"
        );
        // With the root collapsed only the root row should remain (with summary)
        assert_eq!(collapsed, 1);
        v.expand_all();
        assert_eq!(
            v.rows().len(),
            full,
            "expand-all should restore the original tree"
        );
    }

    #[test]
    fn preview_toggle_node_folds_subtree() {
        let mut v = view_of(r#"{"a":{"x":1,"y":2},"b":3}"#);
        let full = v.rows().len();
        // Find container node a's id (the node of the second row when expanded)
        let a_node = v.rows()[1].node;
        assert!(v.toggle_node(a_node));
        let less = v.rows().len();
        assert!(less < full);
        // Expand back
        assert!(v.toggle_node(a_node));
        assert_eq!(v.rows().len(), full);
        // A nonexistent id must not panic or mutate wrongly
        assert!(!v.toggle_node(u32::MAX - 1));
    }

    #[test]
    fn preview_refresh_is_identity_when_disabled() {
        let doc = TextDocument::from("x\ny\nz\n".to_string());
        let mut v = JsonView::default();
        v.refresh(&doc, i18n::Locale::ZhCn);
        assert_eq!(v.total_rows(3), 3);
        assert_eq!(v.rows().len(), 0);
    }

    /// Visual-first default: a valid JSON document opens straight into the
    /// tree view, with the cache prebuilt so the first frame does not reparse.
    #[test]
    fn for_doc_opens_valid_json_in_tree_view_with_prebuilt_cache() {
        let doc = TextDocument::from(r#"{"id":7,"user":{"name":"阿白"},"ok":true}"#.to_string());
        let v = JsonView::for_doc("JSON", &doc);
        assert!(
            v.enabled,
            "valid JSON should open straight into the tree view"
        );
        assert!(v.error().is_none());
        assert!(
            !v.stale(&doc, i18n::Locale::ZhCn),
            "the tree cache must be prebuilt for the first frame"
        );
        assert!(
            v.rows().len() > 5,
            "tree rows should be present: {}",
            v.rows().len()
        );
        // refresh must hit the cache (gen/lines unchanged), not reparse
        let mut v2 = v;
        v2.refresh(&doc, i18n::Locale::ZhCn);
        assert!(!v2.stale(&doc, i18n::Locale::ZhCn));
    }

    /// Only the JSON language spec gets the tree-view default.
    #[test]
    fn for_doc_ignores_non_json_languages() {
        let doc = TextDocument::from(r#"{"a":1}"#.to_string());
        assert!(!JsonView::for_doc("JSON Lines", &doc).enabled);
        assert!(!JsonView::for_doc("Plain Text", &doc).enabled);
    }

    /// Broken JSON must open in the text view: the tree view is read-only, so
    /// an error row the user cannot leave or fix would be hostile.
    #[test]
    fn for_doc_falls_back_to_text_for_unparseable_json() {
        let doc = TextDocument::from("{not valid".to_string());
        let v = JsonView::for_doc("JSON", &doc);
        assert!(!v.enabled, "broken JSON must open in the text view");
        assert!(v.rows().is_empty());
    }

    /// Oversized documents keep the text view (the tree could only show the
    /// "too large" notice; `json_auto_warn_too_large` explains per frame).
    #[test]
    fn for_doc_falls_back_to_text_for_oversized_json() {
        let big = format!("\"{}\"", "a".repeat(MAX_DOC_BYTES + 10));
        let doc = TextDocument::from(big);
        assert!(!JsonView::for_doc("JSON", &doc).enabled);
    }

    #[test]
    fn preview_rejects_overlarge_document() {
        let big = format!("\"{}\"", "a".repeat(MAX_DOC_BYTES + 10));
        let v = view_of(&big);
        assert!(v.error().is_some_and(|e| e.contains("过大")));
        assert_eq!(v.total_rows(1), 1);
        // The cheap cap check matches refresh's over-limit branch
        let doc = TextDocument::from(big.clone());
        assert!(JsonView::is_too_large(&doc));
        let small = TextDocument::from("{\"a\":1}".to_string());
        assert!(!JsonView::is_too_large(&small));
    }
}
