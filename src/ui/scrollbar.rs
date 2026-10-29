//! Scrollbar geometry: the pure math behind the editor's own scrollbars.
//!
//! Blitz paints no native scrollbars — every scrollable surface in this app
//! owns its scroll offset and draws its own bar (see also the find-results
//! panel). This module holds the *pure* half of that job: mapping a scroll
//! offset to a thumb rectangle and back. Keeping it free of `EditorState`
//! (and of any widget) is what makes the fiddly parts — the minimum thumb
//! size, the "everything fits" case, the last-pixel rounding — testable.
//!
//! All lengths are in CSS px and all math is `f64`: the vertical scroll range
//! of a huge document is billions of pixels (see [`crate::ui::state::EditorState::scroll_top`]),
//! and f32 would quantize the thumb position into visible jumps.

/// Thickness of a scrollbar (also the size of its square arrow buttons).
///
/// 14px is what Windows itself uses at 100% DPI; anything thinner stops being
/// clickable and anything thicker steals text area.
pub const BAR: f32 = 14.0;

/// Shortest thumb we will ever draw, in px.
///
/// Without a floor, a million-line document gets a sub-pixel thumb that cannot
/// be grabbed. Windows behaves the same way (the thumb stops shrinking and
/// keeps a minimum usable size).
pub const MIN_THUMB: f32 = 20.0;

/// Where and how big the thumb of one bar is.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ThumbGeom {
    /// Distance from the start of the *draggable* track (i.e. after the first
    /// arrow button) to the thumb's leading edge, in px.
    pub off: f64,
    /// Thumb length along the bar, in px.
    pub len: f64,
    /// Length of the draggable track, in px (bar length minus both arrows).
    pub track: f64,
    /// Maximum scroll offset this geometry describes, in px.
    pub max_scroll: f64,
}

/// Map a scroll offset to a thumb rectangle.
///
/// * `track` – length of the draggable track, px
/// * `content` – full extent of the content along the axis, px
/// * `view` – extent of the visible window onto it, px
/// * `scroll` – current scroll offset, px
///
/// Degenerate inputs are absorbed rather than propagated: a zero-length track
/// yields an empty thumb, and content that fits entirely yields a thumb that
/// fills the track (Windows' "nothing to scroll" look) instead of a division
/// by zero.
pub fn thumb_geom(track: f64, content: f64, view: f64, scroll: f64) -> ThumbGeom {
    let track = if track.is_finite() && track > 0.0 {
        track
    } else {
        0.0
    };
    let max_scroll = if content > view { content - view } else { 0.0 };
    if track <= 0.0 {
        return ThumbGeom {
            off: 0.0,
            len: 0.0,
            track: 0.0,
            max_scroll,
        };
    }
    if max_scroll <= 0.0 {
        // everything visible: one full-width thumb, pinned at the start
        return ThumbGeom {
            off: 0.0,
            len: track,
            track,
            max_scroll: 0.0,
        };
    }
    // Thumb length is the visible fraction of the content, clamped so a huge
    // document still leaves something grabbable.
    let floor = (MIN_THUMB as f64).min(track);
    let len = (track * view / content).clamp(floor, track);
    let free = (track - len).max(0.0);
    let scroll = if scroll.is_finite() {
        scroll.clamp(0.0, max_scroll)
    } else {
        0.0
    };
    let off = if free <= 0.0 {
        0.0
    } else {
        (free * scroll / max_scroll).clamp(0.0, free)
    };
    ThumbGeom {
        off,
        len,
        track,
        max_scroll,
    }
}

/// Inverse of [`thumb_geom`]: the scroll offset that puts the thumb's leading
/// edge at `off` px into the track.
///
/// `off` is clamped into the track, so dragging past either end simply pins the
/// scroll at 0 / `max_scroll` instead of running away.
pub fn scroll_of_thumb(g: &ThumbGeom, off: f64) -> f64 {
    let free = (g.track - g.len).max(0.0);
    if free <= 0.0 || g.max_scroll <= 0.0 {
        return 0.0;
    }
    if !off.is_finite() {
        return g.max_scroll;
    }
    (g.max_scroll * off / free).clamp(0.0, g.max_scroll)
}

/// One addressable part of a scrollbar, for hover / press feedback.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SbPart {
    VThumb,
    HThumb,
    VUp,
    VDown,
    HLeft,
    HRight,
}

impl SbPart {
    /// `true` for the two thumb variants.
    pub fn is_thumb(self) -> bool {
        matches!(self, SbPart::VThumb | SbPart::HThumb)
    }
}

/// Which bar the pointer is dragging, and where inside the thumb it grabbed.
///
/// `grab` is what makes a drag feel right: without it the thumb would snap its
/// leading edge to the pointer on the first move, so grabbing near the bottom
/// of the thumb would jump the view down by a thumb-length.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SbDrag {
    /// `true` = vertical bar, `false` = horizontal
    pub vertical: bool,
    /// Distance from the thumb's leading edge to the pointer at mousedown, px
    pub grab: f64,
}

/// The **input-side** thumb while a drag is live: the offset derived from the
/// newest pointer sample, not from `thumb_geom(scroll_top)`.
///
/// This is the decoupling that makes the thumb track the pointer even when the
/// content frame is still in flight (the same property a native Win32
/// scrollbar gets for free — user32 draws the thumb inside the mouse-move
/// handling, and the text repaints later). Rendered by the thumb's own cheap
/// scope (a dedicated signal, not `Signal<EditorState>`); once the drag ends
/// the thumb falls back to the content side, whose `scroll_top` was landed
/// from the same samples — the two coincide, so there is no jump.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SbLive {
    /// `true` = vertical bar
    pub vertical: bool,
    /// Pointer-derived thumb offset along the track, already clamped, px
    pub off: f64,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn near(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-6
    }

    #[test]
    fn thumb_fills_track_when_everything_fits() {
        let g = thumb_geom(600.0, 400.0, 600.0, 0.0);
        assert_eq!(g.len, 600.0);
        assert_eq!(g.off, 0.0);
        assert_eq!(g.max_scroll, 0.0);
        assert_eq!(scroll_of_thumb(&g, 300.0), 0.0);
    }

    #[test]
    fn thumb_length_is_the_visible_fraction() {
        // half the content visible -> half the track
        let g = thumb_geom(1000.0, 2000.0, 1000.0, 0.0);
        assert!(near(g.len, 500.0), "len = {}", g.len);
        assert_eq!(g.max_scroll, 1000.0);
    }

    #[test]
    fn thumb_stops_shrinking_at_the_floor() {
        // 1e6 px of content in a 1000px window: the proportional thumb would
        // be 1px, which is ungrabbable
        let g = thumb_geom(1000.0, 1e6, 1000.0, 0.0);
        assert!(near(g.len, MIN_THUMB as f64), "len = {}", g.len);
    }

    #[test]
    fn scroll_maps_to_the_whole_track() {
        let (track, content, view) = (1000.0, 5000.0, 1000.0);
        let g = thumb_geom(track, content, view, 0.0);
        assert!(near(g.off, 0.0));
        let at_end = thumb_geom(track, content, view, 4000.0);
        assert!(
            near(at_end.off, g.track - g.len),
            "end off {} vs {}",
            at_end.off,
            g.track - g.len
        );
        let mid = thumb_geom(track, content, view, 2000.0);
        assert!(near(mid.off, (g.track - g.len) / 2.0), "mid {}", mid.off);
    }

    #[test]
    fn round_trip_is_stable() {
        let cases: [(f64, f64, f64); 4] = [
            (1000.0, 5000.0, 1000.0),
            (800.0, 812.0, 800.0),
            (600.0, 1e9, 600.0),
            (200.0, 400.0, 199.0),
        ];
        for (track, content, view) in cases {
            let max = (content - view).max(0.0);
            for frac in [0.0f64, 0.1, 0.25, 0.5, 0.75, 0.9, 1.0] {
                let s = max * frac;
                let g = thumb_geom(track, content, view, s);
                let back = scroll_of_thumb(&g, g.off);
                assert!(
                    (back - s).abs() < 1e-3,
                    "round trip {s} -> {} for {track}/{content}/{view}",
                    back
                );
            }
        }
    }

    #[test]
    fn dragging_outside_the_track_pins_to_the_ends() {
        let g = thumb_geom(1000.0, 5000.0, 1000.0, 0.0);
        assert_eq!(scroll_of_thumb(&g, -500.0), 0.0);
        assert_eq!(scroll_of_thumb(&g, 1e9), 4000.0);
    }

    #[test]
    fn degenerate_tracks_do_not_produce_nans() {
        for (track, content, view, scroll) in [
            (0.0, 100.0, 50.0, 10.0),
            (-5.0, 100.0, 50.0, 10.0),
            (100.0, 0.0, 0.0, 0.0),
            (100.0, 50.0, 50.0, f64::NAN),
            (100.0, 100.0, 50.0, f64::INFINITY),
        ] {
            let g = thumb_geom(track, content, view, scroll);
            assert!(g.off.is_finite(), "off for {track}/{content}/{view}");
            assert!(g.len.is_finite(), "len for {track}/{content}/{view}");
            assert!(g.len >= 0.0 && g.len <= track.max(0.0));
            let back = scroll_of_thumb(&g, g.off);
            assert!(back.is_finite(), "inverse for {track}/{content}/{view}");
        }
    }

    #[test]
    fn thumb_never_leaves_the_track() {
        // a thumb that overflows its track is the classic off-by-floor bug:
        // at max scroll it must stop exactly at the end
        for track in [40.0_f64, 100.0, 600.0, 1200.0] {
            for content in [610.0_f64, 1e4, 1e7] {
                let view = 600.0;
                let max = content - view;
                let g = thumb_geom(track, content, view, max);
                assert!(
                    g.off + g.len <= track + 1e-9,
                    "thumb overflows: off {} + len {} > track {}",
                    g.off,
                    g.len,
                    track
                );
            }
        }
    }
}
