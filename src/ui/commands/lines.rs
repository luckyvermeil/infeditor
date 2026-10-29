//! Line-level operations on [`EditorState`]: case conversion, blank
//! operations, sorting, splitting/joining, comment toggling, insertion
//! helpers and the Tools menu (digests, JSON/XML, Markdown, CSV).
//!
//! Every operation acts on the selection's lines or the whole document
//! (the standard rule) and writes back as a single undo step.

use crate::ui::state::{EditorState, TAB_SIZE};
use crate::ui::{Fmt, Msg};

impl EditorState {
    /// Common skeleton for line operations: mass-op guard → pick target
    /// lines → read out → transform → write back (single undo step) →
    /// optional scroll to caret.
    ///
    /// - `block`: `true` uses `block_lines()` (whole-line selection
    ///   semantics, used by copy/insert line), `false` uses
    ///   `target_lines()` (the selection's / caret's lines);
    /// - `f` receives the whole batch of lines and returns the replacement
    ///   line set (the count may change: deleting empty lines / splitting
    ///   lines rely on it);
    /// - `scroll` follows each command's historical behavior exactly —
    ///   sorting never scrolls, conversions always do.
    fn edit_target_lines(
        &mut self,
        block: bool,
        scroll: bool,
        f: impl FnOnce(Vec<String>) -> Vec<String>,
    ) {
        if !self.guard_mass_op() {
            return;
        }
        let (first, last) = if block {
            self.block_lines()
        } else {
            self.target_lines()
        };
        let out = f(self.read_lines(first, last));
        self.write_lines(first, last, &out);
        if scroll {
            self.scroll_caret_into_view();
        }
    }

    // ---- case conversion --------------------------------------------------

    /// Edit → Convert Case. Acts on the selection, or the whole document.
    pub fn convert_case(&mut self, kind: crate::textops::CaseKind) {
        self.edit_target_lines(false, true, |ls| {
            ls.iter()
                .map(|l| crate::textops::convert_case(l, kind))
                .collect()
        });
    }

    /// Encoding → Unicode Normalization (NFC/NFD), selection or whole document.
    pub fn unicode_normalize(&mut self, nfd: bool) {
        self.edit_target_lines(false, true, |ls| {
            ls.iter()
                .map(|l| crate::textops::unicode_normalize(l, nfd))
                .collect()
        });
        self.status = Some(
            if nfd {
                Msg::StatusUnicodeNfd
            } else {
                Msg::StatusUnicodeNfc
            }
            .into(),
        );
    }

    /// Edit → Insert → Numbering: prefix `start, start+step, …` to each targeted
    /// line.
    pub fn insert_numbering(&mut self, start: i64, step: i64) {
        // The numbering dialog submits without going through `actions::run`, so
        // the read-only gate in `intercept` never sees it — this was the one
        // way to edit a read-only document from a menu command.
        if self.read_only {
            self.readonly_notice();
            return;
        }
        self.edit_target_lines(false, true, |ls| {
            ls.iter()
                .enumerate()
                .map(|(i, l)| {
                    // saturate instead of overflowing: huge start/step inputs
                    // from the numbering dialog must not panic (debug) or wrap
                    // into negatives (release). The value is `start + step*i`
                    // — an extra `* start` made "start=5, step=1" count
                    // 5, 10, 15 and "start=0" label every line 0.
                    let n = (i as i64)
                        .checked_mul(step)
                        .and_then(|d| start.checked_add(d))
                        .unwrap_or(if step >= 0 { i64::MAX } else { i64::MIN });
                    format!("{n}{l}")
                })
                .collect()
        });
        self.status = Some(
            Fmt::StatusNumberingInserted {
                start: start.to_string(),
                step: step.to_string(),
            }
            .into(),
        );
    }

    // ---- Tools: word count / character code value -------------------------

    /// Tools → Word Count: characters/words/lines/bytes.
    pub fn word_count(&mut self) {
        let Some((_, _, text)) = self.tool_target() else {
            return;
        };
        let scope = self.tr(self.tool_scope()).into_owned();
        let chars: usize = text.chars().count();
        let chars_no_ws: usize = text.chars().filter(|c| !c.is_whitespace()).count();
        // "word" definition: consecutive runs of non-whitespace Unicode
        // letters/digits
        let words: usize = text
            .split_whitespace()
            .map(|w| {
                if w.chars().all(|c| !c.is_alphanumeric()) {
                    0 // a pure-symbol run does not count as a word
                } else {
                    1
                }
            })
            .sum();
        let lines = text.lines().count().max(1);
        let bytes = text.len();
        self.show_dialog(crate::ui::state::Dialog::text(
            Fmt::DlgWordCountTitle { scope },
            Fmt::DlgWordCountBody {
                chars,
                chars_no_ws,
                words,
                lines,
                bytes,
            },
        ));
    }

    /// Tools → Character Code Value: the first characters of the
    /// target with their Unicode scalar value, hex and decimal.
    ///
    /// Only the first 64 characters of the target are needed; the whole text
    /// is never materialized (and the large-document guard does not apply).
    pub fn char_code_value(&mut self) {
        let (first, _) = self.target_lines();
        let (start, _) = self.lines_span(first, first);
        let text = {
            let doc = self.active_doc();
            let end = doc.snap((start + 256).min(doc.table.len()));
            doc.text_range(start..end)
        };
        let mut rows: Vec<String> = Vec::new();
        for (i, c) in text.chars().take(64).enumerate() {
            let cp = c as u32;
            rows.push(format!(
                "{i:>3}  U+{cp:04X}  {cp:>7}  {}",
                if c == '\n' {
                    self.tr(Msg::CharCodeNewline).into_owned()
                } else {
                    c.to_string()
                }
            ));
        }
        let note = if text.chars().count() > 64 {
            format!("\n{}", self.tr(Msg::CharCodeTruncated))
        } else {
            String::new()
        };
        let scope = self.tr(self.tool_scope()).into_owned();
        self.show_dialog(crate::ui::state::Dialog::text(
            Fmt::DlgCharCodeTitle { scope },
            Fmt::DlgCharCodeBody {
                rows: rows.join("\n"),
                note,
            },
        ));
    }

    // ---- blank operations -------------------------------------------------

    pub fn trim_lines(&mut self, leading: bool, trailing: bool) {
        self.edit_target_lines(false, true, |ls| {
            ls.iter()
                .map(|l| crate::textops::trim(l, leading, trailing))
                .collect()
        });
    }

    pub fn tabs_to_spaces(&mut self) {
        self.edit_target_lines(false, true, |ls| {
            ls.iter()
                .map(|l| crate::textops::tabs_to_spaces(l, TAB_SIZE))
                .collect()
        });
    }

    pub fn spaces_to_tabs(&mut self, leading_only: bool) {
        self.edit_target_lines(false, true, |ls| {
            ls.iter()
                .map(|l| crate::textops::spaces_to_tabs(l, TAB_SIZE, leading_only))
                .collect()
        });
    }

    /// Edit → Blank Operations → EOL to Space: collapse the targeted lines into one.
    pub fn eol_to_space(&mut self) {
        self.edit_target_lines(false, true, |ls| vec![ls.join(" ")]);
    }

    pub fn remove_empty_lines(&mut self, also_blank: bool) {
        self.edit_target_lines(false, false, |ls| {
            crate::textops::remove_empty(&ls, also_blank)
        });
    }

    // ---- line operations --------------------------------------------------

    pub fn duplicate_lines(&mut self) {
        self.edit_target_lines(true, true, |ls| {
            let mut out = ls.clone();
            out.extend(ls);
            out
        });
    }

    pub fn remove_duplicate_lines(&mut self, consecutive_only: bool) {
        self.edit_target_lines(false, false, |ls| {
            crate::textops::remove_duplicates(&ls, consecutive_only)
        });
    }

    pub fn sort_lines(&mut self, key: crate::textops::SortKey, desc: bool) {
        self.edit_target_lines(false, false, |mut ls| {
            crate::textops::sort_lines(&mut ls, key, desc);
            ls
        });
    }

    pub fn reverse_lines(&mut self) {
        self.edit_target_lines(false, false, |mut ls| {
            ls.reverse();
            ls
        });
    }

    /// Line Operations → Sort Randomly. xorshift + Fisher-Yates keeps this dependency-free.
    pub fn randomize_lines(&mut self) {
        self.edit_target_lines(false, false, |mut lines| {
            let mut seed = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos() as u64)
                .unwrap_or(0x9E37_79B9_7F4A_7C15)
                | 1;
            for i in (1..lines.len()).rev() {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                let j = (seed % (i as u64 + 1)) as usize;
                lines.swap(i, j);
            }
            lines
        });
    }

    /// Move the selected (or current) lines one line up (`dir < 0`) or down.
    pub fn move_lines(&mut self, dir: i64) {
        if !self.guard_mass_op() {
            return;
        }
        let (first, last) = self.block_lines();
        let total = self.active_doc().line_count();
        // remember the caret's line/column so it can follow the moved block
        // (write_lines would otherwise park it at the block start — the
        // opposite of the usual behaviour, where repeated Ctrl+Down walks down)
        let doc = self.active_doc();
        let caret_line = doc.line_of_pos(self.caret().pos);
        let col = self
            .caret()
            .pos
            .saturating_sub(doc.pos_of_line(caret_line.min(total.saturating_sub(1))));
        let caret_in_block = caret_line >= first && caret_line <= last;
        if dir < 0 {
            if first == 0 {
                return;
            }
            let mut lines = self.read_lines(first - 1, last);
            lines.rotate_left(1);
            self.write_lines(first - 1, last, &lines);
            if caret_in_block {
                self.place_caret_at_line_col(caret_line - 1, col);
            }
        } else {
            if last + 1 >= total {
                return;
            }
            let mut lines = self.read_lines(first, last + 1);
            lines.rotate_right(1);
            self.write_lines(first, last + 1, &lines);
            if caret_in_block {
                self.place_caret_at_line_col(caret_line + 1, col);
            }
        }
        self.scroll_caret_into_view();
    }

    /// Park the caret (no selection) at `col` bytes into `line`, clamped.
    fn place_caret_at_line_col(&mut self, line: u64, col: u64) {
        let doc = self.active_doc();
        let line = line.min(doc.line_count().saturating_sub(1));
        let ls = doc.pos_of_line(line);
        let le = ls + doc.line_text(line).len() as u64;
        let p = doc.snap((ls + col).min(le));
        let c = self.caret_mut();
        c.anchor = None;
        c.pos = p;
    }

    pub fn insert_blank_line(&mut self, above: bool) {
        self.edit_target_lines(true, true, |mut out| {
            if above {
                out.insert(0, String::new());
            } else {
                // Two empties for one blank line: `join` already emits the
                // separator in front of the first trailing `""`, and the writer
                // skips its own terminator because the text ends with one. With
                // a single `""` the joined text equals the original (`["b", ""]`
                // → `"b\n"`), so the write was skipped and "Insert Blank Line
                // Below" did nothing at every position.
                out.push(String::new());
                out.push(String::new());
            }
            out
        });
    }

    /// Line Operations → Split Lines: an empty line after each targeted line.
    pub fn split_lines(&mut self) {
        self.edit_target_lines(false, false, |lines| {
            let mut out = Vec::with_capacity(lines.len() * 2);
            for l in lines {
                out.push(l);
                out.push(String::new());
            }
            out
        });
    }

    /// Line Operations → Join Lines: join the targeted lines with a single space.
    pub fn join_lines(&mut self) {
        self.edit_target_lines(false, true, |ls| {
            vec![ls
                .iter()
                .map(|l| l.trim_end())
                .collect::<Vec<_>>()
                .join(" ")]
        });
    }

    // ---- comments ---------------------------------------------------------

    /// Line-comment token of the active document's language (`//` fallback).
    pub fn line_comment_token(&self) -> &'static str {
        self.spec().line_comments.first().copied().unwrap_or("//")
    }

    /// (open, close) block-comment pair of the active language, if it has one.
    pub fn block_comment_pair(&self) -> Option<(&'static str, &'static str)> {
        self.spec().block
    }

    /// Edit → Toggle line comment (Ctrl+Q).
    ///
    /// When every targeted line is already commented the token is removed,
    /// otherwise it is inserted at the first non-blank column. One undo step.
    pub fn toggle_line_comment(&mut self) {
        let token = self.line_comment_token();
        if token.is_empty() {
            self.status = Some(
                Fmt::StatusNoLineComment {
                    lang: self.spec().name.to_string(),
                }
                .into(),
            );
            return;
        }
        let (first, last) = self.block_lines();
        let lines = self.read_lines(first, last);
        if lines.is_empty() {
            return;
        }
        let all_commented = lines.iter().all(|l| {
            let t = l.trim_start();
            !t.is_empty() && t.starts_with(token)
        });
        let out: Vec<String> = lines
            .iter()
            .map(|l| {
                if l.trim().is_empty() {
                    return l.clone();
                }
                if all_commented {
                    // remove the token plus one following space
                    let indent = l.len() - l.trim_start().len();
                    let (head, rest) = l.split_at(indent);
                    let rest = rest.strip_prefix(token).unwrap_or(rest);
                    let rest = rest.strip_prefix(' ').unwrap_or(rest);
                    format!("{head}{rest}")
                } else {
                    let indent = l.len() - l.trim_start().len();
                    let (head, rest) = l.split_at(indent);
                    format!("{head}{token} {rest}")
                }
            })
            .collect();
        self.write_lines(first, last, &out);
        self.scroll_caret_into_view();
    }

    /// Edit → Block comment. Wraps the character selection in `open ... close`
    /// as a **single** undo step.
    pub fn block_comment(&mut self) {
        let Some((open, close)) = self.block_comment_pair() else {
            self.status = Some(
                Fmt::StatusNoBlockComment {
                    lang: self.spec().name.to_string(),
                }
                .into(),
            );
            return;
        };
        let Some(sel) = self.sel_range() else {
            self.status = Some(Msg::StatusBlockCommentNeedsSel.into());
            return;
        };
        let text = self.tab().doc.text_range(sel.clone());
        let new = format!("{open}{text}{close}");
        self.tab_mut().doc.replace_range(sel.clone(), &new);
        let c = self.caret_mut();
        c.anchor = Some(sel.start);
        c.pos = sel.start + new.len() as u64;
        self.scroll_caret_into_view();
    }

    /// Edit → Unblock comment.
    ///
    /// The selection is trimmed of surrounding whitespace; only a `open ... close`
    /// pair that *wraps the whole trimmed selection* is removed, and the
    /// whitespace is put back verbatim so the operation is the exact inverse of
    /// [`Self::block_comment`].
    pub fn block_uncomment(&mut self) {
        let Some((open, close)) = self.block_comment_pair() else {
            self.status = Some(
                Fmt::StatusNoBlockUncomment {
                    lang: self.spec().name.to_string(),
                }
                .into(),
            );
            return;
        };
        let Some(sel) = self.sel_range() else {
            self.status = Some(Msg::StatusUnblockNeedsSel.into());
            return;
        };
        let text = self.tab().doc.text_range(sel.clone());
        let lead = text.len() - text.trim_start().len();
        let trail = text.len() - text.trim_end().len();
        let core = &text[lead..text.len() - trail];
        if !core.starts_with(open) || !core.ends_with(close) {
            self.status = Some(Msg::StatusNoFullBlockComment.into());
            return;
        }
        let inner = &core[open.len()..core.len() - close.len()];
        // drop the single space that `block_comment`'s caller usually adds
        let inner = inner.strip_prefix(' ').unwrap_or(inner);
        let inner = inner.strip_suffix(' ').unwrap_or(inner);
        let new = format!("{}{}{}", &text[..lead], inner, &text[text.len() - trail..]);
        self.tab_mut().doc.replace_range(sel.clone(), &new);
        let c = self.caret_mut();
        c.anchor = Some(sel.start);
        c.pos = sel.start + new.len() as u64;
        self.scroll_caret_into_view();
    }

    // ---- insertion helpers ------------------------------------------------

    /// Edit → Insert → Current Date/Time (Ctrl+F5).
    pub fn insert_datetime(&mut self, long: bool) {
        let now = std::time::SystemTime::now();
        let secs = now
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let s = format_local_time(secs, long);
        self.insert_text(&s);
        self.break_undo_group();
    }

    // ---- Tools / View: digests, JSON/XML formatting, table view -----------

    /// Text the active tool operates on: the selection's lines, else the whole
    /// document, as `(first_line, last_line, text)` — the same target the line
    /// operations use, so a transform can be written back as one undo step.
    ///
    /// Materializing the whole document with no cap guard means one "MD5
    /// digest" click on a 2 GB mmap log is a 2 GB allocation plus minutes of
    /// freeze — share the same 128 MB rule as the line operations.
    fn tool_target(&mut self) -> Option<(u64, u64, String)> {
        let (first, last) = self.target_lines();
        let (start, end) = self.lines_span(first, last);
        let len = end - start;
        if self.sel_range().is_none() && len > Self::MASS_OP_MAX_BYTES {
            self.status = Some(Fmt::StatusDocTooLarge { bytes: len }.into());
            return None;
        }
        let text = self.active_doc().text_range(start..end);
        Some((first, last, text))
    }

    /// Scope qualifier (selection / whole document), returned as a message
    /// key — call sites fill it into the `{scope}` field of dialog titles.
    fn tool_scope(&self) -> i18n::Message {
        if self.sel_range().is_some() {
            Msg::ToolScopeSel.into()
        } else {
            Msg::ToolScopeAll.into()
        }
    }

    /// Tools → MD5 Digest: digest of the selection (or the whole document) in a
    /// dialog — the native message box never becomes visible, see `dialog.rs`.
    pub fn md5_digest(&mut self) {
        let Some((_, _, text)) = self.tool_target() else {
            return;
        };
        let scope = self.tr(self.tool_scope()).into_owned();
        let digest = crate::digest::hex(&crate::digest::md5(text.as_bytes()));
        self.show_dialog(crate::ui::state::Dialog::text(
            Fmt::DlgMd5Title { scope },
            // the digest hex string itself is data; render it as-is
            Fmt::CommonRaw { text: digest },
        ));
    }

    /// Tools → SHA-1 Digest.
    pub fn sha1_digest(&mut self) {
        let Some((_, _, text)) = self.tool_target() else {
            return;
        };
        let scope = self.tr(self.tool_scope()).into_owned();
        let digest = crate::digest::hex(&crate::digest::sha1(text.as_bytes()));
        self.show_dialog(crate::ui::state::Dialog::text(
            Fmt::DlgSha1Title { scope },
            Fmt::CommonRaw { text: digest },
        ));
    }

    /// Tools → Format JSON (2-space indent). A parse error reports the message
    /// on the status bar and leaves the document untouched.
    pub fn format_json(&mut self) {
        let Some((first, last, text)) = self.tool_target() else {
            return;
        };
        match crate::format::json_pretty(&text) {
            Ok(pretty) => {
                self.write_lines(
                    first,
                    last,
                    &pretty.split('\n').map(String::from).collect::<Vec<_>>(),
                );
                self.status = Some(Msg::StatusJsonFormatted.into());
            }
            Err(e) => self.status = Some(Fmt::StatusJsonInvalid { err: e }.into()),
        }
    }

    /// Tools → Format XML (best-effort indentation, never errors).
    pub fn format_xml(&mut self) {
        let Some((first, last, text)) = self.tool_target() else {
            return;
        };
        let pretty = crate::format::xml_pretty(&text);
        self.write_lines(
            first,
            last,
            &pretty.split('\n').map(String::from).collect::<Vec<_>>(),
        );
        self.status = Some(Msg::StatusXmlFormatted.into());
    }

    // (Markdown preview and the CSV table view have moved to in-place views
    // + toolbars, see crate::ui::commands::markdown and
    // crate::ui::commands::csv.)
}

/// Civil date/time from a Unix timestamp, in **local** time.
///
/// No `chrono`: the OS query lives in `platform::local_civil` (Windows
/// `GetLocalTime`; POSIX `localtime_r` with a UTC fallback for unrepresentable
/// timestamps).
fn format_local_time(unix_secs: i64, long: bool) -> String {
    let (y, mo, d, h, mi, s) = platform::local_civil(unix_secs);
    if long {
        format!("{y:04}-{mo:02}-{d:02} {h:02}:{mi:02}:{s:02}")
    } else {
        format!("{h:02}:{mi:02}")
    }
}
