//! JSON documents: toggling and browsing the whole-document tree view
//! preview (collapsing/expanding container nodes, clicking fold markers).
//! Shares the parse layer of [`crate::jsonl`] with the JSONL preview
//! (per-document-line expansion), but previews the whole document and
//! replaces the text box outright (modelled on Firefox's built-in JSON
//! viewer).

use crate::ui::state::json;
use crate::ui::state::{Dialog, EditorState};
use crate::ui::{Fmt, Msg};

impl EditorState {
    /// Toolbar / View menu: toggle the JSON tree view (preview ↔ text).
    ///
    /// When on, the text box is replaced by a coloured collapsible tree;
    /// turning it off returns to plain text editing. Parse failures also
    /// enter the preview (showing a one-line error), so the text can be
    /// fixed in text mode and the preview re-opened.
    ///
    /// Huge documents (> 8 MiB): the tree view preview is unavailable — the
    /// feature is switched off outright and a dialog explains, keeping text
    /// mode to avoid allocating a huge buffer / expanding endless rows for
    /// one giant JSON.
    pub fn json_toggle_preview(&mut self) {
        let ai = self.active;
        let on = !self.tabs[ai].json.enabled;
        let msg: i18n::Message = if on {
            // Huge document: tree view preview unavailable; show a dialog and stay in text mode
            if json::JsonView::is_too_large(&self.tabs[ai].doc) {
                let mb = self.tabs[ai].doc.total_bytes() >> 20;
                self.status = Some(Fmt::StatusJsonTooLarge { mib: mb }.into());
                self.show_dialog(
                    Dialog::text(Msg::DlgJsonTreeTitle, Fmt::DlgJsonTreeBody { mib: mb })
                        .primary(Msg::CommonOk, None),
                );
                return;
            }
            let err = {
                let loc = self.locale;
                let tab = &mut self.tabs[ai];
                tab.json.refresh(&tab.doc, loc);
                tab.json.error().map(|e| e.to_string())
            };
            match err {
                Some(e) => Fmt::StatusViewPreviewError { err: e }.into(),
                None => Msg::StatusViewPreview.into(),
            }
        } else {
            Msg::StatusViewText.into()
        };
        self.tabs[ai].json.enabled = on;
        self.clamp_scroll();
        // view_switched still takes a String for now; the status write is done in place here (equivalent to it).
        self.status = Some(msg);
        self.relayout();
    }

    /// Warn once automatically when a huge (> 8 MiB) JSON document opens:
    /// the tree view preview is unavailable.
    ///
    /// Called every frame by `viewport::build`; deduplicated via
    /// `JsonView::notified_too_large` so the dialog appears once per
    /// document; switching away and back does not nag again.
    pub fn json_auto_warn_too_large(&mut self) {
        let ai = self.active;
        let (is_json, enabled, notified) = {
            let tab = &self.tabs[ai];
            (
                tab.spec.name == "JSON",
                tab.json.enabled,
                tab.json.notified_too_large,
            )
        };
        if !is_json || enabled || notified {
            return;
        }
        if json::JsonView::is_too_large(&self.tabs[ai].doc) {
            let mb = self.tabs[ai].doc.total_bytes() >> 20;
            self.tabs[ai].json.notified_too_large = true;
            self.status = Some(Fmt::StatusJsonTooLarge { mib: mb }.into());
            self.show_dialog(
                Dialog::text(Msg::DlgJsonTreeTitle, Fmt::DlgJsonTreeBody { mib: mb })
                    .primary(Msg::CommonOk, None),
            );
        }
    }

    /// Toolbar / View menu: collapse every container of the JSON tree (root only).
    pub fn json_collapse_all(&mut self) {
        if !self.tabs[self.active].json.enabled {
            return;
        }
        self.tabs[self.active].json.collapse_all();
        self.clamp_scroll();
    }

    /// Toolbar / View menu: expand every container of the JSON tree (full tree).
    pub fn json_expand_all(&mut self) {
        if !self.tabs[self.active].json.enabled {
            return;
        }
        self.tabs[self.active].json.expand_all();
        self.clamp_scroll();
    }

    /// Click on a tree view fold button: `toggle_node` collapses/expands
    /// that container node.
    ///
    /// Returns `true` when the fold column consumed the click (the caller
    /// then does not move the caret). The column is the same slot as the
    /// JSONL fold column; hit testing is independent of the tree's indent
    /// depth.
    pub fn json_marker_click(&mut self, client_x: f64, client_y: f64) -> bool {
        if !self.tabs[self.active].json.enabled {
            return false;
        }
        let Some(row) = self.fold_marker_row_at(client_x, client_y) else {
            return false;
        };
        let ai = self.active;
        let node = {
            let loc = self.locale;
            let tab = &mut self.tabs[ai];
            tab.json.refresh(&tab.doc, loc);
            let total = tab.json.total_rows(tab.doc.line_count());
            if row >= total {
                return false;
            }
            if tab.json.error().is_some() {
                return false; // the error row cannot be folded
            }
            let r = &tab.json.rows()[row as usize];
            if !r.foldable {
                return false; // leaf rows have no marker to click
            }
            r.node
        };
        self.tabs[ai].json.toggle_node(node);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::commands::testutil::st_of;

    /// Huge document: clicking "tree view" must not enable the preview but
    /// show a dialog and stay in text mode.
    #[test]
    fn json_toggle_preview_rejects_oversized_and_shows_dialog() {
        let big = format!("\"{}\"", "a".repeat(json::MAX_DOC_BYTES + 10));
        let mut st = st_of(&big);
        st.json_toggle_preview();
        assert!(
            !st.tabs[0].json.enabled,
            "oversized document must not enable tree view preview"
        );
        assert!(
            st.dialog.is_some(),
            "oversized document should show a dialog"
        );
        assert_eq!(
            st.status.as_ref().map(|m| m.key()),
            Some("With:StatusJsonTooLarge".to_string()),
            "status bar should explain the document is too large: {:?}",
            st.status
        );
    }

    /// Normal document: clicking "tree view" enables the preview (no dialog).
    #[test]
    fn json_toggle_preview_enables_normal_document() {
        let mut st = st_of(r#"{"a":1,"b":[1,2,3]}"#);
        st.json_toggle_preview();
        assert!(
            st.tabs[0].json.enabled,
            "normal document should enable preview"
        );
        assert!(
            st.dialog.is_none(),
            "normal document should not show a dialog"
        );
    }
}
