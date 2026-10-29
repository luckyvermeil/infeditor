//! Command layer for the Markdown toolbar.
//!
//! Toolbar edit commands come in two kinds:
//! - **Wrap** (bold/italic/strikethrough/inline code): the selection gets
//!   wrapped in `**…**` etc.; with no selection an empty pair of markers is
//!   inserted with the caret in the middle.
//! - **Prefix** (heading/quote/list): acts on the caret's line (or every
//!   line the selection covers), using the same line targeting as the line
//!   commands.

use std::path::{Path, PathBuf};

use crate::ui::state::EditorState;
use crate::ui::{Fmt, Msg};

/// Shape of one Markdown edit command (toolbar button → one variant here).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MdAct {
    Bold,
    Italic,
    Strike,
    Code,
    CodeBlock,
    Heading(u8),
    /// Back to a paragraph: strip the heading marker from the target lines
    /// (the text-view counterpart of the headings panel's "Body" row).
    Body,
    Quote,
    Bullet,
    Numbered,
    /// Task list: `- [ ] ` / `- [x] ` (re-running checks/unchecks)
    TaskList,
    Link,
    Image,
    /// Insert a table grid (the toolbar grid picker's rows × columns).
    TableGrid {
        rows: u8,
        cols: u8,
    },
    /// Insert an emoji literal (the toolbar emoji panel's item `i`).
    Emoji(u8),
    Hr,
}

impl EditorState {
    /// Toolbar "document view" button: WYSIWYG editing toggle (crates/wysiwyg).
    ///
    /// - **Enter**: document text seeded into global wys editor (`mdws::seed_from_doc`),
    ///   view routing goes to [`crate::ui::components::mdws::MdWys`]; from
    ///   then on edit keys are redirected by the composer to the wys model,
    ///   each edit written back to the piece table via `mdws::sync_to_doc`.
    /// - **Exit**: content is already synced (write-back is idempotent);
    ///   just reset the flag.
    /// - Markdown documents only.
    pub fn md_toggle_wys(&mut self) {
        if self.tab().md_wys {
            crate::ui::components::mdws::flush_pending(self);
            self.tab_mut().md_wys = false;
            self.view_switched(Msg::StatusMdText);
            return;
        }
        if self.read_only {
            self.readonly_notice();
            return;
        }
        if self.spec().name != "Markdown" {
            self.status = Some(Msg::StatusMdWysUnsupported.into());
            return;
        }
        crate::ui::components::mdws::seed_from_doc(self);
        self.tab_mut().md_wys = true;
        // emit's block spacing / trailing-newline rules normalise the whole
        // document — say so explicitly on entry, lest the user discover
        // after the first edit that "the formatting mysteriously changed"
        // (full-fidelity handling to be discussed separately).
        self.view_switched(Msg::StatusMdWys);
    }

    /// Plain Text view button (`Cmd::MdText`): leave the document view and go
    /// back to plain text view.
    pub fn md_show_text(&mut self) {
        if self.tab().md_wys {
            crate::ui::components::mdws::flush_pending(self);
        }
        self.tab_mut().md_wys = false;
        self.view_switched(Msg::StatusMdText);
    }

    /// Unified entry point for the toolbar's edit commands.
    pub fn md_apply(&mut self, act: MdAct) {
        if self.read_only {
            self.readonly_notice();
            return;
        }
        match act {
            MdAct::Bold => self.md_wrap("**", "**"),
            MdAct::Italic => self.md_wrap("*", "*"),
            MdAct::Strike => self.md_wrap("~~", "~~"),
            MdAct::Code => self.md_wrap("`", "`"),
            MdAct::CodeBlock => self.md_wrap("```\n", "\n```"),
            MdAct::Heading(n) => self.md_prefix(&format!("{} ", "#".repeat(n as usize))),
            MdAct::Body => self.md_body(),
            MdAct::Quote => self.md_prefix("> "),
            MdAct::Bullet => self.md_prefix("- "),
            MdAct::Numbered => self.md_numbered(),
            MdAct::TaskList => self.md_task_list(),
            MdAct::Link => self.md_link(false),
            MdAct::Image => self.md_link(true),
            MdAct::TableGrid { rows, cols } => self.md_insert_table(rows, cols),
            MdAct::Emoji(i) => self.md_insert_emoji(i),
            MdAct::Hr => self.md_prefix("---"),
        }
        self.scroll_caret_into_view();
    }

    // ---- internal implementation --------------------------------------------

    /// Wrap the selection: with a selection, replace it wholesale with
    /// `prefix + text + suffix`; without one, insert an empty pair and place
    /// the caret between the markers. Re-running on the same selection
    /// **strips** the existing wrap (toggle semantics, like the editor's
    /// bold button).
    fn md_wrap(&mut self, prefix: &str, suffix: &str) {
        let Some(sel) = self.sel_range() else {
            let text = format!("{prefix}{suffix}");
            let pos = self.caret().pos;
            self.tab_mut().doc.insert_text(pos, &text);
            let c = self.caret_mut();
            c.anchor = Some(pos + prefix.len() as u64);
            c.pos = pos + prefix.len() as u64;
            self.status = Some(
                Fmt::StatusMdInsertedWrap {
                    prefix: prefix.to_string(),
                    suffix: suffix.to_string(),
                }
                .into(),
            );
            return;
        };
        let text = self.tab().doc.text_range(sel.clone());
        if text.starts_with(prefix)
            && text.ends_with(suffix)
            && text.len() >= prefix.len() + suffix.len()
        {
            let inner = &text[prefix.len()..text.len() - suffix.len()];
            self.replace_sel(sel, inner);
            return;
        }
        let new = format!("{prefix}{text}{suffix}");
        self.replace_sel(sel.clone(), &new);
        // keep the content selected for easy stacking (bold, then italic)
        let c = self.caret_mut();
        c.anchor = Some(sel.start);
        c.pos = sel.start + new.len() as u64;
    }

    /// Line prefix commands: act on the caret's line (or every line the
    /// selection covers). Lines already carrying the same prefix get it
    /// stripped (toggle semantics); headings do **level replacement** rather
    /// than stacking.
    fn md_prefix(&mut self, prefix: &str) {
        let (first, last) = self.target_lines();
        let lines = self.read_lines(first, last);
        let out: Vec<String> = lines
            .iter()
            .map(|l| {
                let ind = l.len() - l.trim_start().len();
                let (head, rest) = l.split_at(ind);
                if prefix.starts_with('#') && rest.starts_with('#') {
                    // Heading: replace with the requested level ("##title" tight spelling counts too)
                    let body = rest
                        .split_once(' ')
                        .map(|x| x.1)
                        .unwrap_or(rest.trim_start_matches('#'));
                    format!("{head}{prefix}{body}")
                } else if let Some(stripped) = rest.strip_prefix(prefix) {
                    // Toggle semantics: strip the duplicate prefix
                    format!("{head}{stripped}")
                } else {
                    format!("{head}{prefix}{rest}")
                }
            })
            .collect();
        self.write_lines(first, last, &out);
        self.status = Some(
            Fmt::StatusMdPrefix {
                prefix: prefix.to_string(),
            }
            .into(),
        );
    }

    /// Strip the heading marker from the target lines (the headings panel's
    /// "Body" row in text view). Lines without a leading `#` run are left
    /// untouched — "back to paragraph" on a paragraph is a no-op.
    fn md_body(&mut self) {
        let (first, last) = self.target_lines();
        let lines = self.read_lines(first, last);
        let out: Vec<String> = lines
            .iter()
            .map(|l| {
                let ind = l.len() - l.trim_start().len();
                let (head, rest) = l.split_at(ind);
                let no_hashes = rest.trim_start_matches('#');
                let body = no_hashes.strip_prefix(' ').unwrap_or(no_hashes);
                format!("{head}{body}")
            })
            .collect();
        self.write_lines(first, last, &out);
    }

    /// Task list: add a `- [ ] ` prefix to target lines; lines already
    /// carrying `- [ ] `/`- [x] ` toggle their check state (GFM task list
    /// syntax, rendered the same as GitHub/VSCode in the preview).
    fn md_task_list(&mut self) {
        let (first, last) = self.target_lines();
        let lines = self.read_lines(first, last);
        let out: Vec<String> = lines
            .iter()
            .map(|l| {
                let ind = l.len() - l.trim_start().len();
                let (head, rest) = l.split_at(ind);
                if let Some(r) = rest.strip_prefix("- [ ] ") {
                    // unchecked → check
                    format!("{head}- [x] {r}")
                } else if let Some(r) = rest.strip_prefix("- [x] ") {
                    // checked → strip the task marker
                    format!("{head}{r}")
                } else if let Some(r) = rest.strip_prefix("- ") {
                    // plain list item → task item
                    format!("{head}- [ ] {r}")
                } else {
                    format!("{head}- [ ] {rest}")
                }
            })
            .collect();
        self.write_lines(first, last, &out);
        self.status = Some(Msg::StatusMdTaskList.into());
    }

    /// Numbered list: 1. 2. 3. … (incrementing per line).
    fn md_numbered(&mut self) {
        let (first, last) = self.target_lines();
        let lines = self.read_lines(first, last);
        let out: Vec<String> = lines
            .iter()
            .enumerate()
            .map(|(i, l)| {
                let ind = l.len() - l.trim_start().len();
                let (head, rest) = l.split_at(ind);
                // Already a list item: strip **all** old numbering digits
                // (stripping one digit of "10. item" would leave "0. item",
                // renumbering into "1. 0. item" — data corruption)
                let rest = rest.trim_start_matches(|c: char| c.is_ascii_digit());
                let rest = rest
                    .strip_prefix(". ")
                    .unwrap_or(rest.strip_prefix('.').unwrap_or(rest));
                format!("{head}{}. {rest}", i + 1)
            })
            .collect();
        self.write_lines(first, last, &out);
        self.status = Some(Msg::StatusMdNumbered.into());
    }

    /// Link/image: `[text](url)`, the selection as link text; an empty
    /// template when there is no selection.
    fn md_link(&mut self, image: bool) {
        let bang = if image { "!" } else { "" };
        let Some(sel) = self.sel_range() else {
            let text = format!("{bang}[](url)");
            let pos = self.caret().pos;
            self.tab_mut().doc.insert_text(pos, &text);
            let c = self.caret_mut();
            // caret lands right after `[` (link text position)
            let at = pos + bang.len() as u64 + 1;
            c.anchor = Some(at);
            c.pos = at;
            self.status = Some(
                if image {
                    Msg::StatusMdImage
                } else {
                    Msg::StatusMdLink
                }
                .into(),
            );
            return;
        };
        let text = self.tab().doc.text_range(sel.clone());
        let new = format!("{bang}[{text}](url)");
        let start = sel.start + bang.len() as u64 + text.len() as u64 + 3;
        self.replace_sel(sel, &new);
        // select `url` so it can be replaced directly
        let c = self.caret_mut();
        c.anchor = Some(start);
        c.pos = start + 3;
        self.status = Some(
            if image {
                Msg::StatusMdImage
            } else {
                Msg::StatusMdLink
            }
            .into(),
        );
    }

    /// Table template: three rows, two columns, inserted after the caret's line.
    /// Insert a `rows × cols` table template after the caret's line (the
    /// toolbar grid picker picks both dimensions; the old fixed template was
    /// 2 × 2).
    fn md_insert_table(&mut self, rows: u8, cols: u8) {
        let (first, _) = self.target_lines();
        let (fs, _) = self.lines_span(first, first);
        // the line end (line_text excludes the terminator) is the insertion point
        let line_text = self.active_doc().line_text(first);
        let insert_at = fs + line_text.len() as u64;
        let cols = cols.max(1);
        let header: String = (1..=cols).map(|c| format!("| 列{c} ")).collect::<String>() + "|";
        let sep: String = (0..cols).map(|_| "| --- ").collect::<String>() + "|";
        let blank: String = (0..cols).map(|_| "|     ").collect::<String>() + "|";
        let mut tpl = format!("\n{header}\n{sep}");
        for _ in 0..rows.max(1).saturating_sub(1) {
            tpl.push('\n');
            tpl.push_str(&blank);
        }
        self.tab_mut().doc.insert_text(insert_at, &tpl);
        self.status = Some(Msg::StatusMdTable.into());
    }

    /// Insert an emoji literal at the caret (the text-view counterpart of the
    /// wys kernel's `InsertText`).
    fn md_insert_emoji(&mut self, i: u8) {
        let Some(e) = crate::ui::components::mdws::EMOJIS.get(i as usize) else {
            return;
        };
        let pos = self.caret().pos;
        self.tab_mut().doc.insert_text(pos, e);
        let at = pos + e.len() as u64;
        let c = self.caret_mut();
        c.anchor = Some(at);
        c.pos = at;
    }

    /// Replace the selection with `new` and reset the caret (single-step
    /// semantics; the caller decides any new selection).
    fn replace_sel(&mut self, sel: std::ops::Range<u64>, new: &str) {
        let start = sel.start;
        self.tab_mut().doc.replace_range(sel, new);
        let end = start + new.len() as u64;
        let c = self.caret_mut();
        c.anchor = Some(start);
        c.pos = end;
    }
}

/// The markdown link URL under the byte offset `pos` — the text-view
/// counterpart of the document view's Ctrl+click (`[label](url)` inline
/// spans on the clicked line; images `![…](…)` count too, their `!` sits
/// outside the matched span). `None` when `pos` is not inside a link.
pub fn link_url_at(st: &EditorState, pos: u64) -> Option<String> {
    let doc = st.active_doc();
    let line = doc.line_of_pos(pos);
    let text = doc.line_text(line);
    let off = (pos - doc.pos_of_line(line)) as usize;
    inline_link_spans(&text)
        .into_iter()
        .find(|(s, e, _)| *s <= off && off <= *e)
        .map(|(_, _, url)| url.to_string())
}

/// Ctrl+click on a link in the text view: resolve and open it outside the app.
/// Returns false when there is no link under `pos` — the caller falls back to
/// normal caret placement (a click off a link keeps editor semantics).
pub fn open_link_at(st: &mut EditorState, pos: u64) -> bool {
    let Some(url) = link_url_at(st, pos) else {
        return false;
    };
    let img_base = st
        .active_doc()
        .meta
        .path
        .as_ref()
        .and_then(|p| p.parent().map(ToOwned::to_owned));
    crate::ui::components::mdws::open_external_link(st, &url, img_base.as_deref())
}

/// The local-path form of a link target: `Some(path)` for scheme-less targets
/// (resolved against the document's directory, forward slashes normalized for
/// the file manager), `None` for real URLs (http/mailto — `open_url`
/// territory). Shared by the document view's Ctrl+click and the text view's.
pub(crate) fn link_path_target(target: &str, img_base: Option<&Path>) -> Option<PathBuf> {
    let path_like = !target.contains("://") && !target.starts_with("mailto:");
    if !path_like {
        return None;
    }
    let p = match img_base {
        Some(base) => base.join(target),
        None => PathBuf::from(target),
    };
    // Explorer (`explorer /select`) only accepts backslash paths, and Windows
    // itself treats `/` as a separator, so fold them. POSIX does the opposite:
    // `\` is a legal filename character and rewriting `/` would produce a path
    // that does not exist (a Markdown link with a relative target used to
    // become `\home\…` on Linux).
    #[cfg(windows)]
    let p = PathBuf::from(p.to_string_lossy().replace('/', "\\"));
    Some(p)
}

/// Inline `[label](url)` spans of one line as `(start, end, url)` byte ranges
/// (`end` is inclusive). `](` after the label bracket and the closing `)` on
/// the same line — a `](` inside a nested bracket pair still matches first,
/// which is exactly what makes `[a [b] c](u)` resolve to `u`.
fn inline_link_spans(text: &str) -> Vec<(usize, usize, &str)> {
    let b = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < b.len() {
        if b[i] != b'[' {
            i += 1;
            continue;
        }
        let mut j = i + 1;
        let mut label_end = None;
        while j + 1 < b.len() {
            if b[j] == b']' && b[j + 1] == b'(' {
                label_end = Some(j);
                break;
            }
            j += 1;
        }
        let Some(label_end) = label_end else {
            i += 1;
            continue;
        };
        let mut k = label_end + 2;
        let mut url_end = None;
        while k < b.len() {
            if b[k] == b')' {
                url_end = Some(k);
                break;
            }
            k += 1;
        }
        match url_end {
            Some(e) if e > label_end + 2 => {
                out.push((i, e, &text[label_end + 2..e]));
                i = e + 1;
            }
            _ => {
                i = label_end + 1;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::super::testutil::st_of;
    use super::{link_path_target, link_url_at, MdAct};
    use crate::ui::state::EditorState;

    fn text_of(st: &EditorState) -> String {
        st.active_doc().text_range(0..st.active_doc().total_bytes())
    }

    #[test]
    fn md_wrap_bold_on_selection_and_back() {
        let mut st = st_of("hello world");
        st.tabs[0].caret.anchor = Some(0);
        st.tabs[0].caret.pos = 5;
        st.md_apply(MdAct::Bold);
        assert_eq!(text_of(&st), "**hello** world");
        // run again to strip (selection is still hello)
        st.md_apply(MdAct::Bold);
        assert_eq!(text_of(&st), "hello world");
    }

    #[test]
    fn md_wrap_without_selection_inserts_empty_pair() {
        let mut st = st_of("ab");
        st.tabs[0].caret.pos = 1;
        st.md_apply(MdAct::Code);
        assert_eq!(text_of(&st), "a``b");
        let c = st.caret();
        let doc = st.active_doc();
        assert_eq!(doc.line_of_pos(c.pos), 0);
        assert_eq!(c.pos, 2);
    }

    #[test]
    fn md_heading_replaces_level() {
        let mut st = st_of("## title\nbody\n");
        // select the first line: heading commands only act on lines the selection covers
        st.tabs[0].caret.anchor = Some(0);
        st.tabs[0].caret.pos = 8;
        st.md_apply(MdAct::Heading(1));
        assert_eq!(text_of(&st), "# title\nbody\n");
    }

    /// Task list three states: plain line → `- [ ] `; unchecked → `- [x] `;
    /// checked → marker stripped. The multi-digit tight-spelling heading
    /// ("##title") fix shares its root with the two-digit numbering fix —
    /// this pins down the task list's own path.
    #[test]
    fn md_task_list_cycles_and_handles_list_items() {
        let mut st = st_of("- 买牛奶\n普通行\n");
        // select the first line (with no selection, prefix commands act on the whole document)
        st.tabs[0].caret.anchor = Some(0);
        st.tabs[0].caret.pos = 6;
        // plain list item → task item
        st.md_apply(MdAct::TaskList);
        assert_eq!(text_of(&st), "- [ ] 买牛奶\n普通行\n");
        // unchecked → check
        st.md_apply(MdAct::TaskList);
        assert_eq!(text_of(&st), "- [x] 买牛奶\n普通行\n");
        // checked → strip the marker
        st.md_apply(MdAct::TaskList);
        assert_eq!(text_of(&st), "买牛奶\n普通行\n");
    }

    /// Two-digit numbered list renumbering: "10. item" must have **all** old
    /// digits stripped — stripping one would produce "1. 0. item" (data
    /// corruption).
    #[test]
    fn md_numbered_handles_two_digit_items() {
        let mut st = st_of("10. item\n11. another\n");
        st.md_apply(MdAct::Numbered);
        assert_eq!(text_of(&st), "1. item\n2. another\n");
    }

    /// Tight headings without a space ("##title", allowed by CommonMark)
    /// must not swallow heading text when the level changes.
    #[test]
    fn md_heading_keeps_text_of_tight_headings() {
        let mut st = st_of("##title\n");
        st.md_apply(MdAct::Heading(1));
        assert_eq!(text_of(&st), "# title\n");
    }

    #[test]
    fn md_quote_prefixes_every_selected_line() {
        let mut st = st_of("a\nb\nc\n");
        st.tabs[0].caret.anchor = Some(0);
        st.tabs[0].caret.pos = 3; // covers a\nb
        st.md_apply(MdAct::Quote);
        assert_eq!(text_of(&st), "> a\n> b\nc\n");
    }

    #[test]
    fn md_numbered_list() {
        let mut st = st_of("x\ny\n");
        st.md_apply(MdAct::Numbered);
        assert_eq!(text_of(&st), "1. x\n2. y\n");
    }

    #[test]
    fn md_link_uses_selection() {
        let mut st = st_of("see docs here");
        st.tabs[0].caret.anchor = Some(4);
        st.tabs[0].caret.pos = 8; // docs
        st.md_apply(MdAct::Link);
        assert_eq!(text_of(&st), "see [docs](url) here");
    }

    // ---- link_url_at (text-view Ctrl+click) ----------------------------------

    /// Offsets inside the label/url spans resolve to the URL; outside (before
    /// `[`, after `)`, on other lines) resolve to None.
    #[test]
    fn link_url_at_finds_the_span_under_the_offset() {
        // line 0: [alpha](https://x) — 0..23; line 2: second line
        let st = st_of("[alpha](https://x) beta\nsecond line\n");
        for pos in [0, 5, 12, 17] {
            assert_eq!(
                link_url_at(&st, pos),
                Some("https://x".into()),
                "pos {pos} is inside the link span"
            );
        }
        // pos 17 = `)` inclusive → still the link; pos 18+ = after it
        assert_eq!(link_url_at(&st, 17), Some("https://x".into()));
        assert_eq!(link_url_at(&st, 18), None);
        assert_eq!(link_url_at(&st, 25), None, "beta is not part of the link");
        assert_eq!(link_url_at(&st, 30), None, "the second line has no link");
    }

    /// Two links on one line: each click hits its own URL; the image form
    /// counts too (its `!` sits outside the matched span).
    #[test]
    fn link_url_at_picks_the_span_under_the_offset() {
        let st = st_of("[a](one) ![b](two.png) [c](three)\n");
        // spans: [0..7) one, [9..20) two.png, [21..32) three
        assert_eq!(link_url_at(&st, 2), Some("one".into()));
        assert_eq!(link_url_at(&st, 14), Some("two.png".into()));
        assert_eq!(link_url_at(&st, 25), Some("three".into()));
        assert_eq!(link_url_at(&st, 8), None, "the gap between the two links");
    }

    /// Relative targets resolve against the document's directory (separators
    /// follow the platform); URLs are left alone.
    #[test]
    fn link_path_target_resolves_relative_against_the_doc_dir() {
        // Platform-native paths on purpose: `C:\abs\p.md` is a *relative*
        // filename on POSIX, so a hardcoded Windows path only exercised the
        // Windows branch (and the old expectation pinned a backslash rewrite
        // that broke Linux link opening).
        #[cfg(windows)]
        let (base, abs) = (std::path::Path::new("C:/docs"), "C:\\abs\\p.md");
        #[cfg(not(windows))]
        let (base, abs) = (std::path::Path::new("/docs"), "/abs/p.md");
        // Windows normalizes the join to backslashes (Explorer needs them);
        // POSIX keeps the forward-slash path untouched.
        #[cfg(windows)]
        let rel = std::path::PathBuf::from("C:\\docs\\sub\\page.html");
        #[cfg(not(windows))]
        let rel = std::path::PathBuf::from("/docs/sub/page.html");

        assert_eq!(link_path_target("sub/page.html", Some(base)), Some(rel));
        assert_eq!(
            link_path_target(abs, Some(base)),
            Some(std::path::PathBuf::from(abs))
        );
        assert_eq!(link_path_target("https://x", Some(base)), None);
        assert_eq!(link_path_target("mailto:a@b", Some(base)), None);
        // No document path (unsaved): the target stays as-is.
        assert_eq!(
            link_path_target("p.md", None),
            Some(std::path::PathBuf::from("p.md"))
        );
    }
}
