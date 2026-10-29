//! **In-frame coalescing** of scroll input: the single entry point shared by
//! the mouse wheel and touch panning.
//!
//! # Why it is needed
//!
//! If every scroll input called `state.write()` directly, a high-resolution
//! wheel / trackpad / touchscreen can deliver 3–10 events in one frame, each
//! triggering a full-app VDOM diff + layout + paint; the intermediate
//! positions within the same frame get overwritten by the later ones and are
//! pure wasted work. Here the deltas accumulate in a process-level
//! accumulator and land in the state in one go from `RedrawRequested` (before
//! blitz renders): at most one write and one render per frame, with not an
//! inch of scroll distance lost.
//!
//! # Why three accumulators
//!
//! The body / CSV table / find results each have their own scrolling
//! state, independent of each other; one gesture lands on one surface, and a
//! mouse wheel also hovers over exactly one surface, so accumulating per
//! target neither bleeds contexts into each other nor requires carrying
//! "current surface" context into the queue.
//!
//! The accumulator is process-level (not an `EditorState` field): enqueueing
//! happens in the event, dequeuing in the frame hook, with a render in
//! between; writing it into the state would mark dirty on every enqueue and
//! defeat the whole point of coalescing.

use std::sync::Mutex;

/// A scrollable surface. Deltas are accumulated per target.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    /// Body viewport (including wheel over the minimap)
    Doc,
    /// CSV table
    Csv,
    /// Find-results panel (scrolls by row, see
    /// `ui::components::findbar::RESULT_ROW_H`)
    Find,
}

const TARGETS: usize = 3;

impl Target {
    fn ix(self) -> usize {
        match self {
            Target::Doc => 0,
            Target::Csv => 1,
            Target::Find => 2,
        }
    }
}

/// The `(dx, dy)` accumulator, in CSS px; the direction is the direction the
/// content should move (same semantics as `WheelDelta`: positive = content
/// moves right/down).
static PENDING: Mutex<[(f64, f64); TARGETS]> = Mutex::new([(0.0, 0.0); TARGETS]);

/// Accumulate one scroll delta. Returns `true` when the queue went from
/// empty to non-empty (the caller needs to schedule a frame).
pub fn queue(target: Target, dx: f64, dy: f64) -> bool {
    if dx == 0.0 && dy == 0.0 {
        return false;
    }
    let mut q = PENDING.lock().unwrap_or_else(|e| e.into_inner());
    let slot = &mut q[target.ix()];
    let was_empty = slot.0 == 0.0 && slot.1 == 0.0;
    slot.0 += dx;
    slot.1 += dy;
    was_empty
}

/// Take all accumulated deltas (called once per frame). The index
/// corresponds to [`Target`]'s `ix()`.
pub fn drain() -> [(f64, f64); TARGETS] {
    let mut q = PENDING.lock().unwrap_or_else(|e| e.into_inner());
    std::mem::replace(&mut *q, [(0.0, 0.0); TARGETS])
}

/// Whether the queue holds deltas waiting to land.
///
/// The frame hook uses it as a **read-only probe**: it never calls
/// `state.write()` when there are no deltas (the write guard's drop
/// unconditionally mark_dirty's, which would relight the idle render loop).
/// The probe is aligned verbatim with [`drain`]'s semantics — only non-zero
/// counts as work.
pub fn pending() -> bool {
    let q = PENDING.lock().unwrap_or_else(|e| e.into_inner());
    q.iter().any(|&(x, y)| x != 0.0 || y != 0.0)
}

/// Clear (for unit tests: process-level statics persist across cases).
#[cfg(test)]
pub fn reset() {
    let mut q = PENDING.lock().unwrap_or_else(|e| e.into_inner());
    *q = [(0.0, 0.0); TARGETS];
}

// ---- Scrollbar drag: input-side pointer (latest-wins) ----------------------
//
// The wheel queue above is *additive* (deltas accumulate); a thumb drag is
// *absolute* — the newest pointer position supersedes every earlier one, so
// the drag side-channel is a single `Option` slot, not an accumulator.
//
// Two consumers, two different needs (see the lag analysis
// tmp/scrollbar-lag-analysis.md):
//
// * the **input-side thumb** reads the latest sample at render time so the
//   thumb tracks the pointer even while the content frame is still in flight;
// * the **frame hook** takes the sample once per frame and lands it in
//   `scroll_top` — the content follows at frame rate, exactly like the wheel.

/// One scrollbar-drag pointer sample, in client CSS px.
#[derive(Clone, Copy, Debug)]
pub struct SbPointer {
    /// `true` = vertical bar
    pub vertical: bool,
    pub cx: f32,
    pub cy: f32,
}

static SB_PENDING: Mutex<Option<SbPointer>> = Mutex::new(None);

/// Park the latest drag pointer sample. Called from the window's pointer-move
/// handler — no state write, no dirty mark: the content lands in the frame
/// hook, and the thumb's cheap scope renders from [`sb_take`]/the live signal.
pub fn sb_queue(p: SbPointer) {
    *SB_PENDING.lock().unwrap_or_else(|e| e.into_inner()) = Some(p);
}

/// Take the pending sample (the frame hook's landing step).
pub fn sb_take() -> Option<SbPointer> {
    SB_PENDING.lock().unwrap_or_else(|e| e.into_inner()).take()
}

/// Whether a sample is waiting (read-only probe, mirrors [`drain`]'s semantics).
pub fn sb_pending() -> bool {
    SB_PENDING
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .is_some()
}

/// Discard any pending sample (gesture torn down without a release: tab
/// switch, locale switch — landing a stale pointer into the new surface
/// would scroll it).
pub fn sb_clear() {
    *SB_PENDING.lock().unwrap_or_else(|e| e.into_inner()) = None;
}

// ---- Drag-selection auto-scroll: input-side pointer (latest-wins) ----------
//
// A press-and-drag selection is driven by DOM `pointermove` **on the
// viewport**, and those events stop arriving the moment the pointer leaves
// that element — drag past the last line and the selection froze at the edge
// without the view ever scrolling. The window-level move stream keeps coming
// (same reason the scrollbar drag above lives there), so the newest position
// is parked here and the frame hook turns it into one scroll step per frame.
//
// Latest-wins, not additive: this is an absolute pointer, like the thumb, and
// a sample is only ever *read*, never consumed — the frame hook needs the same
// position again next frame, which is exactly how a pointer held still outside
// the edge keeps scrolling.

static DRAG_PTR: Mutex<Option<(f32, f32)>> = Mutex::new(None);

/// Park the newest pointer position of a drag selection (client CSS px).
pub fn drag_ptr_set(x: f32, y: f32) {
    *DRAG_PTR.lock().unwrap_or_else(|e| e.into_inner()) = Some((x, y));
}

/// The current pointer position of a drag selection, if one is pressed.
pub fn drag_ptr_peek() -> Option<(f32, f32)> {
    *DRAG_PTR.lock().unwrap_or_else(|e| e.into_inner())
}

/// Forget the pointer (the press ended). Without this, the next drag would
/// start from the previous gesture's last position and scroll before the
/// pointer moves at all.
pub fn drag_ptr_clear() {
    *DRAG_PTR.lock().unwrap_or_else(|e| e.into_inner()) = None;
}

#[cfg(test)]
mod sb_tests {
    use super::*;

    #[test]
    fn sb_pointer_latest_wins() {
        sb_clear();
        sb_queue(SbPointer {
            vertical: true,
            cx: 1.0,
            cy: 10.0,
        });
        sb_queue(SbPointer {
            vertical: true,
            cx: 2.0,
            cy: 20.0,
        });
        let p = sb_take().expect("pending");
        assert_eq!((p.cx, p.cy), (2.0, 20.0), "the newest sample must win");
        assert!(!sb_pending(), "take clears the slot");
        assert!(sb_take().is_none());
    }

    #[test]
    fn sb_clear_discards_without_landing() {
        sb_queue(SbPointer {
            vertical: false,
            cx: 3.0,
            cy: 4.0,
        });
        sb_clear();
        assert!(sb_take().is_none(), "clear must drop, not keep");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deltas_accumulate_per_target() {
        reset();
        assert!(
            queue(Target::Doc, 0.0, -30.0),
            "an empty queue should report needing a frame"
        );
        assert!(
            !queue(Target::Doc, 0.0, -12.0),
            "a non-empty queue does not wake again"
        );
        queue(Target::Csv, 0.0, 40.0);
        queue(Target::Find, 3.0, -2.0);
        let d = drain();
        assert_eq!(
            d[0],
            (0.0, -42.0),
            "wheel deltas in the same frame must coalesce"
        );
        assert_eq!(
            d[1],
            (0.0, 40.0),
            "different surfaces do not bleed into each other"
        );
        assert_eq!(d[2], (3.0, -2.0));
        // Back to the empty state after taking
        assert_eq!(drain(), [(0.0, 0.0); TARGETS]);
    }

    #[test]
    fn zero_delta_is_not_a_wakeup() {
        reset();
        assert!(!queue(Target::Doc, 0.0, 0.0));
        assert_eq!(drain(), [(0.0, 0.0); TARGETS]);
    }
}
