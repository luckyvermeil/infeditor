//! Typing-time auto-completion: `# ` becomes a heading, `- ` a list, `$…$` an
//! inline formula, and so on. Driven from `insert_text` only.

use super::inlines::*;
use super::*;

/// Markdown auto-completion: after inserting a character, check the block
/// prefix. Returns (new block, new caret offset).
pub(crate) fn autoformat(b: &Block, plain: &str) -> Option<(Block, usize)> {
    if !matches!(b, Block::Para(_)) {
        return None;
    }
    if let Some(rest) = plain.strip_suffix(' ') {
        // "# "*: after stripping the trailing space it is all # (1..=6 of them) → heading
        let hashes = rest.chars().filter(|&c| c == '#').count();
        if (1..=6).contains(&hashes) && hashes == rest.chars().count() {
            return Some((
                Block::Head {
                    level: hashes as u8,
                    inlines: vec![],
                },
                0,
            ));
        }
        if rest == "-" || rest == "*" {
            return Some((
                Block::Li {
                    ordered: false,
                    num: 0,
                    level: 0,
                    task: None,
                    inlines: vec![],
                },
                0,
            ));
        }
        if rest == ">" {
            return Some((Block::Quote(vec![]), 0));
        }
        // "1. ": a digit + "." (strip_suffix(' ') already ate the space)
        if let Some(dot) = rest.strip_suffix('.') {
            if !dot.is_empty() && dot.chars().all(|c| c.is_ascii_digit()) {
                if let Ok(num) = dot.parse::<u64>() {
                    return Some((
                        Block::Li {
                            ordered: true,
                            num,
                            level: 0,
                            task: None,
                            inlines: vec![],
                        },
                        0,
                    ));
                }
            }
        }
        // "- [ ] " / "- [x] " → task list
        if rest == "- [ ]" {
            return Some((
                Block::Li {
                    ordered: false,
                    num: 0,
                    level: 0,
                    task: Some(false),
                    inlines: vec![],
                },
                0,
            ));
        }
        if rest == "- [x]" {
            return Some((
                Block::Li {
                    ordered: false,
                    num: 0,
                    level: 0,
                    task: Some(true),
                    inlines: vec![],
                },
                0,
            ));
        }
    }
    // Exactly "---" / "***" → HR
    if plain == "---" || plain == "***" {
        return Some((Block::Hr, 0));
    }
    // Fence: the block text is "```" → code block
    if plain == "```" {
        return Some((
            Block::Code {
                lang: None,
                text: String::new(),
            },
            0,
        ));
    }
    None
}

/// Inline math auto-completion (typing `$…$` renders a formula immediately).
///
/// When the character before the caret is `$`, search forwards for an unescaped
/// opening `$` and replace the `$…$` range with [`Inline::Math`]. `$$…$$` takes
/// the display form. Returns the new caret offset (after the formula).
///
/// Only called when `auto=true` (the typing path); the paste path does not
/// trigger it either.
pub(crate) fn autoformat_math(inl: &mut Vec<Inline>, caret_off: usize) -> Option<usize> {
    let plain = b_plain(inl);
    // The character before the caret must be a complete `$` (the character boundary
    // has to be checked in both places: `caret_off-1` may land in the middle of a
    // multi-byte character).
    if caret_off == 0
        || caret_off > plain.len()
        || !plain.is_char_boundary(caret_off)
        || !plain.is_char_boundary(caret_off - 1)
        || &plain[caret_off - 1..caret_off] != "$"
    {
        return None;
    }
    let bytes = plain.as_bytes();
    // A closing delimiter of `$$` → display, otherwise inline. The opener must match
    // the closer's width.
    let display = caret_off >= 2 && bytes[caret_off - 2] == b'$';
    let w = if display { 2 } else { 1 };
    let open = find_open_delim(bytes, caret_off - w, w)?;
    let src = plain[open + w..caret_off - w].to_string();
    if src.is_empty() {
        return None;
    }
    // Range → before / formula / after: the same split as the delete path
    // ([`split_inlines`]); a Strong/Link shell inside the range is split rather than
    // dropped wholesale.
    let (mut before, tail) = split_inlines(inl, open);
    let (_, after) = split_inlines(&tail, caret_off - open);
    let new_off = open + src.len(); // Math.plain = src
    before.push(Inline::Math { src, display });
    before.extend(after);
    *inl = before;
    Some(new_off)
}

/// Search left from the closing delimiter's start `from` for the matching
/// **unescaped** opening delimiter (`w` is 1 or 2). Returns the opening
/// delimiter's starting byte.
///
/// A single `$` candidate **must not be adjacent to a `$` on either side**: in
/// the intermediate state `$$x$` while typing `$$x$$` one character at a time,
/// treating the second `$` as the opener would turn the formula into inline Math
/// before the `$$` is finished (display semantics lost for good). A `$$`
/// candidate requires both bytes to belong to one unescaped `$` pair.
pub(crate) fn find_open_delim(bytes: &[u8], from: usize, w: usize) -> Option<usize> {
    (0..=from.checked_sub(w)?)
        .rev()
        .find(|&i| is_delim_pair(bytes, i, w))
}

/// Whether `i..i+w` is a usable pair of delimiters (unescaped; a single `$` also
/// must not be adjacent to another on either side).
pub(crate) fn is_delim_pair(bytes: &[u8], i: usize, w: usize) -> bool {
    if !(0..w).all(|k| is_unescaped_dollar(bytes, i + k)) {
        return false;
    }
    if w > 1 {
        return true;
    }
    let glued = |j: usize| bytes.get(j).is_some_and(|&b| b == b'$') && !escaped_at(bytes, j);
    !glued(i.wrapping_sub(1)) && !glued(i + 1)
}

/// `i` is a `$` and is not backslash-escaped.
pub(crate) fn is_unescaped_dollar(bytes: &[u8], i: usize) -> bool {
    bytes.get(i) == Some(&b'$') && !escaped_at(bytes, i)
}

/// Whether `i` is backslash-escaped — decided by the **parity** of the
/// consecutive backslashes: `\$` is escaped, while `\\$` is "literal backslash +
/// delimiter" (emit escapes `\` too, and only reparsing restores it).
pub(crate) fn escaped_at(bytes: &[u8], i: usize) -> bool {
    if i == 0 || i > bytes.len() {
        return false;
    }
    bytes[..i].iter().rev().take_while(|&&b| b == b'\\').count() % 2 == 1
}

// Minimal API for geom/ui (the visual-row part of movement lives in the geom layer)
