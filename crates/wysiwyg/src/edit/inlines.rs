//! Inline-tree kernel: byte-range slicing, insertion and wrapping of the
//! nested inline tree.
//!
//! `Pos.off` is a byte offset into a block's **plain** text, so every operation
//! here has to walk the tree in the same order `Inline::push_plain` emits, and
//! every offset has to land on a char boundary (see `floor_boundary` /
//! `ceil_boundary` in `model`).

use super::*;

impl Editor {
    /// Plain-text interval → the covered interval of each inline. Recursively
    /// collects the nodes inside [range); partially overlapping containers split.
    ///
    /// **Every slice here is boundary-clamped** ([`clamp_span`]): callers pass
    /// byte ranges that originate from untrusted offsets (a click, a stale
    /// caret, an IME commit), and slicing a CJK/emoji character in half used to
    /// panic the whole editor.
    pub(crate) fn collect_range(
        inl: &[Inline],
        range: &Range<usize>,
        base: usize,
        out: &mut Vec<Inline>,
    ) {
        let mut cur = base;
        for node in inl {
            let len = node.plain().len();
            let (s, e) = (cur, cur + len);
            if e <= range.start || s >= range.end {
                cur = e;
                continue;
            }
            if s >= range.start && e <= range.end {
                // Fully covered
                out.push(node.clone());
            } else {
                // Partially overlapping: descend into the container and take the overlapping part
                let sub = range.start.max(s)..range.end.min(e);
                match node {
                    // Line-break nodes are 1 byte: the offset is always on a character boundary, so
                    // they can never be cut in half (defensively: keep the whole node, nothing is
                    // lost)
                    Inline::SoftBreak | Inline::HardBreak => out.push(node.clone()),
                    Inline::Text(t) => {
                        let (a, b) = clamp_span(t, sub.start - s, sub.end - s);
                        if a < b {
                            out.push(Inline::Text(t[a..b].to_string()));
                        }
                    }
                    Inline::Code(t) => {
                        let (a, b) = clamp_span(t, sub.start - s, sub.end - s);
                        if a < b {
                            out.push(Inline::Code(t[a..b].to_string()));
                        }
                    }
                    Inline::Math { src, display } => {
                        let (a, b) = clamp_span(src, sub.start - s, sub.end - s);
                        if a < b {
                            out.push(Inline::Math {
                                src: src[a..b].to_string(),
                                display: *display,
                            });
                        }
                    }
                    Inline::Image { url, alt } => {
                        // alt takes part in plain-text measurement, so the slice must keep the
                        // substring (otherwise the offsets drift)
                        let (a, b) = clamp_span(alt, sub.start - s, sub.end - s);
                        out.push(Inline::Image {
                            url: url.clone(),
                            alt: alt[a..b].to_string(),
                        });
                    }
                    Inline::Strong(c)
                    | Inline::Em(c)
                    | Inline::Del(c)
                    | Inline::Link { url: _, inlines: c } => {
                        let mut part = Vec::new();
                        Self::collect_range(c, &sub, s, &mut part);
                        match node {
                            Inline::Strong(_) => out.push(Inline::Strong(part)),
                            Inline::Em(_) => out.push(Inline::Em(part)),
                            Inline::Del(_) => out.push(Inline::Del(part)),
                            Inline::Link { url, .. } => out.push(Inline::Link {
                                url: url.clone(),
                                inlines: part,
                            }),
                            _ => unreachable!(),
                        }
                    }
                }
            }
            cur = e;
        }
    }

    /// Delete the plain-text range [range). Modifies in place.
    pub(crate) fn inlines_delete(inl: &mut Vec<Inline>, range: &Range<usize>) {
        Self::inlines_delete_at(inl, range, 0);
        compact(inl);
    }

    /// Recursively delete [range) in the [base, base+len) coordinate system. Same
    /// offset measurement as collect_range, but **different deletion semantics**: a
    /// partial delete of an atomic node leaves no shell behind — halving a Math
    /// would emit a truncated `$$…$$` (so delete it whole); after a Link is cut in
    /// the middle the URL would hang off the remaining tail (the remaining text is
    /// unlinked instead: structure kept, shell removed).
    pub(crate) fn inlines_delete_at(inl: &mut Vec<Inline>, range: &Range<usize>, base: usize) {
        let mut new: Vec<Inline> = Vec::new();
        let mut cur = base;
        for node in inl.drain(..) {
            let len = node.plain().len();
            let (s, e) = (cur, cur + len);
            if e <= range.start || s >= range.end {
                new.push(node);
            } else if s >= range.start && e <= range.end {
                // The whole node lies inside the delete range: drop it
            } else {
                match node {
                    // Math/Image are atomic: a partial delete becomes a whole delete (half an `src`
                    // is a leftover shell, and an image node with half an alt is equally
                    // meaningless — on a real machine a backspace turned `![example]` into
                    // `![xample]`, silently rewriting what the image meant).
                    Inline::Math { .. } | Inline::Image { .. } => {}
                    // Link partially deleted → unlink: the remaining text is kept, the URL is dropped.
                    // The range is widened to whole characters **in the link's own plain text**
                    // first, so the recursive delete cannot keep half of a character.
                    Inline::Link { inlines: mut c, .. } => {
                        let plain = b_plain(&c);
                        let (a, b) = clamp_span(
                            &plain,
                            range.start.saturating_sub(s),
                            range.end.saturating_sub(s),
                        );
                        Self::inlines_delete_at(&mut c, &(s + a..s + b), s);
                        new.extend(c);
                    }
                    // Container: recurse into it and keep the shell (what remains after deleting the
                    // middle is still bold)
                    Inline::Strong(mut c) => {
                        Self::inlines_delete_at(&mut c, range, s);
                        if !c.is_empty() {
                            new.push(Inline::Strong(c));
                        }
                    }
                    Inline::Em(mut c) => {
                        Self::inlines_delete_at(&mut c, range, s);
                        if !c.is_empty() {
                            new.push(Inline::Em(c));
                        }
                    }
                    Inline::Del(mut c) => {
                        Self::inlines_delete_at(&mut c, range, s);
                        if !c.is_empty() {
                            new.push(Inline::Del(c));
                        }
                    }
                    // Line-break nodes are 1 byte: they can never be cut by a range (the offset is
                    // always on a character boundary); defensively keep the whole node.
                    Inline::SoftBreak | Inline::HardBreak => new.push(node),
                    // Text/Code: delete the span widened to whole characters (a selection
                    // ending inside a character takes that character with it), then glue
                    // head and tail back into **one node** — two side-by-side halves would
                    // split one code span into two.
                    node @ (Inline::Text(_) | Inline::Code(_)) => {
                        let (text, is_code) = match &node {
                            Inline::Text(t) => (t.as_str(), false),
                            Inline::Code(t) => (t.as_str(), true),
                            _ => unreachable!(),
                        };
                        let (a, b) = clamp_span(
                            text,
                            range.start.saturating_sub(s),
                            range.end.saturating_sub(s),
                        );
                        let mut kept = String::with_capacity(text.len().saturating_sub(b - a));
                        kept.push_str(&text[..a]);
                        kept.push_str(&text[b..]);
                        if !kept.is_empty() {
                            new.push(if is_code {
                                Inline::Code(kept)
                            } else {
                                Inline::Text(kept)
                            });
                        }
                    }
                }
            }
            cur = e;
        }
        *inl = new;
    }

    /// Insert text at plain-text offset `off`. When the offset falls inside
    /// Code/Math/Image, those atomic nodes are not broken — the text goes next to
    /// them.
    pub(crate) fn inlines_insert(inl: &mut Vec<Inline>, off: usize, text: &str) {
        // Math/Image carry no addressable caret position (geom draws such a caret
        // at the atom's front edge), so an interior offset is normalized there
        // first: splitting here produced two half formulas (`$x+$Z$y$`) and, worse,
        // **two images** from one (`![I](u)X![MG](u)`) — one open+save cycle then
        // had an extra image.
        let off = atom_floor(inl, off);
        let mut cur = 0usize;
        let mut idx_split: Option<(usize, usize)> = None; // (node index, offset inside the node)
        for (i, node) in inl.iter().enumerate() {
            let len = node.plain().len();
            // Empty-container descent: `wrap_selection` with an empty selection inserts a
            // zero-length container placeholder (the caret stays where it is), and the next
            // character must land **inside** the container (typing between the `**` pair
            // yields `**X**`). A zero-length node is unreachable at a plain-text offset
            // (`off < cur` is never true), so it needs its own case.
            if len == 0 && hosts_text(node) && off == cur {
                idx_split = Some((i, 0));
                break;
            }
            if off < cur + len {
                idx_split = Some((i, off - cur));
                break;
            }
            cur += len;
        }
        match idx_split {
            None => {
                // Inserted at the end
                if let Some(Inline::Text(t)) = inl.last_mut() {
                    t.push_str(text);
                } else {
                    inl.push(Inline::Text(text.to_string()));
                }
            }
            Some((i, local)) => match &mut inl[i] {
                Inline::Text(t) => {
                    // `local` derives from an untrusted plain offset: floor it so a
                    // mid-character offset cannot make insert_str panic.
                    let local = floor_boundary(t, local);
                    t.insert_str(local, text);
                }
                // Zero-length inline-code placeholder (an empty selection counts as "inline
                // code"): typing goes straight into the code text; the atomic branch would
                // demote it to bare Text (losing the code span's meaning).
                Inline::Code(t) if t.is_empty() => t.push_str(text),
                // Nested container: recurse to insert inside it (keeping the formatting
                // continuous — typing inside bold still yields bold, matching browser behavior)
                Inline::Strong(_) | Inline::Em(_) | Inline::Del(_) | Inline::Link { .. } => {
                    let children: &mut Vec<Inline> = match &mut inl[i] {
                        Inline::Strong(c) | Inline::Em(c) | Inline::Del(c) => c,
                        Inline::Link { inlines: c, .. } => c,
                        _ => unreachable!(),
                    };
                    Self::inlines_insert(children, local, text);
                    compact(children);
                }
                // Atomic node: split into [before, Text(new), after]
                node => {
                    let orig = node.clone();
                    let total = orig.plain().len();
                    let mut before = Vec::new();
                    let mut after = Vec::new();
                    Self::collect_range(std::slice::from_ref(&orig), &(0..local), 0, &mut before);
                    Self::collect_range(&[orig], &(local..total), 0, &mut after);
                    let mut repl: Vec<Inline> = Vec::new();
                    repl.extend(before);
                    repl.push(Inline::Text(text.to_string()));
                    repl.extend(after);
                    let _ = inl.remove(i);
                    for (k, r) in (i..).zip(repl) {
                        inl.insert(k, r);
                    }
                }
            },
        }
        compact(inl);
    }

    /// Wrap a plain-text range in a container (built by `mk`). Nodes fully covered
    /// by the range are left as they are; partially overlapping containers split.
    /// Returns whether anything changed.
    pub(crate) fn inlines_wrap(
        inl: &mut Vec<Inline>,
        range: &Range<usize>,
        mk: &dyn Fn(Vec<Inline>) -> Inline,
    ) -> bool {
        // Whole atoms only: a boundary inside a formula/image would cut it into two
        // truncated nodes (`**$x+$**$y$` is no longer math, `![a](u)` + `![bc](u)`
        // no longer one image). The range is widened to cover the atoms it touches.
        let range = &(atom_floor(inl, range.start)..atom_ceil(inl, range.end));
        // Collect: nodes before the range | the range's content | nodes after the range
        let mut before: Vec<Inline> = Vec::new();
        let mut inside: Vec<Inline> = Vec::new();
        let mut after: Vec<Inline> = Vec::new();
        let mut cur = 0usize;
        let mut touched = false;
        for node in inl.drain(..) {
            let len = node.plain().len();
            let (s, e) = (cur, cur + len);
            if e <= range.start || s >= range.end {
                if cur < range.start {
                    before.push(node);
                } else {
                    after.push(node);
                }
            } else if s >= range.start && e <= range.end {
                inside.push(node);
                touched = true;
            } else {
                // Split
                touched = true;
                let mut head = Vec::new();
                let mut mid = Vec::new();
                let mut tail = Vec::new();
                Self::collect_range(std::slice::from_ref(&node), &(s..range.start), s, &mut head);
                Self::collect_range(
                    std::slice::from_ref(&node),
                    &(range.start.max(s)..range.end.min(e)),
                    s,
                    &mut mid,
                );
                Self::collect_range(&[node], &(range.end.min(e)..e), s, &mut tail);
                before.extend(head);
                inside.extend(mid);
                after.extend(tail);
            }
            cur = e;
        }
        if !touched && inside.is_empty() {
            *inl = before;
            inl.extend(after);
            return false;
        }
        let wrapped = mk(inside);
        before.push(wrapped);
        before.extend(after);
        *inl = before;
        compact(inl);
        true
    }
}

// ----------------------------------------------------------------------
// Free functions and helpers
// ----------------------------------------------------------------------

// ------------------------------------------------------------------
// Command implementations
// ------------------------------------------------------------------

pub(crate) fn take_inlines(b: &mut Block) -> Vec<Inline> {
    match b {
        Block::Para(i) | Block::Quote(i) => std::mem::take(i),
        Block::Head { inlines, .. } | Block::Li { inlines, .. } | Block::Td { inlines, .. } => {
            std::mem::take(inlines)
        }
        // Editing penetrates the quote shell: the shell is kept, the inner block changes
        Block::Quoted { inner, .. } => take_inlines(inner),
        _ => Vec::new(),
    }
}

pub(crate) fn set_inlines(b: &mut Block, inl: Vec<Inline>) {
    match b {
        Block::Para(i) | Block::Quote(i) => *i = inl,
        Block::Head { inlines, .. } | Block::Li { inlines, .. } | Block::Td { inlines, .. } => {
            *inlines = inl
        }
        Block::Quoted { inner, .. } => set_inlines(inner, inl),
        _ => {}
    }
}

pub(crate) fn b_plain(inl: &[Inline]) -> String {
    let mut s = String::new();
    for i in inl {
        i.push_plain(&mut s);
    }
    s
}

/// Structured block merge: `src`'s inline content joins the tail of `dst` as a
/// whole. **Does not go through plain_text** — flattening would evaporate
/// Strong/Link/Image into bare text. Code/Html are plain-text shells and join as
/// source text; Hr cannot carry content, so it is ignored. Quote-shell
/// penetration: the shell is kept and the content joins the inner block's tail.
pub(crate) fn extend_block_inlines(dst: &mut Block, src: &Block) {
    let src_inl = block_src_inlines(src);
    match dst {
        Block::Para(i) | Block::Quote(i) => i.extend(src_inl),
        Block::Head { inlines, .. } | Block::Li { inlines, .. } | Block::Td { inlines, .. } => {
            inlines.extend(src_inl)
        }
        Block::Code { text, .. } | Block::Html { text } => {
            for i in &src_inl {
                i.push_plain(text);
            }
        }
        Block::Hr => {}
        Block::Quoted { inner, .. } => extend_block_inlines(inner, src),
    }
}

/// A snapshot of a block's inline content (through the quote shell): Code/Html
/// fold to Text, Hr is empty.
pub(crate) fn block_src_inlines(src: &Block) -> Vec<Inline> {
    match src {
        Block::Hr => vec![],
        Block::Code { text, .. } | Block::Html { text } => vec![Inline::Text(text.clone())],
        Block::Para(i) | Block::Quote(i) => i.clone(),
        Block::Head { inlines, .. } | Block::Li { inlines, .. } | Block::Td { inlines, .. } => {
            inlines.clone()
        }
        Block::Quoted { inner, .. } => block_src_inlines(inner),
    }
}

/// Normalization before text enters the model: `\r\n` / `\r` → `\n` (the Windows
/// clipboard is routinely CRLF, the HTML parser normalizes `\r\n` to `\n`, and
/// without normalizing the caret offsets drift gradually); `\u{0}` is replaced
/// with U+FFFD (the render layer does the same, 1 byte vs 3 bytes → drift).
pub(crate) fn normalize_text(s: &str) -> std::borrow::Cow<'_, str> {
    if !s.contains('\r') && !s.contains('\0') {
        return std::borrow::Cow::Borrowed(s);
    }
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                out.push('\n');
            }
            '\0' => out.push('\u{FFFD}'),
            _ => out.push(c),
        }
    }
    std::borrow::Cow::Owned(out)
}

pub(crate) fn is_plain_empty(inl: &[Inline]) -> bool {
    b_plain(inl).trim().is_empty()
}

/// Split the inline tree by plain-text offset.
pub(crate) fn split_inlines(inl: &[Inline], off: usize) -> (Vec<Inline>, Vec<Inline>) {
    // An offset inside an atom splits at the atom's front edge (the same
    // normalization as insertion): Enter with the caret "inside" an image alt
    // used to produce two images with half an alt each.
    let off = atom_floor(inl, off);
    let mut head = Vec::new();
    let mut tail = Vec::new();
    Editor::collect_range(inl, &(0..off), 0, &mut head);
    Editor::collect_range(inl, &(off..inl_total(inl)), 0, &mut tail);
    (head, tail)
}

/// Whether this inline node can carry later input **inside** itself (the descent
/// target of an empty-container placeholder). Math/Image are atomic (no internal
/// text accounting) and Soft/HardBreak carry no content.
pub(crate) fn hosts_text(n: &Inline) -> bool {
    matches!(
        n,
        Inline::Strong(_) | Inline::Em(_) | Inline::Del(_) | Inline::Link { .. } | Inline::Code(_)
    )
}

/// A zero-length placeholder node produced by an empty-selection wrap (press B to
/// get a `**` pair with the caret centered).
pub(crate) fn empty_wrapper(kind: WrapKind) -> Inline {
    match kind {
        WrapKind::Bold => Inline::Strong(Vec::new()),
        WrapKind::Italic => Inline::Em(Vec::new()),
        WrapKind::Del => Inline::Del(Vec::new()),
        WrapKind::Code => Inline::Code(String::new()),
    }
}

/// Insert a soft-break node at plain-text offset `off`. The split reuses
/// [`split_inlines`] — the same offset measurement as range slicing (Text is cut
/// in half, atomic nodes split, containers recurse) — and the break becomes its
/// own node, **not merged into an adjacent Text** (plain counts `\n` as one byte).
pub(crate) fn inlines_insert_break(inl: &mut Vec<Inline>, off: usize) {
    inlines_insert_break_node(inl, off, Inline::SoftBreak);
}

/// Same as [`inlines_insert_break`], but the break node can be chosen (Enter
/// inside a cell uses a HardBreak, which emit turns into `<br>`).
pub(crate) fn inlines_insert_break_node(inl: &mut Vec<Inline>, off: usize, node: Inline) {
    let (mut head, tail) = split_inlines(inl, off);
    head.push(node);
    head.extend(tail);
    *inl = head;
}

pub(crate) fn sub_inlines(inl: &[Inline], range: &Range<usize>) -> Vec<Inline> {
    let mut out = Vec::new();
    Editor::collect_range(inl, range, 0, &mut out);
    out
}

pub(crate) fn inl_total(inl: &[Inline]) -> usize {
    inl.iter().map(|i| i.plain().len()).sum()
}

/// Walk the atoms (formula sources / image alts) of an inline tree with their
/// plain byte ranges, in `push_plain` order. No allocation, no cloning.
fn walk_atoms(inl: &[Inline], base: usize, f: &mut impl FnMut(usize, usize)) {
    let mut cur = base;
    for i in inl {
        let len = i.plain_len();
        match i {
            Inline::Math { .. } | Inline::Image { .. } => f(cur, cur + len),
            Inline::Strong(c)
            | Inline::Em(c)
            | Inline::Del(c)
            | Inline::Link { inlines: c, .. } => walk_atoms(c, cur, f),
            _ => {}
        }
        cur += len;
    }
}

/// Snap a plain-text offset **out of** an atom: an offset strictly inside a
/// formula's source / an image's alt is not a caret position the model can
/// represent (geom's `plain_to_layout` draws such a caret at the atom's front
/// edge). Every consumer of an untrusted offset (a click, a stale caret, a table
/// vertical move, an IME commit) goes through here, so a half-covered atom is
/// never cut into two nodes.
///
/// The atom's own edges are left alone: `off == start` (before it) and
/// `off == end` (after it) are legitimate caret positions.
pub(crate) fn atom_floor(inl: &[Inline], off: usize) -> usize {
    let mut out = off;
    walk_atoms(inl, 0, &mut |s, e| {
        if off > s && off < e {
            out = s;
        }
    });
    out
}

/// Widen an offset out of an atom **upwards** (to its trailing edge). Used where
/// a range has to keep whole atoms: a wrap boundary inside a formula would leave
/// a truncated `$…$` on one side.
fn atom_ceil(inl: &[Inline], off: usize) -> usize {
    let mut out = off;
    walk_atoms(inl, 0, &mut |s, e| {
        if off > s && off < e {
            out = e;
        }
    });
    out
}

/// Block-level [`atom_floor`] (penetrates the quote shell; Code/Html carry no
/// inline tree and are left alone).
pub(crate) fn atom_floor_block(b: &Block, off: usize) -> usize {
    match b {
        Block::Para(i) | Block::Quote(i) => atom_floor(i, off),
        Block::Head { inlines, .. } | Block::Li { inlines, .. } | Block::Td { inlines, .. } => {
            atom_floor(inlines, off)
        }
        Block::Quoted { inner, .. } => atom_floor_block(inner, off),
        Block::Code { .. } | Block::Html { .. } | Block::Hr => off,
    }
}

/// Byte range of a block's **atomic nodes** (formula/image) in plain-text
/// coordinates. Caret movement skips them whole: a Math's src and an Image's alt
/// are virtual bytes — visually they are one object, and stopping inside leaves the
/// caret nowhere to draw (browsers treat them as atomic too).
pub(crate) fn atom_bounds(b: &Block) -> Vec<(usize, usize)> {
    pub(crate) fn walk(inl: &[Inline], acc: &mut usize, out: &mut Vec<(usize, usize)>) {
        for i in inl {
            let len = i.plain().len();
            match i {
                Inline::Math { .. } | Inline::Image { .. } => {
                    out.push((*acc, *acc + len));
                }
                Inline::Strong(c) | Inline::Em(c) | Inline::Del(c) => walk(c, acc, out),
                Inline::Link { inlines, .. } => walk(inlines, acc, out),
                _ => {}
            }
            *acc += len;
        }
    }
    let src = block_src_inlines(b);
    let mut acc = 0usize;
    let mut out = Vec::new();
    walk(&src, &mut acc, &mut out);
    out
}

pub(crate) fn merge_inlines(b: Block, tail: Vec<Inline>) -> Block {
    let mut b = b;
    if tail.is_empty() {
        return b;
    }
    match &mut b {
        Block::Para(i) | Block::Quote(i) => {
            i.extend(tail);
        }
        Block::Head { inlines, .. } | Block::Li { inlines, .. } | Block::Td { inlines, .. } => {
            inlines.extend(tail);
        }
        Block::Code { text, .. } | Block::Html { text } => {
            // Source-text block: the tail's plain text joins the source
            let mut t = String::new();
            for i in &tail {
                i.push_plain(&mut t);
            }
            text.push_str(&t);
        }
        // Hr cannot carry text — the caller (paste) pushes a Para to receive it when the
        // last block is an Hr, so this should not be reached.
        Block::Hr => {}
        // Quote-shell penetration: the shell is kept, the content joins the inner block
        Block::Quoted { inner, .. } => {
            let taken = std::mem::replace(inner, Box::new(Block::Para(vec![])));
            **inner = merge_inlines(*taken, tail);
        }
    }
    b
}

pub(crate) fn head_plain_len(b: &Block) -> usize {
    b.plain_text().len()
}

/// Start of the character strictly before `off`; when `off` sits *inside* a
/// character, return that character's own start.
///
/// The offset is untrusted (click position / stale caret / IME commit): slicing
/// `s[..off]` directly panicked on CJK text, so the mid-character case is
/// snapped instead of stepping over the containing character.
pub(crate) fn char_boundary_before(s: &str, off: usize) -> usize {
    let off = off.min(s.len());
    if !s.is_char_boundary(off) {
        return floor_boundary(s, off);
    }
    s[..off]
        .char_indices()
        .next_back()
        .map(|(i, _)| i)
        .unwrap_or(0)
}

pub(crate) fn prev_boundary(s: &str, off: usize) -> usize {
    let off = floor_boundary(s, off);
    s[..off]
        .char_indices()
        .next_back()
        .map(|(i, _)| i)
        .unwrap_or(0)
}

/// End of the character starting at `off`; when `off` sits *inside* a
/// character, snap up to that character's end (never panic on `s[off..]`).
pub(crate) fn char_boundary_after(s: &str, off: usize) -> usize {
    let off = off.min(s.len());
    if !s.is_char_boundary(off) {
        return ceil_boundary(s, off);
    }
    s[off..]
        .chars()
        .next()
        .map(|c| off + c.len_utf8())
        .unwrap_or(off)
}

/// Clean up empty containers (Strong([]) etc.) and merge adjacent Text of the
/// same kind.
pub(crate) fn compact(inl: &mut Vec<Inline>) {
    let mut out: Vec<Inline> = Vec::new();
    for node in inl.drain(..) {
        match node {
            Inline::Strong(c) if c.is_empty() => {}
            Inline::Em(c) if c.is_empty() => {}
            Inline::Del(c) if c.is_empty() => {}
            Inline::Link { inlines: c, .. } if c.is_empty() => {}
            Inline::Text(t) if t.is_empty() => {}
            Inline::Text(t) => {
                if let Some(Inline::Text(prev)) = out.last_mut() {
                    prev.push_str(&t);
                } else {
                    out.push(Inline::Text(t));
                }
            }
            other => out.push(other),
        }
    }
    *inl = out;
}
