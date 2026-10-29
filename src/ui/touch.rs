//! Touch gesture state machine: a **tap** places the caret, a **drag**
//! scrolls the content.
//!
//! A touchscreen has no middle button, no wheel and no hover; the only
//! natural way to scroll a text editor is "push the content up while holding
//! it". blitz's native panning only applies to real scroll containers
//! (`overflow:auto`), while the body viewport is virtualized with a manual
//! `scroll_top`, so this state machine is maintained by the host itself — the
//! **source** of the events is still blitz's native pointer events
//! (`touch-action:none` guarantees finger movement is not swallowed by
//! blitz's Panning).
//!
//! Semantics match mobile editors:
//!
//! * displacement < [`SLOP`]: lifting the finger counts as a tap (placing the
//!   caret) — it must not be judged at press time, otherwise the beginning of
//!   every swipe would first move the caret to the start point;
//! * crossing the threshold: this gesture becomes a pan, the content follows
//!   the finger (scroll amount = finger displacement), and lifting no longer
//!   places the caret;
//! * one gesture only recognizes the first finger; the second finger's
//!   press/move/lift are all ignored.
//!
//! The state lives in a process-level `Mutex` rather than `EditorState`:
//! enqueueing (pointermove) and landing (the frame hook's `ui::scroll`) are
//! separated by a render, and writing into a signal would mark the whole tree
//! dirty on every finger move, defeating the coalescing.

use std::sync::Mutex;

use crate::ui::scroll::Target;

/// The boundary between tap and drag (CSS px). Takes the same value as
/// blitz's `FINGER_TAP_SLOP` (vendor patches eleven/thirteen): a finger
/// drift within 12px counts neither as a drag nor swallows the Click.
///
/// 12 rather than 8: desktop touchscreens commonly run at 150% scaling, and
/// measured tap drift of a fingertip can reach around 10px; the threshold
/// only eats the "first 12px from rest", beyond which the content follows
/// 1:1 and the scrolling feel is unchanged.
pub const SLOP: f64 = 12.0;

/// Which scrollable surface a gesture lands on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PanTarget {
    /// Body viewport
    Doc,
    /// CSV table
    Csv,
    /// Find-results panel
    Find,
}

impl PanTarget {
    fn scroll_target(self) -> Target {
        match self {
            PanTarget::Doc => Target::Doc,
            PanTarget::Csv => Target::Csv,
            PanTarget::Find => Target::Find,
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct Pan {
    target: PanTarget,
    /// The `pointer_id` driving the gesture (the lift event filters by it)
    id: i32,
    start: (f64, f64),
    last: (f64, f64),
    /// Past the threshold, currently panning
    moving: bool,
    /// Time of last activity: used to judge "this ledger is stale" when
    /// gesture events are lost (focus stolen, pointer captured by another
    /// window), so the next press takes over directly; otherwise the editor
    /// would be stuck forever in the "first finger has not lifted yet"
    /// state and no click would place a caret.
    at: std::time::Instant,
}

/// A gesture ledger with no activity for this long counts as stale (a real
/// touch is never silent for this long).
const STALE: std::time::Duration = std::time::Duration::from_millis(2000);

static PAN: Mutex<Option<Pan>> = Mutex::new(None);

/// Finger down (client-area CSS px). Does not take over when a **live**
/// gesture exists (multi-finger protection) and returns `false`; a stale
/// gesture (see `STALE`) is displaced outright.
pub fn begin(target: PanTarget, id: i32, x: f64, y: f64) -> bool {
    let mut g = PAN.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(old) = *g {
        if old.at.elapsed() < STALE {
            return false;
        }
    }
    *g = Some(Pan {
        target,
        id,
        start: (x, y),
        last: (x, y),
        moving: false,
        at: std::time::Instant::now(),
    });
    true
}

/// Finger move: returns `(target, dx, dy)` for the scroll delta this move
/// produces (the content follows the finger — finger up → content scrolls
/// up). Returns `None` before the threshold is crossed.
///
/// `id` is the finger id of the move event: returns `None` when it is not
/// the finger driving the gesture — a window-level `PointerMoved` funnels all
/// pointers' movement into one event stream, and without filtering, the
/// second finger's coordinates would advance someone else's gesture and the
/// content would jump back and forth with two fingers.
pub fn feed(id: i32, x: f64, y: f64) -> Option<(PanTarget, f64, f64)> {
    let mut g = PAN.lock().unwrap_or_else(|e| e.into_inner());
    let p = g.as_mut()?;
    if p.id != id {
        return None;
    }
    p.at = std::time::Instant::now();
    if !p.moving {
        if (x - p.start.0).abs() < SLOP && (y - p.start.1).abs() < SLOP {
            return None;
        }
        // Count from the start point: those few pixels within the threshold
        // are not swallowed, so the content does not jump at the very start
        p.moving = true;
        p.last = p.start;
    }
    // Content follows the finger: finger moving down-right = content moving
    // down-right, same sign as `WheelDelta` (in `scroll_by` the positive
    // content direction = scroll position decreases).
    let dx = x - p.last.0;
    let dy = y - p.last.1;
    p.last = (x, y);
    (dx != 0.0 || dy != 0.0).then_some((p.target, dx, dy))
}

/// Finger up. `id` is the lift event's `pointer_id`: returns `None` when it
/// is not the finger driving the gesture (the window is still being pushed
/// by another finger).
///
/// Returns `(target, press point, whether it panned)`: `false` means this
/// was a tap, and the caller places the caret at the press point.
pub fn end(id: i32) -> Option<(PanTarget, (f64, f64), bool)> {
    let mut g = PAN.lock().unwrap_or_else(|e| e.into_inner());
    let p = (*g)?;
    if p.id != id {
        return None;
    }
    *g = None;
    Some((p.target, p.start, p.moving))
}

/// Abandon the current gesture (context menu popped up, window gesture took
/// over, etc.).
pub fn cancel() {
    *PAN.lock().unwrap_or_else(|e| e.into_inner()) = None;
}

/// Invalidate a gesture by finger: when the system cancels tracking of some
/// pointer (palm rejection / focus stolen), winit only sends `PointerLeft`,
/// with no Released. Only clear **this finger's** ledger — the pan being
/// driven by another finger must not go down with it.
pub fn cancel_id(id: i32) {
    let mut g = PAN.lock().unwrap_or_else(|e| e.into_inner());
    if g.as_ref().is_some_and(|p| p.id == id) {
        *g = None;
    }
}

/// Whether a gesture is in progress (window-level move events divert based
/// on this).
pub fn active() -> bool {
    PAN.lock().unwrap_or_else(|e| e.into_inner()).is_some()
}

/// Queue a pan delta into [`crate::ui::scroll`] (in-frame coalescing).
/// Returns whether a new frame was scheduled.
pub fn queue_scroll(target: PanTarget, dx: f64, dy: f64) -> bool {
    crate::ui::scroll::queue(target.scroll_target(), dx, dy)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reset() {
        *PAN.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    #[test]
    fn small_drift_stays_a_tap() {
        reset();
        assert!(begin(PanTarget::Doc, 7, 100.0, 200.0));
        // Jitter at finger lift: produces no scrolling
        assert_eq!(feed(7, 104.0, 203.0), None);
        let (target, start, moved) = end(7).unwrap();
        assert_eq!(target, PanTarget::Doc);
        assert!(!moved, "within 8px must still count as a tap");
        assert_eq!(start, (100.0, 200.0), "the tap uses the press point");
    }

    #[test]
    fn dragging_past_the_slop_scrolls_from_the_press_point() {
        reset();
        begin(PanTarget::Doc, 1, 100.0, 300.0);
        assert_eq!(
            feed(1, 103.0, 295.0),
            None,
            "no movement within the threshold"
        );
        let (_, dx, dy) = feed(1, 100.0, 280.0).unwrap();
        assert_eq!(
            (dx, dy),
            (0.0, -20.0),
            "content follows the finger: finger moved up 20px"
        );
        let (_, _, moved) = end(1).unwrap();
        assert!(moved);
        // Finger down = content down (scroll_by's positive direction); the
        // sign must match the wheel
        reset();
        begin(PanTarget::Doc, 2, 100.0, 200.0);
        assert_eq!(feed(2, 100.0, 260.0), Some((PanTarget::Doc, 0.0, 60.0)));
    }

    #[test]
    fn another_finger_never_feeds_the_gesture() {
        reset();
        begin(PanTarget::Doc, 1, 100.0, 300.0);
        // No matter how far the second finger is, it never advances the
        // first finger's gesture
        assert_eq!(feed(2, 0.0, 0.0), None);
        assert_eq!(feed(2, 500.0, 0.0), None);
        // The driving finger works as usual
        assert!(feed(1, 100.0, 280.0).is_some());
        // Another finger being cancelled by the system (no lift event) does
        // not clear the driving finger's ledger
        cancel_id(2);
        assert!(active());
        cancel_id(1);
        assert!(!active());
    }

    #[test]
    fn second_finger_never_steals_the_gesture() {
        reset();
        assert!(begin(PanTarget::Doc, 1, 0.0, 0.0));
        assert!(
            !begin(PanTarget::Doc, 2, 50.0, 50.0),
            "the second finger does not take over"
        );
        assert!(end(2).is_none(), "its lift does not end the gesture either");
        assert!(active());
        assert!(end(1).is_some());
        assert!(!active());
    }

    #[test]
    fn cancel_drops_the_gesture() {
        reset();
        begin(PanTarget::Csv, 3, 0.0, 0.0);
        cancel();
        assert!(!active());
        assert!(end(3).is_none());
    }
}
