//! Minimap geometry and ink model: the VS Code "overview ruler" on the right.
//!
//! The minimap is a scaled-down bird's-eye view of the whole document: one row
//! per line, each row painted as a handful of horizontal "ink" bars instead of
//! glyphs. Blitz can only draw boxes, so per-character text at 2px (what VS
//! Code really renders) is not available; collapsing each line into runs of
//! non-whitespace gives the same reading — you navigate by *shape*, not by
//! legibility, and the shape is what survives the downscale.
//!
//! Two things make this cheap enough to rebuild on every frame:
//!
//! * only the rows inside the minimap's own window are modelled (it is
//!   virtualized exactly like the text viewport);
//! * a row is capped at [`MAX_RUNS`] bars, so the element count is bounded
//!   regardless of how busy the line is.
//!
//! Like [`crate::ui::scrollbar`], the math is f64: a million-line document is
//! millions of minimap pixels tall, and f32 would quantize the mapping into
//! visible jumps.

/// Width of the minimap column, in px.
///
/// Mirrors [`crate::ui::state::MINIMAP_W`] (which exists because `state` must
/// not depend on this module's view-side friends).
pub const MM_W: f32 = 84.0;

/// Height of one document line in the minimap, in px.
///
/// 3px keeps ~250 rows in a typical 770px viewport — enough that the shape of a
/// screenful is still recognisable, few enough that rebuilding the model every
/// frame stays under a millisecond.
pub const MM_LH: f32 = 3.0;

/// Horizontal scale: minimap px per character column.
pub const MM_CHAR_W: f32 = 1.0;

/// Columns of a line we bother to model. Anything past this is off the right
/// edge of an 84px strip anyway.
pub const MAX_COLS: usize = (MM_W / MM_CHAR_W) as usize;

/// Hard cap on the number of ink bars per row.
///
/// Without it a line like `a b c d e f g h i j ...` would produce dozens of
/// one-pixel boxes. Past the cap the newest run simply extends the last one, so
/// a very busy line reads as "solid", which is the honest summary anyway.
///
/// The ink is now painted in **one SVG path** (see `components::minimap`),
/// so the cost is the number of subpaths rather than DOM nodes: measured 1
/// run ≈ 0, 3 runs ≈ +1–2ms/frame, 5 runs ≈ +2.5ms/frame. 3 runs keep the
/// "word-block texture" and the shape information a thumbnail is supposed to
/// carry.
pub const MAX_RUNS: usize = 3;

/// A scrub in progress.
///
/// The drag is **glued to the indicator**: the pointer keeps the offset on the
/// indicator it took at press (the indicator's centre lands on the pointer,
/// so the offset is ~0 and the indicator can reach both ends of the strip),
/// and the strip itself stands still — the map does not slide under the
/// pointer the way the old frozen-mapping model made it. Only when the drag
/// pushes the indicator against an edge does the strip follow, minimally (see
/// [`mm_geom_ex`]).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MmDrag {
    /// Pointer y (strip coords) minus indicator top (strip coords), captured
    /// at press.
    pub grab: f64,
    /// The strip's own scroll while the drag is live. Starts at the press
    /// frame's value; `mm_pointer` nudges it only when the indicator reaches
    /// an edge, and the paint reads the same value, so the mapping can never
    /// disagree with what is painted.
    pub top: f64,
}

/// One ink bar: a run of non-whitespace collapsed into a rectangle, positioned
/// inside the strip (px).
///
/// The strip is deliberately **flat** — no per-row wrapper element. A row is
/// 3px tall and a typical line yields one or two runs, so a wrapper would add
/// ~250 elements per frame purely to carry a `top` that is already known.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MmBar {
    pub x: f32,
    pub y: f32,
    pub w: f32,
}

/// What one painted frame of the minimap draws.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MmModel {
    /// ink bars, already positioned in strip coordinates
    pub bars: Vec<MmBar>,
    /// y of the caret's row; `None` when it is outside the painted window
    pub cur_y: Option<f32>,
    /// y of every row holding a search match (the little ticks VS Code puts on
    /// the overview ruler)
    pub hits: Vec<f32>,
}

/// Per-line ink cache (reused across frames).
///
/// The minimap only looks at the ~250 lines inside its own window per frame,
/// and when scrolling a screenful more than nine tenths of them overlap with
/// the previous frame. Without a cache every frame runs one `pos_of_line`
/// (walking back ≤1024 checkpoints) + a text read + a scan for each of those
/// 250 lines — measured 2.5ms/frame on a 100k-line document (debug), an
/// order of magnitude more expensive than building the whole body viewport.
/// The cache invalidates on `(doc.id, doc.gen)`: one edit clears everything;
/// line text cannot change within one generation. The document id is part of
/// the key because `gen` restarts at zero for every document — keying on
/// `gen` alone made a freshly switched tab reuse the previous file's ink.
///
/// Capacity is capped (scrolling through a hundred thousand lines does not
/// accumulate without bound); on overflow the whole table is dropped and
/// rebuilt — one expensive frame beats handing memory to an invisible
/// curve.
pub struct MmInk {
    pub doc: u64,
    pub gen: u64,
    pub lines: std::collections::HashMap<u64, Vec<(u16, u16)>>,
}

/// Cap on cached lines (about 16 screens of minimap).
pub const MM_INK_CAP: usize = 4096;

impl MmInk {
    pub fn new() -> Self {
        MmInk {
            doc: u64::MAX,
            gen: u64::MAX,
            lines: std::collections::HashMap::new(),
        }
    }

    /// Drop everything when the document or its generation changed.
    fn sync(&mut self, doc: u64, gen: u64) {
        if self.doc != doc || self.gen != gen {
            self.doc = doc;
            self.gen = gen;
            self.lines.clear();
        }
    }

    /// Fetch a line's ink within the same document generation; returns `None`
    /// on a miss (the caller computes it and then [`Self::put`]s it).
    pub fn get(&mut self, doc: u64, gen: u64, line: u64) -> Option<Vec<(u16, u16)>> {
        self.sync(doc, gen);
        self.lines.get(&line).cloned()
    }

    pub fn put(&mut self, doc: u64, gen: u64, line: u64, runs: Vec<(u16, u16)>) {
        self.sync(doc, gen);
        if self.lines.len() >= MM_INK_CAP {
            self.lines.clear();
        }
        self.lines.insert(line, runs);
    }
}

impl Default for MmInk {
    fn default() -> Self {
        Self::new()
    }
}

impl MmModel {
    pub fn is_empty(&self) -> bool {
        self.bars.is_empty() && self.cur_y.is_none() && self.hits.is_empty()
    }
}

/// Everything one painted frame of the minimap needs to know.
#[derive(Clone, Debug, PartialEq)]
pub struct MmGeom {
    /// Scroll of the minimap's own content, in minimap px (`0` when the whole
    /// document fits and the strip does not move).
    pub top: f64,
    /// Full height of the minimap content, in minimap px.
    pub content: f64,
    /// First document line to paint.
    pub first: u64,
    /// How many rows to paint (one more than fits, so the last row is whole).
    pub rows: usize,
    /// y of `first` inside the strip, in minimap px (usually slightly negative).
    pub y0: f64,
    /// The viewport indicator: `(top, height)` in minimap px, clipped to the
    /// strip. This is the "you are here" rectangle VS Code draws over the map.
    pub slider: (f32, f32),
    /// Whether the document is taller than the strip (i.e. `top` can move).
    pub scrolls: bool,
}

/// Resolve one frame of minimap geometry.
///
/// * `lines` – document line count
/// * `mm_h` – height of the strip in px (the text area's height)
/// * `scroll_top` / `view_h` – the editor's own vertical scroll and viewport
/// * `lh` – the editor's line height in px
///
/// The strip itself scrolls only when it has to, and then it keeps the viewport
/// indicator **centred** — the VS Code behaviour for scrolls that did not come
/// from the strip itself. A short document is shown in full and only the
/// indicator moves.
pub fn mm_geom(lines: u64, mm_h: f32, scroll_top: f64, view_h: f32, lh: f32) -> MmGeom {
    mm_geom_ex(lines, mm_h, scroll_top, view_h, lh, None)
}

/// [`mm_geom`] with the strip's previous scroll.
///
/// `prev_top` is the drag latch ("the strip was here last frame"). While the
/// indicator stays inside the strip, the strip **stands still** — that is what
/// makes a drag follow the hand: the indicator moves with the pointer and the
/// map does not slide underneath it. Once the drag pushes the indicator against
/// an edge, the strip follows minimally, so the mapping and the paint stay in
/// agreement at every instant. `None` (or a value that cannot apply) falls back
/// to the centring policy.
pub fn mm_geom_ex(
    lines: u64,
    mm_h: f32,
    scroll_top: f64,
    view_h: f32,
    lh: f32,
    prev_top: Option<f64>,
) -> MmGeom {
    let lh = if lh > 0.0 { lh as f64 } else { 1.0 };
    let k = MM_LH as f64 / lh; // minimap px per document px
    let mm_h = mm_h.max(1.0) as f64;
    // A NaN scroll would sail straight through `clamp` (both comparisons are
    // false, so clamp returns the NaN) and poison every y below.
    let scroll_top = if scroll_top.is_finite() {
        scroll_top.max(0.0)
    } else {
        0.0
    };
    let content = lines as f64 * MM_LH as f64;
    let max_top = (content - mm_h).max(0.0);
    let scrolls = max_top > 0.0;
    let top = strip_top(scroll_top, view_h, mm_h, max_top, scrolls, k, prev_top);

    let first = (top / MM_LH as f64).floor().max(0.0) as u64;
    let rows = ((mm_h / MM_LH as f64).ceil() as usize).max(1) + 1;
    let y0 = first as f64 * MM_LH as f64 - top;

    // The indicator covers exactly the lines the editor is showing.
    // `min(content)`: on a document shorter than the viewport the box must
    // cover the document and stop, not run off the end of the last line into
    // the empty part of the strip.
    let s0 = (scroll_top * k - top).clamp(0.0, mm_h);
    let s1 = ((scroll_top + view_h as f64) * k - top)
        .clamp(0.0, mm_h)
        .min(content);
    let slider = (s0 as f32, (s1 - s0).max(2.0) as f32);

    MmGeom {
        top,
        content,
        first,
        rows,
        y0,
        slider,
        scrolls,
    }
}

/// The strip's own scroll for one frame.
///
/// Without a latch the indicator is centred (see [`mm_geom`]'s doc). With one,
/// the strip keeps its previous scroll as long as the indicator fits inside the
/// strip, and clamps to the nearest edge of that window once the drag pushes
/// the indicator out — the strip then follows the drag minimally instead of
/// re-centring (which would throw the map back under a stationary pointer).
fn strip_top(
    scroll_top: f64,
    view_h: f32,
    mm_h: f64,
    max_top: f64,
    scrolls: bool,
    k: f64,
    prev_top: Option<f64>,
) -> f64 {
    if !scrolls {
        return 0.0;
    }
    let centred = ((scroll_top + view_h as f64 / 2.0) * k - mm_h / 2.0).clamp(0.0, max_top);
    let Some(t) = prev_top else {
        return centred;
    };
    if !t.is_finite() {
        return centred;
    }
    // The indicator (height view_h·k, top at scroll·k − top) must stay inside
    // the strip: top ∈ [scroll·k + view_h·k − mm_h, scroll·k] ∩ [0, max_top].
    // The centred value is exactly the midpoint of that window, so an
    // unclamped latch equals it and the two policies agree.
    let view_k = view_h as f64 * k;
    let lo = ((scroll_top + view_h as f64) * k - mm_h).max(0.0);
    let hi = (scroll_top * k).min(max_top);
    if lo > hi || view_k > mm_h {
        // Degenerate (the indicator cannot fit the strip): centring is the
        // only sane answer.
        return centred;
    }
    t.clamp(lo, hi)
}

/// Document pixel offset (from the top of the document) of a point `local_y`
/// px down the strip.
///
/// Inverse of the mapping in [`mm_geom`]; used by click-to-jump and by
/// drag-to-scrub, which both have to turn "I clicked here" into "show me that
/// line".
pub fn mm_doc_px(local_y: f64, top: f64, lh: f32) -> f64 {
    let lh = if lh > 0.0 { lh as f64 } else { 1.0 };
    (local_y + top) / (MM_LH as f64 / lh)
}

/// Document line under a point `local_y` px down the strip.
pub fn mm_line_at(local_y: f64, top: f64, lh: f32, lines: u64) -> u64 {
    let px = mm_doc_px(local_y, top, lh).max(0.0);
    let lh = if lh > 0.0 { lh as f64 } else { 1.0 };
    ((px / lh).floor() as u64).min(lines.saturating_sub(1))
}

/// Collapse one line into runs of "ink", as **column** ranges.
///
/// Leading whitespace is dropped (that is the indentation gutter VS Code also
/// leaves blank), tabs advance to the next tab stop just like the text column,
/// and anything past [`MAX_COLS`] is ignored. Runs are merged once [`MAX_RUNS`]
/// is reached — see that constant for why.
pub fn ink_runs(line: &str, tab: usize, max_cols: usize) -> Vec<(u16, u16)> {
    let tab = tab.max(1);
    let max_cols = max_cols.min(u16::MAX as usize);
    let mut out: Vec<(u16, u16)> = Vec::new();
    let mut col = 0usize;
    let mut start: Option<usize> = None;

    let flush = |out: &mut Vec<(u16, u16)>, s: usize, e: usize| {
        let e = e.min(max_cols);
        if e <= s {
            return;
        }
        if out.len() < MAX_RUNS {
            out.push((s as u16, e as u16));
        } else if let Some(last) = out.last_mut() {
            last.1 = e as u16;
        }
    };

    for ch in line.chars() {
        if col >= max_cols {
            break;
        }
        // `len_utf8() > 1` classified every non-ASCII character as double
        // width, so "café" drew five columns, Cyrillic/Greek were stretched,
        // and combining marks (zero width) took two. The text path measures the
        // real glyph and falls back to unicode-width; the minimap should at
        // least agree on the fallback.
        let step = unicode_width::UnicodeWidthChar::width(ch)
            .unwrap_or(1)
            .max(1);
        match ch {
            ' ' => {
                if let Some(s) = start.take() {
                    flush(&mut out, s, col);
                }
                col += 1;
            }
            '\t' => {
                if let Some(s) = start.take() {
                    flush(&mut out, s, col);
                }
                col = ((col / tab) + 1) * tab;
            }
            _ => {
                if start.is_none() {
                    start = Some(col);
                }
                col += step;
            }
        }
    }
    if let Some(s) = start {
        flush(&mut out, s, col);
    }
    // A run that reached the right edge is clipped, never wrapped.
    for r in out.iter_mut() {
        r.1 = r.1.min(max_cols as u16);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cols(runs: &[(u16, u16)]) -> Vec<(usize, usize)> {
        runs.iter()
            .map(|&(a, b)| (a as usize, b as usize))
            .collect()
    }

    #[test]
    fn indentation_is_left_blank() {
        // 8 columns of indent stay blank, then one bar per word of "let x = 1;"
        // — once word blocks exceed MAX_RUNS the tail merges into the last bar
        // (the same line flattened into a "solid" one)
        assert_eq!(
            cols(&ink_runs("        let x = 1;", 4, 96)),
            vec![(8, 11), (12, 13), (14, 18)]
        );
        // leading tabs blank too
        assert_eq!(
            cols(&ink_runs("\t\tfoo", 4, 96)),
            vec![(8, 11)],
            "two tabs -> indent 8, then the word"
        );
    }

    #[test]
    fn words_become_separate_bars() {
        assert_eq!(cols(&ink_runs("ab cd", 4, 96)), vec![(0, 2), (3, 5)]);
    }

    #[test]
    fn empty_and_blank_lines_have_no_ink() {
        assert!(ink_runs("", 4, 96).is_empty());
        assert!(ink_runs("     ", 4, 96).is_empty());
        assert!(ink_runs("\t", 4, 96).is_empty());
    }

    #[test]
    fn wide_characters_take_two_columns() {
        // Chinese is two chars, each two columns wide
        assert_eq!(cols(&ink_runs("中文", 4, 96)), vec![(0, 4)]);
    }

    #[test]
    fn runs_are_capped_and_clipped() {
        let busy: String = (0..40)
            .map(|i| if i % 2 == 0 { 'x' } else { ' ' })
            .collect();
        let r = ink_runs(&busy, 4, 96);
        assert!(r.len() <= MAX_RUNS, "capped at {MAX_RUNS}, got {}", r.len());
        assert!(r.last().unwrap().1 >= 20, "the tail must still be covered");
        // a line longer than the strip is clipped, never wrapped
        let long = "x".repeat(500);
        for (_, e) in &ink_runs(&long, 4, MAX_COLS) {
            assert!(*e as usize <= MAX_COLS, "run end {e} past {MAX_COLS}");
        }
    }

    #[test]
    fn short_document_shows_in_full_and_never_scrolls() {
        // 50 lines * 20px = 1000 document px: taller than the 770px viewport,
        // but only 150 minimap px, so the strip itself never scrolls and shows
        // the whole document.
        let g = mm_geom(50, 770.0, 0.0, 770.0, 20.0);
        assert_eq!(g.first, 0);
        assert!(!g.scrolls);
        assert_eq!(g.top, 0.0);
        assert_eq!(g.content, 50.0 * MM_LH as f64);
        // the box covers the fraction of the document the viewport shows
        let k = MM_LH as f64 / 20.0;
        assert!(
            (g.slider.1 as f64 - 770.0 * k).abs() < 1e-3,
            "{:?}",
            g.slider
        );

        // shorter than the viewport: the box covers the whole document and
        // never spills past the last line into the empty part of the strip
        let g = mm_geom(20, 770.0, 0.0, 770.0, 20.0);
        assert!(!g.scrolls);
        assert!(
            (g.slider.1 as f64 - g.content).abs() < 1e-3,
            "{:?}",
            g.slider
        );
    }

    #[test]
    fn long_document_keeps_the_indicator_centred() {
        let (lines, mm_h, view_h, lh) = (10_000u64, 770.0f32, 770.0f32, 20.0f32);
        // scroll to the middle: the indicator must sit at the centre
        let mid = (lines as f64 * lh as f64 - view_h as f64) / 2.0;
        let g = mm_geom(lines, mm_h, mid, view_h, lh);
        assert!(g.scrolls);
        let centre = g.slider.0 + g.slider.1 / 2.0;
        assert!(
            (centre - mm_h / 2.0).abs() < 2.0,
            "indicator centre {centre} vs {}",
            mm_h / 2.0
        );
        // and it is clamped at both ends
        let top = mm_geom(lines, mm_h, 0.0, view_h, lh);
        assert_eq!(top.top, 0.0);
        let end = mm_geom(lines, mm_h, 1e12, view_h, lh);
        assert!((end.top - (end.content - mm_h as f64)).abs() < 1e-6);
        assert!(end.first + end.rows as u64 >= lines - 1);
    }

    #[test]
    fn click_maps_back_to_the_line_under_the_pointer() {
        let (lines, mm_h, view_h, lh) = (10_000u64, 770.0f32, 770.0f32, 20.0f32);
        for scroll in [0.0, 5000.0, 100_000.0] {
            let g = mm_geom(lines, mm_h, scroll, view_h, lh);
            // row `i` of the painted window is document line `first + i`
            for i in [0usize, 1, 10, g.rows - 1] {
                let y = g.y0 + i as f64 * MM_LH as f64 + MM_LH as f64 / 2.0;
                let line = mm_line_at(y, g.top, lh, lines);
                let want = (g.first + i as u64).min(lines - 1);
                assert_eq!(line, want, "scroll {scroll} row {i}: {line} != {want}");
            }
        }
    }

    #[test]
    fn inverse_round_trips() {
        for local in [0.0f64, 1.0, 200.0, 769.0] {
            for top in [0.0f64, 1234.5] {
                let px = mm_doc_px(local, top, 20.0);
                let back = (px * (MM_LH as f64 / 20.0)) - top;
                assert!((back - local).abs() < 1e-6, "{back} vs {local}");
            }
        }
    }

    #[test]
    fn degenerate_inputs_stay_finite() {
        for (lines, mm_h, scroll, view_h, lh) in [
            (0u64, 770.0f32, 0.0f64, 770.0f32, 20.0f32),
            (10, 0.0, 0.0, 0.0, 0.0),
            (1, 770.0, f64::NAN, 770.0, 20.0),
        ] {
            let g = mm_geom(lines, mm_h, scroll, view_h, lh);
            assert!(g.top.is_finite(), "top for {lines}/{mm_h}/{scroll}");
            assert!(g.y0.is_finite(), "y0");
            assert!(g.slider.0.is_finite() && g.slider.1.is_finite(), "slider");
            assert!(g.slider.1 >= 2.0, "indicator must stay visible");
            assert!(mm_doc_px(10.0, g.top, lh).is_finite());
            assert!(mm_line_at(0.0, g.top, lh, lines) < lines.max(1));
        }
    }

    /// The ink cache must not serve one document's lines to another: every
    /// document's generation restarts at 0, so `gen` alone is not an identity.
    #[test]
    fn ink_cache_is_scoped_to_the_document() {
        let mut ink = MmInk::new();
        ink.put(1, 0, 5, vec![(0, 2)]);
        assert_eq!(ink.get(1, 0, 5), Some(vec![(0, 2)]));
        assert_eq!(
            ink.get(2, 0, 5),
            None,
            "another document with the same generation must miss"
        );
        // A new generation of the same document also invalidates.
        assert_eq!(ink.get(1, 1, 5), None);
    }
}
