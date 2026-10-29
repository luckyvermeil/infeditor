use super::inlines::*;
use super::*;
use crate::md;

fn ed(src: &str) -> Editor {
    Editor::new(md::parse(src))
}

fn plain(b: &Block) -> String {
    b.plain_text()
}

fn text_at(e: &Editor, i: usize) -> String {
    plain(&e.doc.blocks[i])
}

fn caret(e: &Editor) -> (usize, usize) {
    (e.sel.focus.block, e.sel.focus.off)
}

/// Convenience: put the caret at (block, off).
fn at(e: &mut Editor, block: usize, off: usize) {
    e.sel = Sel::caret(Pos::new(block, off));
}

// ---- Basic insert / delete ----

#[test]
fn insert_text_and_offsets() {
    let mut e = ed("ab");
    at(&mut e, 0, 1);
    assert!(e.apply(Cmd::InsertText("X".into())));
    assert_eq!(text_at(&e, 0), "aXb");
    assert_eq!(caret(&e), (0, 2));
}

#[test]
fn insert_replaces_selection() {
    let mut e = ed("hello");
    e.sel = Sel {
        anchor: Pos::new(0, 1),
        focus: Pos::new(0, 4),
    };
    e.apply(Cmd::InsertText("E".into()));
    assert_eq!(text_at(&e, 0), "hEo");
    assert_eq!(caret(&e), (0, 2));
}

#[test]
fn insert_inside_bold_keeps_container() {
    let mut e = Editor::new(Doc::new(vec![Block::Para(vec![
        Inline::Text("a".into()),
        Inline::Strong(vec![Inline::Text("bc".into())]),
        Inline::Text("d".into()),
    ])]));
    // off=2 is inside Strong (b=1, c=2)
    at(&mut e, 0, 2);
    e.apply(Cmd::InsertText("Z".into()));
    assert_eq!(text_at(&e, 0), "abZcd", "off=2 is between b and c");
    // Z is inserted into the Strong container (inlines_insert recurses into
    // containers, so the formatting stays continuous)
    let Block::Para(inl) = &e.doc.blocks[0] else {
        panic!()
    };
    let has_strong_z = inl
        .iter()
        .any(|i| matches!(i, Inline::Strong(c) if c.iter().any(|j| j.plain().contains('Z'))));
    assert!(has_strong_z, "Z should be inside Strong: {inl:?}");
}

#[test]
fn insert_splits_atomic_code() {
    let mut e = Editor::new(Doc::new(vec![Block::Para(vec![
        Inline::Text("a".into()),
        Inline::Code("cd".into()),
    ])]));
    // off=1 is inside Code → split
    at(&mut e, 0, 1);
    e.apply(Cmd::InsertText("X".into()));
    assert_eq!(text_at(&e, 0), "aXcd");
    let Block::Para(inl) = &e.doc.blocks[0] else {
        panic!()
    };
    assert!(
        matches!(&inl[1], Inline::Code(c) if c == "cd"),
        "Code atomic node stays intact: {inl:?}"
    );
    assert!(inl.iter().take(2).any(|i| i.plain().contains('X')));
}

#[test]
fn delete_back_merges_char_and_unicode() {
    let mut e = ed("汉语");
    at(&mut e, 0, "汉".len());
    e.apply(Cmd::DeleteBack);
    assert_eq!(text_at(&e, 0), "语");
    assert_eq!(caret(&e), (0, 0));
}

#[test]
fn delete_back_cross_block_merges() {
    let mut e = ed("ab\n\ncd");
    at(&mut e, 1, 0);
    e.apply(Cmd::DeleteBack);
    assert_eq!(e.doc.blocks.len(), 1);
    assert_eq!(text_at(&e, 0), "abcd");
    assert_eq!(caret(&e), (0, 2));
}

#[test]
fn delete_forward_at_end_merges_next() {
    let mut e = ed("ab\n\ncd");
    at(&mut e, 0, 2);
    e.apply(Cmd::DeleteForward);
    assert_eq!(e.doc.blocks.len(), 1);
    assert_eq!(text_at(&e, 0), "abcd");
}

/// Ctrl+Backspace / Ctrl+Delete: delete by word (the word boundaries come from
/// the same source as Ctrl+←/→); backspacing once more at the block start merges
/// blocks like a plain backspace.
#[test]
fn delete_word_variants() {
    let mut e = ed("hello world  ");
    at(&mut e, 0, "hello world  ".len());
    assert!(e.apply(Cmd::DeleteBackWord));
    assert_eq!(text_at(&e, 0), "hello ");
    assert_eq!(caret(&e), (0, 6));

    let mut e = ed("hello world");
    at(&mut e, 0, 0);
    assert!(e.apply(Cmd::DeleteForwardWord));
    assert_eq!(text_at(&e, 0), " world");

    // Chinese is segmented by runs of same-kind characters (ASCII alphanumerics and
    // CJK are two different classes)
    let mut e = ed("ab你好世界");
    at(&mut e, 0, "ab你好世界".len());
    assert!(e.apply(Cmd::DeleteBackWord));
    assert_eq!(text_at(&e, 0), "ab");

    // Ctrl+Backspace at the block start: merge blocks (same block-level semantics
    // as a plain backspace)
    let mut e = ed("ab\n\ncd");
    at(&mut e, 1, 0);
    assert!(e.apply(Cmd::DeleteBackWord));
    assert_eq!(e.doc.blocks.len(), 1);
    assert_eq!(text_at(&e, 0), "abcd");

    // Already at the document start: no-op (does not enter undo)
    let mut e = ed("a");
    at(&mut e, 0, 0);
    assert!(!e.apply(Cmd::DeleteBackWord));
    assert_eq!(text_at(&e, 0), "a");
}

/// Ctrl+Home / Ctrl+End (the component's onkeydown path; the host has its own
/// `move_doc_edge`, but the kernel command should agree with it).
#[test]
fn doc_edge_moves() {
    let mut e = ed("a\n\nb\n\nc");
    at(&mut e, 1, 1);
    assert!(e.apply(Cmd::Move(Move::DocEnd)));
    assert_eq!(caret(&e), (2, 1));
    assert!(e.apply(Cmd::Move(Move::DocStart)));
    assert_eq!(caret(&e), (0, 0));
    // Shift extends: the anchor stays
    e.sel = Sel::caret(Pos::new(1, 0));
    assert!(e.apply(Cmd::MoveExtend(Move::DocEnd)));
    assert_eq!(e.sel.anchor, Pos::new(1, 0));
    assert_eq!(e.sel.focus, Pos::new(2, 1));
}

#[test]
fn delete_selection_cross_block() {
    let mut e = ed("ab\n\ncd\n\nef");
    e.sel = Sel {
        anchor: Pos::new(0, 1),
        focus: Pos::new(2, 1),
    };
    e.apply(Cmd::DeleteSelection);
    assert_eq!(e.doc.blocks.len(), 1);
    assert_eq!(text_at(&e, 0), "af");
}

/// After a select-all delete it must return to an empty paragraph — the first
/// block's kind (Head/Li/quote shell) must not linger (found on a real machine:
/// after Ctrl+A+Delete the emit produced "# \n").
#[test]
fn select_all_delete_leaves_empty_para() {
    let mut e = ed("# T\n\npara\n\n---");
    e.apply(Cmd::SelectAll);
    e.apply(Cmd::DeleteForward);
    assert!(matches!(e.doc.blocks[0], Block::Para(_)));
    assert_eq!(md::emit(&e.doc), "\n");
}

/// Typing with the caret on an Hr should insert into a new paragraph (found on a
/// real machine: the keystroke was silently dropped).
#[test]
fn insert_text_on_hr_opens_para() {
    let mut e = ed("a\n\n---\n\nb");
    let hr = e
        .doc
        .blocks
        .iter()
        .position(|b| matches!(b, Block::Hr))
        .unwrap();
    at(&mut e, hr, 0);
    assert!(e.apply(Cmd::InsertText("X".into())));
    assert!(matches!(e.doc.blocks[hr + 1], Block::Para(_)));
    assert_eq!(text_at(&e, hr + 1), "X");
    assert_eq!(caret(&e), (hr + 1, 1));
}

// ---- Enter semantics ----

#[test]
fn enter_splits_paragraph() {
    let mut e = ed("abcd");
    at(&mut e, 0, 2);
    e.apply(Cmd::Enter);
    assert_eq!(e.doc.blocks.len(), 2);
    assert_eq!(text_at(&e, 0), "ab");
    assert_eq!(text_at(&e, 1), "cd");
    assert_eq!(caret(&e), (1, 0));
}

/// An HTML block's plain_text is the source: insert_text/delete_back must
/// insert and delete the source (take_inlines is a no-op for Html, and it used to
/// silently drop characters while the caret moved on).
#[test]
fn html_block_text_edits_operate_on_source() {
    // A GFM HTML block is parsed whole: plain_text = source
    let mut e = ed("<div>\nraw text\n</div>");
    assert!(matches!(e.doc.blocks[0], Block::Html { .. }));
    at(&mut e, 0, 9); // in the middle of "<div>\nraw| text"
    e.apply(Cmd::InsertText("XX".into()));
    assert_eq!(
        text_at(&e, 0),
        "<div>\nrawXX text\n</div>",
        "typed text must land in the html source: {:?}",
        text_at(&e, 0)
    );
    assert_eq!(caret(&e), (0, 11));
    e.apply(Cmd::DeleteBack);
    assert_eq!(text_at(&e, 0), "<div>\nrawX text\n</div>");
    assert_eq!(caret(&e), (0, 10));
}

/// Delete at the end of a code block: the next block's text must be **joined
/// on** (the next block used to be deleted whole and its text evaporated —
/// append_inlines does nothing for Code).
#[test]
fn delete_forward_at_code_end_merges_next_block() {
    let mut e = ed("```\ncode\n```\n\nafter");
    // Move the caret to the end of the code block (inside the code text)
    at(&mut e, 0, 5);
    e.apply(Cmd::Move(Move::BlockEnd));
    let b0 = e.doc.blocks[0].clone();
    assert!(matches!(b0, Block::Code { .. }));
    let ok = e.apply(Cmd::DeleteForward);
    assert!(ok);
    assert_eq!(
        e.doc.blocks.len(),
        1,
        "next block merged away: {:?}",
        e.doc.blocks
    );
    assert!(
        matches!(&e.doc.blocks[0], Block::Code { text, .. } if text.contains("after")),
        "next text appended into code: {:?}",
        e.doc.blocks[0]
    );
}

/// Cross-block selection delete: the first block keeps the inline structure of
/// its prefix (Image/Strong no longer evaporate), and the last block's suffix
/// structure must stay alive too.
#[test]
fn cross_block_delete_keeps_inline_structure() {
    let mut e = ed("![img](x.png) hello\n\nworld tail");
    // Selection: plain offsets (an Image's plain = alt "img", virtual bytes) —
    // dragged from "img he|llo" into the second block "wo|rld tail"
    let s = Pos::new(0, 6);
    let t = Pos::new(1, 2);
    e.sel = Sel {
        anchor: s,
        focus: t,
    };
    e.apply(Cmd::DeleteSelection);
    let plain0 = e.doc.blocks[0].plain_text();
    assert_eq!(plain0, "img herld tail", "merged content: {plain0:?}");
    // The first block's inline structure: the Image inline is still there (not
    // flattened into bare alt text)
    let has_image = matches!(
        &e.doc.blocks[0],
        Block::Para(inl) if inl.iter().any(|i| matches!(i, Inline::Image { .. }))
    );
    assert!(has_image, "Image inline preserved: {:?}", e.doc.blocks[0]);
    let plain = e.doc.blocks[0].plain_text();
    assert!(plain.contains("rld tail"), "end suffix survived: {plain:?}");
}

/// Multi-line paste whose last block is an Hr: the tail text after the caret in
/// the focused block must not evaporate (a Para is added after the Hr to receive
/// it); when the focused block itself is an Hr it must not be replaced by the
/// paste.
#[test]
fn paste_tail_survives_hr_last_block() {
    let mut e = ed("ab");
    at(&mut e, 0, 1);
    e.apply(Cmd::Paste("x\n\n---\n".into()));
    let joined: Vec<String> = (0..e.doc.blocks.len()).map(|i| text_at(&e, i)).collect();
    assert!(
        joined.iter().any(|t| t == "b"),
        "tail `b` must survive: {joined:?}"
    );
    assert!(
        e.doc.blocks.iter().any(|b| matches!(b, Block::Hr)),
        "hr block present: {joined:?}"
    );
}

#[test]
fn paste_on_hr_focus_keeps_hr() {
    let mut e = ed("text\n\n---\n\nmore");
    // Find the Hr block
    let hr = e
        .doc
        .blocks
        .iter()
        .position(|b| matches!(b, Block::Hr))
        .unwrap();
    at(&mut e, hr, 0);
    e.apply(Cmd::Paste("pasted\n\nlines\n".into()));
    assert!(
        e.doc.blocks.iter().any(|b| matches!(b, Block::Hr)),
        "hr must survive paste on hr focus"
    );
    assert!(
        e.doc
            .blocks
            .iter()
            .any(|b| b.plain_text().contains("pasted")),
        "pasted content present"
    );
}

#[test]
fn enter_continues_list_with_next_number() {
    let mut e = ed("1. a\n2. b");
    at(&mut e, 1, 2); // end of "b"
    e.apply(Cmd::Enter);
    assert!(matches!(
        e.doc.blocks[2],
        Block::Li {
            ordered: true,
            num: 3,
            ..
        }
    ));
}

#[test]
fn enter_empty_item_exits_list() {
    let mut e = ed("- a\n- \n- b");
    // Block 1 is an empty item
    at(&mut e, 1, 0);
    e.apply(Cmd::Enter);
    assert!(
        matches!(e.doc.blocks[1], Block::Para(_)),
        "an empty item leaves the list"
    );
}

#[test]
fn enter_in_code_block_inserts_line() {
    let mut e = ed("```\na\nb\n```");
    let len = text_at(&e, 0).len();
    at(&mut e, 0, len);
    e.apply(Cmd::Enter);
    assert!(matches!(&e.doc.blocks[0], Block::Code { text, .. } if text == "a\nb\n"));
    assert_eq!(
        e.doc.blocks.len(),
        1,
        "Enter in a code block does not split the block"
    );
    assert_eq!(caret(&e), (0, 4));
}

#[test]
fn enter_after_head_makes_paragraph() {
    let mut e = ed("# T");
    at(&mut e, 0, 2);
    e.apply(Cmd::Enter);
    assert!(matches!(e.doc.blocks[0], Block::Head { level: 1, .. }));
    assert!(matches!(e.doc.blocks[1], Block::Para(_)));
}

/// Table auto-completion: `| a | b |` + Enter becomes a table directly — the
/// header is that line, one empty row is added to the body, and the caret lands in
/// the body's first cell (typing continues into it).
#[test]
fn enter_after_pipe_row_auto_creates_table() {
    let mut e = ed("| a | b |");
    at(&mut e, 0, 9);
    assert!(e.apply(Cmd::Enter));
    assert_eq!(e.doc.blocks.len(), 4, "{:?}", e.doc.blocks);
    assert!(matches!(
        &e.doc.blocks[0],
        Block::Td {
            row: 0,
            align: 0,
            ..
        }
    ));
    assert_eq!(text_at(&e, 0), "a");
    assert_eq!(text_at(&e, 1), "b");
    assert!(matches!(&e.doc.blocks[2], Block::Td { row: 1, .. }));
    assert!(matches!(&e.doc.blocks[3], Block::Td { row: 1, .. }));
    assert_eq!(caret(&e), (2, 0));
    // Inline markup survives (parsed for real, not flattened)
    let md = md::emit(&e.doc);
    assert!(md.contains("| a | b |"), "{md}");
    assert!(md.contains("|---|---|"), "{md}");
}

#[test]
fn enter_pipe_row_keeps_inline_marks() {
    let mut e = ed("| **a** | `b` |");
    at(&mut e, 0, 15);
    assert!(e.apply(Cmd::Enter));
    let has_strong = matches!(
        &e.doc.blocks[0],
        Block::Td { inlines, .. } if inlines.iter().any(|i| matches!(i, Inline::Strong(_)))
    );
    let has_code = matches!(
        &e.doc.blocks[1],
        Block::Td { inlines, .. } if inlines.iter().any(|i| matches!(i, Inline::Code(_)))
    );
    assert!(has_strong && has_code, "{:?}", e.doc.blocks);
}

/// A paragraph that is not "a whole line of paired pipes" must not be mistaken
/// for a table.
#[test]
fn enter_plain_pipe_text_is_not_table() {
    for src in ["a | b", "| a | b", "a | b |"] {
        let mut e = ed(src);
        at(&mut e, 0, src.len());
        assert!(e.apply(Cmd::Enter), "{src:?}");
        assert!(
            e.doc.blocks.iter().all(|b| !b.is_td()),
            "{src:?} was misjudged as a table: {:?}",
            e.doc.blocks
        );
    }
}

/// Table seq collision: when the document already has tables, the
/// auto-completed table must take a new seq (otherwise rendering and export merge
/// contiguous tables that share a seq).
#[test]
fn table_autoformat_uses_fresh_seq() {
    let mut e = ed("| x | y |\n|---|---|\n| 1 | 2 |\n\n| a | b |");
    let last = e.doc.blocks.len() - 1;
    at(&mut e, last, 9);
    assert!(e.apply(Cmd::Enter));
    let seqs: Vec<u64> = e
        .doc
        .blocks
        .iter()
        .filter_map(|b| match b {
            Block::Td { table_seq, .. } => Some(*table_seq),
            _ => None,
        })
        .collect();
    assert!(seqs.windows(2).any(|w| w[0] != w[1]), "{seqs:?}");
}

/// Enter / Shift+Enter inside a cell inserts `<br>`: it is a HardBreak in the
/// model, emit emits `<br>` inside a table, and reparsing keeps the structure.
#[test]
fn enter_in_table_cell_inserts_br() {
    let mut e = ed("| a | b |\n|---|---|\n| c | d |");
    at(&mut e, 2, 1); // end of cell c
    assert!(e.apply(Cmd::Enter));
    assert_eq!(text_at(&e, 2), "c\n");
    let md = md::emit(&e.doc);
    assert!(md.contains("| c<br> |"), "{md}");
    assert_eq!(md::parse(&md), e.doc, "roundtrip: {md:?}");
    // Shift+Enter has the same semantics
    let mut e = ed("| a | b |\n|---|---|\n| c | d |");
    at(&mut e, 3, 1); // cell d
    assert!(e.apply(Cmd::SoftBreak));
    assert_eq!(text_at(&e, 3), "d\n");
    assert!(md::emit(&e.doc).contains("| d<br> |"));
}

/// Tab inserts 4 spaces inside a code block; Shift+Tab is a no-op.
#[test]
fn tab_in_code_block_inserts_spaces() {
    let mut e = ed("```\nab\n```");
    at(&mut e, 0, 2);
    assert!(e.apply(Cmd::Indent));
    assert!(matches!(&e.doc.blocks[0], Block::Code { text, .. } if text == "ab    "));
    assert!(
        !e.apply(Cmd::Outdent),
        "Shift+Tab should be a no-op in a code block"
    );
}

// ---- Backspace block semantics ----

#[test]
fn backspace_empty_li_exits() {
    let mut e = ed("- a\n- \n- b");
    at(&mut e, 1, 0);
    e.apply(Cmd::DeleteBack);
    assert!(matches!(e.doc.blocks[1], Block::Para(_)));
}

#[test]
fn backspace_nonempty_li_strips_shell() {
    let mut e = ed("- ab");
    at(&mut e, 0, 0);
    e.apply(Cmd::DeleteBack);
    assert!(
        matches!(e.doc.blocks[0], Block::Para(_)),
        "a non-empty item peels the list shell at off 0"
    );
    assert_eq!(text_at(&e, 0), "ab");
}

#[test]
fn backspace_head_demotes_to_para() {
    let mut e = ed("## T");
    at(&mut e, 0, 0);
    e.apply(Cmd::DeleteBack);
    assert!(matches!(e.doc.blocks[0], Block::Para(_)));
}

#[test]
fn backspace_code_off0_demotes_to_para() {
    let mut e = ed("```\ncode\n```");
    at(&mut e, 0, 0);
    e.apply(Cmd::DeleteBack);
    assert!(
        matches!(e.doc.blocks[0], Block::Para(_)),
        "code block peels its shell at off 0: {:?}",
        e.doc.blocks[0]
    );
}

// ---- Auto-completion ----

#[test]
fn autoformat_head_on_space() {
    let mut e = ed("");
    at(&mut e, 0, 0);
    e.apply(Cmd::InsertText("# ".into()));
    assert!(matches!(e.doc.blocks[0], Block::Head { level: 1, .. }));
    assert_eq!(caret(&e), (0, 0));
}

#[test]
fn autoformat_unordered_list() {
    let mut e = ed("");
    at(&mut e, 0, 0);
    e.apply(Cmd::InsertText("- ".into()));
    assert!(matches!(
        e.doc.blocks[0],
        Block::Li {
            ordered: false,
            task: None,
            ..
        }
    ));
}

#[test]
fn autoformat_ordered_list_keeps_number() {
    let mut e = ed("");
    at(&mut e, 0, 0);
    e.apply(Cmd::InsertText("3. ".into()));
    assert!(matches!(
        e.doc.blocks[0],
        Block::Li {
            ordered: true,
            num: 3,
            ..
        }
    ));
}

#[test]
fn autoformat_task_list() {
    let mut e = ed("");
    at(&mut e, 0, 0);
    e.apply(Cmd::InsertText("- [ ] ".into()));
    assert!(matches!(
        e.doc.blocks[0],
        Block::Li {
            task: Some(false),
            ..
        }
    ));
}

#[test]
fn autoformat_quote_and_hr_and_fence() {
    let mut e = ed("");
    e.apply(Cmd::InsertText("> ".into()));
    assert!(matches!(e.doc.blocks[0], Block::Quote(_)));

    let mut e = ed("");
    e.apply(Cmd::InsertText("---".into()));
    assert!(matches!(e.doc.blocks[0], Block::Hr));

    let mut e = ed("");
    e.apply(Cmd::InsertText("```".into()));
    assert!(matches!(e.doc.blocks[0], Block::Code { .. }));
}

#[test]
fn autoformat_inline_math() {
    // Typing `$a \times b$` in order should become Inline::Math
    let mut e = ed("");
    at(&mut e, 0, 0);
    e.apply(Cmd::InsertText("$a \\times b$".into()));
    let Block::Para(inl) = &e.doc.blocks[0] else {
        panic!("expected Para, got {:?}", e.doc.blocks[0]);
    };
    assert_eq!(inl.len(), 1, "inl = {:?}", inl);
    assert!(
        matches!(&inl[0], Inline::Math { src, display: false } if src == "a \\times b"),
        "inl[0] = {:?}",
        inl[0]
    );
    // The caret should land after the formula
    assert_eq!(caret(&e), (0, "a \\times b".len()));
}

#[test]
fn autoformat_display_math() {
    let mut e = ed("");
    at(&mut e, 0, 0);
    e.apply(Cmd::InsertText("$$x^2$$".into()));
    let Block::Para(inl) = &e.doc.blocks[0] else {
        panic!();
    };
    assert_eq!(inl.len(), 1);
    assert!(
        matches!(&inl[0], Inline::Math { src, display: true } if src == "x^2"),
        "inl[0] = {:?}",
        inl[0]
    );
}

#[test]
fn autoformat_math_with_surrounding_text() {
    // Type it one character at a time: first "see ", then "$x$" (which triggers a
    // formula), finally " here"
    let mut e = ed("");
    at(&mut e, 0, 0);
    e.apply(Cmd::InsertText("see ".into()));
    e.apply(Cmd::InsertText("$x$".into()));
    e.apply(Cmd::InsertText(" here".into()));
    let Block::Para(inl) = &e.doc.blocks[0] else {
        panic!();
    };
    assert_eq!(inl.len(), 3, "inl = {:?}", inl);
    assert!(matches!(&inl[0], Inline::Text(t) if t == "see "));
    assert!(matches!(&inl[1], Inline::Math { src, display: false } if src == "x"));
    assert!(matches!(&inl[2], Inline::Text(t) if t == " here"));
}

#[test]
fn autoformat_math_escaped_dollar_not_trigger() {
    let mut e = ed("");
    at(&mut e, 0, 0);
    e.apply(Cmd::InsertText("a \\$ b$".into()));
    // `$` preceded by a backslash does not open, and the final `$` has no matching
    // opener, so it must not become a formula
    let Block::Para(inl) = &e.doc.blocks[0] else {
        panic!();
    };
    assert!(inl.iter().all(|n| !matches!(n, Inline::Math { .. })));
}

/// A double backslash is a **literal backslash** (the emit form): the `$` in
/// `\\$` is not escaped and must open a formula normally. Escapes have to be
/// judged by parity, not by looking only at the previous character.
#[test]
fn autoformat_math_backslash_pair_does_not_escape() {
    let mut e = ed("");
    at(&mut e, 0, 0);
    e.apply(Cmd::InsertText("\\\\$x$".into()));
    let Block::Para(inl) = &e.doc.blocks[0] else {
        panic!();
    };
    assert!(
        inl.iter()
            .any(|n| matches!(n, Inline::Math { src, .. } if src == "x")),
        "inl = {inl:?}"
    );
}

/// Typing `$$x$$` one character at a time: the intermediate states `$$` and
/// `$$x$` must not become an inline formula early; only completing the second `$`
/// makes it a display formula (the old implementation treated the second `$` as
/// the opening delimiter).
#[test]
fn autoformat_display_math_typed_char_by_char() {
    let mut e = ed("");
    at(&mut e, 0, 0);
    for ch in ["$", "$", "x", "$"] {
        e.apply(Cmd::InsertText(ch.into()));
        let Block::Para(inl) = &e.doc.blocks[0] else {
            panic!();
        };
        assert!(
            inl.iter().all(|n| !matches!(n, Inline::Math { .. })),
            "intermediate state {ch:?} must not become a formula: {inl:?}"
        );
    }
    e.apply(Cmd::InsertText("$".into()));
    let Block::Para(inl) = &e.doc.blocks[0] else {
        panic!();
    };
    assert_eq!(inl.len(), 1, "inl = {inl:?}");
    assert!(
        matches!(&inl[0], Inline::Math { src, display: true } if src == "x"),
        "inl[0] = {:?}",
        inl[0]
    );
    assert_eq!(
        caret(&e),
        (0, 1),
        "the caret lands after the formula (Math.plain = src)"
    );
}

// ---- Inline formatting ----

#[test]
fn wrap_bold_selection() {
    let mut e = ed("hello world");
    e.sel = Sel {
        anchor: Pos::new(0, 0),
        focus: Pos::new(0, 5),
    };
    e.apply(Cmd::Wrap(WrapKind::Bold));
    let Block::Para(inl) = &e.doc.blocks[0] else {
        panic!()
    };
    assert!(matches!(&inl[0], Inline::Strong(c) if b_plain(c) == "hello"));
    assert_eq!(text_at(&e, 0), "hello world");
}

#[test]
fn wrap_bold_partial_container_splits() {
    let mut e = Editor::new(Doc::new(vec![Block::Para(vec![Inline::Strong(vec![
        Inline::Text("abcd".into()),
    ])])]));
    e.sel = Sel {
        anchor: Pos::new(0, 1),
        focus: Pos::new(0, 3),
    };
    e.apply(Cmd::Wrap(WrapKind::Italic));
    let Block::Para(inl) = &e.doc.blocks[0] else {
        panic!()
    };
    assert_eq!(
        inl.len(),
        3,
        "split into [Strong(a), Strong(Em(bc)), Strong(d)]: {inl:?}"
    );
    assert_eq!(text_at(&e, 0), "abcd");
}

#[test]
fn wrap_toggle_off_not_required_but_idempotent_text() {
    // Wrapping the same range again nests (this could be toggle-off; here it just
    // preserves the offsets)
    let mut e = ed("ab");
    e.sel = Sel {
        anchor: Pos::new(0, 0),
        focus: Pos::new(0, 2),
    };
    e.apply(Cmd::Wrap(WrapKind::Bold));
    e.apply(Cmd::Wrap(WrapKind::Bold));
    assert_eq!(text_at(&e, 0), "ab");
}

#[test]
fn wrap_bold_collapsed_then_type_lands_inside() {
    // No selection + B → a `**` pair with the caret in the middle; the next typed
    // character becomes bold
    let mut e = ed("ab");
    e.sel = Sel::caret(Pos::new(0, 1));
    e.apply(Cmd::Wrap(WrapKind::Bold));
    // The placeholder container is zero-length: the plain text is unchanged and the
    // caret stays where it was
    assert_eq!(text_at(&e, 0), "ab");
    e.apply(Cmd::InsertText("X".into()));
    let Block::Para(inl) = &e.doc.blocks[0] else {
        panic!()
    };
    assert!(
        matches!(inl.get(1), Some(Inline::Strong(c)) if b_plain(c) == "X"),
        "typed char must land inside Strong: {inl:?}"
    );
    assert_eq!(text_at(&e, 0), "aXb");
    assert_eq!(md::emit(&e.doc), "a**X**b\n");
}

#[test]
fn wrap_inline_code_collapsed_then_type_is_code() {
    let mut e = ed("ab");
    e.sel = Sel::caret(Pos::new(0, 2));
    e.apply(Cmd::Wrap(WrapKind::Code));
    e.apply(Cmd::InsertText("c".into()));
    let Block::Para(inl) = &e.doc.blocks[0] else {
        panic!()
    };
    assert!(
        matches!(inl.last(), Some(Inline::Code(t)) if t == "c"),
        "typed char must land inside Code: {inl:?}"
    );
    assert_eq!(md::emit(&e.doc), "ab`c`\n");
}

#[test]
fn wrap_collapsed_placeholder_emits_nothing() {
    // Before typing, the placeholder emits no marker bytes: `****` would be
    // reparsed as a thematic break
    let mut e = ed("ab");
    e.sel = Sel::caret(Pos::new(0, 1));
    e.apply(Cmd::Wrap(WrapKind::Bold));
    assert_eq!(md::emit(&e.doc), "ab\n");
}

#[test]
fn wrap_link_uses_selection_text_as_url() {
    let mut e = ed("see this");
    e.sel = Sel {
        anchor: Pos::new(0, 4),
        focus: Pos::new(0, 8),
    };
    e.apply(Cmd::Link(String::new()));
    let Block::Para(inl) = &e.doc.blocks[0] else {
        panic!()
    };
    assert!(matches!(&inl[1], Inline::Link { url, .. } if url == "this"));
}

#[test]
fn image_insert_collapsed_is_structured_node() {
    // Empty-selection image insertion: it must be a structured Image node —
    // inserting the literal `![](url)` would be escaped by emit into
    // `!\[\](url)`, which after saving and reparsing is no longer an image (hit on a
    // real machine).
    let mut e = ed("ab");
    e.sel = Sel::caret(Pos::new(0, 2));
    e.apply(Cmd::Image("x.png".into()));
    let Block::Para(inl) = &e.doc.blocks[0] else {
        panic!()
    };
    assert!(
        matches!(inl.last(), Some(Inline::Image { url, .. }) if url == "x.png"),
        "must be a structured Image node: {inl:?}"
    );
    let out = md::emit(&e.doc);
    assert_eq!(out, "ab![](x.png)\n");
    assert!(
        !out.contains("\\["),
        "image brackets must not be escaped: {out:?}"
    );
    assert_eq!(md::parse(&out), e.doc, "roundtrip must stay an image");
}

#[test]
fn image_insert_selection_becomes_alt() {
    let mut e = ed("see this");
    e.sel = Sel {
        anchor: Pos::new(0, 4),
        focus: Pos::new(0, 8),
    };
    e.apply(Cmd::Image("p.png".into()));
    assert_eq!(md::emit(&e.doc), "see ![this](p.png)\n");
    let Block::Para(inl) = &e.doc.blocks[0] else {
        panic!()
    };
    assert!(
        matches!(inl.last(), Some(Inline::Image { url, alt }) if url == "p.png" && alt == "this"),
        "{inl:?}"
    );
}

// ---- Block switching ----

#[test]
fn set_head_toggle_cycle() {
    let mut e = ed("t");
    e.apply(Cmd::SetHead(2));
    assert!(matches!(e.doc.blocks[0], Block::Head { level: 2, .. }));
    e.apply(Cmd::SetHead(2)); // same level again → back to a paragraph
    assert!(matches!(e.doc.blocks[0], Block::Para(_)));
}

#[test]
fn toggle_list_on_and_off() {
    let mut e = ed("t");
    e.apply(Cmd::ToggleList { ordered: false });
    assert!(matches!(e.doc.blocks[0], Block::Li { .. }));
    e.apply(Cmd::ToggleList { ordered: false });
    assert!(matches!(e.doc.blocks[0], Block::Para(_)));
}

#[test]
fn toggle_list_continues_numbering() {
    let mut e = ed("1. a\n\nsecond");
    at(&mut e, 1, 0);
    e.apply(Cmd::ToggleList { ordered: true });
    assert!(matches!(
        e.doc.blocks[1],
        Block::Li {
            ordered: true,
            num: 2,
            ..
        }
    ));
}

#[test]
fn toggle_quote() {
    let mut e = ed("t");
    e.apply(Cmd::ToggleQuote);
    assert!(matches!(e.doc.blocks[0], Block::Quote(_)));
}

#[test]
fn toggle_task() {
    let mut e = ed("t");
    e.apply(Cmd::ToggleTask);
    assert!(matches!(
        e.doc.blocks[0],
        Block::Li {
            task: Some(false),
            ..
        }
    ));
}

#[test]
fn toggle_code_block() {
    let mut e = ed("t");
    e.apply(Cmd::ToggleCodeBlock);
    assert!(matches!(e.doc.blocks[0], Block::Code { .. }));
    e.apply(Cmd::ToggleCodeBlock);
    assert!(matches!(e.doc.blocks[0], Block::Para(_)));
}

#[test]
fn toggle_task_checked_flips() {
    let mut e = ed("- [x] done");
    assert!(matches!(
        e.doc.blocks[0],
        Block::Li {
            task: Some(true),
            ..
        }
    ));
    e.apply(Cmd::ToggleTaskChecked(0));
    assert!(matches!(
        e.doc.blocks[0],
        Block::Li {
            task: Some(false),
            ..
        }
    ));
}

// ---- List indentation ----

#[test]
fn indent_outdent_levels() {
    let mut e = ed("- a\n- b");
    at(&mut e, 1, 2);
    e.apply(Cmd::Indent);
    assert!(matches!(&e.doc.blocks[1], Block::Li { level: 1, .. }));
    e.apply(Cmd::Outdent);
    assert!(matches!(&e.doc.blocks[1], Block::Li { level: 0, .. }));
}

// ---- Movement and selection ----

#[test]
fn move_left_crosses_blocks() {
    let mut e = ed("ab\n\ncd");
    at(&mut e, 1, 0);
    e.apply(Cmd::Move(Move::Left));
    assert_eq!(caret(&e), (0, 2));
}

#[test]
fn move_word_left_right() {
    let mut e = ed("foo bar");
    at(&mut e, 0, 7);
    e.apply(Cmd::Move(Move::WordLeft));
    assert_eq!(caret(&e), (0, 4), "word end → word start");
    e.apply(Cmd::Move(Move::WordLeft));
    assert_eq!(
        caret(&e),
        (0, 0),
        "word start, skip whitespace → previous word start"
    );
    e.apply(Cmd::Move(Move::WordRight));
    assert_eq!(caret(&e), (0, 3), "word start → word end");
    e.apply(Cmd::Move(Move::WordRight));
    assert_eq!(caret(&e), (0, 7));
}

#[test]
fn move_extend_selects() {
    let mut e = ed("abcd");
    at(&mut e, 0, 2);
    e.apply(Cmd::MoveExtend(Move::Left));
    assert_eq!(e.sel.range(), (Pos::new(0, 1), Pos::new(0, 2)));
}

#[test]
fn select_all() {
    let mut e = ed("ab\n\ncd");
    e.apply(Cmd::SelectAll);
    assert_eq!(e.sel.range(), (Pos::new(0, 0), Pos::new(1, 2)));
}

#[test]
fn select_block_whole_block() {
    let mut e = ed("# H\n\npara two\n\n- item\n");
    // The focus lands mid-paragraph
    at(&mut e, 1, 3);
    assert!(
        !e.apply(Cmd::SelectBlock),
        "selection does not edit the document"
    );
    assert_eq!(e.sel.range(), (Pos::new(1, 0), Pos::new(1, 8)));
    // A list item: select to the end of the item's text
    at(&mut e, 2, 2);
    e.apply(Cmd::SelectBlock);
    assert_eq!(e.sel.range(), (Pos::new(2, 0), Pos::new(2, 4)));
    // Empty block: no-op
    let mut e2 = ed("x\n\n\ny");
    at(&mut e2, 1, 0);
    assert!(!e2.apply(Cmd::SelectBlock));
}

/// Double-clicking a table cell (the component's ondoubleclick runs SelectBlock
/// for Td): select the whole cell's content, matching CSV double-click in-place
/// editing — typing directly overwrites that cell.
#[test]
fn select_block_selects_whole_table_cell() {
    let mut e = ed("| h1 | h2 |\n|---|---|\n| ab | b |\n");
    // Block order: 0=h1 1=h2 2=ab 3=b (the separator row is not a Td)
    assert_eq!(
        e.doc.blocks.iter().filter(|b| b.is_td()).count(),
        4,
        "{:?}",
        e.doc.blocks
    );
    at(&mut e, 2, 1); // inside "ab"
    assert!(
        !e.apply(Cmd::SelectBlock),
        "selection does not edit the document"
    );
    assert_eq!(e.sel.range(), (Pos::new(2, 0), Pos::new(2, 2)));
}

#[test]
fn select_word_cjk_and_ascii() {
    let mut e = ed("hello 世界 x");
    at(&mut e, 0, 8); // inside 界
    e.apply(Cmd::SelectWord);
    assert_eq!(e.sel.range(), (Pos::new(0, 6), Pos::new(0, 12)));
}

// ---- Undo ----

#[test]
fn undo_redo_roundtrip() {
    let mut e = ed("");
    e.apply(Cmd::InsertText("abc".into()));
    assert_eq!(text_at(&e, 0), "abc");
    assert!(e.undo());
    assert_eq!(text_at(&e, 0), "");
    assert!(e.redo());
    assert_eq!(text_at(&e, 0), "abc");
}

#[test]
fn undo_keeps_selection() {
    let mut e = ed("ab");
    at(&mut e, 0, 1);
    e.apply(Cmd::InsertText("X".into()));
    e.undo();
    assert_eq!(caret(&e), (0, 1), "undo restores the caret");
}

// ---- Paste ----

#[test]
fn paste_single_line() {
    let mut e = ed("ab");
    at(&mut e, 0, 1);
    e.apply(Cmd::Paste("XY".into()));
    assert_eq!(text_at(&e, 0), "aXYb");
}

#[test]
fn paste_multiline_splits_blocks() {
    let mut e = ed("ab");
    at(&mut e, 0, 1);
    e.apply(Cmd::Paste("# H\n\nbody".into()));
    let texts: Vec<String> = e.doc.blocks.iter().map(plain).collect();
    assert_eq!(
        texts,
        vec!["a", "H", "bodyb"],
        "the tail half joins the last block"
    );
    assert!(matches!(e.doc.blocks[1], Block::Head { level: 1, .. }));
}

#[test]
fn paste_into_code_block_raw() {
    let mut e = ed("```\ncode\n```");
    at(&mut e, 0, 4);
    e.apply(Cmd::Paste("x\ny".into()));
    assert!(matches!(&e.doc.blocks[0], Block::Code { text, .. } if text == "codex\ny"));
}

#[test]
fn paste_html_text_into_code_block_stays_raw() {
    // The Html downgrade only applies to a **non-Code focus**: pasting inside a code
    // block always inserts verbatim ("paste verbatim inside a code block").
    let mut e = ed("```\ncode\n```");
    at(&mut e, 0, 4);
    e.apply(Cmd::Paste("<img src=x onerror=alert(1)>".into()));
    assert!(
        matches!(&e.doc.blocks[0], Block::Code { text, .. } if text == "code<img src=x onerror=alert(1)>"),
        "Html-like text pasted into a Code block must not be parsed or downgraded"
    );
}

#[test]
fn wrap_link_with_url_over_selection() {
    // A pure URL pasted over a selection → `Cmd::Link(url)` wraps the selection in a link
    let mut e = ed("see this");
    e.sel = Sel {
        anchor: Pos::new(0, 4),
        focus: Pos::new(0, 8),
    };
    e.apply(Cmd::Link("https://x.y".into()));
    let Block::Para(inl) = &e.doc.blocks[0] else {
        panic!()
    };
    assert!(matches!(
        &inl[1],
        Inline::Link { url, inlines }
            if url == "https://x.y"
                && matches!(inlines.as_slice(), [Inline::Text(t)] if t == "this")
    ));
    assert_eq!(
        plain(&e.doc.blocks[0]),
        "see this",
        "the selection's text is unchanged and only wrapped in a link"
    );
}

// ---- Toolbar insert commands (insert-before/after, table grid) ----

#[test]
fn insert_before_after_place_caret() {
    let mut e = ed("a\n\nb");
    at(&mut e, 1, 0);
    e.apply(Cmd::InsertBefore);
    assert_eq!(e.doc.blocks.len(), 3);
    assert_eq!(
        e.sel.focus,
        Pos::new(1, 0),
        "the caret enters the new paragraph"
    );
    e.apply(Cmd::InsertText("X".into()));
    assert_eq!(text_at(&e, 1), "X");
    e.apply(Cmd::InsertAfter);
    assert_eq!(
        e.sel.focus,
        Pos::new(2, 0),
        "the new paragraph lands after the focus block"
    );
    assert_eq!(e.doc.blocks.len(), 4);
}

#[test]
fn insert_table_builds_grid() {
    let mut e = ed("ab");
    at(&mut e, 0, 2);
    e.apply(Cmd::InsertTable { rows: 2, cols: 3 });
    let tds: Vec<&Block> = e.doc.blocks.iter().filter(|b| b.is_td()).collect();
    assert_eq!(tds.len(), 6);
    // The same table_seq; row boundaries 0/1; whether the first row renders as a
    // header is decided by row==0
    assert!(tds
        .iter()
        .all(|b| matches!(b, Block::Td { table_seq: 0, .. })));
    assert!(matches!(&e.doc.blocks[1], Block::Td { row: 0, .. }));
    assert!(matches!(&e.doc.blocks[4], Block::Td { row: 1, .. }));
    // The caret lands in the first cell and a receiving paragraph is added after it
    assert_eq!(e.sel.focus, Pos::new(1, 0));
    assert!(matches!(&e.doc.blocks[7], Block::Para(_)));
    // emit → parse round trip still gives the same table shape
    let md = crate::md::emit(&e.doc);
    let re = crate::md::parse(&md);
    assert_eq!(
        re.blocks.iter().filter(|b| b.is_td()).count(),
        6,
        "emit: {md}"
    );
}

// ---- Table commands ----

/// Focus inside a table cell: insert-type commands clamp to outside the whole
/// table and never insert a block into the middle of it (Td is a contiguous run
/// of blocks, and inserting one would split the table apart).
#[test]
fn insert_commands_clamp_to_table_bounds() {
    // table_doc: 4 Td, no other blocks. The focus is in the 3rd cell (index 2).
    // insert-after → an empty paragraph lands after the table
    let mut e = table_doc();
    at(&mut e, 2, 1);
    e.apply(Cmd::InsertAfter);
    assert!(
        matches!(&e.doc.blocks[4], Block::Para(_)),
        "the new paragraph lands after the table"
    );
    assert_eq!(e.sel.focus, Pos::new(4, 0));
    assert!(
        e.doc.blocks[..4].iter().all(|b| b.is_td()),
        "the table's blocks are untouched"
    );

    // insert-before → an empty paragraph lands before the table
    let mut e = table_doc();
    at(&mut e, 3, 0);
    e.apply(Cmd::InsertBefore);
    assert!(
        matches!(&e.doc.blocks[0], Block::Para(_)),
        "the new paragraph lands after the table"
    );
    assert!(e.doc.blocks[1..].iter().all(|b| b.is_td()));
    assert_eq!(e.sel.focus, Pos::new(0, 0));

    // insert-hr → after the table there is an Hr + a receiving paragraph
    let mut e = table_doc();
    at(&mut e, 1, 0);
    e.apply(Cmd::InsertHr);
    assert!(matches!(&e.doc.blocks[4], Block::Hr));
    assert!(matches!(&e.doc.blocks[5], Block::Para(_)));
    assert!(e.doc.blocks[..4].iter().all(|b| b.is_td()));

    // Inserting a table while the focus is inside one → the new table (with its own
    // seq) lands after the old one as a whole
    let mut e = table_doc();
    at(&mut e, 2, 0);
    e.apply(Cmd::InsertTable { rows: 1, cols: 2 });
    let tds: Vec<&Block> = e.doc.blocks.iter().filter(|b| b.is_td()).collect();
    assert_eq!(tds.len(), 6, "old 4 cells + new 2 cells");
    let seqs: Vec<u64> = e.doc.blocks.iter().filter_map(|b| b.table_seq()).collect();
    assert_eq!(
        &seqs[..4],
        &[0, 0, 0, 0],
        "the old table's seq is unchanged"
    );
    assert_eq!(&seqs[4..], &[1, 1], "the new table uses the next seq");
    assert!(
        matches!(&e.doc.blocks[6], Block::Para(_)),
        "the receiving paragraph comes after the new table"
    );
    // The caret lands in the new table's first cell
    assert_eq!(e.sel.focus, Pos::new(4, 0));
}

fn table_doc() -> Editor {
    let seq = 0;
    Editor::new(Doc::new(vec![
        Block::Td {
            table_seq: seq,
            row: 0,
            align: 0,
            inlines: vec![Inline::Text("h1".into())],
        },
        Block::Td {
            table_seq: seq,
            row: 0,
            align: 0,
            inlines: vec![Inline::Text("h2".into())],
        },
        Block::Td {
            table_seq: seq,
            row: 1,
            align: 0,
            inlines: vec![Inline::Text("a".into())],
        },
        Block::Td {
            table_seq: seq,
            row: 1,
            align: 0,
            inlines: vec![Inline::Text("b".into())],
        },
    ]))
}

#[test]
fn table_insert_row() {
    let mut e = table_doc();
    at(&mut e, 2, 1);
    e.apply(Cmd::Table(TableCmd::InsertRowAfter));
    let rows: Vec<usize> = e
        .doc
        .blocks
        .iter()
        .filter_map(|b| match b {
            Block::Td { row, .. } => Some(*row),
            _ => None,
        })
        .collect();
    assert_eq!(rows, vec![0, 0, 1, 1, 2, 2]);
}

#[test]
fn table_delete_row_keeps_header() {
    let mut e = table_doc();
    at(&mut e, 2, 1);
    e.apply(Cmd::Table(TableCmd::DeleteRow));
    let rows: Vec<usize> = e
        .doc
        .blocks
        .iter()
        .filter_map(|b| match b {
            Block::Td { row, .. } => Some(*row),
            _ => None,
        })
        .collect();
    assert_eq!(rows, vec![0, 0]);
}

#[test]
fn table_insert_col() {
    let mut e = table_doc();
    at(&mut e, 0, 2);
    e.apply(Cmd::Table(TableCmd::InsertColAfter));
    assert_eq!(e.doc.blocks.len(), 6, "one cell added to every row");
    // Regression: it used to add a cell only to the focus row, and the table
    let count = |row: usize| {
        e.doc
            .blocks
            .iter()
            .filter(|b| matches!(b, Block::Td { row: r, .. } if *r == row))
            .count()
    };
    assert_eq!(count(0), 3, "row0 = 3 cells");
    assert_eq!(count(1), 3, "row1 must get a cell too = 3 cells");
}

/// Regression: InsertRowAfter's row-number +1 scan used to skip the seq check and
/// lift the row numbers of the second table (a different seq) further down the
/// document too — its row grouping and emit were all misaligned.
#[test]
fn table_insert_row_does_not_touch_other_tables() {
    let mut e = Editor::new(Doc::new(vec![
        Block::Td {
            table_seq: 0,
            row: 0,
            align: 0,
            inlines: vec![Inline::Text("a".into())],
        },
        Block::Para(vec![]),
        Block::Td {
            table_seq: 5,
            row: 1,
            align: 0,
            inlines: vec![Inline::Text("x".into())],
        },
        Block::Td {
            table_seq: 5,
            row: 2,
            align: 0,
            inlines: vec![Inline::Text("y".into())],
        },
    ]));
    at(&mut e, 0, 1); // focus on the first table's row0
    e.apply(Cmd::Table(TableCmd::InsertRowAfter));
    let rows_of = |seq: u64| -> Vec<usize> {
        e.doc
            .blocks
            .iter()
            .filter_map(|b| match b {
                Block::Td {
                    table_seq: s, row, ..
                } if *s == seq => Some(*row),
                _ => None,
            })
            .collect()
    };
    assert_eq!(rows_of(0), vec![0, 1], "the first table gets the new row");
    assert_eq!(
        rows_of(5),
        vec![1, 2],
        "the second table's row numbers are unaffected"
    );
}

/// Regression: when the focus was on a Td in the middle of the table, a
/// multi-line paste used to insert into the middle of the contiguous Td run and
/// split the table into two "tables sharing a seq". The pasted content must land
/// after the whole table.
#[test]
fn paste_multiline_clamped_to_table_bounds() {
    let mut e = table_doc();
    at(&mut e, 1, 1); // focus on row1's first cell (not the table's last cell)
    e.apply(Cmd::Paste("p1\n\np2\n".into()));
    // The table must still be a contiguous run of Td
    let first_td = e.doc.blocks.iter().position(|b| b.is_td()).unwrap();
    let last_td = e.doc.blocks.iter().rposition(|b| b.is_td()).unwrap();
    assert!(
        e.doc.blocks[first_td..=last_td].iter().all(|b| b.is_td()),
        "the table must not be interrupted by a non-Td block"
    );
    // The pasted paragraph lands after the table; the focus cell's tail half (the "2"
    // after off1 of "h2") joins the last block by the existing semantics
    let texts: Vec<String> = e.doc.blocks[last_td + 1..]
        .iter()
        .map(|b| b.plain_text())
        .collect();
    assert_eq!(
        texts,
        vec!["p1", "p22"],
        "the pasted content sits after the table end"
    );
}

// ---- Caret clamping ----

#[test]
fn stale_pos_clamped_after_shrink() {
    let mut e = ed("abcdef");
    at(&mut e, 0, 6);
    e.sel = Sel {
        anchor: Pos::new(0, 0),
        focus: Pos::new(0, 6),
    };
    e.apply(Cmd::DeleteSelection);
    assert_eq!(caret(&e), (0, 0));
    // An out-of-range old caret (clamped inside apply) does not panic
    e.apply(Cmd::Move(Move::Right));
    assert_eq!(caret(&e), (0, 0));
}

// ---- Audit regressions ----

/// A pasted table's table_seq is reallocated from 0; colliding with a table
/// already in the document merges the two into one wrong table — pasted blocks
/// must be renumbered past the document max.
#[test]
fn paste_table_gets_fresh_seq_no_fusion() {
    let mut e = table_doc(); // 4 cells, seq 0
    at(&mut e, 2, 1);
    e.apply(Cmd::Paste("| x |\n| --- |\n| y |".into()));
    let seqs: Vec<u64> = e.doc.blocks.iter().filter_map(|b| b.table_seq()).collect();
    assert_eq!(
        seqs.len(),
        6,
        "old 4 cells + pasted 2 cells: {:?}",
        e.doc.blocks
    );
    assert!(
        seqs[..4].iter().all(|&s| s == seqs[0]),
        "the old table's seq is unchanged"
    );
    assert!(
        seqs[4..].iter().all(|&s| s == seqs[4]),
        "the pasted table shares one seq"
    );
    assert_ne!(
        seqs[0], seqs[4],
        "the two tables must not collide and merge: {seqs:?}"
    );
}

/// An Html block's take_inlines is a no-op — SetHead used to replace the whole
/// HTML source with an empty heading; wrap_link used to silently drop the
/// operation while the caret moved on.
#[test]
fn set_head_and_link_on_html_block_are_noop() {
    let mut e = ed("<div>\nraw\n</div>");
    assert!(matches!(e.doc.blocks[0], Block::Html { .. }));
    at(&mut e, 0, 2);
    assert!(
        !e.apply(Cmd::SetHead(1)),
        "raising an Html block's heading must be a no-op"
    );
    assert!(
        matches!(&e.doc.blocks[0], Block::Html { text } if text.contains("raw")),
        "the HTML source must not be lost: {:?}",
        e.doc.blocks[0]
    );
    assert!(
        !e.apply(Cmd::Link("u".into())),
        "adding a link to an Html block is a no-op"
    );
    assert_eq!(caret(&e), (0, 2), "the caret must not move on");
}

/// When a cross-block selection delete merges, the tail block's inline structure
/// must not be flattened (it used to become `Para(head_plain + tail_plain)` with
/// Strong/Link/Image all evaporated).
#[test]
fn cross_block_delete_keeps_tail_inline_structure() {
    let mut e = ed("head tail\n\nx **y** z");
    e.sel = Sel {
        anchor: Pos::new(0, 5),
        focus: Pos::new(1, 2),
    };
    e.apply(Cmd::DeleteSelection);
    assert_eq!(e.doc.blocks.len(), 1);
    assert_eq!(text_at(&e, 0), "head y z");
    let Block::Para(inl) = &e.doc.blocks[0] else {
        panic!()
    };
    assert!(
        inl.iter().any(|i| matches!(i, Inline::Strong(_))),
        "the tail's Strong is kept: {inl:?}"
    );
}

/// When the first block is Code, the tail's structured suffix used to be
/// flattened by push_plain — the shell is downgraded to a Para carrying the
/// structure.
#[test]
fn cross_block_delete_from_code_keeps_tail_structure() {
    let mut e = ed("```\ncode\n```\n\nx **y** z");
    e.sel = Sel {
        anchor: Pos::new(0, 2),
        focus: Pos::new(1, 2),
    };
    e.apply(Cmd::DeleteSelection);
    assert_eq!(e.doc.blocks.len(), 1);
    assert_eq!(text_at(&e, 0), "coy z");
    let Block::Para(inl) = &e.doc.blocks[0] else {
        panic!(
            "the shell is downgraded to a Para carrying the structure: {:?}",
            e.doc.blocks[0]
        );
    };
    assert!(
        inl.iter().any(|i| matches!(i, Inline::Strong(_))),
        "the tail's Strong is kept: {inl:?}"
    );
}

/// When a Td collapses into a paragraph, the inline structure must not be
/// flattened either.
#[test]
fn cross_block_delete_in_td_keeps_inline_structure() {
    let mut e = Editor::new(Doc::new(vec![
        Block::Td {
            table_seq: 0,
            row: 0,
            align: 0,
            inlines: vec![Inline::Strong(vec![Inline::Text("ab".into())])],
        },
        Block::Td {
            table_seq: 0,
            row: 0,
            align: 0,
            inlines: vec![
                Inline::Text("x".into()),
                Inline::Strong(vec![Inline::Text("y".into())]),
            ],
        },
    ]));
    e.sel = Sel {
        anchor: Pos::new(0, 1),
        focus: Pos::new(1, 1),
    };
    e.apply(Cmd::DeleteSelection);
    // delete (0,1)..(1,1): block 0 keeps "a", block 1 keeps "y"
    assert_eq!(text_at(&e, 0), "ay");
    let Block::Para(inl) = &e.doc.blocks[0] else {
        panic!("the Td should collapse into a Para: {:?}", e.doc.blocks[0]);
    };
    assert_eq!(
        inl.iter()
            .filter(|i| matches!(i, Inline::Strong(_)))
            .count(),
        2,
        "both Strong runs are kept: {inl:?}"
    );
}

/// Math is an atomic node — a partial delete becomes a whole delete, leaving no
/// half-src shell behind (it used to emit a truncated `$$…$$`).
#[test]
fn partial_delete_of_math_removes_whole_node() {
    let doc = Doc::new(vec![Block::Para(vec![
        Inline::Text("a".into()),
        Inline::Math {
            src: "x+y".into(),
            display: true,
        },
    ])]);
    // A selection that ends inside the formula's source takes the atom whole:
    // plain = "ax+y", the range 1..3 covers half of `x+y`.
    let mut e = Editor::new(doc.clone());
    e.sel = Sel {
        anchor: Pos::new(0, 1),
        focus: Pos::new(0, 3),
    };
    e.apply(Cmd::DeleteSelection);
    assert_eq!(
        text_at(&e, 0),
        "a",
        "the Math is deleted whole: {:?}",
        e.doc.blocks[0]
    );
    let Block::Para(inl) = &e.doc.blocks[0] else {
        panic!()
    };
    assert!(!inl.iter().any(|i| matches!(i, Inline::Math { .. })));

    // A **caret** cannot sit inside the atom: it is snapped to the front edge, so
    // Backspace deletes the character before the formula instead of the formula
    // (the old behavior deleted all of `x+y` from a caret the user saw drawn in
    // front of it).
    let mut e = Editor::new(doc);
    at(&mut e, 0, 3);
    assert!(e.apply(Cmd::DeleteBack));
    assert_eq!(
        text_at(&e, 0),
        "x+y",
        "the formula survives: {:?}",
        e.doc.blocks[0]
    );
}

/// A Link partially deleted is unlinked — the remaining text survives, but the URL
/// must not end up hanging off the remaining tail.
#[test]
fn partial_delete_of_link_unwraps() {
    let mut e = Editor::new(Doc::new(vec![Block::Para(vec![
        Inline::Text("a".into()),
        Inline::Link {
            url: "u".into(),
            inlines: vec![Inline::Text("link".into())],
        },
    ])]));
    // plain = "alink", delete the middle "in"
    e.sel = Sel {
        anchor: Pos::new(0, 2),
        focus: Pos::new(0, 4),
    };
    e.apply(Cmd::DeleteSelection);
    assert_eq!(text_at(&e, 0), "alk");
    let Block::Para(inl) = &e.doc.blocks[0] else {
        panic!()
    };
    assert!(
        !inl.iter().any(|i| matches!(i, Inline::Link { .. })),
        "the remaining text is unlinked: {inl:?}"
    );
}

/// delete_back's cross-block merge must be structured (it used to go through
/// plain_text, evaporating Strong/Link/Image).
#[test]
fn delete_back_merge_keeps_inline_structure() {
    let mut e = ed("ab\n\nx **y** z");
    at(&mut e, 1, 0);
    e.apply(Cmd::DeleteBack);
    assert_eq!(e.doc.blocks.len(), 1);
    assert_eq!(text_at(&e, 0), "abx y z");
    let Block::Para(inl) = &e.doc.blocks[0] else {
        panic!()
    };
    assert!(
        inl.iter().any(|i| matches!(i, Inline::Strong(_))),
        "the merge keeps Strong: {inl:?}"
    );
}

/// delete_forward's block-end merge is structured too.
#[test]
fn delete_forward_merge_keeps_inline_structure() {
    let mut e = ed("ab\n\nx **y** z");
    at(&mut e, 0, 2);
    e.apply(Cmd::DeleteForward);
    assert_eq!(e.doc.blocks.len(), 1);
    assert_eq!(text_at(&e, 0), "abx y z");
    let Block::Para(inl) = &e.doc.blocks[0] else {
        panic!()
    };
    assert!(
        inl.iter().any(|i| matches!(i, Inline::Strong(_))),
        "the merge keeps Strong: {inl:?}"
    );
}

/// delete_forward's block-end merge is structured the same way as delete_back's —
/// the paragraph's text joins the code block and the Code shell and language tag
/// are kept (it used to flatten into a multi-line code span inside a paragraph).
#[test]
fn delete_forward_into_code_keeps_code_shell() {
    let mut e = ed("para\n\n```rs\ncode\n```");
    at(&mut e, 0, 4); // end of "para"
    e.apply(Cmd::DeleteForward);
    assert_eq!(e.doc.blocks.len(), 1);
    assert!(
        matches!(&e.doc.blocks[0], Block::Code { lang, text }
                if lang.as_deref() == Some("rs") && text == "para\ncode"),
        "paragraph joined the code block, language preserved: {:?}",
        e.doc.blocks[0]
    );
    assert_eq!(caret(&e), (0, 4));
}

/// Indent/Outdent must enter the undo stack (they used to be left outside
/// may_edit, so undoing a Tab indent was scrambled).
///
/// Use a **two-item** list: an indent needs a preceding sibling at the same level
/// (the prev-sibling feasibility guard), and an orphan item's indent is a no-op.
#[test]
fn indent_outdent_are_undoable() {
    let mut e = ed("- a\n- b");
    at(&mut e, 1, 2);
    assert!(e.apply(Cmd::Indent));
    assert!(matches!(&e.doc.blocks[1], Block::Li { level: 1, .. }));
    assert!(e.undo(), "Indent must enter the undo stack");
    assert!(
        matches!(&e.doc.blocks[1], Block::Li { level: 0, .. }),
        "undo restores the indentation level"
    );
}

/// Feasibility guards: indenting an orphan first item and outdenting at the top
/// level are both no-ops — otherwise "the button does nothing, the text does not
/// change, yet a dirty marker and an extra undo entry are left behind".
#[test]
fn indent_outdent_outside_feasible_range_is_noop() {
    let mut e = ed("- a");
    at(&mut e, 0, 2);
    assert!(
        !e.apply(Cmd::Indent),
        "an orphan first item (no preceding sibling) is a no-op"
    );
    assert!(matches!(&e.doc.blocks[0], Block::Li { level: 0, .. }));
    assert!(
        !e.apply(Cmd::Outdent),
        "outdent at the top level is a no-op"
    );
    assert!(matches!(&e.doc.blocks[0], Block::Li { level: 0, .. }));
}

/// Backspace at off 0 inside a table only moves the caret — demoting the cell to
/// a Para would tear the table shell.
#[test]
fn backspace_at_td_start_keeps_table() {
    let mut e = table_doc();
    at(&mut e, 2, 0);
    assert!(
        !e.apply(Cmd::DeleteBack),
        "it only moves the caret and does not change the document"
    );
    assert_eq!(
        e.doc.blocks.iter().filter(|b| b.is_td()).count(),
        4,
        "the table does not collapse: {:?}",
        e.doc.blocks
    );
    assert_eq!(
        caret(&e),
        (1, 2),
        "the caret stepped back into the previous cell's end"
    );
}

/// Enter in an empty Quote head leaves the quote shell first and then splits as a
/// paragraph (it used to produce "para\n> tail" or an orphan `> ` shell block).
#[test]
fn enter_at_quote_head_exits_shell_before_split() {
    let mut e = Editor::new(Doc::new(vec![Block::Quote(vec![Inline::Text(
        "abc".into(),
    )])]));
    at(&mut e, 0, 0);
    e.apply(Cmd::Enter);
    assert_eq!(e.doc.blocks.len(), 2);
    assert!(matches!(e.doc.blocks[0], Block::Para(_)));
    assert!(
        matches!(e.doc.blocks[1], Block::Para(_)),
        "the tail half is no longer inside a Quote shell: {:?}",
        e.doc.blocks[1]
    );
    assert_eq!(text_at(&e, 1), "abc");

    // Enter in an empty quote: leaves into a single empty paragraph (no orphan `> `
    // shell block is left behind)
    let mut e = Editor::new(Doc::new(vec![Block::Quote(vec![])]));
    at(&mut e, 0, 0);
    e.apply(Cmd::Enter);
    assert_eq!(e.doc.blocks.len(), 1);
    assert!(matches!(e.doc.blocks[0], Block::Para(_)));

    // Non-empty head: still splits inside the quote (the existing behaviour is unchanged)
    let mut e = Editor::new(Doc::new(vec![Block::Quote(vec![Inline::Text(
        "abc".into(),
    )])]));
    at(&mut e, 0, 2);
    e.apply(Cmd::Enter);
    assert!(matches!(e.doc.blocks[0], Block::Quote(_)));
    assert!(matches!(e.doc.blocks[1], Block::Quote(_)));
}

// ---- Editing semantics of quote ownership (the Quoted shell) ----

/// A list item inside a quote continues the list on Enter and both halves stay
/// inside the quote shell (the new block inherits the neighbour's depth and
/// emit's `>` prefix is not lost).
#[test]
fn enter_in_quoted_list_continues_quote() {
    let li = Block::Li {
        ordered: false,
        num: 0,
        level: 0,
        task: None,
        inlines: vec![Inline::Text("ab".into())],
    };
    let mut e = Editor::new(Doc::new(vec![wrap_quote(1, li)]));
    at(&mut e, 0, 1);
    e.apply(Cmd::Enter);
    assert_eq!(e.doc.blocks.len(), 2);
    for b in &e.doc.blocks {
        assert_eq!(
            b.quote_depth(),
            1,
            "both halves stay inside one quote level: {b:?}"
        );
        assert!(matches!(b.core(), Block::Li { .. }), "{b:?}");
    }
    assert_eq!(text_at(&e, 0), "a");
    assert_eq!(text_at(&e, 1), "b");
    let out = crate::md::emit(&e.doc);
    assert!(out.contains("> - a") && out.contains("> - b"), "{out}");
    assert!(
        out.lines().all(|l| l.starts_with("> ")),
        "the emit prefix is not lost: {out:?}"
    );
}

/// Enter on an empty quote block peels one quote level off (nested quotes peel
/// level by level; depth 1 is the "empty Quote head peels the shell" behaviour,
/// and the old Quote semantics stay compatible).
#[test]
fn enter_on_empty_quoted_para_exits_one_level() {
    let mut e = Editor::new(Doc::new(vec![wrap_quote(2, Block::Para(vec![]))]));
    at(&mut e, 0, 0);
    e.apply(Cmd::Enter);
    assert_eq!(e.doc.blocks.len(), 1);
    assert_eq!(
        e.doc.blocks[0].quote_depth(),
        1,
        "peels one level: {:?}",
        e.doc.blocks[0]
    );
    e.apply(Cmd::Enter);
    assert_eq!(e.doc.blocks.len(), 1);
    assert_eq!(
        e.doc.blocks[0].quote_depth(),
        0,
        "peels one more level: {:?}",
        e.doc.blocks[0]
    );
}

/// Backspace at the start of a block inside a quote peels one shell level off (the
/// inner block's kind and content are kept).
#[test]
fn backspace_at_quoted_head_drops_quote_level() {
    let mut e = Editor::new(Doc::new(vec![wrap_quote(
        2,
        Block::Para(vec![Inline::Text("x".into())]),
    )]));
    at(&mut e, 0, 0);
    e.apply(Cmd::DeleteBack);
    assert_eq!(e.doc.blocks[0].quote_depth(), 1, "{:?}", e.doc.blocks[0]);
    assert_eq!(text_at(&e, 0), "x");
    e.apply(Cmd::DeleteBack);
    assert_eq!(e.doc.blocks[0].quote_depth(), 0, "{:?}", e.doc.blocks[0]);
    assert_eq!(text_at(&e, 0), "x");
}

/// Typing inside a quote shell penetrates the shell and changes the content (the
/// shell is not lost, and emit still writes the `>` prefix).
#[test]
fn typing_in_quoted_list_keeps_shell() {
    let li = Block::Li {
        ordered: false,
        num: 0,
        level: 0,
        task: None,
        inlines: vec![Inline::Text("ab".into())],
    };
    let mut e = Editor::new(Doc::new(vec![wrap_quote(1, li)]));
    at(&mut e, 0, 2);
    e.apply(Cmd::InsertText("x".into()));
    assert_eq!(text_at(&e, 0), "abx");
    assert_eq!(e.doc.blocks[0].quote_depth(), 1, "{:?}", e.doc.blocks[0]);
    assert!(matches!(e.doc.blocks[0].core(), Block::Li { .. }));
    let out = crate::md::emit(&e.doc);
    assert!(out.starts_with("> - abx"), "{out}");
}

/// A single-line paste does not trigger autoformat (a pasted `- ` or `# ` is
/// content, not input).
#[test]
fn paste_single_line_skips_autoformat() {
    let mut e = ed("");
    at(&mut e, 0, 0);
    e.apply(Cmd::Paste("- x".into()));
    assert!(
        matches!(e.doc.blocks[0], Block::Para(_)),
        "a paste does not become a list: {:?}",
        e.doc.blocks[0]
    );
    assert_eq!(text_at(&e, 0), "- x");
    e.apply(Cmd::Paste("# ".into()));
    assert!(
        matches!(e.doc.blocks[0], Block::Para(_)),
        "a paste does not turn into a heading"
    );
    // Typing still auto-completes (for contrast)
    let mut e = ed("");
    e.apply(Cmd::InsertText("- ".into()));
    assert!(matches!(e.doc.blocks[0], Block::Li { .. }));
}

/// wrap_link with an empty selection constructs an Inline::Link node directly (no
/// "[x](u)" text pass).
#[test]
fn wrap_link_empty_selection_inserts_link_node() {
    let mut e = ed("ab");
    at(&mut e, 0, 1);
    assert!(e.apply(Cmd::Link("https://u".into())));
    assert_eq!(text_at(&e, 0), "ahttps://ub");
    let Block::Para(inl) = &e.doc.blocks[0] else {
        panic!()
    };
    assert!(
        inl.iter().any(|i| matches!(i, Inline::Link { url, inlines }
                if url == "https://u" && b_plain(inlines) == "https://u")),
        "a Link node was inserted, not literal text: {inl:?}"
    );
    assert_eq!(caret(&e), (0, 1 + "https://u".len()));
}

/// Pure selection commands do not change the document and apply returns false
/// (the contract is "did it change").
#[test]
fn select_word_and_block_return_false() {
    let mut e = ed("ab cd");
    at(&mut e, 0, 3);
    assert!(!e.apply(Cmd::SelectWord));
    at(&mut e, 0, 3);
    assert!(!e.apply(Cmd::SelectBlock));
}

/// A double-click at the block end falls back to the last word (it used to do
/// nothing).
#[test]
fn select_word_at_block_end_picks_last_word() {
    let mut e = ed("ab cd");
    at(&mut e, 0, 5); // end of block
    e.apply(Cmd::SelectWord);
    assert_eq!(e.sel.range(), (Pos::new(0, 3), Pos::new(0, 5)));
    // Trailing whitespace: still selects the last word
    let mut e = ed("ab cd ");
    at(&mut e, 0, 6);
    e.apply(Cmd::SelectWord);
    assert_eq!(e.sel.range(), (Pos::new(0, 3), Pos::new(0, 5)));
}

/// Double-click on whitespace selects the next word by convention (it used to
/// select a "punctuation + whitespace" run).
#[test]
fn select_word_on_whitespace_picks_next_word() {
    let mut e = ed("ab cd");
    at(&mut e, 0, 2); // the space
    e.apply(Cmd::SelectWord);
    assert_eq!(e.sel.range(), (Pos::new(0, 3), Pos::new(0, 5)));
}

/// Text entering the model is normalized at one entry point (`\r\n` / `\r` →
/// `\n`, `\u{0}` replaced) — otherwise the HTML parser's normalization makes the
/// caret offsets drift gradually.
#[test]
fn insert_text_normalizes_crlf_and_nul() {
    let mut e = ed("");
    at(&mut e, 0, 0);
    e.apply(Cmd::InsertText("a\r\nb\0c".into()));
    assert_eq!(text_at(&e, 0), "a\nb\u{FFFD}c");
    assert_eq!(
        caret(&e),
        (0, "a\nb\u{FFFD}c".len()),
        "the caret follows the normalized offset"
    );
}

/// The paste entry point normalizes too (the Windows clipboard is routinely CRLF).
#[test]
fn paste_normalizes_crlf() {
    let mut e = ed("```\ncode\n```");
    at(&mut e, 0, 4);
    e.apply(Cmd::Paste("x\r\ny".into()));
    assert!(
        matches!(&e.doc.blocks[0], Block::Code { text, .. } if text == "codex\ny"),
        "CRLF normalized to \n: {:?}",
        e.doc.blocks[0]
    );
    assert_eq!(
        caret(&e),
        (0, 7),
        "the caret follows the normalized offset (CRLF counts as 1)"
    );
}

/// A plain-text paste must never be upgraded into an executable Html block — it
/// is downgraded to an ordinary paragraph and the original markup stays as
/// literal Text (the meaning of an Html block opened manually is unchanged).
#[test]
fn paste_html_like_text_becomes_literal_para() {
    let mut e = ed("text");
    at(&mut e, 0, 4);
    e.apply(Cmd::Paste("<img src=x onerror=alert(1)>\n\nafter".into()));
    assert!(
        !e.doc.blocks.iter().any(|b| matches!(b, Block::Html { .. })),
        "a paste must not produce an Html block: {:?}",
        e.doc.blocks
    );
    assert!(
        e.doc
            .blocks
            .iter()
            .any(|b| matches!(b, Block::Para(inl) if b_plain(inl).contains("<img"))),
        "the original markup became literal text: {:?}",
        e.doc.blocks
    );
}

// ---- In-block soft break (Shift+Enter) ----

/// Undo-stack statistics: (depth, total plain bytes of the snapshots).
fn undo_stats(e: &Editor) -> (usize, usize) {
    let log = e.undo.lock().unwrap_or_else(|e| e.into_inner());
    (
        log.undo.len(),
        log.undo.iter().map(|s| doc_plain_bytes(&s.doc)).sum(),
    )
}

/// In-block soft break — no block split, plain counts `\n`, caret offset +1 (it
/// lands after the break).
#[test]
fn soft_break_inserts_break_without_splitting() {
    let mut e = ed("abcd");
    at(&mut e, 0, 2);
    e.apply(Cmd::SoftBreak);
    assert_eq!(e.doc.blocks.len(), 1, "no block split");
    assert_eq!(text_at(&e, 0), "ab\ncd");
    assert_eq!(
        caret(&e),
        (0, 3),
        "plain contains \n, caret after the break"
    );
    // emit turns it into an in-paragraph break and the round trip is lossless
    assert_eq!(md::emit(&e.doc), "ab\ncd\n");
    assert_eq!(md::parse("ab\ncd\n"), e.doc);
    // Typing after the break: it lands on the new line
    e.apply(Cmd::InsertText("X".into()));
    assert_eq!(text_at(&e, 0), "ab\nXcd");
}

/// A soft break keeps the list/quote shell (multiple lines inside an item become a
/// lazy continuation, and the round trip is lossless).
#[test]
fn soft_break_keeps_li_quote_shell() {
    let mut e = ed("- item");
    at(&mut e, 0, 2);
    e.apply(Cmd::SoftBreak);
    assert!(
        matches!(&e.doc.blocks[0], Block::Li { .. }),
        "the shell is not peeled"
    );
    assert_eq!(text_at(&e, 0), "it\nem");
    let out = md::emit(&e.doc);
    assert_eq!(md::parse(&out), e.doc, "in-block break round trip: {out:?}");

    let mut e = ed("> quote");
    at(&mut e, 0, 2);
    e.apply(Cmd::SoftBreak);
    assert!(
        matches!(&e.doc.blocks[0], Block::Quote(_)),
        "the shell is not peeled"
    );
    assert_eq!(md::parse(&md::emit(&e.doc)), e.doc);
}

/// Single-line blocks (Head/Td) have no in-block line-break semantics — a no-op;
/// a Code block inserts `\n` like Enter does.
#[test]
fn soft_break_head_noop_td_breaks_code_inserts_newline() {
    let mut e = ed("## head");
    at(&mut e, 0, 3);
    assert!(!e.apply(Cmd::SoftBreak), "Head no-op");
    assert_eq!(text_at(&e, 0), "head");

    // A cell: Shift+Enter and Enter have the same semantics (both insert <br>)
    let mut e = table_doc();
    at(&mut e, 1, 1);
    assert!(e.apply(Cmd::SoftBreak), "Td inserts <br>");
    assert_eq!(text_at(&e, 1), "h\n2");

    let mut e = ed("```\ncode\n```");
    at(&mut e, 0, 2);
    e.apply(Cmd::SoftBreak);
    assert_eq!(text_at(&e, 0), "co\nde");
}

/// The break byte can be deleted (Backspace eats the `\n` and the line, keeping
/// the offset ledger consistent).
#[test]
fn delete_back_across_break_merges_lines() {
    let mut e = ed("ab\ncd");
    at(&mut e, 0, 3);
    e.apply(Cmd::DeleteBack);
    assert_eq!(text_at(&e, 0), "abcd");
    assert_eq!(caret(&e), (0, 2));
}

// ---- Table keyboard navigation ----

/// The Tab chain visits every cell (selecting its whole content); Tab on the last
/// cell appends a row at the table end and lands on the new row's first cell.
#[test]
fn table_tab_chain_and_append_row_at_end() {
    let mut e = table_doc();
    at(&mut e, 0, 0);
    for expect in [1usize, 2, 3] {
        e.apply(Cmd::Table(TableCmd::CellNext));
        assert_eq!(
            e.sel.focus.block, expect,
            "the Tab chain reached cell {expect}"
        );
        // The whole cell is selected (the next keystroke replaces it)
        assert_eq!(e.sel.anchor, Pos::new(expect, 0));
        assert_eq!(e.sel.focus.off, e.doc.blocks[expect].plain_text().len());
    }
    // Tab on the last cell: append a row (2 cells) at the table end and land on the
    // new row's first cell
    e.apply(Cmd::Table(TableCmd::CellNext));
    assert_eq!(e.doc.blocks.len(), 6);
    assert!(
        e.doc.blocks[4..]
            .iter()
            .all(|b| matches!(b, Block::Td { row: 2, .. })),
        "new row row=2: {:?}",
        e.doc.blocks
    );
    assert_eq!(e.sel.focus, Pos::new(4, 0));
}

/// Shift+Tab moves back; the first cell is a no-op.
#[test]
fn table_shift_tab_walks_back() {
    let mut e = table_doc();
    at(&mut e, 3, 2);
    e.apply(Cmd::Table(TableCmd::CellPrev));
    assert_eq!(e.sel.focus.block, 2);
    assert_eq!(e.sel.anchor, Pos::new(2, 0));
    e.apply(Cmd::Table(TableCmd::CellPrev));
    assert_eq!(e.sel.focus.block, 1);
    e.apply(Cmd::Table(TableCmd::CellPrev));
    e.apply(Cmd::Table(TableCmd::CellPrev));
    assert_eq!(e.sel.focus.block, 0);
    // Shift+Tab on the first cell again: nothing moves
    assert!(!e.apply(Cmd::Table(TableCmd::CellPrev)));
    assert_eq!(e.sel.focus.block, 0);
}

/// Backspace at the start of a cell jumps to the previous cell's end; the table
/// shell is not torn.
#[test]
fn table_backspace_at_cell_start_goes_prev_cell() {
    let mut e = table_doc();
    at(&mut e, 2, 0);
    assert!(
        !e.apply(Cmd::DeleteBack),
        "a pure move does not change the document"
    );
    assert_eq!(e.sel.focus, Pos::new(1, 2), "the previous cell's end");
    assert!(
        e.doc.blocks.iter().all(|b| b.is_td()),
        "the table is not split"
    );
}

/// ↑/↓ move between Td by column across rows (Move's cross-block fallback has a
/// special case for Td).
#[test]
fn table_vertical_moves_by_column() {
    let mut e = table_doc();
    at(&mut e, 3, 1); // row1 col1
    e.apply(Cmd::Move(Move::PrevBlockEnd));
    assert_eq!(
        e.sel.focus.block, 1,
        "↑ reaches the same column's row above"
    );
    e.apply(Cmd::Move(Move::NextBlockStart));
    assert_eq!(
        e.sel.focus.block, 3,
        "↓ returns to the same column's row below"
    );
    // Table boundary (no blocks before/after the table): stay put
    at(&mut e, 1, 1);
    e.apply(Cmd::Move(Move::PrevBlockEnd));
    assert_eq!(
        e.sel.focus,
        Pos::new(1, 1),
        "↑ at the table's first row with no block outside: stays put"
    );
}

/// Table boundary: the first row moving ↑ leaves the table for the previous
/// block's end, the last row moving ↓ leaves it for the next block's start.
#[test]
fn table_vertical_leaves_table_at_edges() {
    let seq = 7;
    let td = |row, col| Block::Td {
        table_seq: seq,
        row,
        align: 0,
        inlines: vec![Inline::Text(format!("r{row}c{col}"))],
    };
    let mut e = Editor::new(Doc::new(vec![
        Block::Para(vec![Inline::Text("pre".into())]),
        td(0, 0),
        td(0, 1),
        td(1, 0),
        td(1, 1),
        Block::Para(vec![Inline::Text("post".into())]),
    ]));
    at(&mut e, 1, 2); // first cell of the table's first row
    e.apply(Cmd::Move(Move::PrevBlockEnd));
    assert_eq!(
        e.sel.focus,
        Pos::new(0, 3),
        "left the table, landed at the previous block's end"
    );
    at(&mut e, 4, 3); // last cell of the table's last row
    e.apply(Cmd::Move(Move::NextBlockStart));
    assert_eq!(
        e.sel.focus,
        Pos::new(5, 0),
        "left the table, landed at the next block's start"
    );
}

/// Cell navigation is a pure caret move (not in undo, does not clear redo);
/// appending a row on the last cell does enter undo.
#[test]
fn table_cell_nav_is_undo_free_row_append_is_undoable() {
    let mut e = table_doc();
    at(&mut e, 0, 0);
    e.apply(Cmd::Table(TableCmd::CellNext));
    e.apply(Cmd::Table(TableCmd::CellNext));
    e.apply(Cmd::Table(TableCmd::CellPrev));
    assert_eq!(undo_stats(&e).0, 0, "a pure move does not enter undo");
    // Appending a row on the last cell: one snapshot, and undo removes the whole row
    at(&mut e, 3, 0);
    e.apply(Cmd::Table(TableCmd::CellNext));
    assert_eq!(undo_stats(&e).0, 1);
    e.undo();
    assert_eq!(
        e.doc.blocks.len(),
        4,
        "one undo removes the whole row insertion"
    );
}

// ---- The memory gate on full-document undo snapshots ----

/// Keystroke-wave coalescing: a run of typing/backspaces/deletes in one wave
/// pushes a single snapshot.
/// The old implementation: one snapshot per DeleteBack/Forward key → 100 levels ≈ 100N bytes.
#[test]
fn undo_coalesces_typing_and_delete_bursts() {
    let mut e = ed(&"a".repeat(500));
    at(&mut e, 0, 500);
    for _ in 0..100 {
        e.apply(Cmd::DeleteBack);
    }
    assert_eq!(text_at(&e, 0).len(), 400);
    assert_eq!(
        undo_stats(&e).0,
        1,
        "100 consecutive backspaces = 1 snapshot"
    );
    e.undo();
    assert_eq!(
        text_at(&e, 0).len(),
        500,
        "one undo steps back over the whole wave"
    );
}

/// Large documents get a lowered cap: when plain > 2MB the undo depth drops from
/// 100 to 30, so the snapshot total is capped too. The old implementation: 100
/// edits = 100 full snapshots ≈ 100N (a 3MB document ≈ 300MB); the current value
/// is asserted below.
#[test]
fn undo_big_doc_depth_capped() {
    // 200 blocks × 11KB ≈ 2.2MB plain (above the BIG_DOC_BYTES threshold)
    let para = "x".repeat(11 * 1024);
    let blocks: Vec<Block> = (0..200)
        .map(|_| Block::Para(vec![Inline::Text(para.clone())]))
        .collect();
    let nbytes = blocks.iter().map(|b| b.plain_text().len()).sum::<usize>();
    assert!(
        nbytes > BIG_DOC_BYTES,
        "constructed a large document ({nbytes} bytes)"
    );
    let mut e = Editor::new(Doc::new(blocks));
    // Enter does not take part in keystroke-wave coalescing — it pushes its own
    // snapshot each time, exposing the stack depth cap
    for _ in 0..100 {
        at(&mut e, 0, 0);
        e.apply(Cmd::Enter);
    }
    let (depth, bytes) = undo_stats(&e);
    // Measurement output (visible with --nocapture): the old implementation was
    // 100 levels / ≈100N bytes in the same scenario
    println!("undo_big_doc: document {nbytes}B × 100 edits → stack depth {depth}, snapshot total {bytes}B");
    assert!(
        depth <= UNDO_CAP_BIGDOC,
        "large-document stack depth ≤ {UNDO_CAP_BIGDOC}, measured {depth}"
    );
    assert!(
        bytes <= UNDO_CAP_BIGDOC * nbytes,
        "snapshot total is capped ≈ depth × document: {bytes} > {UNDO_CAP_BIGDOC} × {nbytes}"
    );
}

/// A small document keeps a depth of 100 (so ordinary editing keeps its undo span).
#[test]
fn undo_small_doc_keeps_full_depth() {
    let mut e = ed("ab");
    for _ in 0..120 {
        at(&mut e, 0, 0);
        e.apply(Cmd::Enter);
    }
    assert_eq!(
        undo_stats(&e).0,
        UNDO_CAP,
        "the small-document stack depth cap is still 100"
    );
}

// ---- Editing operations inside a quote take effect (Quoted shell penetration) ----

/// Row-number sequence (collects Td.row through the quote shell).
fn td_rows(e: &Editor) -> Vec<usize> {
    e.doc
        .blocks
        .iter()
        .map(|b| match b.core() {
            Block::Td { row, .. } => *row,
            other => panic!("everything in the table should be a Td: {other:?}"),
        })
        .collect()
}

/// A Td inside a Quoted shell is no longer blocked by the shell — the TableCmd
/// row/column operations take effect, the new cells inherit the quote depth, and
/// emit produces a correct table structure (rectangular, row order aligned) with
/// the `> ` prefix intact.
#[test]
fn table_cmd_pierces_quote_shell() {
    let mut e = ed("> | h1 | h2 |\n> | --- | --- |\n> | a | b |\n");
    assert_eq!(e.doc.blocks.len(), 4, "{:?}", e.doc.blocks);
    assert!(e
        .doc
        .blocks
        .iter()
        .all(|b| b.is_td() && b.quote_depth() == 1));
    // Insert a row: there are still rows **below** the focus row (regression: the
    // row numbers must renumber — colliding row numbers make emit group them into a
    // ragged shape)
    at(&mut e, 0, 1);
    assert!(e.apply(Cmd::Table(TableCmd::InsertRowAfter)));
    assert_eq!(e.doc.blocks.len(), 6);
    assert_eq!(td_rows(&e), vec![0, 0, 1, 1, 2, 2], "row numbers renumber");
    assert!(
        e.doc.blocks.iter().all(|b| b.quote_depth() == 1),
        "the new cell inherits the quote depth: {:?}",
        e.doc.blocks
    );
    // Insert a column: every row gets a cell (located through the shell, so the
    // table does not split into a ragged shape)
    at(&mut e, 2, 0);
    assert!(e.apply(Cmd::Table(TableCmd::InsertColAfter)));
    assert_eq!(e.doc.blocks.len(), 9);
    let counts = |e: &Editor| {
        (0..3)
            .map(|r| td_rows(e).iter().filter(|&&x| x == r).count())
            .collect::<Vec<_>>()
    };
    assert_eq!(
        counts(&e),
        vec![3, 3, 3],
        "3×3 rectangle: {:?}",
        e.doc.blocks
    );
    // emit: the `> ` prefix is intact and the round trip is lossless
    let out = md::emit(&e.doc);
    assert!(out.lines().all(|l| l.starts_with("> ")), "{out:?}");
    assert_eq!(md::parse(&out), e.doc, "round trip is lossless: {out}");
    // Delete a column → 2 cells per row; delete a row → 2 rows of 4 cells, still rectangular
    at(&mut e, 2, 0);
    assert!(e.apply(Cmd::Table(TableCmd::DeleteCol)));
    assert_eq!(counts(&e), vec![2, 2, 2], "{:?}", e.doc.blocks);
    at(&mut e, 0, 0);
    assert!(e.apply(Cmd::Table(TableCmd::DeleteRow)));
    assert_eq!(e.doc.blocks.len(), 4);
    assert_eq!(td_rows(&e), vec![0, 0, 1, 1], "{:?}", e.doc.blocks);
    let out = md::emit(&e.doc);
    assert!(out.lines().all(|l| l.starts_with("> ")), "{out:?}");
    assert_eq!(md::parse(&out), e.doc, "round-trip fidelity: {out}");
}

/// Indent / outdent on a list item inside a quote works (it changes Li.level and
/// leaves the Quoted shell alone).
/// Use two items: an indent needs a preceding sibling at the same level (see
/// [`Editor::indent`]'s feasibility guard).
#[test]
fn indent_outdent_inside_quote() {
    let mut e = ed("> - ab\n> - cd");
    assert!(
        matches!(e.doc.blocks[1].core(), Block::Li { .. }),
        "prerequisite"
    );
    at(&mut e, 1, 2);
    assert!(e.apply(Cmd::Indent));
    assert!(
        matches!(e.doc.blocks[1].core(), Block::Li { level: 1, .. }),
        "{:?}",
        e.doc.blocks[1]
    );
    assert_eq!(e.doc.blocks[1].quote_depth(), 1, "the shell is untouched");
    assert!(e.apply(Cmd::Outdent));
    assert!(matches!(e.doc.blocks[1].core(), Block::Li { level: 0, .. }));
    assert_eq!(e.doc.blocks[1].quote_depth(), 1, "the shell is untouched");
    // Mutating operations enter the undo stack
    e.apply(Cmd::Indent);
    assert!(e.undo(), "Indent must enter the undo stack");
    assert!(matches!(e.doc.blocks[1].core(), Block::Li { level: 0, .. }));
}

/// Turning a paragraph inside a quote into a list works (the new Li inherits the
/// same quote_depth); pressing again cancels it.
#[test]
fn toggle_list_inside_quote() {
    let mut e = ed("> para");
    assert_eq!(
        e.doc.blocks[0].quote_depth(),
        1,
        "prerequisite: {:?}",
        e.doc.blocks[0]
    );
    assert!(e.apply(Cmd::ToggleList { ordered: false }));
    assert!(
        matches!(e.doc.blocks[0].core(), Block::Li { .. }),
        "{:?}",
        e.doc.blocks[0]
    );
    assert_eq!(
        e.doc.blocks[0].quote_depth(),
        1,
        "the new Li inherits the same quote_depth"
    );
    let out = md::emit(&e.doc);
    assert!(out.starts_with("> - para"), "{out}");
    assert_eq!(md::parse(&out), e.doc, "{out}");
    // Pressing it again cancels the list: it stays in the quote (depth 1 normalizes
    // to the Quote form kept for compatibility)
    assert!(e.apply(Cmd::ToggleList { ordered: false }));
    assert_eq!(e.doc.blocks[0].quote_depth(), 1, "{:?}", e.doc.blocks[0]);
    assert!(
        matches!(e.doc.blocks[0].core(), Block::Para(_) | Block::Quote(_)),
        "{:?}",
        e.doc.blocks[0]
    );
    // A nested quote (depth 2) behaves the same way
    let mut e = ed("> > deep");
    at(&mut e, 0, 0);
    assert!(e.apply(Cmd::ToggleList { ordered: true }));
    assert!(matches!(
        e.doc.blocks[0].core(),
        Block::Li { ordered: true, .. }
    ));
    assert_eq!(e.doc.blocks[0].quote_depth(), 2, "{:?}", e.doc.blocks[0]);
}

/// Inside a quote, an ordered list continues and increments on Enter — the new
/// item gets num+1 and the following same-level items renumber (through the
/// shell; the old implementation had the quote shell block the renumbering, so
/// the numbers never incremented).
#[test]
fn enter_in_quoted_ordered_list_renumbers() {
    let mut e = ed("> 1. a\n> 2. b\n> 3. c");
    at(&mut e, 0, 1); // end of the first item "a"
    e.apply(Cmd::Enter);
    let nums: Vec<u64> = e
        .doc
        .blocks
        .iter()
        .map(|b| match b.core() {
            Block::Li {
                ordered: true, num, ..
            } => *num,
            other => panic!("everything inside the quote should be an ordered item: {other:?}"),
        })
        .collect();
    assert_eq!(nums, vec![1, 2, 3, 4], "following items renumber: {nums:?}");
    assert!(e.doc.blocks.iter().all(|b| b.quote_depth() == 1));
    let out = md::emit(&e.doc);
    assert!(
        out.contains("> 2. ") && out.contains("> 3. b") && out.contains("> 4. c"),
        "{out}"
    );
    assert_eq!(md::parse(&out), e.doc, "{out}");
}

/// The renumbering logic is the same outside quotes (the old bump loop started
/// scanning at p.block+2 and skipped the adjacent item while the new item was not
/// inserted yet — a 3-item list only renumbered as far as the third item).
#[test]
fn enter_in_ordered_list_renumbers_following() {
    let mut e = ed("1. a\n2. b\n3. c");
    at(&mut e, 0, 1);
    e.apply(Cmd::Enter);
    let nums: Vec<u64> = e
        .doc
        .blocks
        .iter()
        .map(|b| match b {
            Block::Li {
                ordered: true, num, ..
            } => *num,
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(nums, vec![1, 2, 3, 4], "{nums:?}");
}

// Hard-line-break merging (delete the break node, joining two lines into one)

/// Two trailing spaces = a hard break; Backspace at the start of the second line
/// deletes the break node and joins the two lines (the break really disappears
/// and leaves no empty Text behind).
#[test]
fn delete_back_merges_hard_break_lines() {
    let mut e = ed("ab  \ncd");
    assert!(
        matches!(&e.doc.blocks[0], Block::Para(i) if matches!(i[1], Inline::HardBreak)),
        "prerequisite: a hard-break node: {:?}",
        e.doc.blocks[0]
    );
    at(&mut e, 0, 3); // start of the second line (plain "ab\ncd")
    assert!(e.apply(Cmd::DeleteBack));
    assert_eq!(
        e.doc.blocks[0],
        Block::Para(vec![Inline::Text("abcd".into())]),
        "merged into one line, no empty Text/break left behind: {:?}",
        e.doc.blocks[0]
    );
    assert_eq!(caret(&e), (0, 2));
    // emit merges the lines and the round trip has no line break left
    assert_eq!(md::emit(&e.doc), "abcd\n");
    assert_eq!(md::parse("abcd\n"), e.doc);
}

/// Delete (caret immediately before the break) deletes and merges the break too;
/// the same goes for a soft break.
#[test]
fn delete_forward_merges_break_lines() {
    let mut e = ed("ab  \ncd");
    at(&mut e, 0, 2); // just before the hard break
    assert!(e.apply(Cmd::DeleteForward));
    assert_eq!(text_at(&e, 0), "abcd");
    assert_eq!(md::emit(&e.doc), "abcd\n");

    let mut e = ed("ab\ncd"); // a single line break is a soft break
    assert!(
        matches!(&e.doc.blocks[0], Block::Para(i) if matches!(i[1], Inline::SoftBreak)),
        "prerequisite: a soft break"
    );
    at(&mut e, 0, 2); // just before the soft break
    assert!(e.apply(Cmd::DeleteForward));
    assert_eq!(text_at(&e, 0), "abcd");
    assert_eq!(md::emit(&e.doc), "abcd\n");
}

// ---- Table column-alignment command (kernel) ----

/// `TableCmd::Align` sets the align of every Td in the focus cell's column and
/// enters the undo stack; emit writes the alignment style on the separator row.
#[test]
fn table_align_column_and_undo() {
    let mut e = table_doc();
    at(&mut e, 2, 1); // row1's first cell (column 0)
    assert!(e.apply(Cmd::Table(TableCmd::Align(2)))); // centre
    let aligns: Vec<u8> = e
        .doc
        .blocks
        .iter()
        .map(|b| match b.core() {
            Block::Td { align, .. } => *align,
            _ => 0,
        })
        .collect();
    assert_eq!(
        aligns,
        vec![2, 0, 2, 0],
        "only the focus column changed: {aligns:?}"
    );
    // undo restores the original align (Align must enter the undo stack)
    assert!(e.undo(), "Align must enter the undo stack");
    let aligns: Vec<u8> = e
        .doc
        .blocks
        .iter()
        .map(|b| match b.core() {
            Block::Td { align, .. } => *align,
            _ => 0,
        })
        .collect();
    assert!(aligns.iter().all(|&a| a == 0), "undo restores: {aligns:?}");
    // The separator row's style (column 0 centred `:-:`, column 1 right `--:`)
    at(&mut e, 0, 0);
    assert!(e.apply(Cmd::Table(TableCmd::Align(2))));
    at(&mut e, 1, 0);
    assert!(e.apply(Cmd::Table(TableCmd::Align(3))));
    let out = md::emit(&e.doc);
    assert!(
        out.contains("|:-:|--:|"),
        "separator row alignment styles: {out}"
    );
    // The separator row's styles (column 0 centred `:-:`, column 1 right `--:`)
    // Compare blocks only: table_doc builds next_table_seq=0 by hand, while parse would allocate 1
    assert_eq!(
        md::parse(&out).blocks,
        e.doc.blocks,
        "alignment round trip is lossless: {out}"
    );
}

/// Column alignment penetrates the quote shell (a Td inside a Quoted shell is
/// aligned too).
#[test]
fn table_align_inside_quote_shell() {
    let mut e = ed("> | h1 | h2 |\n> | --- | --- |\n> | a | b |");
    at(&mut e, 2, 0); // first cell of row 1 in the quoted table
    assert!(e.apply(Cmd::Table(TableCmd::Align(1))));
    for (i, b) in e.doc.blocks.iter().enumerate() {
        let Block::Td { align, .. } = b.core() else {
            panic!("{b:?}")
        };
        assert_eq!(*align, if i % 2 == 0 { 1 } else { 0 }, "block {i}: {b:?}");
    }
    let out = md::emit(&e.doc);
    assert!(out.contains("> |:--|---|"), "{out}");
    assert_eq!(md::parse(&out), e.doc, "{out}");
}

// ---- Caret × atomic nodes (formula/image) × tables: multi-path matrix ----

/// An inline formula/image is **one visual object**: ←/→ cross it whole and never
/// stop inside its source/alt virtual bytes (→ used to walk into the source of
/// `$E=mc^2$`, where the caret had nowhere to be drawn).
#[test]
fn caret_steps_over_math_and_image_atoms() {
    let math_len = "E=mc^2".len();
    let mut e = ed("a$E=mc^2$b");
    at(&mut e, 0, 1);
    assert!(e.apply(Cmd::Move(Move::Right)));
    assert_eq!(
        caret(&e),
        (0, 1 + math_len),
        "→ should skip the formula whole"
    );
    assert!(e.apply(Cmd::Move(Move::Left)));
    assert_eq!(
        caret(&e),
        (0, 1),
        "← should jump back to the atomic's left edge in one step"
    );

    let alt_len = "示例图".len();
    let mut e = ed("a![示例图](./pic.png)b");
    at(&mut e, 0, 1);
    assert!(e.apply(Cmd::Move(Move::Right)));
    assert_eq!(caret(&e), (0, 1 + alt_len), "→ should skip the whole image");
    assert!(e.apply(Cmd::Move(Move::Left)));
    assert_eq!(caret(&e), (0, 1));

    // An atomic node inside a container (Strong): still skipped whole
    let mut e = ed("**$x$**");
    at(&mut e, 0, 0);
    assert!(e.apply(Cmd::Move(Move::Right)));
    assert_eq!(caret(&e), (0, 1));
    assert!(e.apply(Cmd::Move(Move::Left)));
    assert_eq!(caret(&e), (0, 0));

    // Word movement (Ctrl+←/→) likewise never splits a formula's source into words
    let mut e = ed("词 $a+b$ 尾");
    at(&mut e, 0, 0);
    let after_math = "词 ".len() + "a+b".len();
    assert!(e.apply(Cmd::Move(Move::WordRight)));
    assert!(
        caret(&e).1 >= after_math,
        "Ctrl+→ landed at {:?}",
        caret(&e)
    );
}

/// Backspace/Delete remove an atom whole at its boundary (no half-source left
/// behind).
#[test]
fn delete_removes_math_and_image_atoms_whole() {
    let mut e = ed("a$E=mc^2$b");
    at(&mut e, 0, 1 + "E=mc^2".len());
    assert!(e.apply(Cmd::DeleteBack));
    assert_eq!(text_at(&e, 0), "ab");

    let mut e = ed("a![示例图](./pic.png)b");
    at(&mut e, 0, 1);
    assert!(e.apply(Cmd::DeleteForward));
    assert_eq!(text_at(&e, 0), "ab");
    assert_eq!(md::emit(&e.doc), "ab\n");
}

/// A formula inside a table cell: the round trip is lossless, Tab/↑↓ still move
/// by cell, and the formula's source is never cut apart as ordinary text.
#[test]
fn table_cells_with_math_and_marks_roundtrip() {
    let src = "| **粗** | $a^2$ | `c` |\n|---|---|---|\n| $x$ | b | [l](u) |\n";
    let mut e = ed(src);
    // 6 cells in the same table; the header cell's formula is skipped whole
    assert_eq!(e.doc.blocks.iter().filter(|b| b.is_td()).count(), 6);
    at(&mut e, 1, 0);
    assert!(e.apply(Cmd::Move(Move::Right)));
    assert_eq!(caret(&e), (1, "a^2".len()));

    // Tab reaches the next cell (its whole content is selected, focus at the cell start)
    assert!(e.apply(Cmd::Table(TableCmd::CellNext)));
    assert_eq!(caret(&e).0, 2);
    // ↓ moves by column across rows: cell 3 → cell 6
    assert!(e.apply(Cmd::Move(Move::NextBlockStart)));
    assert_eq!(caret(&e).0, 5);
    // ↑ back to the header row's column
    assert!(e.apply(Cmd::Move(Move::PrevBlockEnd)));
    assert_eq!(caret(&e).0, 2);

    let out = md::emit(&e.doc);
    assert!(out.contains("$a^2$") && out.contains("$x$"), "{out}");
    assert_eq!(
        md::parse(&out),
        e.doc,
        "formula/markup table round trip: {out}"
    );
}

/// Backspace at the start of a cell = move to the previous cell (a pure move:
/// not in undo, no document change).
#[test]
fn backspace_at_cell_start_only_moves() {
    let mut e = table_doc();
    at(&mut e, 2, 0);
    let before = e.doc.clone();
    let r = e.apply(Cmd::DeleteBack);
    assert!(!r, "a pure move must not report a document change");
    assert_eq!(caret(&e).0, 1, "should enter the previous cell");
    assert_eq!(e.doc, before, "the document must not change");
    assert_eq!(undo_stats(&e).0, 0, "a pure move does not enter undo");
}

/// Full-feature sample: at any block boundary the caret ←/→ must never stop
/// inside an atomic node; one step right then one step left returns to the
/// original boundary (the block-end → next-block-start → previous-block-end ring
/// holds too).
#[test]
fn all_features_caret_never_lands_inside_atoms() {
    const SAMPLE: &str = include_str!("../../../../samples/wys-all-features.md");
    let doc = md::parse(SAMPLE);
    let mut e = Editor::new(doc.clone());
    for (bi, b) in doc.blocks.iter().enumerate() {
        let atoms = atom_bounds(b);
        let plain = b.plain_text();
        let len = plain.len();
        let mut probes = vec![0, len];
        if let Some((i, _)) = plain.char_indices().nth(1) {
            probes.push(i);
        }
        probes.sort_unstable();
        probes.dedup();
        for off in probes {
            at(&mut e, bi, off);
            e.apply(Cmd::Move(Move::Right));
            let (rb, ro) = caret(&e);
            if rb == bi {
                assert!(
                    !atoms.iter().any(|&(s, en)| ro > s && ro < en),
                    "block {bi} off {off} → landed inside an atomic node {ro}"
                );
            }
            e.apply(Cmd::Move(Move::Left));
            let (lb, lo) = caret(&e);
            if lb == bi {
                assert!(
                    !atoms.iter().any(|&(s, en)| lo > s && lo < en),
                    "block {bi} off {off} ← landed inside an atomic node {lo}"
                );
            }
        }
    }
}

/// Enter / Backspace after a display formula (`$$` block) does not break the
/// formula node; the formula takes part in block-level editing as a whole.
#[test]
fn display_math_block_editing_keeps_math_whole() {
    let mut e = ed("$$\n\\int_0^1 x\\,dx\n$$");
    assert_eq!(e.doc.blocks.len(), 1);
    let len = text_at(&e, 0).len();
    at(&mut e, 0, len);
    assert!(e.apply(Cmd::Enter));
    assert_eq!(e.doc.blocks.len(), 2);
    assert!(matches!(&e.doc.blocks[1], Block::Para(p) if p.is_empty()));
    // Backspace in the new paragraph merges back into the formula block: the formula
    // is still there and is not split apart
    assert!(e.apply(Cmd::DeleteBack));
    assert!(matches!(
        &e.doc.blocks[0],
        Block::Para(inl) if inl.iter().any(|i| matches!(i, Inline::Math { display: true, .. }))
    ));
    let out = md::emit(&e.doc);
    assert!(out.contains("$$"), "{out}");
    assert_eq!(md::parse(&out), e.doc, "{out}");
}

// ---- UTF-8 boundary safety ------------------------------------------
//
// Regression: the release build died within seconds with
//   `assertion failed: self.is_char_boundary(idx)` and
//   `byte index 14 is not a char boundary; it is inside '如'`
// on the main thread (crash.log). Root cause: byte offsets arriving from
// outside the model (a click hit-test, a stale caret, an IME commit) land
// **inside** 3-byte CJK characters, and every string op slicing there
// panicked. These tests install mid-character offsets on purpose.

/// First byte offset that lands *inside* a multibyte character.
fn mid_offset(s: &str) -> usize {
    s.char_indices()
        .find(|(_, c)| c.len_utf8() > 1)
        .map(|(i, _)| i + 1)
        .expect("the sample must contain a multibyte character")
}

#[test]
fn insert_at_mid_char_snaps_to_the_character_start() {
    let mut e = ed("例如");
    at(&mut e, 0, mid_offset("例如"));
    assert!(e.apply(Cmd::InsertText("x".into())));
    assert_eq!(text_at(&e, 0), "x例如");
    assert_eq!(
        caret(&e),
        (0, 1),
        "the caret follows the inserted text, not the stale offset"
    );
}

#[test]
fn code_block_edits_at_mid_char_never_panic() {
    let mut e = ed("```\n例如\n```");
    let m = mid_offset("例如");
    at(&mut e, 0, m);
    assert!(e.apply(Cmd::InsertText("x".into())));
    assert_eq!(text_at(&e, 0), "x例如");
    // Still inside 例 after the insert; Enter must not slice it in half.
    let m = mid_offset(&text_at(&e, 0));
    at(&mut e, 0, m);
    assert!(e.apply(Cmd::Enter));
    assert!(text_at(&e, 0).contains('例'), "{}", text_at(&e, 0));
    let m = mid_offset(&text_at(&e, 0));
    at(&mut e, 0, m);
    e.apply(Cmd::SoftBreak);
    assert!(text_at(&e, 0).contains('例'), "{}", text_at(&e, 0));
}

#[test]
fn delete_back_and_forward_at_mid_char_never_panic() {
    // "a例如b": 例 is 1..4, so offset 2 sits inside it.
    let m = mid_offset("a例如b");
    let mut e = ed("a例如b");
    at(&mut e, 0, m);
    assert!(e.apply(Cmd::DeleteBack));
    assert_eq!(
        text_at(&e, 0),
        "例如b",
        "backspace deletes what precedes the snapped caret"
    );

    let mut e = ed("a例如b");
    at(&mut e, 0, m);
    assert!(e.apply(Cmd::DeleteForward));
    assert_eq!(
        text_at(&e, 0),
        "a如b",
        "forward delete removes the character under the snapped caret"
    );
}

#[test]
fn selected_text_survives_mid_char_bounds() {
    let mut e = ed("例如");
    e.sel = Sel {
        anchor: Pos::new(0, 1),
        focus: Pos::new(0, 5),
    };
    assert_eq!(
        e.selected_text(),
        "例如",
        "partial characters are included whole"
    );

    // Out-of-range end clamps to the block end.
    e.sel = Sel {
        anchor: Pos::new(0, 9),
        focus: Pos::new(0, 0),
    };
    assert_eq!(e.selected_text(), "例如");

    // A stale block index must not index past the document.
    e.sel = Sel {
        anchor: Pos::new(7, 0),
        focus: Pos::new(9, 3),
    };
    let copied = e.selected_text();
    assert!(
        copied.chars().count() <= 2,
        "stale blocks copy at most the clamped block: {copied:?}"
    );
}

#[test]
fn delete_selection_with_mid_char_bounds_never_panics() {
    let mut e = ed("a例如b");
    e.sel = Sel {
        anchor: Pos::new(0, 2),
        focus: Pos::new(0, 5),
    };
    assert!(e.apply(Cmd::DeleteSelection));
    assert_eq!(
        text_at(&e, 0),
        "ab",
        "the partially covered characters go with the selection"
    );

    // A Code block (plain-text shell) takes the same path.
    let mut e = ed("```\na例如b\n```");
    e.sel = Sel {
        anchor: Pos::new(0, 2),
        focus: Pos::new(0, 5),
    };
    assert!(e.apply(Cmd::DeleteSelection));
    assert_eq!(text_at(&e, 0), "ab");

    // Cross-block: head truncate and tail split_off must both snap. The head
    // starts inside 如 (which is deleted whole), the tail ends inside 例 (which is
    // also taken), so only the first block's 例 survives.
    let mut e = ed("例如\n\n如例");
    e.sel = Sel {
        anchor: Pos::new(0, 4),
        focus: Pos::new(1, 4),
    };
    assert!(e.apply(Cmd::DeleteSelection));
    assert_eq!(e.doc.blocks.len(), 1);
    assert_eq!(text_at(&e, 0), "例");
}

#[test]
fn caret_moves_from_a_mid_char_offset_land_on_boundaries() {
    for m in [1usize, 2] {
        let mut e = ed("a例如b");
        at(&mut e, 0, m);
        for cmd in [
            Cmd::Move(Move::Left),
            Cmd::Move(Move::Right),
            Cmd::Move(Move::WordLeft),
            Cmd::Move(Move::WordRight),
        ] {
            e.apply(cmd.clone());
            let (b, off) = caret(&e);
            let p = text_at(&e, b);
            assert!(
                off <= p.len() && p.is_char_boundary(off),
                "{cmd:?} left the caret at {off}, inside a character of {p:?}"
            );
        }
    }
}

#[test]
fn task_list_chinese_mid_char_edits_never_panic() {
    let mut e = ed("- [ ] 例如");
    let text = text_at(&e, 0);
    assert_eq!(text, "例如");
    at(&mut e, 0, mid_offset(&text));
    assert!(e.apply(Cmd::InsertText("中".into())));
    e.apply(Cmd::ToggleTaskChecked(0));
    e.apply(Cmd::DeleteBack);
    e.apply(Cmd::Enter);
    let out = md::emit(&e.doc);
    let _ = md::parse(&out);
    assert!(!e.doc.blocks.is_empty());
}

/// Deterministic operation storm over CJK text, task lists, quotes and code:
/// every command must survive offsets that are stale, out of range, or inside
/// a character. This is the regression net for the char-boundary crash family.
#[test]
fn random_ops_over_chinese_never_panic() {
    let mut e = ed("- [ ] 例如：这是一个任务\n\n> 引用 `代码` 与 **粗体**\n\n```\nfn 主() {}\n```");
    let mut seed: u64 = 0x2545_F491_4F6C_DD1D;
    let mut rnd = |n: usize| -> usize {
        seed = seed
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        ((seed >> 33) as usize) % n.max(1)
    };
    let samples = [
        "中", "文", "如", "例", "\n", "a", "🙂", "**b**", "- ", "[ ] ", "`c`", "$x$",
    ];
    for step in 0..3000 {
        let op = rnd(16);
        // Op 8 installs a deliberately corrupt caret and runs no command, so the
        // bounds invariants below are checked only for the commands' outputs.
        let mut corrupted = false;
        let changed = match op {
            0..=3 => e.apply(Cmd::InsertText(samples[rnd(samples.len())].into())),
            4 => e.apply(Cmd::DeleteBack),
            5 => e.apply(Cmd::DeleteForward),
            6 => e.apply(Cmd::Enter),
            7 => e.apply(Cmd::SoftBreak),
            8 => {
                // Deliberately corrupt: an arbitrary block/offset, mid-character
                // or past the end.
                let b = rnd(e.doc.blocks.len());
                e.sel = Sel::caret(Pos::new(b, rnd(64)));
                corrupted = true;
                false
            }
            9 => e.apply(Cmd::Move(match rnd(4) {
                0 => Move::Left,
                1 => Move::Right,
                2 => Move::WordLeft,
                _ => Move::WordRight,
            })),
            // Structural commands: these touch table/list/quote shells with
            // computed block indices — the paths a hand-written test rarely
            // reaches (0-row tables, outdent at level 0, a quote inside a cell).
            10 => e.apply(Cmd::ToggleList {
                ordered: rnd(2) == 1,
            }),
            11 => e.apply(Cmd::ToggleTask),
            12 => e.apply(Cmd::ToggleQuote),
            13 => e.apply(if rnd(2) == 0 {
                Cmd::Indent
            } else {
                Cmd::Outdent
            }),
            14 => e.apply(Cmd::SetHead((1 + rnd(6)) as u8)),
            _ => e.apply(Cmd::InsertTable {
                rows: rnd(4) as u8,
                cols: rnd(4) as u8,
            }),
        };
        assert!(
            !e.doc.blocks.is_empty(),
            "step {step}: the document never empties"
        );
        let (b, off) = caret(&e);
        assert!(
            b < e.doc.blocks.len(),
            "step {step}: caret block out of range"
        );
        let p = text_at(&e, b);
        if !corrupted {
            assert!(
                off <= p.len(),
                "step {step} op {op}: caret past the block end ({off} of {})",
                p.len()
            );
        }
        if changed {
            assert!(
                    p.is_char_boundary(off),
                    "step {step}: a changed document left the caret inside a character ({off} of {p:?})"
                );
        }
    }
    // The end state still emits and reparses.
    let out = md::emit(&e.doc);
    let _ = md::parse(&out);
}

/// A trailing space typed at a paragraph end must survive the emit → parse
/// round trip. CommonMark strips trailing whitespace, so a raw space is lost
/// and every offset after it shifts — the "type a space, switch views, the
/// caret is wrong" report.
#[test]
fn trailing_space_survives_emit_round_trip() {
    let mut e = ed("abc");
    at(&mut e, 0, 3);
    e.apply(Cmd::InsertText(" ".into()));
    let out = md::emit(&e.doc);
    let back = md::parse(&out);
    assert_eq!(
        back.blocks[0].plain_text(),
        "abc ",
        "trailing space lost on reparse; emitted {out:?}"
    );
}

/// Copy must preserve Markdown structure (inline markup / tables / formulas)
/// instead of flattening the selection into plain text.
#[test]
fn selected_markdown_keeps_structure() {
    // A partial block re-emits the inline tree: `**` and `` ` `` survive.
    // Plain offsets ignore the markup: "bold" is 0..4 of "bold and `code`".
    let mut e = ed("**bold** and `code`");
    e.sel = Sel {
        anchor: Pos::new(0, 0),
        focus: Pos::new(0, 4),
    };
    assert_eq!(e.selected_markdown(), "**bold**");

    // A whole table keeps its rows and cells.
    let mut e = ed("| a | b |\n|---|---|\n| 1 | 2 |");
    let n = e.doc.blocks.len();
    let last_len = e.doc.blocks[n - 1].plain_text().len();
    e.sel = Sel {
        anchor: Pos::new(0, 0),
        focus: Pos::new(n - 1, last_len),
    };
    let md = e.selected_markdown();
    assert!(md.contains("| a | b |"), "header row missing: {md}");
    assert!(md.contains("| 1 | 2 |"), "body row missing: {md}");

    // A display formula stays a `$$` block.
    let mut e = ed("$$\nx+y\n$$");
    let len = e.doc.blocks[0].plain_text().len();
    e.sel = Sel {
        anchor: Pos::new(0, 0),
        focus: Pos::new(0, len),
    };
    let md = e.selected_markdown();
    assert!(
        md.contains("x+y") && md.contains("$$"),
        "formula lost: {md}"
    );
}

// ---- Review regressions: quote shell penetration, invariants ----

/// Selecting backwards over `---` used to delete the whole following
/// paragraph: `Hr` carries no inlines, so `take_inlines`/`set_inlines` are
/// no-ops and the merged tail was silently dropped.
#[test]
fn delete_selection_across_hr_keeps_the_tail() {
    let mut e = ed("text\n\n---\n\nhello");
    assert!(matches!(e.doc.blocks[1].core(), Block::Hr));
    let last = e.doc.blocks.len() - 1;
    e.sel = Sel {
        anchor: Pos::new(1, 0),
        focus: Pos::new(last, 3),
    };
    assert!(e.apply(Cmd::DeleteSelection));
    // The rule is gone, the first paragraph survives, and the tail of "hello"
    // lands in the block the rule used to occupy.
    assert_eq!(e.doc.blocks.len(), 2, "{:?}", e.doc.blocks);
    assert_eq!(text_at(&e, 0), "text");
    assert_eq!(text_at(&e, 1), "lo");
}

/// `>` quoted code is `Quoted { inner: Code }`; the multi-line paste path
/// matched the bare block, missed the early return, and rebuilt the block as
/// an empty paragraph — the code text evaporated.
#[test]
fn paste_multiline_into_quoted_code_keeps_the_code() {
    let mut e = ed("> ```\n> code\n> ```");
    assert!(
        matches!(e.doc.blocks[0].core(), Block::Code { .. }),
        "{:?}",
        e.doc.blocks
    );
    assert_eq!(e.doc.blocks[0].quote_depth(), 1);
    at(&mut e, 0, 2);
    assert!(e.paste("a\nb"));
    assert!(
        matches!(e.doc.blocks[0].core(), Block::Code { .. }),
        "the code shell must survive: {:?}",
        e.doc.blocks
    );
    assert_eq!(text_at(&e, 0), "coa\nbde");
    assert_eq!(e.doc.blocks[0].quote_depth(), 1);
}

/// A one-column table has no trailing paragraph, so deleting its only column
/// removed every block. `Doc` guarantees at least one block and every caret
/// command indexes `blocks[0]` — the next Backspace panicked.
#[test]
fn delete_col_refuses_to_empty_the_table() {
    let mut e = ed("| a |\n| --- |\n| b |");
    assert!(e.doc.blocks.iter().all(|b| b.is_td()), "{:?}", e.doc.blocks);
    at(&mut e, 0, 0);
    assert!(
        !e.apply(Cmd::Table(TableCmd::DeleteCol)),
        "the last column must be refused"
    );
    assert_eq!(e.doc.blocks.len(), 2);
    // The invariant survives: Backspace still works instead of panicking
    assert!(e.apply(Cmd::Table(TableCmd::DeleteRow)) || !e.doc.blocks.is_empty());
}

/// Turning a quoted paragraph into a heading replaced the whole block, so the
/// `>` prefix was gone after one open+save. The shell must be rebuilt around
/// the new core.
#[test]
fn set_head_inside_a_quote_keeps_the_shell() {
    let mut e = ed("> hello");
    assert_eq!(e.doc.blocks[0].quote_depth(), 1);
    at(&mut e, 0, 0);
    assert!(e.apply(Cmd::SetHead(2)));
    assert_eq!(e.doc.blocks[0].quote_depth(), 1, "{:?}", e.doc.blocks);
    assert!(matches!(
        e.doc.blocks[0].core(),
        Block::Head { level: 2, .. }
    ));
    let out = md::emit(&e.doc);
    assert!(out.starts_with("> ## hello"), "{out:?}");
}

/// Soft break inside a quoted code block fell into the inline fallback, where
/// take_inlines/set_inlines are no-ops: nothing changed, yet the caret jumped
/// and the command reported an edit.
#[test]
fn soft_break_inside_quoted_code_inserts_a_newline() {
    let mut e = ed("> ```\n> code\n> ```");
    assert!(matches!(e.doc.blocks[0].core(), Block::Code { .. }));
    at(&mut e, 0, 2);
    assert!(e.apply(Cmd::SoftBreak));
    assert_eq!(text_at(&e, 0), "co\nde");
    assert_eq!(caret(&e), (0, 3));
}

/// Delete inside an Html block took the inline path (a no-op for Html): the
/// text stayed and the command still reported a change.
#[test]
fn delete_selection_inside_an_html_block_edits_the_source() {
    let mut e = Editor::new(Doc::new(vec![Block::Html {
        text: "abcdef".into(),
    }]));
    e.sel = Sel {
        anchor: Pos::new(0, 1),
        focus: Pos::new(0, 4),
    };
    assert!(e.apply(Cmd::DeleteSelection));
    assert_eq!(text_at(&e, 0), "aef");
}

/// Undo coalescing only looked at the clock, so typing in two different places
/// within 500 ms shared one snapshot and a single Ctrl+Z removed both.
#[test]
fn undo_does_not_merge_typing_across_a_caret_move() {
    let mut e = ed("");
    at(&mut e, 0, 0);
    assert!(e.apply(Cmd::InsertText("abc".into())));
    assert_eq!(caret(&e), (0, 3));
    at(&mut e, 0, 0);
    assert!(e.apply(Cmd::InsertText("x".into())));
    assert_eq!(text_at(&e, 0), "xabc");
    assert!(e.undo());
    assert_eq!(text_at(&e, 0), "abc", "only the second edit is undone");
}

/// A no-op command must not wipe the redo stack (it used to clear it in
/// push_undo before discovering the command changed nothing).
#[test]
fn noop_command_keeps_the_redo_stack() {
    let mut e = Editor::new(Doc::new(vec![Block::Code {
        lang: None,
        text: "abc".into(),
    }]));
    at(&mut e, 0, 0);
    assert!(e.apply(Cmd::InsertText("x".into())));
    assert_eq!(text_at(&e, 0), "xabc");
    assert!(e.undo());
    assert_eq!(text_at(&e, 0), "abc");
    // SetHead on a Code block is refused (no document change): the redo stack
    // must survive it, otherwise Ctrl+Y is gone for good.
    assert!(!e.apply(Cmd::SetHead(1)));
    assert!(e.redo());
    assert_eq!(text_at(&e, 0), "xabc");
}

/// An atom (formula source / image alt) is one indivisible word: the class run
/// used to walk straight through it into the surrounding text.
#[test]
fn word_motion_treats_an_atom_as_one_word() {
    // "abc" + Math("x+y") + "d"  →  plain "abcx+yd", atom 3..6
    let mut e = Editor::new(Doc::new(vec![Block::Para(vec![
        Inline::Text("abc".into()),
        Inline::Math {
            src: "x+y".into(),
            display: false,
        },
        Inline::Text("d".into()),
    ])]));
    // Ctrl+Left at the atom's right edge stops at its left edge, not in "abc"
    at(&mut e, 0, 6);
    e.apply(Cmd::Move(Move::WordLeft));
    assert_eq!(caret(&e), (0, 3), "the atom is one word");
    // and again from there into the text before it
    e.apply(Cmd::Move(Move::WordLeft));
    assert_eq!(caret(&e), (0, 0));
    // Ctrl+Right from the block start stops at the atom's left edge
    at(&mut e, 0, 0);
    e.apply(Cmd::Move(Move::WordRight));
    assert_eq!(caret(&e), (0, 3), "text run ends where the atom begins");
    // from the atom's left edge it jumps over the atom
    e.apply(Cmd::Move(Move::WordRight));
    assert_eq!(caret(&e), (0, 6), "the atom is skipped whole");
}

/// A vertical move into a CJK cell clamps the column onto a character boundary
/// of the *target* text: the source cell's offset is a boundary there, not here.
#[test]
fn table_vertical_move_lands_on_a_char_boundary() {
    let doc = Doc::new(vec![
        Block::Td {
            table_seq: 0,
            row: 0,
            align: 0,
            inlines: vec![Inline::Text("abcd".into())],
        },
        Block::Td {
            table_seq: 0,
            row: 1,
            align: 0,
            inlines: vec![Inline::Text("\u{4e2d}\u{4e2d}".into())],
        },
    ]);
    // off 1 in "abcd" is a boundary; in "\u{4e2d}\u{4e2d}" it is inside the first character
    let to = crate::edit::table::table_cell_vertical(&doc, Pos::new(0, 1), false)
        .expect("there is a cell below");
    assert_eq!(to, Pos::new(1, 0), "snapped down to a character boundary");
}

// ---------------------------------------------------------------------------
// Review regressions: DeleteForward's block-end merge
// ---------------------------------------------------------------------------

/// Delete on a rule removes the rule itself, never the block behind it: the
/// generic merge used to be a no-op for `Hr` and then removed the next block
/// together with its text (one keypress ate a paragraph).
#[test]
fn delete_forward_on_hr_removes_the_rule_not_the_text() {
    let mut e = Editor::new(Doc::new(vec![
        Block::Hr,
        Block::Para(vec![Inline::Text("abc".into())]),
    ]));
    at(&mut e, 0, 0);
    assert!(e.apply(Cmd::DeleteForward));
    assert_eq!(e.doc.blocks.len(), 1);
    assert_eq!(text_at(&e, 0), "abc");
    assert_eq!(md::emit(&e.doc), "abc\n");
}

/// Delete at the end of a paragraph that sits before a table joins nothing: a
/// table cell keeps its shell, otherwise the first cell was deleted and its text
/// moved into the paragraph (the saved table lost its whole header row).
#[test]
fn delete_forward_does_not_merge_a_table_cell() {
    let mut e = Editor::new(Doc::new(vec![
        Block::Para(vec![Inline::Text("para".into())]),
        Block::Td {
            table_seq: 0,
            row: 0,
            align: 0,
            inlines: vec![Inline::Text("cell".into())],
        },
        Block::Td {
            table_seq: 0,
            row: 1,
            align: 0,
            inlines: vec![Inline::Text("b".into())],
        },
    ]));
    at(&mut e, 0, 4);
    assert!(!e.apply(Cmd::DeleteForward), "nothing may change");
    assert_eq!(e.doc.blocks.len(), 3, "{:?}", e.doc.blocks);
    assert_eq!(text_at(&e, 1), "cell");
    assert_eq!(caret(&e), (1, 0), "the caret parks at the cell start");
}

/// The same mirror rule inside the table: Delete at the end of a cell used to
/// merge the next cell into it, deleting a cell and leaving the rows ragged.
#[test]
fn delete_forward_inside_a_table_never_removes_a_cell() {
    let mut e = Editor::new(Doc::new(vec![
        Block::Td {
            table_seq: 0,
            row: 0,
            align: 0,
            inlines: vec![Inline::Text("a".into())],
        },
        Block::Td {
            table_seq: 0,
            row: 1,
            align: 0,
            inlines: vec![Inline::Text("b".into())],
        },
    ]));
    at(&mut e, 0, 1);
    assert!(!e.apply(Cmd::DeleteForward));
    assert_eq!(e.doc.blocks.len(), 2, "{:?}", e.doc.blocks);
    assert_eq!(text_at(&e, 0), "a");
    assert_eq!(text_at(&e, 1), "b");
    assert_eq!(caret(&e), (1, 0));
}

/// Delete at the end of a code block joins the next block as a **new source
/// line**. Appending it straight onto the last line ("fn a() {}next") changed the
/// code's meaning; delete_back already joins with a newline the other way round.
#[test]
fn delete_forward_at_code_end_joins_on_a_new_line() {
    let mut e = Editor::new(Doc::new(vec![
        Block::Code {
            lang: Some("rs".into()),
            text: "fn a() {}".into(),
        },
        Block::Para(vec![Inline::Text("next".into())]),
    ]));
    at(&mut e, 0, "fn a() {}".len());
    assert!(e.apply(Cmd::DeleteForward));
    assert_eq!(e.doc.blocks.len(), 1);
    assert!(
        matches!(&e.doc.blocks[0], Block::Code { lang, text }
                if lang.as_deref() == Some("rs") && text == "fn a() {}\nnext"),
        "the line boundary survives: {:?}",
        e.doc.blocks[0]
    );
    assert_eq!(
        caret(&e),
        (0, "fn a() {}".len()),
        "the caret stays at the join"
    );
}

// ---------------------------------------------------------------------------
// Review regressions: atoms (formula source / image alt) are indivisible
// ---------------------------------------------------------------------------

/// Typing with the caret "inside" an image alt must not split the image: it used
/// to become **two** images with half an alt each (`![I](u)X![MG](u)`).
#[test]
fn typing_inside_an_image_alt_does_not_duplicate_the_image() {
    let mut e = Editor::new(Doc::new(vec![Block::Para(vec![
        Inline::Text("a".into()),
        Inline::Image {
            url: "u.png".into(),
            alt: "IMG".into(),
        },
    ])]));
    at(&mut e, 0, 2);
    assert!(e.apply(Cmd::InsertText("X".into())));
    let Block::Para(inl) = &e.doc.blocks[0] else {
        panic!()
    };
    assert_eq!(
        inl.iter()
            .filter(|i| matches!(i, Inline::Image { .. }))
            .count(),
        1,
        "one image in, one image out: {inl:?}"
    );
    assert_eq!(text_at(&e, 0), "aXIMG");
    assert_eq!(md::emit(&e.doc), "aX![IMG](u.png)\n");
}

/// Typing with the caret "inside" a formula's source keeps the formula whole (it
/// used to split into `$x+$Z$y$`).
#[test]
fn typing_inside_a_math_source_keeps_the_formula_whole() {
    let mut e = Editor::new(Doc::new(vec![Block::Para(vec![Inline::Math {
        src: "x+y".into(),
        display: false,
    }])]));
    at(&mut e, 0, 2);
    assert!(e.apply(Cmd::InsertText("Z".into())));
    let Block::Para(inl) = &e.doc.blocks[0] else {
        panic!()
    };
    assert_eq!(
        inl.iter()
            .filter(|i| matches!(i, Inline::Math { .. }))
            .count(),
        1,
        "one formula in, one formula out: {inl:?}"
    );
    assert!(matches!(&inl[1], Inline::Math { src, .. } if src == "x+y"));
    assert_eq!(text_at(&e, 0), "Zx+y");
}

/// Enter with the caret "inside" an atom splits at its front edge: the atom moves
/// into the new block whole (it used to be cut in half across the two blocks).
#[test]
fn enter_inside_an_atom_moves_it_whole() {
    let mut e = Editor::new(Doc::new(vec![Block::Para(vec![Inline::Image {
        url: "u".into(),
        alt: "IMG".into(),
    }])]));
    at(&mut e, 0, 2);
    assert!(e.apply(Cmd::Enter));
    assert_eq!(e.doc.blocks.len(), 2);
    assert!(text_at(&e, 0).is_empty());
    let Block::Para(inl) = &e.doc.blocks[1] else {
        panic!()
    };
    assert_eq!(inl.len(), 1, "one image, not two halves: {inl:?}");
    assert!(matches!(&inl[0], Inline::Image { alt, .. } if alt == "IMG"));
}

/// ↑/↓ between table cells lands on a representable caret: the source cell's byte
/// offset can fall inside the target's formula, and the next keystroke used to
/// split it in two.
#[test]
fn table_vertical_move_never_lands_inside_an_atom() {
    let doc = Doc::new(vec![
        Block::Td {
            table_seq: 0,
            row: 0,
            align: 0,
            inlines: vec![Inline::Text("abcd".into())],
        },
        Block::Td {
            table_seq: 0,
            row: 1,
            align: 0,
            inlines: vec![Inline::Math {
                src: "x+y".into(),
                display: false,
            }],
        },
    ]);
    let to = crate::edit::table::table_cell_vertical(&doc, Pos::new(0, 2), false)
        .expect("there is a cell below");
    assert_eq!(to, Pos::new(1, 0), "snapped to the atom's front edge");
}

// ---------------------------------------------------------------------------
// Review regressions: undo/redo and arrow keys with a live selection
// ---------------------------------------------------------------------------

/// A pure caret move must not invalidate the redo chain: after Ctrl+Z one arrow
/// key used to grey out Redo (`apply` cleared it for every command that reported
/// "changed", which includes selection updates).
#[test]
fn redo_survives_a_pure_caret_move() {
    let mut e = ed("hello");
    at(&mut e, 0, 5);
    assert!(e.apply(Cmd::InsertText("X".into())));
    assert!(e.undo());
    assert_eq!(text_at(&e, 0), "hello");
    // Move the caret around, change the selection, and select all.
    e.apply(Cmd::Move(Move::Left));
    e.apply(Cmd::MoveExtend(Move::Left));
    e.apply(Cmd::SelectAll);
    e.apply(Cmd::SelectWord);
    assert!(
        e.redo(),
        "the redo chain must survive pure caret/selection commands"
    );
    assert_eq!(text_at(&e, 0), "helloX");
}

/// ← / → (and Ctrl+← / Ctrl+→) with a live selection collapse it to the matching
/// edge instead of stepping on from the focus: a backwards drag used to lose the
/// selection start and land one character before the focus.
#[test]
fn arrow_with_a_selection_collapses_to_its_edge() {
    let sel = || Sel {
        anchor: Pos::new(0, 1),
        focus: Pos::new(0, 4),
    };
    let mut e = ed("abcdef");
    e.sel = sel();
    assert!(e.apply(Cmd::Move(Move::Left)));
    assert_eq!(
        e.sel,
        Sel::caret(Pos::new(0, 1)),
        "← collapses to the start"
    );
    assert!(e.apply(Cmd::Move(Move::Left)));
    assert_eq!(caret(&e), (0, 0), "the next ← steps one character back");

    let mut e = ed("abcdef");
    e.sel = sel();
    assert!(e.apply(Cmd::Move(Move::Right)));
    assert_eq!(e.sel, Sel::caret(Pos::new(0, 4)), "→ collapses to the end");
    assert!(e.apply(Cmd::Move(Move::Right)));
    assert_eq!(caret(&e), (0, 5));

    let mut e = ed("abcdef");
    e.sel = sel();
    e.apply(Cmd::Move(Move::WordLeft));
    assert_eq!(caret(&e), (0, 1));

    let mut e = ed("abcdef");
    e.sel = sel();
    e.apply(Cmd::Move(Move::WordRight));
    assert_eq!(caret(&e), (0, 4));

    // Shift+movement still extends instead of collapsing.
    let mut e = ed("abcdef");
    e.sel = sel();
    e.apply(Cmd::MoveExtend(Move::Right));
    assert_eq!(
        e.sel,
        Sel {
            anchor: Pos::new(0, 1),
            focus: Pos::new(0, 5)
        }
    );
}

/// The merge decision looks through a quote shell: a quoted code block
/// (`> ```…``` ` = `Quoted { inner: Code }`) used to fall into the generic merge,
/// which dissolved the fence into paragraph text (`para\ncode`).
#[test]
fn delete_forward_through_a_quote_shell_keeps_the_code_fence() {
    let mut e = Editor::new(Doc::new(vec![
        Block::Para(vec![Inline::Text("para".into())]),
        wrap_quote(
            1,
            Block::Code {
                lang: Some("rs".into()),
                text: "code".into(),
            },
        ),
    ]));
    at(&mut e, 0, 4);
    assert!(e.apply(Cmd::DeleteForward));
    assert_eq!(e.doc.blocks.len(), 1);
    assert!(
        matches!(&e.doc.blocks[0], Block::Code { lang, text }
                if lang.as_deref() == Some("rs") && text == "para\ncode"),
        "the code fence survives: {:?}",
        e.doc.blocks[0]
    );
}

/// Ctrl+click open target: the model lookup at the placed caret finds the
/// covering `Link` span's url (the DOM hit-test's replacement — see
/// `Editor::link_at_pos`).
#[test]
fn link_at_pos_finds_the_covering_link() {
    let e = ed("[alpha](https://a) and [beta](https://b) tail\n");
    // The label spans in plain text: "alpha" at 0..5, "beta" at 10..14.
    assert_eq!(
        e.link_at_pos(Pos::new(0, 2)),
        Some("https://a".into()),
        "inside the first label"
    );
    assert_eq!(
        e.link_at_pos(Pos::new(0, 5)),
        Some("https://a".into()),
        "the label's last byte is inclusive"
    );
    assert_eq!(e.link_at_pos(Pos::new(0, 7)), None, "the gap between links");
    assert_eq!(
        e.link_at_pos(Pos::new(0, 12)),
        Some("https://b".into()),
        "inside the second label"
    );
    assert_eq!(
        e.link_at_pos(Pos::new(0, 0)),
        Some("https://a".into()),
        "the `[` itself is the span's first byte"
    );
    // A block without links: None.
    let plain = ed("just text\n");
    assert_eq!(plain.link_at_pos(Pos::new(0, 3)), None);
}

// ---------------------------------------------------------------------------
// Review regressions: table shells, motion at the document end, redo
// ---------------------------------------------------------------------------

fn td(seq: u64, row: usize, text: &str) -> Block {
    Block::Td {
        table_seq: seq,
        row,
        align: 0,
        inlines: vec![Inline::Text(text.into())],
    }
}

/// Backspace at the start of a paragraph after a table must not merge the
/// paragraph into the previous cell (the mirror of delete_forward's table
/// guard); the caret parks at the cell's end instead.
#[test]
fn delete_back_does_not_merge_a_paragraph_into_a_table_cell() {
    let mut e = Editor::new(Doc::new(vec![
        td(0, 0, "a"),
        td(0, 1, "b"),
        Block::Para(vec![Inline::Text("x".into())]),
    ]));
    at(&mut e, 2, 0);
    assert!(!e.apply(Cmd::DeleteBack), "nothing may change");
    assert_eq!(e.doc.blocks.len(), 3, "{:?}", e.doc.blocks);
    assert_eq!(text_at(&e, 1), "b", "the cell keeps its text");
    assert_eq!(text_at(&e, 2), "x", "the paragraph survives");
    assert_eq!(caret(&e), (1, 1), "the caret parks at the cell end");
}

/// A cell-to-cell Tab is pure movement: it must not clear the redo history.
#[test]
fn table_tab_between_cells_keeps_redo() {
    let mut e = Editor::new(Doc::new(vec![td(0, 0, "a"), td(0, 0, "b")]));
    at(&mut e, 0, 0);
    assert!(e.apply(Cmd::InsertText("X".into())));
    assert_eq!(text_at(&e, 0), "Xa");
    assert!(e.undo());
    assert_eq!(text_at(&e, 0), "a");
    // Move to the second cell with Tab: no document change.
    assert!(e.apply(Cmd::Table(TableCmd::CellNext)));
    assert!(e.redo(), "Tab between cells must preserve the redo stack");
    assert_eq!(text_at(&e, 0), "Xa");
}

/// Row-appending Tab still counts as an edit and invalidates redo.
#[test]
fn table_tab_appending_a_row_clears_redo() {
    let mut e = Editor::new(Doc::new(vec![td(0, 0, "a")]));
    at(&mut e, 0, 0);
    assert!(e.apply(Cmd::InsertText("X".into())));
    assert!(e.undo());
    assert!(
        e.apply(Cmd::Table(TableCmd::CellNext)),
        "last cell appends a row"
    );
    assert!(!e.redo(), "appending a row is a document edit");
}

/// Right / Ctrl+Right at the end of the last block must stay put instead of
/// wrapping the caret back to that block's start.
#[test]
fn horizontal_motion_stops_at_the_document_end() {
    let mut e = ed("abc");
    at(&mut e, 0, 3);
    assert!(!e.apply(Cmd::Move(Move::Right)));
    assert_eq!(caret(&e), (0, 3));
    assert!(!e.apply(Cmd::Move(Move::WordRight)));
    assert_eq!(caret(&e), (0, 3));
    // The same holds for the block-motion fallback (Ctrl+Down).
    assert!(!e.apply(Cmd::Move(Move::NextBlockStart)));
    assert_eq!(caret(&e), (0, 3));
    // Ctrl+Up from block 0 must not jump to the end of the same block.
    at(&mut e, 0, 1);
    assert!(!e.apply(Cmd::Move(Move::PrevBlockEnd)));
    assert_eq!(caret(&e), (0, 1));
}

/// Enter inside a table cell wrapped in a quote shell inserts `<br>`, exactly
/// like a top-level cell (this used to be a silent no-op).
#[test]
fn enter_in_a_quoted_table_cell_inserts_a_break() {
    let mut e = Editor::new(Doc::new(vec![Block::Quoted {
        depth: 1,
        inner: Box::new(td(0, 0, "ab")),
    }]));
    at(&mut e, 0, 1);
    assert!(e.apply(Cmd::Enter));
    assert_eq!(text_at(&e, 0), "a\nb");
    assert_eq!(caret(&e), (0, 2));
}
