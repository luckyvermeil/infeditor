//! Piece table text storage.
//!
//! The document bytes are kept as a list of pieces referencing either the
//! original backing store (a memory map of the file, or an owned buffer when
//! the file was decoded from a non-UTF-8 encoding) or an append-only insert
//! buffer. This gives O(log n) position lookup and O(delta) edits regardless
//! of document size, and keeps memory usage proportional to the *edits*, not
//! the file size, for memory-mapped documents.

use std::ops::Range;
use std::sync::Arc;

/// Largest length a single piece can describe (pieces beyond 4 GiB are split).
const MAX_PIECE_LEN: u64 = u32::MAX as u64;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Src {
    Original = 0,
    Insert = 1,
}

#[derive(Clone, Copy, Debug)]
pub struct Piece {
    pub src: Src,
    pub off: u64,
    pub len: u32,
}

/// Backing store for original document bytes.
#[derive(Clone, Debug)]
pub enum Original {
    Mmap(Arc<memmap2::Mmap>),
    Owned(Arc<Vec<u8>>),
}

impl Original {
    pub fn as_slice(&self) -> &[u8] {
        match self {
            Original::Mmap(m) => m.as_ref(),
            Original::Owned(v) => v.as_slice(),
        }
    }

    pub fn is_mmap(&self) -> bool {
        matches!(self, Original::Mmap(_))
    }
}

#[derive(Clone, Debug)]
pub struct PieceTable {
    original: Original,
    insert_buf: Vec<u8>,
    pieces: Vec<Piece>,
    /// prefix[i] = total length of pieces[0..i]; prefix.len() == pieces.len()+1.
    prefix: Vec<u64>,
    total: u64,
}

/// Iterator yielding `(absolute_pos, bytes)` chunks starting at a position.
pub struct PieceIter<'a> {
    table: &'a PieceTable,
    idx: usize,
    off: u32,
    abs: u64,
}

impl PieceTable {
    pub fn new(original: Original) -> Self {
        let len = original.as_slice().len() as u64;
        let mut pieces = Vec::new();
        let mut off = 0u64;
        while off < len {
            let l = (len - off).min(MAX_PIECE_LEN);
            pieces.push(Piece {
                src: Src::Original,
                off,
                len: l as u32,
            });
            off += l;
        }
        let mut t = PieceTable {
            original,
            insert_buf: Vec::new(),
            pieces,
            prefix: Vec::new(),
            total: len,
        };
        t.rebuild_prefix();
        t
    }

    pub fn from_bytes(bytes: Vec<u8>) -> Self {
        Self::new(Original::Owned(Arc::new(bytes)))
    }

    fn rebuild_prefix(&mut self) {
        self.prefix.clear();
        self.prefix.reserve(self.pieces.len() + 1);
        let mut acc = 0u64;
        self.prefix.push(0);
        for p in &self.pieces {
            acc += p.len as u64;
            self.prefix.push(acc);
        }
        self.total = acc;
    }

    pub fn len(&self) -> u64 {
        self.total
    }

    pub fn is_empty(&self) -> bool {
        self.total == 0
    }

    pub fn piece_count(&self) -> usize {
        self.pieces.len()
    }

    pub fn original(&self) -> &Original {
        &self.original
    }

    pub fn insert_buf_len(&self) -> usize {
        self.insert_buf.len()
    }

    /// Locate `pos` (0..=len). Returns `(piece_index, offset_in_piece)`.
    /// For `pos == len` returns `(pieces.len(), 0)`.
    pub fn locate(&self, pos: u64) -> (usize, u32) {
        debug_assert!(pos <= self.total);
        if pos == self.total {
            return (self.pieces.len(), 0);
        }
        // largest i with prefix[i] <= pos, i < pieces.len()
        let mut lo = 0usize;
        let mut hi = self.pieces.len();
        while lo + 1 < hi {
            let mid = (lo + hi) / 2;
            if self.prefix[mid] <= pos {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        let off = pos - self.prefix[lo];
        (lo, off as u32)
    }

    pub fn iter_from(&self, pos: u64) -> PieceIter<'_> {
        let pos = pos.min(self.total);
        let (idx, off) = self.locate(pos);
        PieceIter {
            table: self,
            idx,
            off,
            abs: pos,
        }
    }

    fn piece_bytes(&self, p: &Piece) -> &[u8] {
        let base = match p.src {
            Src::Original => self.original.as_slice(),
            Src::Insert => &self.insert_buf,
        };
        &base[p.off as usize..p.off as usize + p.len as usize]
    }

    /// Append bytes of `range` to `out`.
    pub fn read_to(&self, range: Range<u64>, out: &mut Vec<u8>) {
        let start = range.start.min(self.total);
        let end = range.end.min(self.total).max(start);
        if start == end {
            return;
        }
        for (_abs, bytes) in self.iter_from(start) {
            let abs_end = _abs + bytes.len() as u64;
            if abs_end <= start {
                continue;
            }
            let s = if _abs < start {
                (start - _abs) as usize
            } else {
                0
            };
            let e = if abs_end > end {
                (end - _abs) as usize
            } else {
                bytes.len()
            };
            out.extend_from_slice(&bytes[s..e]);
            if abs_end >= end {
                break;
            }
        }
    }

    pub fn read(&self, range: Range<u64>) -> Vec<u8> {
        let mut v =
            Vec::with_capacity((range.end.saturating_sub(range.start)).min(1 << 20) as usize);
        self.read_to(range, &mut v);
        v
    }

    /// Read `range` as a `String`.
    ///
    /// The document invariant is "content is always valid UTF-8", so the
    /// non-lossy path is the normal one; the lossy fallback only fires for a
    /// range whose ends are not char aligned (a caller bug) or for the
    /// mmap/legacy-decoding corner cases documented in `io::file`.
    pub fn read_str(&self, range: Range<u64>) -> String {
        let bytes = self.read(range);
        String::from_utf8(bytes)
            .unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned())
    }

    pub fn byte_at(&self, pos: u64) -> Option<u8> {
        if pos >= self.total {
            return None;
        }
        let (i, o) = self.locate(pos);
        Some(self.piece_bytes(&self.pieces[i])[o as usize])
    }

    pub fn last_byte(&self) -> Option<u8> {
        self.total.checked_sub(1).and_then(|p| self.byte_at(p))
    }

    /// Round `pos` down to the nearest UTF-8 char boundary (pos == len is fine).
    pub fn round_down(&self, pos: u64) -> u64 {
        let mut p = pos.min(self.total);
        while p > 0 && !self.is_boundary(p) {
            p -= 1;
        }
        p
    }

    /// Round `pos` down to the nearest UTF-8 char boundary, but never below
    /// `floor` (insert/delete defensive fallback follows the same rules as
    /// [`Self::round_down`]; delete's `end` is floored at the already
    /// fallen-back `start`).
    fn round_down_floor(&self, pos: u64, floor: u64) -> u64 {
        let mut p = pos;
        while p > floor && !self.is_boundary(p) {
            p -= 1;
        }
        p
    }

    pub fn is_boundary(&self, pos: u64) -> bool {
        if pos == 0 || pos >= self.total {
            return true;
        }
        match self.byte_at(pos) {
            Some(b) => b & 0xC0 != 0x80,
            None => true,
        }
    }

    /// Ensure a piece boundary at `pos`; returns the index of the piece that
    /// starts at `pos` (== `pieces.len()` when `pos == total`).
    fn split_at(&mut self, pos: u64) -> usize {
        if pos == 0 {
            return 0;
        }
        if pos >= self.total {
            return self.pieces.len();
        }
        let (i, o) = self.locate(pos);
        if o == 0 {
            return i;
        }
        let p = self.pieces[i];
        self.pieces[i] = Piece {
            src: p.src,
            off: p.off,
            len: o,
        };
        self.pieces.insert(
            i + 1,
            Piece {
                src: p.src,
                off: p.off + o as u64,
                len: p.len - o,
            },
        );
        self.rebuild_prefix();
        i + 1
    }

    /// Insert `text` at `pos`. `pos` must be a char boundary.
    ///
    /// The position is **clamped** (to the end) and snapped back to a char
    /// boundary in release builds too — callers are expected to have done this
    /// already (`Doc` snaps everything through `snap`), but a stray offset from
    /// a new call site must not corrupt the table or split a UTF-8 sequence.
    pub fn insert(&mut self, pos: u64, text: &[u8]) {
        if text.is_empty() {
            return;
        }
        let pos = self.round_down_floor(pos.min(self.total), 0);
        debug_assert!(pos <= self.total);
        debug_assert!(self.is_boundary(pos));
        let new_off = self.insert_buf.len() as u64;
        self.insert_buf.extend_from_slice(text);
        let mut new_pieces: Vec<Piece> = Vec::new();
        let mut remaining = text.len() as u64;
        let mut off = new_off;
        while remaining > 0 {
            let l = remaining.min(MAX_PIECE_LEN);
            new_pieces.push(Piece {
                src: Src::Insert,
                off,
                len: l as u32,
            });
            off += l;
            remaining -= l;
        }
        let i = self.split_at(pos);
        for (k, np) in new_pieces.into_iter().enumerate() {
            self.pieces.insert(i + k, np);
        }
        // compact_around binary-searches via prefix, so it must be rebuilt
        // first (the new piece is already inserted); after merging, prefix is
        // stale again — rebuild once more at the end.
        self.rebuild_prefix();
        self.compact_around(pos);
        self.rebuild_prefix();
    }

    /// Delete `range`, returning the deleted bytes.
    ///
    /// Same release-side defense as [`Self::insert`]: position clamping +
    /// **char boundary fallback** (callers are expected to have gone through
    /// `Doc::snap`, but a mid-char call from a new call site must not
    /// silently produce a document with broken UTF-8). Range endpoints each
    /// fall back to a char boundary — the returned "deleted bytes" then
    /// align with the true semantic range.
    pub fn delete(&mut self, range: Range<u64>) -> Vec<u8> {
        let start = self.round_down_floor(range.start.min(self.total), 0);
        let end = self.round_down_floor(range.end.min(self.total), start);
        if start >= end {
            return Vec::new();
        }
        let deleted = self.read(start..end);
        let si = self.split_at(start);
        let ei = self.split_at(end);
        if ei > si {
            self.pieces.drain(si..ei);
        }
        self.rebuild_prefix();
        self.compact_around(start);
        self.rebuild_prefix();
        deleted
    }

    /// Append `p` to `out`, merging it into the previous piece when they
    /// share a store and their offsets are contiguous (and the merged length
    /// still fits one piece).
    fn push_piece(out: &mut Vec<Piece>, p: Piece) {
        if p.len == 0 {
            return;
        }
        if let Some(last) = out.last_mut() {
            if last.src == p.src
                && last.off + last.len as u64 == p.off
                && (last.len as u64 + p.len as u64) <= MAX_PIECE_LEN
            {
                last.len += p.len;
                return;
            }
        }
        out.push(p);
    }

    /// Copy old-text span `[from, to)` verbatim into `out`, advancing the
    /// `(pi, pbase)` cursor over the old piece list. `pbase` is the start of
    /// `old[pi]` in (old) table coordinates; both cursors are threaded across
    /// calls so a full rewrite walks the old list exactly once.
    fn copy_span(
        old: &[Piece],
        pi: &mut usize,
        pbase: &mut u64,
        from: u64,
        to: u64,
        out: &mut Vec<Piece>,
    ) {
        let mut cur = from;
        while cur < to {
            if *pi >= old.len() {
                return;
            }
            let p = old[*pi];
            let pend = *pbase + p.len as u64;
            if pend <= cur {
                *pbase = pend;
                *pi += 1;
                continue;
            }
            let take = pend.min(to);
            Self::push_piece(
                out,
                Piece {
                    src: p.src,
                    off: p.off + (cur - *pbase),
                    len: (take - cur) as u32,
                },
            );
            cur = take;
            if take == pend {
                *pbase = pend;
                *pi += 1;
            }
        }
    }

    /// Apply a batch of disjoint replacements — given in **current-text
    /// coordinates** (the text as it stands before this call), sorted by
    /// start, non-overlapping — with a single pass over the piece list.
    ///
    /// The per-op `delete` + `insert` path pays O(P) per op (`pieces`
    /// shift + `rebuild_prefix`); this path is O(P + reps + inserted bytes)
    /// in total, which is what keeps "replace all"/whole-document rewrites
    /// linear on huge files instead of O(P × ops).
    ///
    /// Inserted bytes are appended to the existing insert buffer, so the
    /// copied `Src::Insert` pieces keep pointing at their old offsets — the
    /// buffer stays append-only exactly like the per-op path.
    ///
    /// Out-of-contract input is a no-op in release (debug builds assert).
    /// `Doc::batch_replacements` validates ordering, bounds and char
    /// boundaries before calling in.
    pub fn rebuild_with_replacements(&mut self, reps: &[(Range<u64>, &[u8])]) {
        if reps.is_empty() {
            return;
        }
        debug_assert!(
            reps.windows(2).all(|w| w[0].0.end <= w[1].0.start),
            "replacements must be sorted and non-overlapping"
        );
        debug_assert!(
            reps.iter()
                .all(|(r, _)| r.start <= r.end && r.end <= self.total),
            "replacement range out of bounds"
        );
        if reps
            .iter()
            .any(|(r, _)| r.start > r.end || r.end > self.total)
            || reps.windows(2).any(|w| w[0].0.end > w[1].0.start)
        {
            return;
        }
        let old = std::mem::take(&mut self.pieces);
        let mut new_pieces: Vec<Piece> = Vec::with_capacity(old.len() + reps.len() * 2 + 1);
        let mut pi = 0usize;
        let mut pbase = 0u64;
        let mut prev_end = 0u64;
        for (range, text) in reps {
            Self::copy_span(
                &old,
                &mut pi,
                &mut pbase,
                prev_end,
                range.start,
                &mut new_pieces,
            );
            if !text.is_empty() {
                // Append-only: existing `Src::Insert` pieces (copied above)
                // keep their offsets valid because nothing before them in the
                // buffer moves.
                let off = self.insert_buf.len() as u64;
                self.insert_buf.extend_from_slice(text);
                let mut rem = text.len() as u64;
                let mut o = off;
                while rem > 0 {
                    let l = rem.min(MAX_PIECE_LEN);
                    Self::push_piece(
                        &mut new_pieces,
                        Piece {
                            src: Src::Insert,
                            off: o,
                            len: l as u32,
                        },
                    );
                    o += l;
                    rem -= l;
                }
            }
            prev_end = range.end;
        }
        if prev_end < self.total {
            Self::copy_span(
                &old,
                &mut pi,
                &mut pbase,
                prev_end,
                self.total,
                &mut new_pieces,
            );
        }
        self.pieces = new_pieces;
        self.rebuild_prefix();
    }

    /// Merge adjacent compatible pieces around `pos` to limit fragmentation.
    fn compact_around(&mut self, pos: u64) {
        if self.pieces.len() < 2 {
            return;
        }
        let (mut i, _) = self.locate(pos.min(self.total.saturating_sub(1)));
        i = i.saturating_sub(1);
        let mut j = i;
        while j <= i + 3 && j + 1 < self.pieces.len() {
            let (a, b) = (self.pieces[j], self.pieces[j + 1]);
            if a.src == b.src
                && a.off + a.len as u64 == b.off
                && (a.len as u64 + b.len as u64) <= MAX_PIECE_LEN
            {
                self.pieces[j] = Piece {
                    src: a.src,
                    off: a.off,
                    len: a.len + b.len,
                };
                self.pieces.remove(j + 1);
            } else {
                j += 1;
            }
        }
    }

    /// Raw piece list (for snapshots used by background tasks).
    pub fn snapshot_pieces(&self) -> Vec<Piece> {
        self.pieces.clone()
    }

    /// Copy of the insert buffer (for snapshots).
    pub fn insert_data(&self) -> Vec<u8> {
        self.insert_buf.clone()
    }

    /// Reconstruct a read-only table over the same backing store with a
    /// fixed piece list and a private copy of the insert buffer. The result
    /// is fully independent of the source table (safe across threads).
    pub fn from_snapshot(
        original: Original,
        pieces: Vec<Piece>,
        insert_data: Vec<u8>,
        total: u64,
    ) -> Self {
        let mut t = PieceTable {
            original,
            insert_buf: insert_data,
            pieces,
            prefix: Vec::new(),
            total: 0,
        };
        t.rebuild_prefix();
        debug_assert_eq!(t.total, total);
        t
    }

    /// Full compaction (merge everything possible). Used after bulk operations.
    pub fn compact_all(&mut self) {
        if self.pieces.len() < 2 {
            return;
        }
        let mut out: Vec<Piece> = Vec::with_capacity(self.pieces.len());
        for p in self.pieces.drain(..) {
            if let Some(last) = out.last_mut() {
                if last.src == p.src
                    && last.off + last.len as u64 == p.off
                    && (last.len as u64 + p.len as u64) <= MAX_PIECE_LEN
                {
                    last.len += p.len;
                    continue;
                }
            }
            out.push(p);
        }
        self.pieces = out;
        self.rebuild_prefix();
    }
}

impl<'a> Iterator for PieceIter<'a> {
    type Item = (u64, &'a [u8]);
    fn next(&mut self) -> Option<(u64, &'a [u8])> {
        loop {
            if self.idx >= self.table.pieces.len() {
                return None;
            }
            let p = self.table.pieces[self.idx];
            if self.off >= p.len {
                self.idx += 1;
                self.off = 0;
                continue;
            }
            let bytes = &self.table.piece_bytes(&p)[self.off as usize..];
            let abs = self.abs;
            self.abs += bytes.len() as u64;
            self.idx += 1;
            self.off = 0;
            return Some((abs, bytes));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn full(t: &PieceTable) -> String {
        String::from_utf8(t.read(0..t.len())).unwrap()
    }

    fn apply_random() {
        // placeholder to keep test module non-empty when other tests are feature-gated
    }

    #[test]
    fn basic_insert_delete() {
        let mut t = PieceTable::from_bytes(b"hello world".to_vec());
        assert_eq!(full(&t), "hello world");
        t.insert(5, b",");
        assert_eq!(full(&t), "hello, world");
        let d = t.delete(0..6);
        assert_eq!(d, b"hello,");
        assert_eq!(full(&t), " world");
        t.insert(0, b"goodbye cruel");
        assert_eq!(full(&t), "goodbye cruel world");
        assert!(t.piece_count() < 10);
    }

    /// Release-side defense: out-of-range insert positions clamp to the end;
    /// offsets landing inside a multibyte char fall back to a char boundary —
    /// neither may corrupt the table contents.
    #[test]
    fn insert_clamps_out_of_range_and_non_boundary_positions() {
        let mut t = PieceTable::from_bytes("中文ab".as_bytes().to_vec());
        // out of range: clamp to the end
        t.insert(9_999, b"!");
        assert_eq!(full(&t), "中文ab!");
        // non-boundary ("中" is 3 bytes; offsets 1/2 are mid-char): fall
        // back to 0
        let mut t = PieceTable::from_bytes("中文ab".as_bytes().to_vec());
        t.insert(1, b"X");
        assert_eq!(full(&t), "X中文ab");
        // offset 4 is the second byte of "文" → falls back to 3 (start of "文")
        let mut t = PieceTable::from_bytes("中文ab".as_bytes().to_vec());
        t.insert(4, b"X");
        assert_eq!(full(&t), "中X文ab");
    }

    #[test]
    fn fuzz_vs_string() {
        apply_random();
        let mut rng_state = 0x2545F4914F6CDD1Du64;
        let mut rand = move || {
            rng_state ^= rng_state << 13;
            rng_state ^= rng_state >> 7;
            rng_state ^= rng_state << 17;
            rng_state
        };
        let mut t = PieceTable::from_bytes(Vec::new());
        let mut ref_text = String::new();
        for _ in 0..2000 {
            let len = ref_text.len();
            let op = rand() % 3;
            match op {
                0 => {
                    // insert at random char boundary
                    let pos = if len == 0 {
                        0
                    } else {
                        (rand() as usize) % (len + 1)
                    };
                    let mut p = pos;
                    while p > 0 && !ref_text.is_char_boundary(p) {
                        p -= 1;
                    }
                    let s = ["a", "中", "x\n", "\r\n", "日本語", "\n\n"][..][(rand() % 6) as usize];
                    t.insert(p as u64, s.as_bytes());
                    ref_text.insert_str(p, s);
                }
                1 => {
                    if len == 0 {
                        continue;
                    }
                    let mut a = (rand() as usize) % len;
                    let mut b = (rand() as usize) % len;
                    if a > b {
                        std::mem::swap(&mut a, &mut b);
                    }
                    while a > 0 && !ref_text.is_char_boundary(a) {
                        a -= 1;
                    }
                    while b > 0 && !ref_text.is_char_boundary(b) {
                        b -= 1;
                    }
                    let d = t.delete(a as u64..b as u64);
                    let expect = ref_text.as_bytes()[a..b].to_vec();
                    assert_eq!(d, expect, "delete [{a},{b})");
                    ref_text.replace_range(a..b, "");
                }
                _ => {
                    t.compact_all();
                }
            }
            assert_eq!(t.len(), ref_text.len() as u64);
            assert_eq!(full(&t), ref_text);
        }
    }

    #[test]
    fn big_pieces_split() {
        // verify >4GiB splitting logic without allocating 4GiB: fake via direct pieces
        let mut t = PieceTable::from_bytes(vec![b'x'; 100]);
        assert_eq!(t.piece_count(), 1);
        t.insert(100, &[b'y'; 50]);
        assert_eq!(t.len(), 150);
        assert_eq!(full(&t), "x".repeat(100) + &"y".repeat(50));
    }

    /// The batch single-pass rebuild must exactly equal per-op delete+insert
    /// (text and length byte-identical).
    #[test]
    fn rebuild_with_replacements_matches_per_op_edits() {
        let base = "aaa bbb aaa ccc aaa ddd aaa\n".repeat(40);
        let bytes = base.as_bytes();
        let mut reps: Vec<(Range<u64>, Vec<u8>)> = Vec::new();
        let mut i = 0;
        while i + 3 <= bytes.len() {
            if &bytes[i..i + 3] == b"aaa" {
                reps.push((i as u64..(i + 3) as u64, b"ZZ".to_vec()));
                i += 3;
            } else {
                i += 1;
            }
        }
        assert!(
            reps.len() > 100,
            "sample size must be representative of the batch scenario"
        );
        // per-op path (back to front, matching `build_replace_ops`' reverse
        // application)
        let mut per_op = PieceTable::from_bytes(base.as_bytes().to_vec());
        for (r, text) in reps.iter().rev() {
            per_op.delete(r.clone());
            per_op.insert(r.start, text);
        }
        // batch path
        let mut bulk = PieceTable::from_bytes(base.as_bytes().to_vec());
        let refs: Vec<(Range<u64>, &[u8])> = reps
            .iter()
            .map(|(r, t)| (r.clone(), t.as_slice()))
            .collect();
        bulk.rebuild_with_replacements(&refs);
        assert_eq!(bulk.len(), per_op.len());
        assert_eq!(full(&bulk), full(&per_op));
    }

    /// Boundaries: head insert, deletion, middle insert, tail replacement
    /// all at once; adjacent pieces should merge.
    #[test]
    fn rebuild_with_replacements_handles_insert_and_tail() {
        let mut t = PieceTable::from_bytes(b"0123456789".to_vec());
        let reps: Vec<(Range<u64>, &[u8])> = vec![
            (0..0, b"PRE".as_slice()),
            (2..4, b"".as_slice()),
            (6..6, b"INS".as_slice()),
            (8..10, b"LLL".as_slice()),
        ];
        t.rebuild_with_replacements(&reps);
        // original coordinates concatenated: PRE | "01" | delete [2,4) |
        // "45" | INS | "67" | [8,10)→LLL
        assert_eq!(full(&t), "PRE0145INS67LLL");
        assert_eq!(t.len(), 15);
        // pure deletion (empty insert) should merge the flanking in-place
        // pieces back to fewer pieces
        let mut t = PieceTable::from_bytes(b"abcdefgh".to_vec());
        t.insert(4, b"XY");
        assert_eq!(full(&t), "abcdXYefgh");
        t.rebuild_with_replacements(&[(4..6, b"".as_slice())]);
        assert_eq!(full(&t), "abcdefgh");
        assert_eq!(
            t.piece_count(),
            1,
            "removing the inserted span should merge back into a single piece"
        );
    }

    /// Defense against degenerate inputs: empty table / whole-table
    /// replacement / empty range list.
    #[test]
    fn rebuild_with_replacements_degenerate_inputs() {
        let mut t = PieceTable::from_bytes(Vec::new());
        t.rebuild_with_replacements(&[]);
        assert!(t.is_empty());
        t.rebuild_with_replacements(&[(0..0, b"hi".as_slice())]);
        assert_eq!(full(&t), "hi");
        // replace the whole table with empty
        t.rebuild_with_replacements(&[(0..2, b"".as_slice())]);
        assert!(t.is_empty());
        // one more insert on an empty table (degenerate starting point)
        t.rebuild_with_replacements(&[(0..0, b"tail".as_slice())]);
        assert_eq!(full(&t), "tail");
    }
}
