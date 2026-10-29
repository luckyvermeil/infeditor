//! Sparse line index over a [`PieceTable`].
//!
//! A checkpoint (byte position + line number) is stored every `CKPT_LINES`
//! lines. `line -> pos` and `pos -> line` resolve by binary searching to the
//! nearest checkpoint and scanning forward at most `CKPT_LINES` lines.
//! Memory cost is ~16 bytes per 1024 lines (~12 MB for a 24 GB / 800 M line
//! file). Edits update the index incrementally without rescanning the whole
//! document; after bulk edits a single rescan from a checkpoint is used.

use super::piece_table::PieceTable;
use memchr::memchr2;

pub const CKPT_LINES: u64 = 1024;

/// Byte granularity of load-progress reports.
///
/// A memory-mapped document is a *single* piece covering the whole file, so
/// reporting per piece would give exactly one tick at the very end — useless
/// for a 1 GB file. The scan therefore checks its own position against this
/// step, which costs one compare per line found (~64 compares per 256 MB file)
/// while still producing a smooth progress bar.
pub const PROGRESS_STEP: u64 = 4 * 1024 * 1024;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Newline {
    #[default]
    Lf,
    CrLf,
    Cr,
}

impl Newline {
    /// **Display name** (status bar / menu / restore manifest) — not the
    /// newline itself!
    ///
    /// Historical lesson: `nl_of` once used this return value directly as
    /// the newline when joining into the document, so every bulk line
    /// operation (uppercase, trim whitespace, ...) replaced the real `\n`
    /// with the literal text `"LF"`. All lines written to the document must
    /// use [`Newline::terminator`]. (`as_str` was renamed `display` — the
    /// name no longer implies "byte content".)
    pub fn display(self) -> &'static str {
        match self {
            Newline::Lf => "LF",
            Newline::CrLf => "CRLF",
            Newline::Cr => "CR",
        }
    }
    /// Display name → `Newline`; `None` when unrecognized (caller decides
    /// the fallback).
    pub fn from_display(s: &str) -> Option<Self> {
        match s {
            "LF" => Some(Newline::Lf),
            "CRLF" => Some(Newline::CrLf),
            "CR" => Some(Newline::Cr),
            _ => None,
        }
    }
    /// The **real** line terminator written into the document.
    pub fn terminator(self) -> &'static str {
        match self {
            Newline::Lf => "\n",
            Newline::CrLf => "\r\n",
            Newline::Cr => "\r",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ckpt {
    pub pos: u64,
    pub line: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LineSpan {
    pub line: u64,
    /// content start (first byte of the line)
    pub start: u64,
    /// content end, excluding the line terminator
    pub end: u64,
    /// start of the next line (== end of terminator); == total_bytes on last line
    pub next: u64,
    /// Terminator **kind code** — 0 = none (EOF), 1 = LF, 2 = CRLF, 3 = CR.
    ///
    /// This is an enum encoded in a byte, *not* a length: CR is code 3 but one
    /// byte long. Use [`Self::next`] (== `end + terminator length`) whenever
    /// the byte extent of the line including its terminator is needed —
    /// `end + term` silently overcounts CR-terminated lines by two bytes.
    pub term: u8,
}

#[derive(Clone, Debug)]
pub struct LineIndex {
    ckpts: Vec<Ckpt>,
    pub total_lines: u64,
    pub total_bytes: u64,
    pub newline: Newline,
    pub ends_with_newline: bool,
    pub mixed_newline: bool,
    lf_count: u64,
    cr_count: u64,
    crlf_count: u64,
    /// Byte watermark of the progressive scan: every checkpoint and every
    /// precise query result lies before this position. `== total_bytes` when
    /// the whole document has been scanned.
    pub scanned_to: u64,
    /// Line breaks counted inside `0..scanned_to` (cumulative).
    scanned_lines: u64,
    /// True once the scan has reached the end of the document. While false,
    /// `total_lines` is an estimate and far queries fall back to byte-domain
    /// arithmetic (see [`Self::avg_line_len`]).
    pub complete: bool,
    /// `build_prefix` stopped with an unresolved `\r` as its last byte (it may
    /// pair with a `\n` just past the watermark — or stand alone). The
    /// continuation scan must be told about it, or that one break is lost and
    /// every line number after the seam shifts by one. Cleared as soon as a
    /// patch has been merged (the first patch resolves it).
    pub pending_cr: bool,
}

#[derive(Default, Clone, Copy)]
struct Counts {
    breaks: u64,
    lf: u64,
    cr: u64,
    crlf: u64,
}

/// Scan `range` counting line breaks; `on_break(line_start_pos)` is invoked
/// for every break with the absolute position of the first byte after the
/// terminator.
fn scan_breaks(
    table: &PieceTable,
    range: std::ops::Range<u64>,
    mut on_break: impl FnMut(u64),
) -> Counts {
    let mut pending = None;
    let mut c = scan_breaks_with(table, range, &mut pending, &mut on_break, &mut |_| {});
    // Preserve the historical behaviour for standalone ranges: a trailing CR
    // counts as a real break (either the range ends at EOF, or the caller's
    // windowed counting has always treated it this way).
    if let Some(pc) = pending {
        c.breaks += 1;
        c.cr += 1;
        on_break(pc + 1);
    }
    c
}

/// A CRLF whose `\r` sits at the very end of the scanned range: the break is
/// *not* counted here; the caller either resolves it against the next byte
/// (progressive scanning) or counts it as a lone CR (EOF).
type PendingCr = Option<u64>;
/// [`scan_breaks`] with a `progress(bytes_done)` hook, called at most every
/// [`PROGRESS_STEP`] bytes (and always at least once for a non-empty range).
///
/// The counter is checked *inside* the break loop, so it advances for the
/// single-piece mmap case too — the common shape for a huge file.
///
/// `pending` carries a lone `\r` across calls: a `\r` at the end of the range
/// is left unresolved in `*pending` (it may pair with an `\n` just past the
/// range). Progressive scans thread it into the next segment; one-shot scans
/// resolve it themselves.
fn scan_breaks_with(
    table: &PieceTable,
    range: std::ops::Range<u64>,
    pending: &mut PendingCr,
    on_break: &mut dyn FnMut(u64),
    progress: &mut dyn FnMut(u64),
) -> Counts {
    let mut c = Counts::default();
    let start = range.start.min(table.len());
    let end = range.end.min(table.len()).max(start);
    let mut next_report = start.saturating_add(PROGRESS_STEP);
    for (abs, bytes) in table.iter_from(start) {
        if abs >= end {
            break;
        }
        let chunk_end = (abs + bytes.len() as u64).min(end);
        let usable = &bytes[..(chunk_end - abs) as usize];
        if usable.is_empty() {
            continue;
        }
        let mut i = 0usize;
        if let Some(pc) = pending.take() {
            if usable[0] == b'\n' {
                c.breaks += 1;
                c.crlf += 1;
                on_break(abs + 1);
                i = 1;
            } else {
                c.breaks += 1;
                c.cr += 1;
                on_break(pc + 1);
            }
        }
        while i < usable.len() {
            let done = abs + i as u64;
            if done >= next_report {
                progress(done);
                next_report = done.saturating_add(PROGRESS_STEP);
            }
            match memchr2(b'\n', b'\r', &usable[i..]) {
                None => break,
                Some(off) => {
                    let q = i + off;
                    if usable[q] == b'\r' {
                        if q + 1 < usable.len() {
                            if usable[q + 1] == b'\n' {
                                c.breaks += 1;
                                c.crlf += 1;
                                on_break(abs + q as u64 + 2);
                                i = q + 2;
                            } else {
                                c.breaks += 1;
                                c.cr += 1;
                                on_break(abs + q as u64 + 1);
                                i = q + 1;
                            }
                        } else {
                            *pending = Some(abs + q as u64);
                            break;
                        }
                    } else {
                        c.breaks += 1;
                        c.lf += 1;
                        on_break(abs + q as u64 + 1);
                        i = q + 1;
                    }
                }
            }
        }
    }
    c
}

fn dominant_newline(lf: u64, cr: u64, crlf: u64) -> (Newline, bool) {
    let kinds = (lf > 0) as u32 + (cr > 0) as u32 + (crlf > 0) as u32;
    if kinds == 0 {
        // No terminators: no decidable kind. Without this early return, the
        // `crlf >= lf && crlf >= cr` tie-break rule would judge all-zero as
        // CRLF, inconsistent with `empty()`'s LF (deleting all content
        // would flip newline from LF to CRLF).
        return (Newline::Lf, false);
    }
    let dominant = if crlf >= lf && crlf >= cr {
        Newline::CrLf
    } else if lf >= cr {
        Newline::Lf
    } else {
        Newline::Cr
    };
    (dominant, kinds > 1)
}

/// How far past the scan watermark a query may walk linearly before falling
/// back to byte-domain estimation (16 MB ≈ a few ms at memory speed).
pub const EST_WALK_LIMIT: u64 = 16 * 1024 * 1024;

/// One slice of a progressive index scan, produced by [`scan_continue`] on a
/// background thread and folded into a [`LineIndex`] via [`LineIndex::merge_patch`].
///
/// `breaks` / `lf` / `cr` / `crlf` are *deltas* since the previous patch;
/// `ckpts` are absolute (byte pos, whole-document line number).
#[derive(Debug, Default, Clone)]
pub struct IndexPatch {
    pub scanned_to: u64,
    pub complete: bool,
    pub ckpts: Vec<Ckpt>,
    pub breaks: u64,
    pub lf: u64,
    pub cr: u64,
    pub crlf: u64,
}

/// Continue a progressive scan from `from` (the previous watermark) to EOF,
/// emitting an [`IndexPatch`] roughly every [`PROGRESS_STEP`] bytes.
///
/// `base_line` is the number of line breaks already counted before `from`, so
/// checkpoint line numbers come out in whole-document coordinates.
///
/// `pending_cr` must be [`LineIndex::pending_cr`] when the continuation starts
/// at that index's watermark: the byte at `from - 1` is then an *uncounted*
/// `\r` that this scan has to resolve (against a following `\n`, or as a lone
/// CR). Passing `false` there loses that one break and shifts every later line
/// number by one — the stale-comment bug this parameter exists to kill.
/// Returns `false` when cancelled (checked once per progress step).
pub fn scan_continue(
    table: &PieceTable,
    from: u64,
    base_line: u64,
    pending_cr: bool,
    mut emit: impl FnMut(IndexPatch),
    cancelled: &std::sync::atomic::AtomicBool,
    mut progress: impl FnMut(u64, u64),
) -> bool {
    use std::sync::atomic::Ordering::Relaxed;

    let total = table.len();
    let start = from.min(total);
    // Resolve the carried-over CR. The byte check guards against a stale flag
    // (an edit may have replaced the CR at the watermark with other text).
    let mut pending: Option<u64> =
        if pending_cr && start > 0 && table.byte_at(start - 1) == Some(b'\r') {
            Some(start - 1)
        } else {
            None
        };
    let mut ckpts: Vec<Ckpt> = Vec::new();
    let mut cur_line = base_line;
    let mut since = 0u64;
    let (mut d_lf, mut d_cr, mut d_crlf, mut d_breaks) = (0u64, 0u64, 0u64, 0u64);
    let mut next_report = start.saturating_add(PROGRESS_STEP);
    // honour a cancellation that was raised before the scan even started:
    // the in-loop check below only fires once per PROGRESS_STEP bytes
    if cancelled.load(Relaxed) {
        return false;
    }

    // a checkpoint becomes possible only every CKPT_LINES lines
    macro_rules! bump {
        ($ls:expr) => {{
            cur_line += 1;
            d_breaks += 1;
            since += 1;
            if since >= CKPT_LINES {
                ckpts.push(Ckpt {
                    pos: $ls,
                    line: cur_line,
                });
                since = 0;
            }
        }};
    }
    macro_rules! flush {
        ($pos:expr, $complete:expr) => {{
            emit(IndexPatch {
                scanned_to: $pos,
                complete: $complete,
                ckpts: std::mem::take(&mut ckpts),
                breaks: d_breaks,
                lf: d_lf,
                cr: d_cr,
                crlf: d_crlf,
            });
            d_lf = 0;
            d_cr = 0;
            d_crlf = 0;
            d_breaks = 0;
        }};
    }

    for (abs, bytes) in table.iter_from(start) {
        let chunk_end = (abs + bytes.len() as u64).min(total);
        let usable = &bytes[..(chunk_end - abs) as usize];
        if usable.is_empty() {
            continue;
        }
        let mut i = 0usize;
        // resolve a lone CR carried over from the previous segment
        if let Some(pc) = pending.take() {
            if usable[0] == b'\n' {
                d_crlf += 1;
                bump!(abs + 1);
                i = 1;
            } else {
                d_cr += 1;
                bump!(pc + 1);
            }
        }
        while i < usable.len() {
            let done = abs + i as u64;
            if done >= next_report {
                flush!(done, false);
                progress(done, total);
                next_report = done.saturating_add(PROGRESS_STEP);
                if cancelled.load(Relaxed) {
                    return false;
                }
            }
            match memchr2(b'\n', b'\r', &usable[i..]) {
                None => break,
                Some(off) => {
                    let q = i + off;
                    if usable[q] == b'\r' {
                        if q + 1 < usable.len() {
                            if usable[q + 1] == b'\n' {
                                d_crlf += 1;
                                bump!(abs + q as u64 + 2);
                                i = q + 2;
                            } else {
                                d_cr += 1;
                                bump!(abs + q as u64 + 1);
                                i = q + 1;
                            }
                        } else {
                            pending = Some(abs + q as u64);
                            break;
                        }
                    } else {
                        d_lf += 1;
                        bump!(abs + q as u64 + 1);
                        i = q + 1;
                    }
                }
            }
        }
    }
    // lone CR at EOF is a real break
    if let Some(pc) = pending {
        d_cr += 1;
        bump!(pc + 1);
    }
    let _ = since; // the final `since = 0` inside bump! is deliberately unread
                   // final patch: emit without resetting the counters (nothing reads them
                   // afterwards, and resetting would trip the "value never read" lint)
    emit(IndexPatch {
        scanned_to: total,
        complete: true,
        ckpts,
        breaks: d_breaks,
        lf: d_lf,
        cr: d_cr,
        crlf: d_crlf,
    });
    progress(total, total);
    true
}

/// True when `pos` sits on a line start: right after a complete terminator
/// (or at the document start). A position between the `\r` and the `\n` of a
/// CRLF is *inside* the terminator, so it does not start a line.
fn is_line_start(table: &PieceTable, pos: u64) -> bool {
    if pos == 0 {
        return true;
    }
    match table.byte_at(pos - 1) {
        Some(b'\n') => true,
        Some(b'\r') => table.byte_at(pos) != Some(b'\n'),
        _ => false,
    }
}

/// Walk backwards from `pos` to the first line start at or after `pos`'s own
/// terminator, using `memrchr` over a bounded window. This is how navigation
/// into the unswept region of a huge file stays fast while the index is
/// still building.
///
/// A `pos` inside the LF half of a CRLF yields the byte after that LF — the
/// start of the next line, not the line `line_of_pos` would report. Only the
/// progressive `pos_of_line` estimate uses this, and there the next line
/// start is the useful answer.
pub fn refine_line_start(table: &PieceTable, pos: u64) -> u64 {
    let pos = pos.min(table.len());
    if pos == 0 {
        return 0;
    }
    let mut window = 64 * 1024u64;
    loop {
        let start = pos.saturating_sub(window);
        let bytes = table.read(start..pos);
        if let Some(off) = memchr::memrchr2(b'\n', b'\r', &bytes) {
            let mut s = start + off as u64 + 1;
            // a position on the LF half of a CRLF starts the *next* line
            if table.byte_at(s) == Some(b'\n') && s > 0 && table.byte_at(s - 1) == Some(b'\r') {
                s += 1;
            }
            return s.min(table.len());
        }
        if start == 0 {
            return 0;
        }
        // pathological: a single line longer than 256 MB — give up and treat
        // everything as one line rather than stalling the UI
        if window >= 256 * 1024 * 1024 {
            return 0;
        }
        window *= 8;
    }
}

impl LineIndex {
    pub fn empty() -> Self {
        LineIndex {
            ckpts: Vec::new(),
            total_lines: 1,
            total_bytes: 0,
            newline: Newline::Lf,
            ends_with_newline: false,
            mixed_newline: false,
            lf_count: 0,
            cr_count: 0,
            crlf_count: 0,
            scanned_to: 0,
            scanned_lines: 0,
            complete: true,
            pending_cr: false,
        }
    }

    /// Full index build (also used for small docs and after bulk edits).
    ///
    /// `progress(done_bytes, total_bytes)` is called periodically during the
    /// scan and once more at the end. Both arguments are byte counts — a
    /// consumer can render `done * 100 / total` directly.
    pub fn build<F: FnMut(u64, u64)>(table: &PieceTable, mut progress: F) -> Self {
        let total = table.len();
        let mut ckpts: Vec<Ckpt> = Vec::new();
        let mut cur_line = 0u64;
        let mut pending: PendingCr = None;
        let mut c = scan_breaks_with(
            table,
            0..total,
            &mut pending,
            &mut |ls| {
                cur_line += 1;
                if cur_line % CKPT_LINES == 0 {
                    ckpts.push(Ckpt {
                        pos: ls,
                        line: cur_line,
                    });
                }
            },
            &mut |done| progress(done.min(total), total),
        );
        // lone CR at EOF is a real break
        if let Some(_pc) = pending {
            c.breaks += 1;
            c.cr += 1;
            cur_line += 1;
        }
        if total > 0 {
            ckpts.insert(0, Ckpt { pos: 0, line: 0 });
        }
        let ends = matches!(table.last_byte(), Some(b'\n') | Some(b'\r'));
        let total_lines = if total == 0 {
            1
        } else if ends {
            cur_line
        } else {
            cur_line + 1
        };
        // terminal tick, so a consumer always sees 100% even when the last
        // intermediate report landed short of `total`
        progress(total, total);
        let (newline, mixed) = dominant_newline(c.lf, c.cr, c.crlf);
        LineIndex {
            ckpts,
            total_lines,
            total_bytes: total,
            newline,
            ends_with_newline: ends,
            mixed_newline: mixed,
            lf_count: c.lf,
            cr_count: c.cr,
            crlf_count: c.crlf,
            scanned_to: total,
            scanned_lines: cur_line,
            complete: true,
            pending_cr: false,
        }
    }

    /// Phase 1 of a progressive build: scan only the first `min(upto, len)`
    /// bytes so a huge document becomes visible in constant time. The rest is
    /// filled in by [`scan_continue`] on a background thread and folded in
    /// through [`Self::merge_patch`].
    pub fn build_prefix<F: FnMut(u64, u64)>(
        table: &PieceTable,
        upto: u64,
        mut progress: F,
    ) -> Self {
        let total = table.len();
        let limit = upto.min(total);
        if limit >= total {
            let ix = Self::build(table, &mut progress);
            return ix;
        }
        let mut ckpts: Vec<Ckpt> = Vec::new();
        let mut cur_line = 0u64;
        let mut pending: PendingCr = None;
        // the trailing CR is deliberately left unresolved: it may pair with an
        // `\n` just past the prefix, and `scan_continue` resolves it there
        let c = scan_breaks_with(
            table,
            0..limit,
            &mut pending,
            &mut |ls| {
                cur_line += 1;
                if cur_line % CKPT_LINES == 0 {
                    ckpts.push(Ckpt {
                        pos: ls,
                        line: cur_line,
                    });
                }
            },
            &mut |done| progress(done.min(total), total),
        );
        debug_assert!(pending.is_none() || limit < total);
        if total > 0 {
            ckpts.insert(0, Ckpt { pos: 0, line: 0 });
        }
        let (newline, mixed) = dominant_newline(c.lf, c.cr, c.crlf);
        let mut ix = LineIndex {
            ckpts,
            total_lines: 1,
            total_bytes: total,
            newline,
            ends_with_newline: matches!(table.last_byte(), Some(b'\n') | Some(b'\r')),
            mixed_newline: mixed,
            lf_count: c.lf,
            cr_count: c.cr,
            crlf_count: c.crlf,
            scanned_to: limit,
            scanned_lines: cur_line,
            complete: false,
            pending_cr: pending.is_some(),
        };
        ix.estimate_total();
        progress(limit, total);
        ix
    }

    /// Average bytes per line (terminator included) observed in the scanned
    /// prefix. Drives every estimate made past `scanned_to`.
    pub fn avg_line_len(&self) -> f64 {
        let seg_lines = (self.scanned_lines + 1).max(1) as f64;
        (self.scanned_to as f64 / seg_lines).max(1.0)
    }

    fn estimate_total(&mut self) {
        if self.complete {
            return;
        }
        let lines_scanned = self.scanned_lines + 1;
        let est = if self.scanned_to > 0 && self.total_bytes > 0 {
            ((self.total_bytes as f64) * lines_scanned as f64 / self.scanned_to as f64).round()
                as u64
        } else {
            1
        };
        self.total_lines = est.max(lines_scanned);
    }

    /// Fold a background [`IndexPatch`] into the index. The table must be the
    /// same text the patch was scanned against (the caller checks `gen`).
    pub fn merge_patch(&mut self, table: &PieceTable, p: IndexPatch) {
        if self.complete {
            return;
        }
        // the first patch resolved the carried-over CR (see `pending_cr`)
        self.pending_cr = false;
        self.ckpts.extend(p.ckpts);
        self.scanned_to = p.scanned_to;
        self.scanned_lines += p.breaks;
        self.lf_count += p.lf;
        self.cr_count += p.cr;
        self.crlf_count += p.crlf;
        self.ends_with_newline = matches!(table.last_byte(), Some(b'\n') | Some(b'\r'));
        if p.complete {
            self.complete = true;
            // Backstop convergence: `apply_edit`'s per-kind deltas drift
            // temporarily when the edit window covers a pending `\r` at the
            // scan watermark (the window comparison counts it, the index has
            // not yet). Recount fully once so every stat equals exactly what
            // a full build would produce.
            let mut pending: PendingCr = None;
            let mut c = scan_breaks_with(
                table,
                0..table.len(),
                &mut pending,
                &mut |_| {},
                &mut |_| {},
            );
            // lone CR at EOF is a real break (same rule as `build`)
            if pending.is_some() {
                c.breaks += 1;
                c.cr += 1;
            }
            self.lf_count = c.lf;
            self.cr_count = c.cr;
            self.crlf_count = c.crlf;
            self.scanned_lines = c.breaks;
            self.scanned_to = self.total_bytes;
            let (newline, mixed) = dominant_newline(self.lf_count, self.cr_count, self.crlf_count);
            self.newline = newline;
            self.mixed_newline = mixed;
            self.total_lines = if self.total_bytes == 0 {
                1
            } else if self.ends_with_newline {
                self.scanned_lines
            } else {
                self.scanned_lines + 1
            };
        } else {
            self.estimate_total();
        }
    }

    pub fn checkpoint_count(&self) -> usize {
        self.ckpts.len()
    }

    pub fn is_built(&self) -> bool {
        self.complete && (self.total_bytes == 0 || !self.ckpts.is_empty())
    }

    /// Largest checkpoint with `line <= line`.
    fn ckpt_for_line(&self, line: u64) -> Ckpt {
        if self.ckpts.is_empty() {
            return Ckpt { pos: 0, line: 0 };
        }
        let idx = self.ckpts.partition_point(|c| c.line <= line);
        if idx == 0 {
            Ckpt { pos: 0, line: 0 }
        } else {
            self.ckpts[idx - 1]
        }
    }

    /// Largest checkpoint with `pos <= pos`.
    fn ckpt_for_pos(&self, pos: u64) -> Ckpt {
        if self.ckpts.is_empty() {
            return Ckpt { pos: 0, line: 0 };
        }
        let idx = self.ckpts.partition_point(|c| c.pos <= pos);
        if idx == 0 {
            Ckpt { pos: 0, line: 0 }
        } else {
            self.ckpts[idx - 1]
        }
    }

    /// Byte position of the start of `line`. `line == total_lines` returns
    /// `total_bytes` (one-past-last-line, for convenience).
    ///
    /// While the index is still building, lines far past the scanned watermark
    /// resolve through byte-domain estimation + a backwards `memrchr` to the
    /// real line start — O(window) instead of O(file), which is what keeps
    /// Ctrl+End and deep scrolling responsive before the scan finishes.
    pub fn pos_of_line(&self, table: &PieceTable, line: u64) -> u64 {
        if line == 0 {
            return 0;
        }
        if line >= self.total_lines {
            return self.total_bytes;
        }
        if !self.complete {
            let est = line as f64 * self.avg_line_len();
            let last_line = self.ckpts.last().map(|c| c.line).unwrap_or(0);
            if line > last_line
                && est > self.scanned_to as f64
                && est - self.scanned_to as f64 > EST_WALK_LIMIT as f64
            {
                return refine_line_start(table, est as u64);
            }
            // otherwise the walk below is bounded by ~EST_WALK_LIMIT + one
            // checkpoint stride, i.e. milliseconds at memory speed
        }
        let ck = self.ckpt_for_line(line);
        let mut w = LineWalker::new_at(table, ck.pos, ck.line);
        let mut cur = ck.line;
        let mut last_next = ck.pos;
        while cur < line {
            match w.next_span(table) {
                Some(ls) => {
                    cur += 1;
                    last_next = ls.next;
                }
                None => break,
            }
        }
        if cur == line {
            last_next.min(self.total_bytes)
        } else {
            self.total_bytes
        }
    }

    /// Line number containing `pos` (0-based). Positions inside a line
    /// terminator belong to that (earlier) line.
    ///
    /// Positions far past the scan watermark return an *estimate* (byte
    /// position / average line length); the caller treats it as approximate
    /// until the background scan reaches the region.
    pub fn line_of_pos(&self, table: &PieceTable, pos: u64) -> u64 {
        let pos = pos.min(table.len());
        if pos == 0 || self.total_lines <= 1 {
            return 0;
        }
        if pos >= self.total_bytes {
            return self.total_lines - 1;
        }
        if !self.complete && pos > self.scanned_to && pos - self.scanned_to > EST_WALK_LIMIT {
            let est = pos as f64 / self.avg_line_len();
            return (est as u64).clamp(self.scanned_lines, self.total_lines.max(1) - 1);
            // near the watermark: the walk below costs at most ~EST_WALK_LIMIT
        }
        let ck = self.ckpt_for_pos(pos);
        if ck.pos == pos {
            return ck.line;
        }
        let mut cur = ck.line;
        let mut walker = LineWalker::new_at(table, ck.pos, ck.line);
        let limit = pos + 1; // include the byte at `pos` so terminators are seen
        loop {
            match walker.next_span_upto(table, limit) {
                Some(ls) => {
                    if ls.next > pos {
                        return cur;
                    }
                    cur += 1;
                }
                None => return cur,
            }
        }
    }

    /// Span of the given line (start..end excludes the terminator).
    pub fn line_span(&self, table: &PieceTable, line: u64) -> LineSpan {
        let start = self.pos_of_line(table, line);
        let mut w = LineWalker::new_at(table, start, line);
        w.next_span(table).unwrap_or(LineSpan {
            line,
            start,
            end: start,
            next: start,
            term: 0,
        })
    }

    /// Apply an edit to the index. The *table must already contain the new
    /// text*. `removed` / `added` count line terminators **by kind** in a
    /// one-byte-padded window around the old / new edited ranges (see
    /// `Doc::apply_ops_raw` for why the padding is required).
    ///
    /// `total_lines`, the checkpoint table and the per-kind counts
    /// (`lf/cr/crlf` + the derived `newline`/`mixed_newline`) all converge
    /// incrementally here — after an edit, `newline_stats()` matches a full
    /// rebuild (see the per-kind assertions in
    /// `incremental_edit_matches_rebuild`).
    #[allow(clippy::too_many_arguments)]
    pub fn apply_edit(
        &mut self,
        table: &PieceTable,
        start: u64,
        old_len: u64,
        new_len: u64,
        removed: BreakCounts,
        added: BreakCounts,
    ) {
        let old_end = start + old_len;
        let delta_pos = new_len as i64 - old_len as i64;
        let delta_lines = added.breaks as i64 - removed.breaks as i64;
        // Line delta inside the scanned region (identical to the global one
        // for a complete index; cross-watermark edits are fixed by the
        // rescan below — the global delta includes breaks past the watermark)
        let mut delta_lines_scanned = delta_lines;
        self.total_bytes = (self.total_bytes as i64 + delta_pos).max(0) as u64;
        // `total_lines` is `breaks + (ends_with_newline ? 0 : 1)` (see
        // `build`), so a raw break delta is only right while the trailing
        // newline flag stays put. Deleting the final LF (or typing one at
        // EOF) flips it and adds/removes the phantom last line without
        // changing any break: book the phantom separately, otherwise the
        // last line loses its span (Pos::of_line returns EOF, so the row
        // stops rendering) until the file is reopened.
        let old_phantom = i64::from(!self.ends_with_newline);
        self.ends_with_newline = matches!(table.last_byte(), Some(b'\n') | Some(b'\r'));
        let new_phantom = i64::from(!self.ends_with_newline);
        self.total_lines =
            (self.total_lines as i64 - old_phantom + delta_lines + new_phantom).max(1) as u64;
        // Per-kind counts: window-comparison deltas (both windows count with
        // the same CRLF rules; the difference is the real change — half-CRLF
        // edits hold per kind too, with fuzz tests asserting equality with a
        // full rebuild item by item). Note: during progressive scanning, if
        // the edit window happens to cover a pending `\r` at the watermark
        // (not yet counted), this drifts temporarily; `merge_patch` re-does
        // a full recount when the scan completes as a backstop.
        self.lf_count = (self.lf_count as i64 + added.lf as i64 - removed.lf as i64).max(0) as u64;
        self.cr_count = (self.cr_count as i64 + added.cr as i64 - removed.cr as i64).max(0) as u64;
        self.crlf_count =
            (self.crlf_count as i64 + added.crlf as i64 - removed.crlf as i64).max(0) as u64;
        let (nl, mixed) = dominant_newline(self.lf_count, self.cr_count, self.crlf_count);
        // All-zero (document emptied): `dominant_newline` returns LF —
        // consistent with `build` on an empty document, so assign
        // unconditionally; the fuzz test asserts post-edit
        // `newline`/`mixed_newline` equal a full rebuild item by item.
        self.newline = nl;
        self.mixed_newline = mixed;

        // Progressive invariant: every checkpoint lies before `scanned_to`.
        // An edit entirely inside the unswept region moves no checkpoint —
        // only the totals above change (and the line estimate is refreshed).
        if !self.complete && start >= self.scanned_to {
            self.estimate_total();
            return;
        }
        if !self.complete {
            // the edit sits inside the scanned region: shift the watermark
            // with the text so the background scan continues at the right byte
            let old_scanned = self.scanned_to;
            let straddle = start < old_scanned && old_end >= old_scanned;
            // A pending `\r` (at old_scanned-1, not yet counted in
            // scanned_lines) touched by the edit window: window comparison
            // treats it as counted → line count drifts by ±1
            let pending_touch = self.pending_cr && old_end + 1 >= old_scanned;
            if straddle || pending_touch {
                // Edit straddles the watermark (or touches a pending `\r`):
                // the window comparison counts breaks past the watermark, so
                // `scanned_lines`/tail ckpt line numbers drift (they only
                // count bytes inside the watermark; merge_patch's completion
                // backstop only fixes counts and ckpt rebuild — during the
                // seam, line numbers/pos_of_line stay wrong). Rescan the
                // scanned prefix exactly — only happens for edits at the
                // seam; the background scan soon pushes the watermark past
                // the edit, amortized O(1).
                let new_scanned = (old_scanned as i64 + delta_pos).max(0) as u64;
                let mut pending: PendingCr = None;
                let c = scan_breaks_with(
                    table,
                    0..new_scanned,
                    &mut pending,
                    &mut |_| {},
                    &mut |_| {},
                );
                self.pending_cr = pending.is_some();
                delta_lines_scanned = c.breaks as i64 - self.scanned_lines as i64;
                self.scanned_to = new_scanned;
                self.scanned_lines = c.breaks;
            } else {
                self.scanned_to = (old_scanned as i64 + delta_pos).max(0) as u64;
                self.scanned_lines = (self.scanned_lines as i64 + delta_lines).max(0) as u64;
            }
        }

        // Keep checkpoints at/before `start` (a line-start at `start` stays
        // valid), drop those strictly inside the old range, shift those at or
        // after `old_end` by the edit delta. Line-number shifting uses the
        // line delta **inside the scanned region** (for cross-watermark edits
        // the global delta includes breaks past the watermark, which would
        // push all tail line numbers off).
        let keep_end = self.ckpts.partition_point(|c| c.pos <= start);
        let first_after_old =
            keep_end + self.ckpts[keep_end..].partition_point(|c| c.pos < old_end);
        let mut tail: Vec<Ckpt> = self.ckpts[first_after_old..]
            .iter()
            .map(|c| Ckpt {
                pos: (c.pos as i64 + delta_pos).max(0) as u64,
                line: (c.line as i64 + delta_lines_scanned).max(0) as u64,
            })
            .collect();
        self.ckpts.truncate(keep_end);

        let base = if keep_end > 0 {
            self.ckpts[keep_end - 1]
        } else if self.total_bytes > 0 {
            self.ckpts.push(Ckpt { pos: 0, line: 0 });
            Ckpt { pos: 0, line: 0 }
        } else {
            Ckpt { pos: 0, line: 0 }
        };
        // Never rescan past the scan watermark: everything beyond it has no
        // checkpoints by design and belongs to the background scan.
        let scan_limit = if self.complete {
            self.total_bytes
        } else {
            self.scanned_to.min(self.total_bytes)
        };
        let until = tail
            .first()
            .map(|c| c.pos)
            .unwrap_or(scan_limit)
            .min(scan_limit);
        if until > base.pos {
            let mut rescan: Vec<Ckpt> = Vec::new();
            let mut cur_line = base.line;
            let mut since = 0u64;
            scan_breaks(table, base.pos..until, |ls| {
                cur_line += 1;
                since += 1;
                if since >= CKPT_LINES && ls < until {
                    rescan.push(Ckpt {
                        pos: ls,
                        line: cur_line,
                    });
                    since = 0;
                }
            });
            self.ckpts.extend(rescan);
        }
        self.ckpts.append(&mut tail);
        // Invariant: checkpoints are real line starts with strictly
        // increasing (pos, line) — `pos_of_line` and `line_of_pos` both
        // binary-search the table and then walk forward from the hit, so a
        // checkpoint that is not a line start silently returns the wrong
        // line (and a duplicate line number makes the debug assert panic).
        // Two shifts can break it: a tail item that sat exactly on `old_end`
        // moves to `start + new_len`, which is a line start only if the edit
        // left a terminator there; and the rescan grid below can emit the
        // same line number as that shifted item, at a slightly different
        // position. Drop the offenders rather than trusting the arithmetic:
        // a missing checkpoint only costs a few extra bytes on the next
        // walk, a wrong one returns wrong text.
        let mut prev: Option<Ckpt> = None;
        self.ckpts.retain(|c| {
            let ok = is_line_start(table, c.pos)
                && prev.is_none_or(|p| p.pos < c.pos && p.line < c.line);
            if ok {
                prev = Some(*c);
            }
            ok
        });
        if !self.complete {
            self.estimate_total();
        }
        debug_assert!(self
            .ckpts
            .windows(2)
            .all(|w| w[0].pos < w[1].pos && w[0].line < w[1].line));
    }

    /// Full rescan from the checkpoint nearest `from_pos` to EOF.
    /// Used after bulk edits where incremental updates are too slow.
    pub fn rebuild_from(&mut self, table: &PieceTable, from_pos: u64) {
        let base = if from_pos == 0 {
            Ckpt { pos: 0, line: 0 }
        } else {
            self.ckpt_for_pos(from_pos)
        };
        let keep = self.ckpts.partition_point(|c| c.pos < base.pos);
        self.ckpts.truncate(keep);
        self.ckpts.push(base);
        let mut new_ck: Vec<Ckpt> = Vec::new();
        let mut cur_line = base.line;
        let mut since = 0u64;
        scan_breaks(table, base.pos..table.len(), |ls| {
            cur_line += 1;
            since += 1;
            if since >= CKPT_LINES {
                new_ck.push(Ckpt {
                    pos: ls,
                    line: cur_line,
                });
                since = 0;
            }
        });
        self.ckpts.extend(new_ck);
        self.total_bytes = table.len();
        let ends = matches!(table.last_byte(), Some(b'\n') | Some(b'\r'));
        self.ends_with_newline = ends;
        // recount terminator stats over the whole table: the incremental
        // bookkeeping in `apply_edit` tracks break deltas but not their kind,
        // so the pre-existing counters can be stale after many edits
        let mut pending: PendingCr = None;
        let mut c = scan_breaks_with(
            table,
            0..table.len(),
            &mut pending,
            &mut |_| {},
            &mut |_| {},
        );
        if pending.is_some() {
            c.breaks += 1;
            c.cr += 1;
        }
        self.lf_count = c.lf;
        self.cr_count = c.cr;
        self.crlf_count = c.crlf;
        let (nl, mixed) = dominant_newline(c.lf, c.cr, c.crlf);
        self.newline = nl;
        self.mixed_newline = mixed;
        self.total_lines = if self.total_bytes == 0 {
            1
        } else if ends {
            c.breaks
        } else {
            c.breaks + 1
        };
        // a full rescan reaches EOF, so the index is complete again — this is
        // also how a progressive index gets finished after a bulk edit
        self.scanned_to = self.total_bytes;
        self.scanned_lines = c.breaks;
        self.complete = true;
        debug_assert!(self
            .ckpts
            .windows(2)
            .all(|w| w[0].pos < w[1].pos && w[0].line < w[1].line));
    }

    /// Iterate the raw checkpoints (for diagnostics).
    pub fn ckpts(&self) -> &[Ckpt] {
        &self.ckpts
    }

    /// Newline style statistics (count of each terminator kind).
    pub fn newline_stats(&self) -> (u64, u64, u64) {
        (self.lf_count, self.cr_count, self.crlf_count)
    }

    /// Line breaks counted inside `0..scanned_to` — the base line number a
    /// continuation scan resumes from.
    pub fn scanned_breaks(&self) -> u64 {
        self.scanned_lines
    }
}

/// Sequential line walker over the piece table.
pub struct LineWalker {
    pos: u64,
    line: u64,
    end: u64,
}

impl LineWalker {
    pub fn new_at(table: &PieceTable, pos: u64, line: u64) -> Self {
        LineWalker {
            pos: pos.min(table.len()),
            line,
            end: table.len(),
        }
    }

    pub fn pos(&self) -> u64 {
        self.pos
    }

    /// Next line that starts before `limit` (a terminator straddling `limit`
    /// yields a line whose `end` is clipped at the limit).
    ///
    /// Both this and [`Self::next_span`] advance `self.line`, so consecutive
    /// spans carry increasing line numbers — `next_span_upto` used to skip
    /// the increment and report the starting line on every span.
    pub fn next_span_upto(&mut self, table: &PieceTable, limit: u64) -> Option<LineSpan> {
        let limit = limit.min(table.len());
        if self.pos >= limit {
            return None;
        }
        let r = self.next_impl(table, limit);
        if r.is_some() {
            self.line += 1;
        }
        r
    }

    /// Next full line.
    pub fn next_span(&mut self, table: &PieceTable) -> Option<LineSpan> {
        self.next_span_upto(table, self.end)
    }

    fn next_impl(&mut self, table: &PieceTable, limit: u64) -> Option<LineSpan> {
        if self.pos >= limit {
            return None;
        }
        let start = self.pos;
        let line = self.line;
        let mut pending_cr: Option<u64> = None;
        for (abs, bytes) in table.iter_from(start) {
            if abs >= limit {
                break;
            }
            let chunk_end = (abs + bytes.len() as u64).min(limit);
            let usable = &bytes[..(chunk_end - abs) as usize];
            if usable.is_empty() {
                continue;
            }
            if let Some(pc) = pending_cr.take() {
                if usable[0] == b'\n' {
                    let span = LineSpan {
                        line,
                        start,
                        end: pc,
                        next: abs + 1,
                        term: 2,
                    };
                    self.pos = abs + 1;
                    return Some(span);
                } else {
                    let span = LineSpan {
                        line,
                        start,
                        end: pc,
                        next: pc + 1,
                        term: 3,
                    };
                    self.pos = pc + 1;
                    return Some(span);
                }
            }
            // One memchr pass per chunk: every hit either ends the line (and
            // returns) or is a `\r` at the chunk boundary (and defers via
            // `pending_cr`), so a second iteration can never happen.
            if let Some(off) = memchr2(b'\n', b'\r', usable) {
                let q = off;
                if usable[q] == b'\r' {
                    if q + 1 < usable.len() {
                        let (next, term) = if usable[q + 1] == b'\n' {
                            (abs + q as u64 + 2, 2)
                        } else {
                            (abs + q as u64 + 1, 3)
                        };
                        let span = LineSpan {
                            line,
                            start,
                            end: abs + q as u64,
                            next,
                            term,
                        };
                        self.pos = next;
                        return Some(span);
                    } else {
                        pending_cr = Some(abs + q as u64);
                    }
                } else {
                    let span = LineSpan {
                        line,
                        start,
                        end: abs + q as u64,
                        next: abs + q as u64 + 1,
                        term: 1,
                    };
                    self.pos = abs + q as u64 + 1;
                    return Some(span);
                }
            }
        }
        if let Some(pc) = pending_cr {
            let span = LineSpan {
                line,
                start,
                end: pc,
                next: pc + 1,
                term: 3,
            };
            self.pos = pc + 1;
            return Some(span);
        }
        if start < limit {
            let span = LineSpan {
                line,
                start,
                end: limit,
                next: limit,
                term: 0,
            };
            self.pos = limit;
            return Some(span);
        }
        None
    }
}

/// Per-kind counts of line terminators within one edit window (`\r\n`
/// counts once as `crlf`, not again as `cr`/`lf`). Window convention: a
/// lone `\r` at the end of the range counts as one `cr`.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct BreakCounts {
    pub breaks: u64,
    pub lf: u64,
    pub cr: u64,
    pub crlf: u64,
}

/// Per-kind counts of line terminators within one edit window (`apply_edit`
/// input).
pub fn count_breaks_by_kind(table: &PieceTable, range: std::ops::Range<u64>) -> BreakCounts {
    let c = scan_breaks(table, range, |_| {});
    BreakCounts {
        breaks: c.breaks,
        lf: c.lf,
        cr: c.cr,
        crlf: c.crlf,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn idx(t: &PieceTable) -> LineIndex {
        LineIndex::build(t, |_, _| {})
    }

    /// The progress hook is the only feedback a user gets while a huge file
    /// loads, so it must be monotonic, expressed in bytes, and end exactly at
    /// `total` — the old code passed a *line* count as the second argument,
    /// which made every consumer compute nonsense (or divide wrongly).
    #[test]
    fn progress_is_monotonic_bytes_ending_at_total() {
        // enough data to cross at least one PROGRESS_STEP window
        let mut body: Vec<u8> = Vec::with_capacity(PROGRESS_STEP as usize + 4096);
        let mut i = 0u32;
        while (body.len() as u64) < PROGRESS_STEP + 1024 {
            body.extend_from_slice(format!("{i:06} | x\n").as_bytes());
            i += 1;
        }
        let total = body.len() as u64;
        let t = PieceTable::from_bytes(body);

        let mut seen: Vec<(u64, u64)> = Vec::new();
        let ix = LineIndex::build(&t, |done, tot| seen.push((done, tot)));

        assert!(!seen.is_empty(), "no progress was reported");
        assert!(
            seen.iter().all(|&(_, tot)| tot == total),
            "total must always be the byte length, saw {seen:?}"
        );
        assert!(
            seen.windows(2).all(|w| w[0].0 <= w[1].0),
            "progress went backwards: {seen:?}"
        );
        assert!(
            seen.iter().all(|&(d, t)| d <= t),
            "done exceeded total: {seen:?}"
        );
        let last = *seen.last().unwrap();
        assert_eq!(last, (total, total), "progress did not finish at 100%");
        assert_eq!(ix.total_bytes, total);
        // at least one intermediate tick, i.e. the scan reports *during* itself
        assert!(seen.len() > 1, "only the terminal tick was reported");
    }

    /// An empty document must still fire exactly one (0, 0) tick and never a
    /// divide-by-zero shape for the UI.
    #[test]
    fn progress_for_empty_document_is_zero() {
        let t = PieceTable::from_bytes(Vec::new());
        let mut seen: Vec<(u64, u64)> = Vec::new();
        LineIndex::build(&t, |done, tot| seen.push((done, tot)));
        assert_eq!(seen, vec![(0, 0)]);
    }

    #[test]
    fn empty_and_single() {
        let t = PieceTable::from_bytes(Vec::new());
        let ix = idx(&t);
        assert_eq!(ix.total_lines, 1);
        assert_eq!(ix.pos_of_line(&t, 0), 0);
        assert_eq!(ix.line_of_pos(&t, 0), 0);

        let t = PieceTable::from_bytes(b"a".to_vec());
        let ix = idx(&t);
        assert_eq!(ix.total_lines, 1);
        assert_eq!(ix.line_span(&t, 0).end, 1);
    }

    #[test]
    fn trailing_newline() {
        let t = PieceTable::from_bytes(b"a\nb\n".to_vec());
        let ix = idx(&t);
        assert_eq!(ix.total_lines, 2);
        assert!(ix.ends_with_newline);
        assert_eq!(ix.newline, Newline::Lf);
        assert_eq!(ix.pos_of_line(&t, 1), 2);
        assert_eq!(ix.pos_of_line(&t, 2), 4); // phantom
        let s = ix.line_span(&t, 1);
        assert_eq!((s.start, s.end, s.term), (2, 3, 1));
    }

    #[test]
    fn crlf() {
        let t = PieceTable::from_bytes(b"aa\r\nbb\r\ncc".to_vec());
        let ix = idx(&t);
        assert_eq!(ix.total_lines, 3);
        assert_eq!(ix.newline, Newline::CrLf);
        assert_eq!(ix.pos_of_line(&t, 1), 4);
        assert_eq!(ix.pos_of_line(&t, 2), 8);
        let s = ix.line_span(&t, 0);
        assert_eq!((s.start, s.end, s.next, s.term), (0, 2, 4, 2));
        // positions inside the CRLF belong to line 0
        assert_eq!(ix.line_of_pos(&t, 2), 0);
        assert_eq!(ix.line_of_pos(&t, 3), 0);
        assert_eq!(ix.line_of_pos(&t, 4), 1);
        assert_eq!(ix.line_of_pos(&t, 8), 2);
    }

    #[test]
    fn lone_cr() {
        let t = PieceTable::from_bytes(b"a\rb\rc".to_vec());
        let ix = idx(&t);
        assert_eq!(ix.total_lines, 3);
        assert_eq!(ix.newline, Newline::Cr);
        assert_eq!(ix.pos_of_line(&t, 1), 2);
        assert_eq!(ix.pos_of_line(&t, 2), 4);
        assert_eq!(ix.line_of_pos(&t, 2), 1);
    }

    #[test]
    fn mixed_newlines() {
        let t = PieceTable::from_bytes(b"a\r\nb\nc\rd".to_vec());
        let ix = idx(&t);
        assert_eq!(ix.total_lines, 4);
        assert!(ix.mixed_newline);
        assert_eq!(ix.pos_of_line(&t, 1), 3);
        assert_eq!(ix.pos_of_line(&t, 2), 5);
        assert_eq!(ix.pos_of_line(&t, 3), 7);
    }

    #[test]
    fn many_lines_checkpoints() {
        let mut s = String::new();
        for i in 0..5000 {
            s.push_str(&format!("line{i}\n"));
        }
        let t = PieceTable::from_bytes(s.clone().into_bytes());
        let ix = idx(&t);
        assert_eq!(ix.total_lines, 5000);
        assert_eq!(ix.checkpoint_count(), 5); // 0,1024,2048,3072,4096
        for l in [0u64, 1, 1023, 1024, 1025, 2500, 4999] {
            let p = ix.pos_of_line(&t, l);
            let expect = s.find(&format!("line{l}\n")).unwrap() as u64;
            assert_eq!(p, expect, "line {l}");
            assert_eq!(ix.line_of_pos(&t, p), l, "line {l}");
        }
    }

    #[test]
    fn incremental_edit_matches_rebuild() {
        let mut rng_state = 0x9E3779B97F4A7C15u64;
        let mut rand = move || {
            rng_state ^= rng_state << 13;
            rng_state ^= rng_state >> 7;
            rng_state ^= rng_state << 17;
            rng_state
        };
        let mut text = String::new();
        for i in 0..3000 {
            text.push_str(&format!("第{i}行 abc def\n"));
        }
        let mut t = PieceTable::from_bytes(text.clone().into_bytes());
        let mut ix = idx(&t);
        for op in 0..300 {
            let len = t.len();
            let mut pos = rand() % len;
            let line = ix.line_of_pos(&t, pos);
            pos = ix.pos_of_line(&t, line);
            let span = ix.line_span(&t, line);
            // line tail including the terminator = span.next (term is a
            // kind code; CR=3 but only 1 byte)
            let old_end = span.next.min(len);
            let old_len = old_end - pos;
            let new_text = ["X", "X\nY", "\n", "多行\n文本\n", ""][..][(rand() % 5) as usize];
            // window-padded counts, same contract as Doc::apply_ops_raw
            let win_start = pos.saturating_sub(1);
            let removed = count_breaks_by_kind(&t, win_start..(old_end + 1).min(len));
            t.delete(pos..old_end);
            t.insert(pos, new_text.as_bytes());
            let added = count_breaks_by_kind(
                &t,
                win_start..(pos + new_text.len() as u64 + 1).min(t.len()),
            );
            ix.apply_edit(&t, pos, old_len, new_text.len() as u64, removed, added);
            let rebuilt = LineIndex::build(&t, |_, _| {});
            assert_eq!(ix.total_lines, rebuilt.total_lines, "op {op}");
            assert_eq!(ix.total_bytes, rebuilt.total_bytes);
            // per-kind counts and the derived newline/mixed must also
            // converge incrementally to a full rebuild
            assert_eq!(ix.newline_stats(), rebuilt.newline_stats(), "op {op}");
            assert_eq!(ix.newline, rebuilt.newline, "op {op}");
            assert_eq!(ix.mixed_newline, rebuilt.mixed_newline, "op {op}");
            for _ in 0..10 {
                let l = rand() % ix.total_lines.max(1);
                let p1 = ix.pos_of_line(&t, l);
                let p2 = rebuilt.pos_of_line(&t, l);
                assert_eq!(p1, p2, "op {op} line {l}");
                let q = rand() % (t.len() + 1);
                assert_eq!(
                    ix.line_of_pos(&t, q),
                    rebuilt.line_of_pos(&t, q),
                    "op {op} pos {q}"
                );
            }
        }
    }

    /// Regression: deleting a span that exactly straddles two checkpoints
    /// left the shifted first tail item at base's position. The old code
    /// kept two same-position checkpoints — debug builds panicked on the
    /// strict increase debug_assert; in release, binary search depended on
    /// order.
    #[test]
    fn deleting_a_checkpoint_span_keeps_ckpts_strictly_increasing() {
        let mut text = String::new();
        for i in 0..6000 {
            text.push_str(&format!("line-{i} 数据\n"));
        }
        let mut t = PieceTable::from_bytes(text.clone().into_bytes());
        let mut ix = idx(&t);
        assert!(ix.ckpts.len() >= 3, "need at least 3 checkpoints");
        // Take two adjacent checkpoints (skipping the pos=0 first one)
        let (p1, p2) = (ix.ckpts[1].pos, ix.ckpts[2].pos);
        let len = t.len();
        let start = p1.saturating_sub(1);
        let removed = count_breaks_by_kind(&t, start..(p2 + 1).min(len));
        t.delete(p1..p2);
        let added = count_breaks_by_kind(&t, start..(p1 + 1).min(t.len()));
        ix.apply_edit(&t, p1, p2 - p1, 0, removed, added);
        let rebuilt = LineIndex::build(&t, |_, _| {});
        assert_eq!(ix.total_lines, rebuilt.total_lines);
        for l in (0..ix.total_lines).step_by(97) {
            assert_eq!(
                ix.pos_of_line(&t, l),
                rebuilt.pos_of_line(&t, l),
                "line {l}"
            );
        }
    }

    // ---- progressive index -------------------------------------------------

    /// Run a prefix scan + background-style continuation and verify the merged
    /// index is indistinguishable from a full build.
    fn progressive_of(text: Vec<u8>, prefix: u64) -> LineIndex {
        let t = PieceTable::from_bytes(text);
        let mut ix = LineIndex::build_prefix(&t, prefix, |_, _| {});
        assert!(!ix.complete, "prefix scan must leave the index incomplete");
        assert_eq!(ix.scanned_to, prefix.min(t.len()));
        let cancelled = std::sync::atomic::AtomicBool::new(false);
        let base_line = ix.scanned_breaks();
        let from = ix.scanned_to;
        let pending_cr = ix.pending_cr;
        scan_continue(
            &t,
            from,
            base_line,
            pending_cr,
            |p| ix.merge_patch(&t, p),
            &cancelled,
            |_, _| {},
        );
        assert!(ix.complete);
        ix
    }

    // helper so the loop below doesn't rebuild the table inline awkwardly
    fn ix_table_ref(s: &str) -> PieceTable {
        PieceTable::from_bytes(s.as_bytes().to_vec())
    }

    /// Regression: when the prefix stops exactly on a lone `\r` (or between
    /// the `\r`/`\n` of a CRLF), that break was **never counted** in the
    /// prefix — the continuation must resolve it. The old code dropped the
    /// pending local, permanently losing the single break at the seam and
    /// shifting every later line number down by one.
    #[test]
    fn prefix_ending_on_unresolved_cr_counts_the_break() {
        // CR-only file: make the prefix end exactly on a `\r`
        let mut s = String::new();
        for i in 0..500 {
            s.push_str(&format!("line{i}\r"));
        }
        s.push_str("tail\r");
        let full = {
            let t = ix_table_ref(&s);
            LineIndex::build(&t, |_, _| {})
        };
        // The first CR is after "line0"; going a bit further is fine too —
        // covers the three seam kinds: lone CR end, after a CRLF's \r,
        // after a CRLF's \n
        let cr_pos = s.find('\r').unwrap() as u64;
        for (name, prefix) in [
            ("bare-CR end", cr_pos + 1),
            ("prefix holds a full CRLF", cr_pos + 2),
            ("prefix breaks mid-CRLF", cr_pos + 1),
        ] {
            let ix = progressive_of(s.as_bytes().to_vec(), prefix);
            assert_eq!(ix.total_lines, full.total_lines, "{name}: prefix {prefix}");
            assert_eq!(ix.newline_stats(), full.newline_stats(), "{name}");
            // spot-check the positions of several line numbers
            for l in [0u64, 1, 100, 400, full.total_lines - 1] {
                assert_eq!(
                    ix.pos_of_line(&ix_table_ref(&s), l),
                    full.pos_of_line(&ix_table_ref(&s), l),
                    "{name} line {l}"
                );
            }
        }
        // CRLF file: prefix breaks between \r and \n
        let mut s2 = String::new();
        for i in 0..500 {
            s2.push_str(&format!("line{i}\r\n"));
        }
        let full2 = {
            let t = ix_table_ref(&s2);
            LineIndex::build(&t, |_, _| {})
        };
        let cr = s2.find('\r').unwrap() as u64;
        for prefix in [cr + 1, cr + 2] {
            let ix = progressive_of(s2.as_bytes().to_vec(), prefix);
            assert_eq!(ix.total_lines, full2.total_lines, "CRLF prefix {prefix}");
            assert_eq!(ix.newline_stats(), full2.newline_stats());
        }
    }

    #[test]
    fn prefix_plus_continue_matches_full_build() {
        let mut s = String::new();
        for i in 0..3000 {
            s.push_str(&format!("第{i}行 abc\r\n"));
        }
        s.push_str("tail no newline");
        let full = {
            let t = PieceTable::from_bytes(s.as_bytes().to_vec());
            LineIndex::build(&t, |_, _| {})
        };
        // a prefix that stops exactly between the \r and \n of a CRLF
        let mid_crlf = s.find("第100行").unwrap() as u64 + "第100行".len() as u64 + 1;
        for prefix in [1u64, 7, mid_crlf, 4096, s.len() as u64 / 2] {
            let ix = progressive_of(s.as_bytes().to_vec(), prefix);
            assert_eq!(ix.total_lines, full.total_lines, "prefix {prefix}");
            assert_eq!(ix.newline, full.newline, "prefix {prefix}");
            assert_eq!(ix.mixed_newline, full.mixed_newline, "prefix {prefix}");
            assert_eq!(ix.newline_stats(), full.newline_stats(), "prefix {prefix}");
            assert_eq!(ix.scanned_to, ix.total_bytes, "prefix {prefix}");
            let t = ix_table_ref(&s);
            for l in [0u64, 1, 999, 1500, ix.total_lines - 1] {
                assert_eq!(
                    ix.pos_of_line(&t, l),
                    full.pos_of_line(&t, l),
                    "prefix {prefix} line {l}"
                );
            }
        }
    }

    #[test]
    fn cancelled_scan_returns_false_and_leaves_index_incomplete() {
        let s: String = (0..2000).map(|i| format!("row {i}\n")).collect();
        let t = PieceTable::from_bytes(s.into_bytes());
        let ix = LineIndex::build_prefix(&t, 10, |_, _| {});
        let cancelled = std::sync::atomic::AtomicBool::new(true);
        let got = scan_continue(
            &t,
            ix.scanned_to,
            ix.scanned_breaks(),
            ix.pending_cr,
            |_| {},
            &cancelled,
            |_, _| {},
        );
        assert!(!got, "a pre-cancelled scan must report cancellation");
        assert!(!ix.complete);
    }

    #[test]
    fn edits_inside_unswept_region_keep_the_index_coherent() {
        // prefix stops at 32 bytes; every edit happens past the watermark
        let mut s = String::new();
        for i in 0..200 {
            s.push_str(&format!("line {i} of the file\n"));
        }
        let mut t = PieceTable::from_bytes(s.clone().into_bytes());
        let mut ix = LineIndex::build_prefix(&t, 32, |_, _| {});
        assert!(!ix.complete);
        let wm = ix.scanned_to;
        // insert a few lines at the very end (window counts computed per
        // `apply_ops_raw`'s one-byte padding contract, matching Doc's call
        // style)
        let len0 = t.len();
        let removed = count_breaks_by_kind(&t, len0.saturating_sub(1)..len0);
        t.insert(len0, b"appended\nlines\n");
        let added = count_breaks_by_kind(&t, len0.saturating_sub(1)..(len0 + 16).min(t.len()));
        ix.apply_edit(&t, len0, 0, 15, removed, added);
        assert_eq!(
            ix.scanned_to, wm,
            "watermark must not move for unswept edits"
        );
        assert_eq!(ix.total_bytes, t.len());
        // delete the appended text again
        let len1 = t.len();
        let removed = count_breaks_by_kind(&t, len0.saturating_sub(1)..len1);
        t.delete(len1 - 15..len1);
        let added = count_breaks_by_kind(&t, len0.saturating_sub(1)..len0);
        ix.apply_edit(&t, len0, 15, 0, removed, added);
        assert_eq!(ix.total_bytes, s.len() as u64);
        // finish the scan and verify against a full build
        let cancelled = std::sync::atomic::AtomicBool::new(false);
        let from = ix.scanned_to;
        let base = ix.scanned_breaks();
        let pending_cr = ix.pending_cr;
        scan_continue(
            &t,
            from,
            base,
            pending_cr,
            |p| ix.merge_patch(&t, p),
            &cancelled,
            |_, _| {},
        );
        let full = LineIndex::build(&t, |_, _| {});
        assert_eq!(ix.total_lines, full.total_lines);
        assert_eq!(ix.pos_of_line(&t, 199), full.pos_of_line(&t, 199));
    }

    /// A random-edit fuzzer over a progressive index: edits inside the scanned
    /// region must keep checkpoints/watermark coherent so that finishing the
    /// scan converges to a full build.
    #[test]
    fn progressive_index_survives_random_edits() {
        let mut rng_state = 0xDEADBEEFCAFEBABEu64;
        let mut rand = move || {
            rng_state ^= rng_state << 13;
            rng_state ^= rng_state >> 7;
            rng_state ^= rng_state << 17;
            rng_state
        };
        let mut text = String::new();
        for i in 0..2000 {
            text.push_str(&format!("第{i}行 abc def\n"));
        }
        let mut t = PieceTable::from_bytes(text.clone().into_bytes());
        let mut ix = LineIndex::build_prefix(&t, 4096, |_, _| {});
        for op in 0..200 {
            let len = t.len();
            let mut pos = rand() % len;
            let line = ix.line_of_pos(&t, pos);
            pos = ix.pos_of_line(&t, line);
            let span = ix.line_span(&t, line);
            // line tail including the terminator = span.next (term is a
            // kind code; CR=3 but only 1 byte)
            let old_end = span.next.min(len);
            let old_len = old_end - pos;
            let new_text = ["X", "X\nY", "\n", "多行\n文本\n", ""][..][(rand() % 5) as usize];
            // window-padded counts, same contract as Doc::apply_ops_raw
            let win_start = pos.saturating_sub(1);
            let removed = count_breaks_by_kind(&t, win_start..(old_end + 1).min(len));
            t.delete(pos..old_end);
            t.insert(pos, new_text.as_bytes());
            let added = count_breaks_by_kind(
                &t,
                win_start..(pos + new_text.len() as u64 + 1).min(t.len()),
            );
            ix.apply_edit(&t, pos, old_len, new_text.len() as u64, removed, added);
            // queries are exact (the doc is far below EST_WALK_LIMIT, so
            // pos_of_line/line_of_pos walk the real text), but total_lines
            // is still an *estimate* while incomplete — the prefix's average
            // line length differs from the whole-file average, so only
            // require the estimate to stay within 10% of the exact value
            let rebuilt = LineIndex::build(&t, |_, _| {});
            let diff = ix.total_lines.abs_diff(rebuilt.total_lines);
            assert!(
                diff * 10 <= rebuilt.total_lines.max(1),
                "op {op}: estimate {} vs exact {}",
                ix.total_lines,
                rebuilt.total_lines
            );
            for _ in 0..6 {
                let l = rand() % ix.total_lines.max(1);
                assert_eq!(
                    ix.pos_of_line(&t, l),
                    rebuilt.pos_of_line(&t, l),
                    "op {op} line {l}"
                );
                let q = rand() % (t.len() + 1);
                assert_eq!(
                    ix.line_of_pos(&t, q),
                    rebuilt.line_of_pos(&t, q),
                    "op {op} pos {q}"
                );
            }
        }
        // finish the background scan; the merged result must equal a full build
        let cancelled = std::sync::atomic::AtomicBool::new(false);
        let (from, base) = (ix.scanned_to, ix.scanned_breaks());
        let pending_cr = ix.pending_cr;
        scan_continue(
            &t,
            from,
            base,
            pending_cr,
            |p| ix.merge_patch(&t, p),
            &cancelled,
            |_, _| {},
        );
        let full = LineIndex::build(&t, |_, _| {});
        assert!(ix.complete);
        assert_eq!(ix.total_lines, full.total_lines);
        assert_eq!(ix.newline_stats(), full.newline_stats());
        for l in [0u64, 17, 999, ix.total_lines - 1] {
            assert_eq!(ix.pos_of_line(&t, l), full.pos_of_line(&t, l), "line {l}");
        }
    }

    /// Edits across the scan watermark: a delete range starting inside the
    /// scanned region and ending past it, containing newlines — the window
    /// comparison's line delta counted breaks outside the watermark, which
    /// used to push `scanned_lines` off (the background continuation
    /// inherited a wrong base_line; line numbers/pos_of_line after the seam
    /// were permanently wrong). After the exact prefix rescan it must match
    /// a byte-for-byte recount, and the finished continuation must equal a
    /// full build.
    #[test]
    fn edit_across_watermark_keeps_scanned_prefix_exact() {
        let text: String = (0..8).map(|i| format!("line{i} abc\n")).collect();
        let mut t = PieceTable::from_bytes(text.into_bytes());
        let seam = "line0 abc\nline1 abc\nline2 abc\nline3 abc\n".len() as u64;
        let mut ix = LineIndex::build_prefix(&t, seam, |_, _| {});
        assert!(!ix.complete);
        // delete range: starts inside line3 crossing the watermark,
        // swallowing all of line4 (its `\n` lies outside the watermark,
        // never counted in scanned_lines)
        let pos = seam - "abc\n".len() as u64;
        let old_end = seam + "line4 abc\n".len() as u64;
        let old_len = old_end - pos;
        let removed = count_breaks_by_kind(&t, pos.saturating_sub(1)..(old_end + 1).min(t.len()));
        t.delete(pos..old_end);
        t.insert(pos, b"Z");
        let added = count_breaks_by_kind(&t, pos.saturating_sub(1)..(pos + 2).min(t.len()));
        ix.apply_edit(&t, pos, old_len, 1, removed, added);
        // exact reconciliation: scanned prefix line count == rescan
        let mut pending: PendingCr = None;
        let c = scan_breaks_with(&t, 0..ix.scanned_to, &mut pending, &mut |_| {}, &mut |_| {});
        assert_eq!(
            ix.scanned_lines, c.breaks,
            "scanned prefix must stay exact across a straddling edit"
        );
        // continue the scan to completion: matches the full build item by item
        let cancelled = std::sync::atomic::AtomicBool::new(false);
        let (from, base) = (ix.scanned_to, ix.scanned_breaks());
        scan_continue(
            &t,
            from,
            base,
            ix.pending_cr,
            |p| ix.merge_patch(&t, p),
            &cancelled,
            |_, _| {},
        );
        let full = LineIndex::build(&t, |_, _| {});
        assert_eq!(ix.total_lines, full.total_lines, "line count after resume");
        for l in 0..full.total_lines {
            assert_eq!(ix.pos_of_line(&t, l), full.pos_of_line(&t, l), "line {l}");
        }
    }

    /// refine_line_start must return the start of the line containing `pos`
    /// (and step over the LF half of a CRLF it lands on).
    #[test]
    fn refine_line_start_walks_backwards_correctly() {
        let t = PieceTable::from_bytes(b"alpha\nbeta\rgamma\r\ndelta".to_vec());
        assert_eq!(refine_line_start(&t, 0), 0);
        assert_eq!(refine_line_start(&t, 3), 0); // inside "alpha"
        assert_eq!(refine_line_start(&t, 6), 6); // "beta"
        assert_eq!(refine_line_start(&t, 11), 11); // "gamma"
                                                   // landing exactly on the LF of the CRLF starts the next line
        assert_eq!(refine_line_start(&t, 17), 18); // "delta"
        assert_eq!(refine_line_start(&t, 20), 18);
        assert_eq!(refine_line_start(&t, 24), 18);
        assert_eq!(refine_line_start(&t, 999), 18); // clamped past EOF
    }

    /// Estimates past the watermark stay within sane bounds on a document big
    /// enough to cross EST_WALK_LIMIT (2.2M lines × 9 bytes ≈ 20 MB).
    #[test]
    fn estimated_queries_past_watermark_are_bounded() {
        let line = "xxxxxxxx\n";
        let repeat = EST_WALK_LIMIT * 3 / 2 / line.len() as u64;
        let mut s = String::with_capacity((repeat * line.len() as u64) as usize);
        for _ in 0..repeat {
            s.push_str(line);
        }
        let t = PieceTable::from_bytes(s.into_bytes());
        let ix = LineIndex::build_prefix(&t, 64 * 1024, |_, _| {});
        assert!(!ix.complete);
        // far line -> estimation path, but the result is still a real line start
        let far = ix.total_lines - 1;
        let p = ix.pos_of_line(&t, far);
        assert_eq!(
            t.byte_at(p),
            Some(b'x'),
            "estimate must land on a line start"
        );
        assert_eq!(
            ix.line_of_pos(&t, t.len() - 1),
            far,
            "estimate of the last line"
        );
        // Ctrl+End semantics: the end position resolves immediately
        assert_eq!(ix.line_of_pos(&t, t.len()), ix.total_lines - 1);
    }

    /// `total_lines` is `breaks + (ends_with_newline ? 0 : 1)`. An edit that
    /// flips the trailing-newline flag changes the *phantom* line count
    /// without changing any break, so adding the raw break delta drifts by
    /// one — exactly at the document end, where users press Backspace most.
    #[test]
    fn trailing_newline_flip_keeps_line_count() {
        // delete the final LF: "a\nb\n" (2 lines) -> "a\nb" (2 lines)
        let mut t = PieceTable::from_bytes(b"a\nb\n".to_vec());
        let mut ix = idx(&t);
        let start = 3;
        let old_len = 1;
        let removed = count_breaks_by_kind(&t, 2..5);
        t.delete(start..start + old_len);
        let added = count_breaks_by_kind(&t, 2..4);
        ix.apply_edit(&t, start, old_len, 0, removed, added);
        assert_eq!(ix.total_lines, LineIndex::build(&t, |_, _| {}).total_lines);
        assert_eq!(ix.total_lines, 2);
        assert_eq!(ix.line_of_pos(&t, 3), 1);
        assert_eq!(ix.pos_of_line(&t, 1), 2);

        // insert a final LF: "a\nb" (2 lines) -> "a\nb\n" (2 lines)
        let mut t = PieceTable::from_bytes(b"a\nb".to_vec());
        let mut ix = idx(&t);
        let removed = count_breaks_by_kind(&t, 2..4);
        t.insert(3, b"\n");
        let added = count_breaks_by_kind(&t, 2..5);
        ix.apply_edit(&t, 3, 0, 1, removed, added);
        assert_eq!(ix.total_lines, LineIndex::build(&t, |_, _| {}).total_lines);
        assert_eq!(ix.total_lines, 2);
        assert_eq!(ix.pos_of_line(&t, 2), 4);
    }

    /// A checkpoint that lands exactly on `old_end` is shifted to
    /// `start + new_len`. When the inserted text does not end with a
    /// terminator that position is *not* a line start, so the checkpoint
    /// must be dropped instead of shifted — otherwise debug builds panic on
    /// the strict-increase assert and release builds return a line start
    /// inside the previous line.
    #[test]
    fn edit_ending_on_a_checkpoint_keeps_line_starts_valid() {
        let mut text = String::new();
        for i in 0..3000 {
            text.push_str(&format!("line-{i}\n"));
        }
        let mut t = PieceTable::from_bytes(text.into_bytes());
        let mut ix = idx(&t);
        let ckpt = ix.ckpts[1];
        assert!(ckpt.line > 0);
        // Replace "N\n" with "N\nN" — equal length, ends mid-line.
        let start = ckpt.pos - 2;
        let old_len = 2;
        let removed = count_breaks_by_kind(&t, start - 1..ckpt.pos + 1);
        let old = t.read_str(start..ckpt.pos);
        let new = format!("{}x", old.chars().next().unwrap());
        t.delete(start..ckpt.pos);
        t.insert(start, new.as_bytes());
        let added = count_breaks_by_kind(&t, start - 1..start + new.len() as u64 + 1);
        ix.apply_edit(&t, start, old_len, new.len() as u64, removed, added);
        let rebuilt = LineIndex::build(&t, |_, _| {});
        assert_eq!(ix.total_lines, rebuilt.total_lines);
        for line in [0u64, 1, ckpt.line - 1, ckpt.line, ckpt.line + 1, 2999] {
            assert_eq!(
                ix.pos_of_line(&t, line),
                rebuilt.pos_of_line(&t, line),
                "line {line}"
            );
        }
    }
}
