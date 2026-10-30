//! Pure caret motion (char / word). Visual line movement needs layout and is
//! handled upstream by the geom layer.

use super::inlines::*;
use super::*;

/// Pure-logic movement: the previous character boundary (across blocks).
///
/// `edit` is the plain range of the formula whose **source is on screen** (see
/// [`crate::reveal`]): that atom is text the user is editing, so the atom
/// skipping below must not apply to it.
pub(crate) fn move_left(doc: &Doc, p: Pos, edit: Option<(usize, usize)>) -> Pos {
    if p.off > 0 {
        let plain = doc
            .blocks
            .get(p.block)
            .map(|b| b.plain_text())
            .unwrap_or_default();
        let mut off = char_boundary_before(&plain, p.off);
        // Atomic nodes (formula/image) are **one visual object**: the caret cannot stop
        // inside their source/alt virtual bytes. Pressing ← from the atomic's end (or
        // from inside it) jumps straight to the atomic's left edge.
        if let Some(b) = doc.blocks.get(p.block) {
            for (s, e) in atom_bounds(b) {
                if Some((s, e)) == edit {
                    continue; // an editing formula is ordinary text
                }
                if off > s && off <= e {
                    off = s;
                    break;
                }
            }
        }
        return Pos::new(p.block, off);
    }
    if p.block == 0 {
        return p;
    }
    let prev = p.block - 1;
    Pos::new(
        prev,
        doc.blocks.get(prev).map_or(0, |b| b.plain_text().len()),
    )
}

pub(crate) fn move_right(doc: &Doc, p: Pos, edit: Option<(usize, usize)>) -> Pos {
    let plain = doc
        .blocks
        .get(p.block)
        .map(|b| b.plain_text())
        .unwrap_or_default();
    if p.off < plain.len() {
        let mut off = char_boundary_after(&plain, p.off.min(plain.len()));
        // Same as move_left: atomic nodes are skipped whole (pressing → at the atomic's
        // left edge or inside it lands on the atomic's right edge).
        if let Some(b) = doc.blocks.get(p.block) {
            for (s, e) in atom_bounds(b) {
                if Some((s, e)) == edit {
                    continue; // an editing formula is ordinary text
                }
                if off > s && off <= e {
                    off = e;
                    break;
                }
            }
        }
        return Pos::new(p.block, off);
    }
    // Last block: the caret stays at the end (the old `min(len-1)` wrapped it
    // back to the block's start — pressing → at the document end jumped to the
    // beginning of the final line).
    if p.block + 1 >= doc.blocks.len() {
        return p;
    }
    Pos::new(p.block + 1, 0)
}

/// Word movement: find a word boundary inside the block; at the end, jump to the
/// next block.
pub(crate) fn move_word_left(doc: &Doc, p: Pos, edit: Option<(usize, usize)>) -> Pos {
    let plain = doc
        .blocks
        .get(p.block)
        .map(|b| b.plain_text())
        .unwrap_or_default();
    if p.off > 0 {
        let mut i = char_boundary_before(&plain, p.off);
        let mut atoms = doc.blocks.get(p.block).map(atom_bounds).unwrap_or_default();
        // An editing formula is text: no atom jump, no "one indivisible word".
        if let Some(edit) = edit {
            atoms.retain(|a| *a != edit);
        }
        let mut out_of_atom = false;
        for (s, e) in &atoms {
            if i > *s && i <= *e {
                i = *s;
                out_of_atom = true;
                break;
            }
        }
        // An atom (formula source / image alt) is one indivisible word: jumping
        // out of it lands on its left edge and stops. Continuing into the text
        // before it read the source as part of the surrounding word — Ctrl+←
        // from "abc⟨x+y⟩" ran all the way to the block start instead of the
        // formula's left edge.
        if out_of_atom {
            return Pos::new(p.block, i);
        }
        // Skip the whitespace to the left of the caret first (standard behaviour: from a
        // word start, keep jumping to the previous word start)
        while i > 0 && plain[i..].starts_with(char::is_whitespace) {
            i = prev_boundary(&plain, i);
        }
        // A run of same-kind characters
        let cls = char_class(plain[i..].chars().next().unwrap_or(' '));
        while i > 0 {
            let prev = prev_boundary(&plain, i);
            // Never walk into an atom's source: it is a word of its own.
            if atoms.iter().any(|(s, e)| prev >= *s && prev < *e) {
                break;
            }
            let c = plain[prev..].chars().next().unwrap_or(' ');
            if char_class(c) == cls && !c.is_whitespace() {
                i = prev;
            } else {
                break;
            }
        }
        return Pos::new(p.block, i);
    }
    if p.block == 0 {
        return p;
    }
    Pos::new(
        p.block - 1,
        doc.blocks
            .get(p.block - 1)
            .map_or(0, |b| b.plain_text().len()),
    )
}

pub(crate) fn move_word_right(doc: &Doc, p: Pos, edit: Option<(usize, usize)>) -> Pos {
    let plain = doc
        .blocks
        .get(p.block)
        .map(|b| b.plain_text())
        .unwrap_or_default();
    if p.off < plain.len() {
        let mut i = char_boundary_after(&plain, p.off.min(plain.len()));
        let mut atoms = doc.blocks.get(p.block).map(atom_bounds).unwrap_or_default();
        if let Some(edit) = edit {
            atoms.retain(|a| *a != edit);
        }
        // Same as move_word_left: an atom is one word. Leaving it goes to its
        // right edge and stops there — the class run below used to walk straight
        // through the source into the text after it.
        for (s, e) in &atoms {
            if i > *s && i < *e {
                return Pos::new(p.block, *e);
            }
        }
        while i < plain.len() && plain[i..].starts_with(char::is_whitespace) {
            i = char_boundary_after(&plain, i);
        }
        // Skipping whitespace can land on an atom's left edge: the atom *is*
        // the next word, so the step ends at its right edge. Never let the
        // class run below treat its source as ordinary characters.
        for (s, e) in &atoms {
            if i >= *s && i < *e {
                return Pos::new(p.block, *e);
            }
        }
        let cls = char_class(plain[i..].chars().next().unwrap_or(' '));
        while i < plain.len() {
            // Stop at an atom's left edge: the run ended where the atom begins.
            if atoms.iter().any(|(s, _)| i == *s) {
                break;
            }
            let c = plain[i..].chars().next().unwrap_or(' ');
            if char_class(c) == cls && !c.is_whitespace() {
                i = char_boundary_after(&plain, i);
            } else {
                break;
            }
        }
        // Skip whole atomic nodes (never treat a Math source or an image alt as
        // a word). Strictly *inside* only: the left edge is already outside the
        // atom and is where the class run above deliberately stops.
        for (s, e) in &atoms {
            if i > *s && i < *e {
                i = *e;
            }
        }
        return Pos::new(p.block, i);
    }
    // Mirror of `move_word_left`'s block-0 branch: at the last block the
    // caret must stay put instead of wrapping to the block start.
    if p.block + 1 >= doc.blocks.len() {
        return p;
    }
    Pos::new(p.block + 1, 0)
}

pub(crate) fn char_class(c: char) -> u8 {
    if c.is_alphanumeric() {
        if c.is_ascii() {
            1
        } else {
            2
        }
    } else {
        3
    }
}
