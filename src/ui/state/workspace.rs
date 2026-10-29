//! Workspace (folder sidebar) state: the directory tree shown in the left panel.
//!
//! # Design
//!
//! * **Lazy loading**: [`EditorState::ws_nodes`] caches only the direct
//!   children of directories *that have been expanded* — one `read_dir` per
//!   expanded level, so even a tree with hundreds of thousands of files never
//!   stalls on folder open; refresh ([`EditorState::ws_refresh`]) re-reads
//!   only the root and the expanded directories.
//! * **Rendering is a pure read**: the visible rows are a *precomputed flat
//!   cache* [`EditorState::ws_rows`] (pre-order, following only expanded
//!   branches); components clone-and-render following the `tabs.rs` snapshot
//!   pattern and take no part in `state::heal` — every field read during
//!   rendering changes only inside event callbacks (click/refresh/open), and
//!   each change rebuilds the cache synchronously at its end. Neither side
//!   promises to "derive it from other fields", so there is simply no drift
//!   to probe/correct.
//! * **Layout**: while the sidebar is visible, [`EditorState::relayout`]
//!   shifts the whole viewport right by [`EditorState::sidebar_w`]
//!   (= [`SIDEBAR_W`](super::SIDEBAR_W)); scrollbar and minimap position
//!   against the `view` rect and follow automatically. Collapsed simply
//!   means no sidebar (the icon toggle lives at the bottom-left of the
//!   status bar, see `components::status`).

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use super::{EditorState, Fmt, Msg};

/// A node of the directory tree (a direct child of some directory).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WsNode {
    pub path: PathBuf,
    pub name: String,
    pub is_dir: bool,
}

/// One visible row of the sidebar: the tree's pre-order traversal flattened,
/// following only expanded directories.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WsRow {
    pub path: PathBuf,
    pub name: String,
    pub is_dir: bool,
    /// Nesting depth (direct children of the root = 0); drives indentation
    pub depth: usize,
    /// Whether a directory row is expanded (the render arrow needs it).
    /// Computed when the cache is built — components no longer re-probe the
    /// `ws_expanded` hash table per row (thousands of lookups per frame for
    /// a multi-thousand-row tree).
    pub expanded: bool,
}

impl EditorState {
    /// Whether the sidebar has anything to show (**not the same as currently
    /// visible**): a workspace exists, or the outline panel occupies the
    /// sidebar slot (outline is Markdown-only, see [`Self::outline_supported`]).
    ///
    /// No icon here — the sidebar toggle lives at the bottom-left of the
    /// status bar (`components::status`), so a collapsed panel means sidebar
    /// width 0, leaving no icon strip behind.
    pub fn sidebar_available(&self) -> bool {
        self.ws_root.is_some() || self.outline_panel_shown()
    }

    /// Width currently occupied by the sidebar (0 when collapsed / unavailable).
    pub fn sidebar_w(&self) -> f32 {
        if self.ws_show && self.sidebar_available() {
            self.ws_width
        } else {
            0.0
        }
    }

    /// File → Open Folder...: native directory picker + enter the workspace.
    pub fn ws_open_folder_dialog(&mut self) {
        // Owner decoration lives in `winshell::dialog::owned` (only Windows
        // has owner semantics).
        let dlg = winshell::dialog::owned(rfd::FileDialog::new())
            .set_title(self.tr(Msg::DlgOpenFolderTitle));
        if let Some(p) = dlg.pick_folder() {
            self.ws_enter(p);
        }
    }

    /// Open `path` as the workspace root (menu path; the sidebar is forced
    /// open). An invalid path does not expand the sidebar — a failed open
    /// must not change the layout, let alone persist a bad path into settings.
    pub fn ws_enter(&mut self, path: PathBuf) {
        if self.ws_load_root(path) {
            self.ws_show = true;
        }
        self.relayout();
    }

    /// Startup restore: the last workspace recorded in settings. The
    /// difference from [`Self::ws_enter`] is that it does **not** touch
    /// `ws_show` — if the user had collapsed the sidebar, a restart must not
    /// force it open. A missing directory is dropped silently (a moved drive
    /// should not error on every startup).
    pub fn ws_restore(&mut self, path: PathBuf) {
        if !path.is_dir() {
            self.ws_root = None;
            self.ws_show = false;
            return;
        }
        self.ws_load_root(path);
        self.relayout();
    }

    /// Close the workspace (× in the sidebar header).
    pub fn ws_close(&mut self) {
        if self.ws_root.is_none() {
            return;
        }
        self.ws_root = None;
        self.ws_nodes.clear();
        self.ws_expanded.clear();
        self.ws_rows = Rc::new(Vec::new());
        self.ws_show = false;
        self.relayout();
        self.status = Some(Msg::StatusWorkspaceClosed.into());
    }

    /// View → Folder Sidebar (Ctrl+B). With no workspace ever opened, first
    /// hint to open a directory instead of popping the native picker here —
    /// a mere "toggle visibility" command blocking on a dialog when exercised
    /// by tests/macros is unacceptable.
    ///
    /// While the outline panel occupies the sidebar, this treats it as
    /// "folder sidebar not shown" — triggering switches back to the directory
    /// tree panel (same semantics as VS Code's Ctrl+Shift+E) rather than
    /// closing the whole sidebar.
    pub fn toggle_sidebar(&mut self) {
        if self.ws_root.is_none() {
            self.status = Some(Msg::StatusNoFolderOpen.into());
            return;
        }
        if self.ws_show && self.side_panel == crate::ui::state::SidePanel::Workspace {
            self.ws_show = false;
        } else {
            self.side_panel = crate::ui::state::SidePanel::Workspace;
            self.ws_show = true;
        }
        self.relayout();
        self.status = Some(
            if self.ws_show {
                Msg::StatusSidebarOn
            } else {
                Msg::StatusSidebarOff
            }
            .into(),
        );
    }

    /// Collapse the sidebar (× in the panel header): hides only; does not
    /// close the workspace or lose panel state — different from
    /// [`Self::ws_close`] (which truly closes the workspace and clears the
    /// directory cache).
    pub fn sidebar_hide(&mut self) {
        if !self.ws_show {
            return;
        }
        self.ws_show = false;
        self.relayout();
    }

    /// The "Explorer" button at the bottom-left of the status bar: toggles
    /// the workspace panel.
    ///
    /// Same semantics as VS Code's explorer icon — collapse if the panel is
    /// already open; switch back to the directory tree if the outline panel
    /// is showing. Returns `false` with no workspace, leaving the native
    /// directory picker to the caller (the status bar is UI edge; `state`
    /// does no blocking dialogs).
    pub fn sidebar_toggle(&mut self) -> bool {
        if self.ws_root.is_none() {
            return false;
        }
        self.toggle_sidebar();
        true
    }

    /// Row `index` of the sidebar was clicked: collapse/expand a directory,
    /// open a file as a tab.
    ///
    /// Out of range is the norm (menus/tree render last frame's rows) —
    /// ignore silently, never panic.
    pub fn ws_click(&mut self, index: usize) {
        let Some(row) = self.ws_rows.get(index) else {
            return;
        };
        let (path, is_dir) = (row.path.clone(), row.is_dir);
        if is_dir {
            self.ws_toggle_expand(&path);
        } else {
            self.open_path_async(path);
        }
    }

    /// Collapse/expand a directory. On first expand, read one level
    /// synchronously (a single `read_dir` is microseconds to milliseconds —
    /// not worth a thread).
    pub fn ws_toggle_expand(&mut self, dir: &Path) {
        if !self.ws_expanded.remove(dir) {
            // A missing **or empty** cache is (re)read: an empty cache cannot
            // tell "already read, and genuinely an empty directory" from
            // "cache cleared during a render gap"; re-reading once costs
            // nothing — without the retry the directory would show ▾ with no
            // child rows, and it would never heal
            if self.ws_nodes.get(dir).is_none_or(|v| v.is_empty()) {
                self.ws_load_dir(dir);
            }
            self.ws_expanded.insert(dir.to_path_buf());
        }
        self.ws_rebuild_rows();
    }

    /// Re-read the root and all still-existing expanded directories
    /// ("Refresh" in the sidebar header).
    pub fn ws_refresh(&mut self) {
        let Some(root) = self.ws_root.clone() else {
            return;
        };
        if !root.is_dir() {
            // The workspace itself was deleted/unplugged: close entirely,
            // clearing the row cache
            self.ws_close();
            self.status = Some(Msg::StatusWorkspaceGone.into());
            return;
        }
        let dirs: Vec<PathBuf> = self.ws_expanded.iter().cloned().collect();
        for d in dirs {
            if d.is_dir() {
                self.ws_load_dir(&d);
            } else {
                // Subdirectory deleted: collapse it (cache and expanded set
                // cleared together; it vanishes from the rows)
                self.ws_nodes.remove(&d);
                self.ws_expanded.remove(&d);
            }
        }
        self.ws_load_dir(&root);
        self.ws_rebuild_rows();
        self.status = Some(Msg::StatusWorkspaceRefreshed.into());
    }

    /// Set the root and load the first level (used by [`Self::ws_enter`] /
    /// [`Self::ws_restore`]). Returns success (`false` when the path is not
    /// a directory; the caller decides whether to expand the sidebar).
    fn ws_load_root(&mut self, path: PathBuf) -> bool {
        if !path.is_dir() {
            self.status = Some(
                Fmt::StatusNotADirectory {
                    path: path.display().to_string(),
                }
                .into(),
            );
            return false;
        }
        self.ws_root = Some(path.clone());
        self.ws_nodes.clear();
        self.ws_expanded.clear();
        self.ws_expanded.insert(path.clone());
        self.ws_load_dir(&path);
        self.ws_rebuild_rows();
        self.status = Some(
            Fmt::StatusWorkingDirectory {
                path: path.display().to_string(),
            }
            .into(),
        );
        true
    }

    /// Read one directory level into the cache (directories first, then
    /// files, each sorted case-insensitively by name).
    fn ws_load_dir(&mut self, dir: &Path) {
        self.ws_nodes.insert(dir.to_path_buf(), ws_read_dir(dir));
    }

    /// Rebuild the visible-row cache from `ws_nodes` × `ws_expanded`.
    fn ws_rebuild_rows(&mut self) {
        let mut rows = Vec::new();
        if let Some(root) = self.ws_root.clone() {
            let nodes = &self.ws_nodes;
            let expanded = &self.ws_expanded;
            push_children(&root, 0, nodes, expanded, &mut rows);
        }
        self.ws_rows = Rc::new(rows);
    }
}

/// Pre-order traversal: a directory row is immediately followed by its
/// child rows (if expanded).
fn push_children(
    dir: &Path,
    depth: usize,
    nodes: &std::collections::HashMap<PathBuf, Vec<WsNode>>,
    expanded: &HashSet<PathBuf>,
    out: &mut Vec<WsRow>,
) {
    let Some(children) = nodes.get(dir) else {
        return;
    };
    for n in children {
        let open = n.is_dir && expanded.contains(&n.path);
        out.push(WsRow {
            path: n.path.clone(),
            name: n.name.clone(),
            is_dir: n.is_dir,
            depth,
            expanded: open,
        });
        if open {
            push_children(&n.path, depth + 1, nodes, expanded, out);
        }
    }
}

/// Read one directory level; return empty when it cannot be opened
/// (permissions/racy deletion) — one directory is not worth an error interrupt.
fn ws_read_dir(dir: &Path) -> Vec<WsNode> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut nodes: Vec<WsNode> = rd
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let is_dir = e.file_type().ok()?.is_dir();
            Some(WsNode {
                path: e.path(),
                name: e.file_name().to_string_lossy().into_owned(),
                is_dir,
            })
        })
        .collect();
    // Directories first, files after, each sorted case-insensitively by name
    // (Windows Explorer semantics). sort_by_cached_key: the key is computed
    // once per element — per-comparison to_lowercase on a 100k-entry
    // directory adds hundreds of thousands of String allocations (an expand
    // click could stall for hundreds of ms).
    nodes.sort_by_cached_key(|n| (std::cmp::Reverse(n.is_dir), n.name.to_lowercase()));
    nodes
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::state::SIDEBAR_W;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// One unique temp directory per test (parallel tests do not interfere).
    fn tmp_dir(tag: &str) -> PathBuf {
        static N: AtomicUsize = AtomicUsize::new(0);
        let d = std::env::temp_dir().join(format!(
            "infeditor-ws-{}-{}-{}",
            std::process::id(),
            tag,
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn names(st: &EditorState) -> Vec<String> {
        st.ws_rows.iter().map(|r| r.name.clone()).collect()
    }

    #[test]
    fn ws_enter_lists_dirs_first_sorted_case_insensitively() {
        let d = tmp_dir("sort");
        std::fs::write(d.join("b.txt"), "b").unwrap();
        std::fs::write(d.join("A.txt"), "a").unwrap();
        std::fs::create_dir_all(d.join("zdir")).unwrap();

        let mut st = EditorState::minimal();
        st.ws_enter(d.clone());
        assert_eq!(st.ws_root.as_deref(), Some(d.as_path()));
        assert!(st.ws_show, "opening a workspace must expand the sidebar");
        assert_eq!(names(&st), vec!["zdir", "A.txt", "b.txt"]);
        assert_eq!(st.ws_rows[0].depth, 0);
        assert!(st.ws_rows[0].is_dir);
        assert!(!st.ws_rows[1].is_dir);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn expand_and_collapse_nested_directories() {
        let d = tmp_dir("nested");
        std::fs::create_dir_all(d.join("outer/inner")).unwrap();
        std::fs::write(d.join("outer/inner/leaf.txt"), "x").unwrap();
        std::fs::write(d.join("outer/mid.txt"), "x").unwrap();

        let mut st = EditorState::minimal();
        st.ws_enter(d.clone());
        assert_eq!(
            names(&st),
            vec!["outer"],
            "root level holds only directories"
        );

        st.ws_click(0); // expand outer
        assert_eq!(names(&st), vec!["outer", "inner", "mid.txt"]);
        assert_eq!(st.ws_rows[1].depth, 1);
        assert_eq!(st.ws_rows[2].depth, 1);

        st.ws_click(1); // expand inner
        assert_eq!(names(&st), vec!["outer", "inner", "leaf.txt", "mid.txt"]);
        assert_eq!(st.ws_rows[2].depth, 2);

        st.ws_click(0); // collapse outer (the whole subtree disappears with it)
        assert_eq!(names(&st), vec!["outer"]);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn clicking_a_file_opens_it_as_a_tab() {
        let d = tmp_dir("open");
        std::fs::write(d.join("hello.txt"), "hello").unwrap();

        let mut st = EditorState::minimal();
        st.ws_enter(d);
        st.ws_click(0); // hello.txt
        assert_eq!(
            st.pending_opens.len(),
            1,
            "clicking a file must queue a background open"
        );
        st.poll_opens();
        // The initial untitled scratch tab gets dropped (drop_scratch_tab),
        // so the total stays 1 — what is asserted is "hello.txt became the
        // active document"
        assert_eq!(st.tabs.len(), 1);
        assert_eq!(st.active, 0);
        assert_eq!(
            st.tabs[0].doc.meta.display_name, "hello.txt",
            "clicking a sidebar file should open it as the active tab (status={:?})",
            st.status
        );
    }

    #[test]
    fn out_of_range_click_is_ignored() {
        let mut st = EditorState::minimal();
        st.ws_click(999); // no index into an empty tree should panic
        assert!(st.ws_rows.is_empty());
        assert_eq!(st.pending_opens.len(), 0);
    }

    #[test]
    fn refresh_picks_up_new_entries_and_prunes_deleted_dirs() {
        let d = tmp_dir("refresh");
        std::fs::create_dir_all(d.join("gone")).unwrap();
        std::fs::write(d.join("gone/x.txt"), "x").unwrap();

        let mut st = EditorState::minimal();
        st.ws_enter(d.clone());
        st.ws_click(0); // expand gone
        assert_eq!(names(&st), vec!["gone", "x.txt"]);

        std::fs::write(d.join("new.txt"), "n").unwrap();
        std::fs::remove_dir_all(d.join("gone")).unwrap();
        st.ws_refresh();
        assert_eq!(
            names(&st),
            vec!["new.txt"],
            "new file appears, deleted directory collapses entirely"
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn refresh_closes_workspace_when_root_vanishes() {
        let d = tmp_dir("vanish");
        let mut st = EditorState::minimal();
        st.ws_enter(d.clone());
        std::fs::remove_dir_all(&d).unwrap();
        st.ws_refresh();
        assert!(st.ws_root.is_none());
        assert!(st.ws_rows.is_empty());
        assert_eq!(st.sidebar_w(), 0.0);
    }

    #[test]
    fn sidebar_toggle_changes_layout_geometry() {
        let d = tmp_dir("layout");
        std::fs::write(d.join("a.txt"), "a").unwrap();

        let mut st = EditorState::minimal();
        st.set_window_size(1000.0, 800.0);
        let base_x = st.view.x;
        let base_w = st.view.w;
        assert_eq!(
            base_x, 0.0,
            "the viewport is no longer inset for a zoom bar; its left edge hugs the window"
        );

        st.ws_enter(d.clone());
        assert!((st.sidebar_w() - SIDEBAR_W).abs() < 0.01);
        assert!(
            (st.view.x - SIDEBAR_W).abs() < 0.01,
            "the viewport yields on the right"
        );
        assert!((st.view.w - (base_w - SIDEBAR_W)).abs() < 0.01);

        st.toggle_sidebar(); // collapse panel: the whole sidebar disappears (icon lives in the status bar)
        assert_eq!(st.sidebar_w(), 0.0);
        assert!(
            (st.view.x - base_x).abs() < 0.01,
            "viewport back to the window's left edge"
        );
        assert!((st.view.w - base_w).abs() < 0.01);

        st.toggle_sidebar(); // expand again
        assert!((st.sidebar_w() - SIDEBAR_W).abs() < 0.01);
        let _ = std::fs::remove_dir_all(&d);
    }

    /// The explorer toggle at the bottom-left of the status bar: switches
    /// the workspace panel; returns `false` with no workspace (the UI layer
    /// decides to pop a directory picker), leaving no width behind.
    #[test]
    fn status_bar_sidebar_toggle_switches_the_workspace_panel() {
        let d = tmp_dir("sb-toggle");
        std::fs::write(d.join("a.txt"), "a").unwrap();

        let mut st = EditorState::minimal();
        st.set_window_size(1000.0, 800.0);
        assert_eq!(
            st.sidebar_w(),
            0.0,
            "Plain Text + no workspace: entire sidebar does not appear"
        );
        assert!(
            !st.sidebar_toggle(),
            "no workspace: opening a directory is left to the caller"
        );
        assert_eq!(st.sidebar_w(), 0.0);

        st.ws_enter(d.clone());
        assert!(st.sidebar_toggle());
        assert!(!st.ws_show);
        assert_eq!(st.sidebar_w(), 0.0, "collapsed means no sidebar");
        assert!(st.sidebar_toggle());
        assert!(st.ws_show);
        assert!((st.sidebar_w() - SIDEBAR_W).abs() < 0.01);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn toggle_sidebar_without_a_workspace_only_hints() {
        let mut st = EditorState::minimal();
        st.toggle_sidebar();
        assert!(!st.ws_show);
        let key = st.status.as_ref().expect("status set").key();
        assert!(
            key.contains("StatusNoFolderOpen"),
            "should hint to open a folder first: {key}"
        );
    }

    #[test]
    fn ws_close_clears_everything() {
        let d = tmp_dir("close");
        std::fs::write(d.join("a.txt"), "a").unwrap();
        let mut st = EditorState::minimal();
        st.ws_enter(d);
        st.ws_close();
        assert!(st.ws_root.is_none());
        assert!(st.ws_nodes.is_empty());
        assert!(st.ws_expanded.is_empty());
        assert!(st.ws_rows.is_empty());
        assert!(!st.ws_show);
        assert_eq!(st.sidebar_w(), 0.0);
    }

    #[test]
    fn ws_restore_skips_missing_directory_and_keeps_show_untouched() {
        let mut st = EditorState::minimal();
        st.ws_show = false;
        st.ws_restore(PathBuf::from("Z:/definitely/not/here"));
        assert!(st.ws_root.is_none());
        assert!(
            !st.ws_show,
            "must not conjure the sidebar open when the directory is missing"
        );
    }

    #[test]
    fn ws_restore_loads_tree_without_forcing_visibility() {
        let d = tmp_dir("restore");
        std::fs::write(d.join("a.txt"), "a").unwrap();
        let mut st = EditorState::minimal();
        st.ws_show = false;
        st.ws_restore(d.clone());
        assert!(st.ws_root.is_some());
        assert_eq!(names(&st), vec!["a.txt"], "tree data is ready");
        assert!(
            !st.ws_show,
            "the user had collapsed the sidebar; restore keeps it collapsed"
        );
        assert_eq!(
            st.sidebar_w(),
            0.0,
            "collapsed means no sidebar and no layout width"
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn ws_enter_with_invalid_path_does_not_show_sidebar() {
        let mut st = EditorState::minimal();
        st.ws_enter(PathBuf::from("Z:/definitely/not/here"));
        assert!(st.ws_root.is_none());
        assert!(!st.ws_show, "an invalid path must not expand the sidebar");
        assert_eq!(st.sidebar_w(), 0.0);
        let key = st.status.as_ref().expect("status set").key();
        assert!(
            key.contains("StatusNotADirectory"),
            "should report the failure reason: {key}"
        );
    }

    #[test]
    fn expanding_a_dir_with_empty_cache_retries_read_dir() {
        let d = tmp_dir("retry");
        let sub = d.join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join("x.txt"), "x").unwrap();

        let mut st = EditorState::minimal();
        st.ws_enter(d.clone());
        st.ws_nodes.remove(&sub); // simulate sub's cache being cleared during a render gap (root cache remains)
        st.ws_click(0); // expand sub: an empty cache must be re-read to get child rows
        assert_eq!(
            names(&st),
            vec!["sub", "x.txt"],
            "expanding an empty cache must re-read the directory; cannot show ▾ with child rows forever"
        );
        let _ = std::fs::remove_dir_all(&d);
    }
}
