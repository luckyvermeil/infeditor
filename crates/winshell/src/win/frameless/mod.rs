//! Window-level glue for the frameless window（`crate::win::frameless`）。
//!
//! The window is created with `decorations(false)` (see `main.rs`), so there is
//! no OS title bar and every native affordance has to be re-created: move,
//! resize, snap, minimize, maximize, close.
//!
//! # Move and resize go through winit, not through us
//!
//! The one thing that must **not** be hand-rolled is the drag itself. A
//! borderless window that follows the cursor with its own `SetWindowPos` loop
//! loses edge snapping, drag-to-top maximize, the correct resize cursor and the
//! OS's own gesture continuity — and it stutters, because every step forces a
//! full relayout of the page.
//!
//! `Window::drag_window` / `drag_resize_window` are winit's own entry points for
//! exactly this situation. On Windows they do the canonical thing
//! (`ReleaseCapture` then `PostMessage(WM_NCLBUTTONDOWN, HTCAPTION|HTxxx, …)`),
//! which hands the gesture to the **system** modal move/size loop: smooth,
//! snapped, and with the right cursor for free. They also hop onto the event
//! loop thread first, so nothing here runs a modal loop inside a DOM handler.
//!
//! All this module needs for that is the window handle, which the app root
//! parks in a process-wide slot ([`set_window`]). It is not a field of
//! `EditorState` on purpose: the modal loop runs while the caller's stack
//! unwinds, and a `Signal` borrow must never be alive across it.
//!
//! The `HWND` slot ([`current_hwnd`]) is kept for the one place that needs a raw
//! handle: the Win32 `TrackPopupMenu` right-click menu.

use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use dioxus_native::winit::window::{ResizeDirection, Window};

// Platform implementation, one of two: this module is the
// **platform-agnostic facade** — not a single public function has a cfg, and
// there is no HWND in the parameters (the window resolves from the [`WINDOW`]
// slot). Both implementation modules have matching function signatures; see
// `win32.rs` (Win32 API) and `winit_impl.rs` (winit's cross-platform
// capabilities).
#[cfg(windows)]
mod win32;
#[cfg(not(windows))]
mod winit_impl;

#[cfg(windows)]
use win32 as os;
#[cfg(not(windows))]
use winit_impl as os;

/// Win32 non-client hit-test codes for the drag/resize loops (`WM_NCLBUTTONDOWN`
/// `wParam`). Numbers per `winuser.h`.
#[cfg(windows)]
mod ht {
    pub const HTCAPTION: isize = 2;
    pub const HTLEFT: isize = 10;
    pub const HTRIGHT: isize = 11;
    pub const HTTOP: isize = 12;
    pub const HTTOPLEFT: isize = 13;
    pub const HTTOPRIGHT: isize = 14;
    pub const HTBOTTOM: isize = 15;
    pub const HTBOTTOMLEFT: isize = 16;
    pub const HTBOTTOMRIGHT: isize = 17;
}
#[cfg(windows)]
use ht::*;

/// The main window, stashed once by the app root.
static WINDOW: OnceLock<Arc<dyn Window>> = OnceLock::new();

/// Record the main window. Called once from `EditorApp`.
pub fn set_window(window: Arc<dyn Window>) {
    let _ = WINDOW.set(window);
}

/// The main window, if it has been registered yet.
pub fn window() -> Option<&'static Arc<dyn Window>> {
    WINDOW.get()
}

/// The main window handle, stashed once by the app root.
///
/// Used by the Win32 right-click menu (`TrackPopupMenu` needs an owner
/// `HWND`).
static CURRENT_HWND: AtomicIsize = AtomicIsize::new(0);

/// Record the main window handle. Called once from `EditorApp`.
pub fn set_hwnd(hwnd: isize) {
    CURRENT_HWND.store(hwnd, Ordering::Relaxed);
}

/// The main window handle (0 before [`set_hwnd`] runs).
pub fn current_hwnd() -> isize {
    CURRENT_HWND.load(Ordering::Relaxed)
}

/// The main window's **client area** top-left in screen logical coordinates
/// (for defaulting popups to centered placement).
///
/// After revision three the client differs from the outer frame by one native
/// border ([`frame_border_phys`] physical px on left/right/bottom); a popup's
/// anchor is computed against the client content, so the client origin
/// ([`client_origin_physical`]) must be used, not `outer_position`.
pub fn main_window_pos() -> (f32, f32) {
    let scale = window()
        .map(|w| w.scale_factor() as f32)
        .filter(|s| *s > 0.0)
        .unwrap_or(1.0);
    match client_origin_physical() {
        Some((x, y)) => (x as f32 / scale, y as f32 / scale),
        None => (0.0, 0.0),
    }
}

/// The main window's **client area** top-left in screen physical pixels.
///
/// After revision three the client ≠ the outer frame (`WM_NCCALCSIZE` insets
/// by one native border width on left/right/bottom); this is the anchor for
/// every "DOM client coordinates ↔ screen coordinates" conversion.
pub fn client_origin_physical() -> Option<(i32, i32)> {
    os::client_origin_physical()
}

/// The main window's client rect on screen (physical pixels
/// `(left, top, width, height)`).
///
/// Used for "screen coordinates − client rect" conversions (soft-keyboard
/// avoidance: the client bottom = top+height; the outer frame bottom must not
/// be used — that would add one extra bottom border).
pub fn client_rect_screen() -> Option<(i32, i32, i32, i32)> {
    os::client_rect_screen()
}

/// Whether `code` is one of the system's resize hit codes (`HTLEFT`..
/// `HTBOTTOMRIGHT`). The system answers these natively for the real
/// non-client borders (revision 3); when it does, the gesture must be armed
/// as a **resize** so `WM_ENTERSIZEMOVE` can tell it from a caption move.
#[cfg(windows)]
pub fn is_resize_ht(code: isize) -> bool {
    (HTLEFT..=HTBOTTOMRIGHT).contains(&code)
}

/// Begin a native move drag (pressing the title bar background).
///
/// Returns once the OS has been asked to start the gesture; the drag itself
/// runs in the system's own modal loop, exactly like a real title bar.
pub fn start_move() {
    // The snap module has to know what kind of gesture is starting: the
    // sizing messages the system sends during its loop do not say. (Windows
    // only — the non-Windows arm_move is a no-op.)
    crate::win::snap::arm_move();
    os::start_move();
}

/// The `HWND` of a winit window as an `isize` (0 when unavailable).
///
/// Platform-agnostic: raw-window-handle's enums compile on every platform; a
/// non-Win32 handle returns 0 — the hwnd recorded by touch gestures is just a
/// constant-0 placeholder on non-Windows.
fn hwnd_of_w(window: &dyn Window) -> isize {
    use dioxus_native::winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
    match window.window_handle() {
        Ok(h) => match h.as_raw() {
            RawWindowHandle::Win32(w) => w.hwnd.get(),
            _ => 0,
        },
        Err(_) => 0,
    }
}

// ---------------------------------------------------------------------------
// Double-click on the caption
// ---------------------------------------------------------------------------

/// Client position and instant of the previous press on the title bar.
static LAST_PRESS: Mutex<Option<(std::time::Instant, i32, i32)>> = Mutex::new(None);

/// Does this press complete a double-click on the title bar?
///
/// Blitz's own `dblclick` never reached the handler: each press on the title
/// bar starts the system's move loop, and the synthetic mouse-up winit posts
/// when the loop ends lands between the two presses, so the click pair that
/// `dblclick` is built from never lines up. Detecting the pair here — before
/// arming the gesture — is independent of that, and a double press on the
/// caption must *not* start a move (Windows maximizes instead, and so do we).
///
/// The pair is consumed when it matches, so a third press right after starts
/// fresh: Windows counts clicks in pairs too.
pub fn is_double_press(x: i32, y: i32) -> bool {
    const WITHIN: std::time::Duration = std::time::Duration::from_millis(450);
    /// How far apart two presses may be and still count as one double click.
    ///
    /// Generous on purpose: a finger double-tap jitters far more than a mouse
    /// double-click (easily 8-10 CSS px between the two presses), and a missed
    /// detection here means the second tap starts a window move instead of
    /// maximizing. The pointer-event data does not expose the pointer type, so
    /// one tolerance covers both inputs — 10px is still tight enough that two
    /// genuinely distinct clicks never collide.
    const NEAR: i32 = 10;

    let mut last = LAST_PRESS.lock().unwrap_or_else(|e| e.into_inner());
    let now = std::time::Instant::now();
    let hit = matches!(
        *last,
        Some((t, px, py))
            if now.duration_since(t) < WITHIN && (x - px).abs() <= NEAR && (y - py).abs() <= NEAR
    );
    *last = if hit { None } else { Some((now, x, y)) };
    hit
}

// ---------------------------------------------------------------------------
// Border hit-testing: the resize border (WM_NCHITTEST)
// ---------------------------------------------------------------------------
//
// The window keeps the standard `WS_THICKFRAME` styles, and `ui::snap` takes
// over `WM_NCCALCSIZE` so the left/right/bottom resize borders are *real*
// non-client area (DefWindowProc answers the hit test natively — exactly like
// a caption'd window). Only the top edge needs an in-client band, because the
// client area is flush with the window top (custom title bar):
// `top_band_hit_arbitrated`. Mouse and touch share the system paths (a
// contact that hits an `HT*` band becomes the system's own `WM_NCPOINTER*`
// move/size loop — winit only intercepts `WM_POINTER*`), the resize cursor
// comes for free from `WM_SETCURSOR`, and the border never overlaps content.

/// The native resize border width (**physical px**, scaled by the main
/// window's DPI): `SM_CXSIZEFRAME + SM_CXPADDEDBORDER` — 8px at 96 DPI,
/// 12px at 150%, the same width as every native window's invisible resize
/// border. Falls back to 2px on failure, never to 0 — "no border" is exactly
/// the root cause of "no resize cursor on the edge".
#[cfg(windows)]
pub fn frame_border_phys() -> i32 {
    use windows_sys::Win32::UI::HiDpi::GetDpiForWindow;
    use windows_sys::Win32::UI::HiDpi::GetSystemMetricsForDpi;
    use windows_sys::Win32::UI::WindowsAndMessaging::{SM_CXPADDEDBORDER, SM_CXSIZEFRAME};
    let dpi = unsafe { GetDpiForWindow(current_hwnd() as _) };
    let dpi = if dpi > 0 { dpi } else { 96 };
    let frame = unsafe { GetSystemMetricsForDpi(SM_CXSIZEFRAME, dpi) }
        + unsafe { GetSystemMetricsForDpi(SM_CXPADDEDBORDER, dpi) };
    frame.max(2)
}

#[cfg(not(windows))]
pub fn frame_border_phys() -> i32 {
    8
}

/// The resize hit band (CSS px) = the native border width.
///
/// After revision three, the left/right/bottom borders are **real non-client
/// area** (inset by `WM_NCCALCSIZE`, see `snap::nc_calc_size`) and the system
/// answers their hit tests natively; only the **top edge** needs a hand-drawn
/// hit band inside the client (the client is flush with the top, there is no
/// top non-client area) — its width matches the native border.
/// Returns `(edge, corner)`, which are equal (the native border is one uniform
/// strip; corners come from the overlap of two edge bands).
#[cfg(windows)]
pub fn resize_band_css() -> (f32, f32) {
    use windows_sys::Win32::UI::HiDpi::GetDpiForWindow;
    let dpi = unsafe { GetDpiForWindow(current_hwnd() as _) } as f32;
    let scale = if dpi > 0.0 { dpi / 96.0 } else { 1.0 };
    let css = frame_border_phys() as f32 / scale;
    (css, css)
}

/// The fallback band width on non-Windows (compile-only; frameless resize is
/// the Win32 path).
#[cfg(not(windows))]
pub fn resize_band_css() -> (f32, f32) {
    (8.0, 8.0)
}

/// The interactive-component rect set (client CSS px `[x, y, w, h]`).
///
/// Hit testing runs inside the window procedure: it **must not** touch dioxus
/// `Signal`s, so the layout layer (the app-side `EditorState::relayout` — all
/// geometry lives there) pushes rects into this slot, and the `WM_NCHITTEST`
/// top-band branch of `ui::snap` reads it.
///
/// Yielding rule (revision three): the left/right/bottom resize borders are
/// **real non-client area** with zero overlap with content, so no yielding is
/// needed; the only hit band still overlapping content is the **top edge**
/// (the client is flush with the top; there is no top non-client area), so
/// this rect set only receives the title-bar window button cluster — the
/// buttons take priority as a whole and even yield the top resize band (the
/// close button's top pixel must still be the close button; a native
/// window's HTCLOSE likewise beats HTTOP).
static HIT_EXEMPT: Mutex<Vec<[f32; 4]>> = Mutex::new(Vec::new());

/// Write the interactive-component rect set (the layout layer calls it on
/// every `relayout`; unchanged content is not rewritten).
pub fn set_hit_exempt(rects: Vec<[f32; 4]>) {
    let mut g = HIT_EXEMPT.lock().unwrap_or_else(|e| e.into_inner());
    if *g != rects {
        *g = rects;
    }
}

/// Borrow-read the interactive-component rect set (`WM_NCHITTEST` runs it on
/// every pointer move; the closure form avoids allocating a snapshot).
pub fn with_hit_exempt<T>(f: impl FnOnce(&[[f32; 4]]) -> T) -> T {
    let g = HIT_EXEMPT.lock().unwrap_or_else(|e| e.into_inner());
    f(&g)
}

/// The hand-drawn top-edge hit band (pure function, unit-testable).
///
/// The client is flush with the top (only 1px of shadow non-client area up
/// there), so the top edge's resize grab is a narrow band at the client's
/// outermost strip, as wide as the native border (`frame`, physical px); the
/// top-left/top-right corner grabs are the junctions of the top band with the
/// real left/right borders, equally `frame` wide — consistent with a native
/// window's `frame×frame` top corners. `(x, y)` are client physical
/// coordinates and `w` is the client's physical width.
///
/// Returns `None` = this point is not in the top band (left/right/bottom
/// resize is natively hit-tested by the real non-client area).
pub fn top_band_hit(x: i32, y: i32, w: i32, frame: i32) -> Option<ResizeDirection> {
    if y >= frame {
        return None;
    }
    if x < frame {
        Some(ResizeDirection::NorthWest)
    } else if x >= w - frame {
        Some(ResizeDirection::NorthEast)
    } else {
        Some(ResizeDirection::North)
    }
}

/// Top-band hit + the yielding-component arbitration (no Win32 dependency,
/// unit-testable).
///
/// A hit inside a yielding rect (the title-bar window button cluster) →
/// `None` (HTCLIENT): the buttons take priority as a whole and even yield the
/// top resize band; otherwise → the top-band direction. `exempt`'s rects are
/// client CSS px, converted by `scale`.
pub fn top_band_hit_arbitrated(
    x: i32,
    y: i32,
    w: i32,
    frame: i32,
    scale: f32,
    exempt: &[[f32; 4]],
) -> Option<ResizeDirection> {
    let dir = top_band_hit(x, y, w, frame)?;
    if exempt.iter().any(|r| in_exempt(*r, x, y, scale)) {
        return None;
    }
    Some(dir)
}

/// Does the client CSS px rect `(x, y, w, h)` contain the physical pixel
/// point `(px, py)`?
///
/// Both ends are converted then `round`ed (rather than converting the width
/// and rounding that), so that rects at 150% DPI match the pixels the layout
/// layer painted exactly.
fn in_exempt(r: [f32; 4], px: i32, py: i32, scale: f32) -> bool {
    let x0 = (r[0] * scale).round() as i32;
    let y0 = (r[1] * scale).round() as i32;
    let x1 = ((r[0] + r[2]) * scale).round() as i32;
    let y1 = ((r[1] + r[3]) * scale).round() as i32;
    px >= x0 && px < x1 && py >= y0 && py < y1
}

/// `ResizeDirection` → the non-client hit code returned by
/// `WM_NCHITTEST`.
///
/// Once the corresponding code is returned the system takes over the gesture:
/// correct cursor shape, Snap, Esc cancellation are all system behaviors
/// (`WM_SETCURSOR` reads the low word of the hit code, so the resize cursor
/// comes for free).
#[cfg(windows)]
pub fn resize_ht_code(dir: ResizeDirection) -> isize {
    match dir {
        ResizeDirection::East => HTRIGHT,
        ResizeDirection::North => HTTOP,
        ResizeDirection::NorthEast => HTTOPRIGHT,
        ResizeDirection::NorthWest => HTTOPLEFT,
        ResizeDirection::South => HTBOTTOM,
        ResizeDirection::SouthEast => HTBOTTOMRIGHT,
        ResizeDirection::SouthWest => HTBOTTOMLEFT,
        ResizeDirection::West => HTLEFT,
    }
}

// ---------------------------------------------------------------------------
// Touch gestures (custom move loop)
// ---------------------------------------------------------------------------

/// A touch displacement on the title bar only counts as "dragging the
/// window" beyond this distance (physical px); below it, it is a tap: a
/// finger at rest jitters 2-3px easily, so without a threshold every tap on
/// the title bar would drift the window.
#[cfg_attr(not(windows), allow(dead_code))] // The consumer is the Windows follow-the-hand loop
const MOVE_TAP_PX: f64 = 4.0;

/// The threshold (physical px) for dragging a maximized window down to
/// restore it, larger than [`MOVE_TAP_PX`]: observed touchscreen tap drift is
/// around 10px (150% scaling; see `ui::touch::SLOP`'s forensics), and a mere
/// jitter yanking a maximized window down would be too unfair.
#[cfg_attr(not(windows), allow(dead_code))] // The consumer is the Windows follow-the-hand loop
const RESTORE_DRAG_PX: f64 = 12.0;

/// The anchor state of a touch window gesture (all in screen physical pixels,
/// same coordinate system as `GetCursorPos` / `SetWindowPos`; `sx/sy` are
/// additionally stored as f64 to preserve sub-pixel precision).
///
/// Only title-bar dragging remains: left/right/bottom edge resize goes
/// through the real non-client area's system NC gesture chain and the top edge
/// goes through the same chain via the hand-drawn hit band (see `ui::snap`'s
/// `WM_NCHITTEST` / `WM_NCCALCSIZE` branches), so no custom resize loop is
/// needed — winit only intercepts `WM_POINTER*` and never eats
/// `WM_NCPOINTER*`, so the system's NC gesture loop runs as usual. The
/// consumers (follow-the-hand loop / snap settlement) are all on the
/// cfg(windows) path.
#[cfg_attr(not(windows), allow(dead_code))]
struct TouchGesture {
    /// The finger's screen position at the moment of press (grab offset);
    /// re-anchored to the finger's position at restore time after a maximized
    /// restore (see [`update_touch_gesture`]'s restore branch)
    sx: f64,
    sy: f64,
    /// The window's outer-frame origin at press (= the restore landing's outer
    /// origin after re-anchoring)
    ox: i32,
    oy: i32,
    /// The window's full outer-frame rect at press (a maximized restore lands
    /// by width fraction, needing right/bottom)
    orig: (i32, i32, i32, i32),
    hwnd: isize,
    /// Past the tap threshold and the window actually moved (flipped once per
    /// gesture)
    moved: bool,
    /// The window was maximized at press: dragging past the restore threshold
    /// → restore the original size and re-anchor
    started_maximized: bool,
    /// The maximized window was restored to floating mid-drag
    restored: bool,
    /// The finger has entered the screen-top snap band: the window freezes in
    /// place and lifting the finger maximizes ([`end_touch_gesture`]); leaving
    /// the snap band mid-drag resumes following
    snap_top: bool,
    /// The DOM `pointer_id` of the finger that started the gesture: lifting a
    /// second finger must not end this window drag (mouse/pen never get here;
    /// a constant-0 id is fine)
    finger: i32,
    /// The queued target window position `(x, y)` (screen physical pixels).
    ///
    /// Instead of `SetWindowPos` per event, only the target is recorded and it
    /// lands in one shot when the event loop is idle ([`flush_touch_gesture`]):
    /// one winit `WM_POINTERUPDATE` replays a whole frame's history and is
    /// split into multiple `PointerMoved`s; moving the window per event would
    /// amplify the co-dispatched `SurfaceResized` (blitz rebuilding the surface
    /// and relaying out the whole tree) N times — the root cause of drag
    /// flicker. Coordinate precision is unaffected: event coordinates are
    /// converted by winit at dispatch time using the window rect of that
    /// moment (`ScreenToClient`), and since this loop does not move the window
    /// between events, "current origin + rel" always equals the finger's
    /// screen coordinates.
    pending: Option<(i32, i32)>,
}

static TOUCH: Mutex<Option<TouchGesture>> = Mutex::new(None);
static TOUCH_ACTIVE: AtomicBool = AtomicBool::new(false);

/// Whether a touch window gesture (title-bar drag) is in progress.
///
/// The input layer uses it to route subsequent pointer moves into the gesture
/// loop; `ui::shell` also uses it to shield against interference from touch
/// emulators and similar diagnostics.
pub fn in_touch_gesture() -> bool {
    TOUCH_ACTIVE.load(Ordering::Relaxed)
}

/// Which finger started the current gesture (DOM `pointer_id`); `None` when
/// there is no gesture.
///
/// Lift events carry a `finger_id`, which decides "is this the finger that
/// drives the window drag letting go" — otherwise a second finger's tap-then-
/// lift on the title bar would cut the in-progress window drag short.
pub fn touch_gesture_finger() -> Option<i32> {
    TOUCH
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .map(|g| g.finger)
}

/// Whether the HWND-level **touch follow loop** ([`begin_touch_move`]) exists.
///
/// Windows needs its own loop: winit consumes touch as pointer input and
/// suppresses the mouse messages the system's non-client move loop relies on,
/// so a finger drag never reaches `WM_NCLBUTTONUP`. Other platforms have no
/// non-client loop to begin with — `drag_window()` already hands the gesture
/// to the window manager for every pointer type, so the generic path is used
/// and [`begin_touch_move`] must not be armed.
pub fn touch_move_loop_supported() -> bool {
    cfg!(windows)
}

/// Start a "touch-drag the whole window" gesture. `css_x` / `css_y` are the
/// press point's window client coordinates (CSS px, straight from the DOM
/// pointer event's `client_coordinates`), and `pointer_id` is the same
/// event's finger id.
///
/// Only meaningful where [`touch_move_loop_supported`] is true (the caller
/// checks it); on other platforms this returns without moving the window.
pub fn begin_touch_move(css_x: f64, css_y: f64, pointer_id: i32) {
    begin_touch(css_x, css_y, pointer_id);
}

fn begin_touch(css_x: f64, css_y: f64, pointer_id: i32) {
    // Only one window gesture at a time: a second finger pressing the title
    // bar must not open another one (its moves are dropped by
    // update_touch_gesture's finger filter), otherwise the window would jump
    // back and forth between two fingers' target positions.
    if TOUCH_ACTIVE.load(Ordering::Relaxed) {
        return;
    }
    let Some(w) = window() else {
        return;
    };
    let hwnd = hwnd_of_w(w.as_ref());
    if hwnd == 0 {
        return;
    }
    let scale = w.scale_factor();
    let scale = if scale > 0.0 { scale } else { 1.0 };
    #[cfg(windows)]
    let Some((ox, oy, oright, obottom)) = win_rect(hwnd) else {
        return;
    };
    #[cfg(not(windows))]
    let (ox, oy, oright, obottom) = (0, 0, 0, 0);
    let g = TouchGesture {
        // The anchor takes the **outer-frame** origin: `css` is
        // client-relative and `rel` is also client-relative, so when both ends
        // are in the same space the border offset cancels out in dx; and since
        // `SetWindowPos` positions the outer frame, `ox/oy` (the basis of nx)
        // must store the outer-frame origin
        sx: ox as f64 + css_x * scale,
        sy: oy as f64 + css_y * scale,
        ox,
        oy,
        orig: (ox, oy, oright, obottom),
        hwnd,
        moved: false,
        started_maximized: is_zoomed(),
        restored: false,
        snap_top: false,
        finger: pointer_id,
        pending: None,
    };
    *TOUCH.lock().unwrap_or_else(|e| e.into_inner()) = Some(g);
    TOUCH_ACTIVE.store(true, Ordering::Relaxed);
}

/// Finger move: compute the target window position and queue it
/// ([`flush_touch_gesture`] lands it). Returns whether this move produced a
/// new target position (the window is about to move).
///
/// `finger` is the move event's finger id (winit
/// `PointerSource::Touch{finger_id}`'s `into_raw`, same namespace as the DOM
/// `pointer_id()`): **only the finger that started the gesture** can advance
/// it. The window-level `PointerMoved` funnels every pointer's movement into
/// one event stream — a second finger's or a hovering pen's coordinates also
/// arrive here; without filtering, the absolute target position would get
/// dragged onto another pointer and the window would jump between several
/// positions (the root cause of window jumping).
///
/// `rel_x` / `rel_y` are the **window client physical pixel coordinates**
/// winit's `PointerMoved::position` passes through unchanged. The finger's
/// screen coordinates = the client's screen origin + that relative
/// coordinate; this function uses "outer-frame origin + rel" as an equivalent
/// anchor (taking the outer-frame origin on the begin side too, so the border
/// offset cancels in `dx = fx - sx`): event coordinates are computed with the
/// window rect of the moment the message is handled, and between those
/// moments only this loop moves the window, so the two are necessarily
/// frame-consistent — hence no `GetCursorPos` (there is no guarantee that the
/// system cursor tracks the touch point), and it cannot grab the wrong point
/// under mixed mouse/touch input.
pub fn update_touch_gesture(finger: i32, rel_x: f64, rel_y: f64) -> bool {
    let mut guard = TOUCH.lock().unwrap_or_else(|e| e.into_inner());
    let Some(g) = guard.as_mut() else {
        return false;
    };
    if g.finger != finger {
        return false;
    }
    #[cfg(windows)]
    {
        let Some((l, t, _r, _b)) = win_rect(g.hwnd) else {
            return false;
        };
        let fx = l as f64 + rel_x;
        let fy = t as f64 + rel_y;
        let dx = fx - g.sx;
        let dy = fy - g.sy;
        if !g.moved {
            // Do not touch the window within the tap threshold: a "tap on the
            // title bar" should not make the window budge
            if dx.abs() < MOVE_TAP_PX && dy.abs() < MOVE_TAP_PX {
                return false;
            }
            g.moved = true;
        }
        // Dragging a maximized window: first restore the original size (same as
        // Windows — the pointer keeps the same fraction of the maximized width
        // and the grab point lands on the caption strip), then re-anchor to the
        // finger's position at restore time and the restored outer origin;
        // subsequent moves keep following 1:1.
        if g.started_maximized && !g.restored {
            if dx.abs() < RESTORE_DRAG_PX && dy.abs() < RESTORE_DRAG_PX {
                return false;
            }
            let Some(((nl, nt), _size)) = crate::win::snap::restore_floating(
                g.hwnd,
                (g.sx.round() as i32, g.sy.round() as i32),
                g.orig,
                (fx.round() as i32, fy.round() as i32),
            ) else {
                return false;
            };
            g.restored = true;
            g.sx = fx;
            g.sy = fy;
            g.ox = nl;
            g.oy = nt;
            return true;
        }
        // Dragged into the screen-top snap band (same predicate as the system
        // drag loop, `snap::TOP_BAND`): the window freezes in place and lifting
        // the finger maximizes ([`end_touch_gesture`]); leaving the snap band
        // mid-drag immediately resumes following.
        let finger_pt = (fx.round() as i32, fy.round() as i32);
        let top_zone =
            crate::win::snap::in_top_band(finger_pt.1, crate::win::snap::work_area_top(finger_pt));
        if top_zone != g.snap_top {
            g.snap_top = top_zone;
            if top_zone {
                return true; // Entered the band: freeze (no pending landing target)
            }
            // Left the band: do not return; this event immediately resumes
            // following the hand
        }
        if g.snap_top {
            return false; // Frozen inside the band: the window stays put awaiting the lift
        }
        // Absolute target = origin at press + the finger's total displacement
        // (not incremental accumulation, so it cannot drift)
        let nx = (g.ox as f64 + dx).round() as i32;
        let ny = (g.oy as f64 + dy).round() as i32;
        // Repeated events to the same target are folded away (the current
        // position takes the queued target as truth).
        let (cx, cy) = g.pending.unwrap_or((l, t));
        if (nx, ny) == (cx, cy) {
            return false;
        }
        // Only record the target, do not move the window immediately (see
        // `TouchGesture::pending`); `flush_touch_gesture` lands it in one shot
        // each time the event loop is idle.
        g.pending = Some((nx, ny));
        g.moved = true;
        true
    }
    #[cfg(not(windows))]
    {
        let _ = (rel_x, rel_y);
        false
    }
}

/// Actually apply the queued window position (one `SetWindowPos`).
///
/// Called once each time the event loop is idle
/// (`shell::ShellApp::about_to_wait`), plus one backstop call when the gesture
/// ends. The reason for queueing is documented on `TouchGesture::pending`.
pub fn flush_touch_gesture() {
    let mut guard = TOUCH.lock().unwrap_or_else(|e| e.into_inner());
    let Some(g) = guard.as_mut() else {
        return;
    };
    let Some((x, y)) = g.pending.take() else {
        return;
    };
    #[cfg(windows)]
    {
        move_window(g.hwnd, x, y);
    }
    #[cfg(not(windows))]
    {
        let _ = (x, y);
    }
}

/// End the current touch gesture (called whenever any finger lifts).
///
/// `cancel` = the gesture did not end with a normal lift (the system reclaimed
/// the pointer / the window lost focus): only settle the books, do not execute
/// the snap semantics — the intent of sending the finger to the top band is
/// voided along with the cancel.
///
/// On a normal lift: having been dragged into the screen-top snap band
/// (`TouchGesture::snap_top`) maximizes, the same as the system drag loop's
/// snap (`snap::end`'s handling of `WM_EXITSIZEMOVE`).
pub fn end_touch_gesture(cancel: bool) {
    // Backstop-land the final displacement: otherwise the window would
    // "shrink back" at the very end, exactly the not-following-the-hand feel.
    flush_touch_gesture();
    let mut guard = TOUCH.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(g) = guard.as_ref() {
        if !cancel && g.snap_top && g.moved {
            // The gesture only ever happens on the main window's title bar —
            // the maximize target is the main window itself.
            os::maximize();
        }
    }
    *guard = None;
    TOUCH_ACTIVE.store(false, Ordering::Relaxed);
}

/// Move any winit window to screen physical coordinates `(x, y)` (position
/// only, no resize).
///
/// For popup soft-keyboard avoidance: the shell's popup table only holds
/// `Arc<dyn Window>`, with no HWND slot like the main window's. Same
/// coordinate system as the touch gestures (physical pixels); internally it is
/// `move_window`.
pub fn move_window_screen(window: &dyn Window, x: i32, y: i32) {
    os::move_window_screen(window, x, y);
}

/// Position only, no resize (title-bar dragging / popup avoidance).
///
/// **`SWP_NOREDRAW` is the key to flicker-free dragging**: without it, every
/// `SetWindowPos` makes the system mark the whole client area dirty ("the
/// window moved, repaint"), so the follow-the-hand loop triggers a full-window
/// repaint per move — before the repaint finishes the window has already moved
/// to the next position, and the screen shows old/new frames alternating in a
/// flicker; a large document adds one full layout pass on top. With DWM
/// composition enabled the window's pixel surface **travels with the window**,
/// so a pure translation needs no redraw at all: with repaints off, moving the
/// window becomes a pure compositor translation (what the system's own drag
/// loop does internally too).
///
/// Popup avoidance (soft keyboard) takes the same path: the popup is woken by
/// `request_redraw` to re-layout itself; moving the window itself needs no
/// incidental repaint.
#[cfg(windows)]
fn move_window(hwnd: isize, x: i32, y: i32) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        SetWindowPos, SWP_NOACTIVATE, SWP_NOREDRAW, SWP_NOSIZE, SWP_NOZORDER,
    };
    unsafe {
        SetWindowPos(
            hwnd as _,
            std::ptr::null_mut(),
            x,
            y,
            0,
            0,
            SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_NOREDRAW,
        )
    };
}

/// The window's outer frame (screen physical pixels):
/// `(left, top, right, bottom)`, `None` on failure.
#[cfg(windows)]
fn win_rect(hwnd: isize) -> Option<(i32, i32, i32, i32)> {
    use windows_sys::Win32::Foundation::RECT;
    use windows_sys::Win32::UI::WindowsAndMessaging::GetWindowRect;
    let mut r: RECT = unsafe { std::mem::zeroed() };
    if unsafe { GetWindowRect(hwnd as _, &mut r) } == 0 {
        return None;
    }
    Some((r.left, r.top, r.right, r.bottom))
}

// ---------------------------------------------------------------------------
// Window-operation facade (platform-agnostic public API, routed to `os::`)
// ---------------------------------------------------------------------------
//
// These functions **take no HWND parameter**: the window resolves from the
// [`WINDOW`] slot (registered by the app root on its first frame). The handle
// is a Win32 concept and should not appear in cross-platform signatures; a
// caller that genuinely needs a raw handle (TrackPopupMenu, PopupShow's owner
// parameter) goes through [`current_hwnd`].

/// Minimize to the taskbar.
pub fn minimize() {
    os::minimize();
}

/// Maximize the window.
pub fn maximize() {
    os::maximize();
}

/// Restore (un-maximize / un-minimize).
pub fn restore() {
    os::restore();
}

/// Maximize when normal, restore when maximized.
pub fn toggle_maximize(maximized: bool) {
    if maximized {
        restore()
    } else {
        maximize()
    }
}

/// Ask the window to close.
///
/// Windows posts `WM_CLOSE`, which winit turns into `CloseRequested` — the
/// app's own verdict path. Other platforms have no message to post: winit
/// offers no "synthesize CloseRequested" API, so the shell layer installs a
/// hook ([`set_close_hook`]) at launch that routes the request through the
/// event loop as `ShellEvt::CloseMain`, which the shell turns into a
/// synthetic `CloseRequested` for the main window — the exact same path a
/// real `×` / Alt+F4 takes (unsaved-documents prompt included).
pub fn close() {
    os::close();
}

/// The slot holding the "request close the main window" hook. Present on both
/// platforms: the shell layer injects it unconditionally at `launch` (routing
/// the request into `ShellEvt::CloseMain`, from which the shell synthesizes
/// the main window's `CloseRequested` and the app-layer verdict runs —
/// dirty-document confirmation dialog / clean immediate exit).
///
/// Windows's [`close`] **never reads it** (`PostMessageW(WM_CLOSE)` already
/// produces a real CloseRequested); the POSIX implementation reads it. A hook
/// rather than calling `shell::send` directly: the `win` layer does not depend
/// on the shell layer's event enum — the dependency direction stays shell →
/// win.
static CLOSE_HOOK: OnceLock<fn()> = OnceLock::new();

/// Install the close hook (called once from `shell::launch`).
pub fn set_close_hook(f: fn()) {
    let _ = CLOSE_HOOK.set(f);
}

/// Whether the window is currently maximized.
///
/// Used to keep the merged title bar's restore glyph honest when the window is
/// maximized by something other than our own button (Win+Up, snap, the
/// taskbar), and to disable the resize edges while maximized.
pub fn is_zoomed() -> bool {
    os::is_zoomed()
}

/// Keep the window above every other window (topmost).
///
/// Returns whether the change was applied, so the caller can report a failure
/// (a modal dialog owns the window's thread, and `SetWindowPos` on the
/// topmost band is refused while one is up) instead of drawing a check mark
/// that does nothing.
pub fn set_topmost(on: bool) -> bool {
    os::set_topmost(on)
}

/// Whether the window is currently in the topmost band.
pub fn is_topmost() -> bool {
    os::is_topmost()
}

/// Ensure the process is per-monitor-v2 DPI aware.
///
/// winit does this too when creating the event loop (`become_dpi_aware`),
/// but that is already later than the top of `main` — if any earlier code
/// triggered the awareness "lock" (manifest file, compatibility settings),
/// this explicit call guarantees we learn which one we got and can log it:
/// for problems like "the Save As dialog is blurry", the first step is to
/// establish which awareness level is actually in effect.
///
/// Returns a description of the effective path (logging only).
pub fn ensure_dpi_awareness() -> &'static str {
    os::ensure_dpi_awareness()
}

/// Give keyboard focus back to the main window (the find child window calls
/// it on `Focused(true)`: the child window never
/// holds the keyboard; IME/input always go through the main window's
/// composer).
pub fn focus_main() {
    os::focus_main();
}

/// Bring the window to the front and give it keyboard focus.
///
/// A popup menu has to be owned by the foreground window (the app's context
/// menu), and so does the "always on top" toggle
/// — clicking the title bar of a background window expects it to come forward.
pub fn activate() {
    os::activate();
}

/// Update the window title (for the taskbar, Alt+Tab, and the system menu;
/// the caller decides when the text changes — it is always written here, and
/// a no-change `SetWindowTextW` is cheap).
pub fn set_title(title: &str) {
    os::set_title(title);
}

/// Begin the system's own **move** loop (window menu → Move).
///
/// Unlike [`start_move`], which is armed from a title-bar press, this starts the
/// gesture at the *pointer* with no button held — which is what "Move" in the
/// window menu means.
pub fn start_system_move() {
    os::start_system_move();
}

/// Begin the system's own **resize** loop (window menu → Size).
///
/// Same mechanism as [`start_system_move`], with the bottom-right corner as the
/// grab handle so the pointer drives the same diagonal drag the system shows
/// for "Size" on any window.
pub fn start_system_resize() {
    os::start_system_resize();
}

/// Begin a resize gesture from a specific edge/corner.
///
/// This is the entry point for the **DOM resize strips** the app renders on
/// platforms without a native non-client hit test (see
/// `ui::components::resize`): Windows answers `WM_NCHITTEST` itself (the
/// left/right/bottom borders are real non-client area and `ui::snap` handles
/// the top band), while winit platforms have no such hook — a `decorations
/// (false)` window there has no border to grab at all.
///
/// On Windows this maps to the same `WM_NCLBUTTONDOWN` drag as the menu's
/// "Size"; it is compiled on every platform so the UI component can call it
/// unconditionally (the component simply never renders on Windows).
pub fn start_resize(dir: ResizeDirection) {
    os::start_resize(dir);
}

// Windows-exclusive dialog owner semantics (rfd modality, popup ownership);
// other platforms have no corresponding concept, and all call sites are under
// `cfg(windows)`.
#[cfg(windows)]
pub use self::win32::{attach_owner, dialog_owner, DialogOwner};
