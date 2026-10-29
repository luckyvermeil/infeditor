//! Snap behaviour for the frameless window: drag the title bar to the top edge
//! to maximize, and drag a maximized window away to get it back.
//!
//! # Why this module exists
//!
//! The move/resize **gesture** belongs to the system. [`crate::win::frameless`]
//! posts `WM_NCLBUTTONDOWN` so `DefWindowProc` runs its own modal move loop —
//! that is what keeps dragging smooth: the app does no layout and no painting
//! per frame, the window simply follows the cursor. Taking the gesture back
//! (a hand-rolled `SetCapture` + `SetWindowPos` loop) was tried before and it
//! stutters, because every mouse move becomes a relayout of the whole page.
//!
//! What the system does *not* do for us is the shell's snap rules. The window
//! carries the full `WS_OVERLAPPEDWINDOW` styles (`WS_THICKFRAME`,
//! `WS_MAXIMIZEBOX`) and dragging a caption to the top edge
//! maximizes it, then dragging it back down restores it — but the same gesture
//! on our window does neither, because our move loop was entered by a posted
//! message instead of a real non-client hit test, so the shell never sees a
//! caption grab.
//!
//! This module closes that gap without touching the gesture: it subclasses the
//! window procedure and observes the three messages the system already sends
//! while *its* loop runs.
//!
//! * `WM_ENTERSIZEMOVE` — where the gesture started, and whether the window
//!   was maximized at that moment.
//! * `WM_MOVING` — one call per step with the *proposed* rectangle. The
//!   pointer is read fresh, so the top band can be watched even before the
//!   window has moved, and the rectangle can be rewritten when the system's
//!   proposal is wrong (it keeps the offset it captured for the maximized
//!   window).
//! * `WM_EXITSIZEMOVE` — apply the pending maximize, unless the drag was
//!   cancelled with Escape (what Windows does).
//!
//! Every other message goes straight to winit's window procedure, so moving,
//! the menus and the window buttons behave exactly as before. The gesture
//! stays native: correct cursor, correct cursor shape, no app work per frame.
//!
//! It also owns the native-border client calculation here (see
//! `nc_calc_size`): `WM_NCCALCSIZE` insets the client by the native frame
//! width on left/right/bottom, so those resize borders are *real* non-client
//! area — DefWindowProc answers their hit tests natively, exactly like a
//! caption'd window (Chrome / Windows Terminal recipe). Only the top edge,
//! where the client stays flush with the window top (custom title bar),
//! needs an in-client hit band (`top_band_code`); the module also arms the
//! resize gesture when the system answers a native border hit, so
//! `WM_ENTERSIZEMOVE` can tell resize from caption move.

#![cfg_attr(not(windows), allow(dead_code))] // Snap settlement is only implemented on Windows

use std::sync::atomic::{AtomicIsize, AtomicU8, Ordering};
use std::sync::Mutex;

/// Which gesture the system is about to run. Armed by [`crate::win::frameless`]
/// immediately before the message that starts it is posted (`start_move` from
/// the title bar; the `WM_NCHITTEST` hit on an edge band) and consumed by
/// `WM_ENTERSIZEMOVE` — the sizing messages themselves do not say whether they
/// came from the title bar or from an edge band.
const NONE: u8 = 0;
const MOVE: u8 = 1;
const RESIZE: u8 = 2;

/// The gesture armed by the last title-bar drag / edge hit test.
static KIND: AtomicU8 = AtomicU8::new(NONE);

/// The window procedure to forward to (winit's). `0` until [`install`] runs.
static ORIG_PROC: AtomicIsize = AtomicIsize::new(0);

/// How close to the top of the work area the pointer has to be for the release
/// to maximize, in physical pixels. The shell's band is about this wide; it is
/// deliberately small so pointing *near* the top does not steal the gesture
/// from a user who means to park the window there.
const TOP_BAND: i32 = 10;

/// The system's own drag threshold (`SM_CXDRAG` / `SM_CYDRAG`), physical
/// pixels. A click below it must not snap or restore anything: clicking the
/// caption of a maximized window leaves it maximized, like Windows does.
/// Queried live from the system (default 4) so it scales with the user's
/// DPI setting — a hardcoded 4 at 150 % DPI let a 5 px nudge on the top-band
/// caption maximize the window.
fn drag_threshold() -> i32 {
    #[cfg(windows)]
    unsafe {
        let v = windows_sys::Win32::UI::WindowsAndMessaging::GetSystemMetrics(
            windows_sys::Win32::UI::WindowsAndMessaging::SM_CXDRAG,
        );
        if v > 0 {
            return v;
        }
    }
    4
}

/// State of the gesture currently inside the system's modal move loop.
#[derive(Debug)]
struct Gesture {
    kind: u8,
    /// maximized when the gesture began — the drag-down has to give it back
    was_max: bool,
    /// cursor position when `WM_ENTERSIZEMOVE` arrived
    start: (i32, i32),
    /// window rect when the gesture began (basis of the proportional restore)
    orig: (i32, i32, i32, i32),
    /// the pointer travelled far enough to count as a real drag
    moved: bool,
    /// the maximized window became floating again mid-drag
    restored: bool,
    /// the vertical grab-offset floor applied to the restore, so follow-up
    /// steps keep the same landing rule (`restore_target`'s `min_off_y`)
    min_off_y: i32,
    /// its size after the restore, so the follow-up steps keep it
    restored_size: (i32, i32),
    /// pointer was inside the top band on the last step → maximize on release
    snap_top: bool,
}

impl Gesture {
    fn new(kind: u8, start: (i32, i32), orig: (i32, i32, i32, i32), was_max: bool) -> Self {
        Self {
            kind,
            was_max,
            start,
            orig,
            moved: false,
            restored: false,
            min_off_y: 0,
            restored_size: (0, 0),
            snap_top: false,
        }
    }
}

/// The gesture inside the loop. The window procedure is single-threaded; the
/// mutex only keeps the type honest.
static GESTURE: Mutex<Option<Gesture>> = Mutex::new(None);

fn lock() -> std::sync::MutexGuard<'static, Option<Gesture>> {
    // Nothing unwinds out of the window procedure, so a poisoned lock still
    // holds usable state — do not lose the gesture over it.
    GESTURE.lock().unwrap_or_else(|e| e.into_inner())
}

// ---------------------------------------------------------------------------
// Armed by the window glue
// ---------------------------------------------------------------------------

/// The next sizing loop is a title-bar move (release-to-maximize applies).
pub fn arm_move() {
    KIND.store(MOVE, Ordering::SeqCst);
}

/// The next sizing loop comes from an edge band (`WM_NCHITTEST`): no snap on
/// release, because dragging the north edge up to the top edge is a resize,
/// not a snap request.
#[cfg_attr(not(windows), allow(dead_code))]
pub fn arm_resize() {
    KIND.store(RESIZE, Ordering::SeqCst);
}

/// Whether the system's modal move/size loop is running right now.
///
/// While it runs, the **OS owns the cursor** and the armed-gesture flag has
/// already been consumed by `WM_ENTERSIZEMOVE`. The resize bands themselves
/// no longer care — they are `WM_NCHITTEST` answers now, not DOM nodes — but
/// `arm_resize_gesture` reads it, so a hit *inside* the loop cannot re-arm
/// `KIND`: a leftover would mislabel the next gesture that arms nothing.
pub fn in_gesture() -> bool {
    lock().is_some()
}

/// Subclass the window procedure of the main window.
///
/// Idempotent — the app root may run this on every render — and harmless if
/// the OS refuses: the caller keeps the plain, non-snap behaviour. Returns
/// whether the subclass is installed.
pub fn install() -> bool {
    let hwnd = crate::win::frameless::current_hwnd();
    if hwnd == 0 {
        return false;
    }
    if ORIG_PROC.load(Ordering::SeqCst) != 0 {
        return true;
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::UI::WindowsAndMessaging::{SetWindowLongPtrW, GWLP_WNDPROC};
        let prev = unsafe {
            SetWindowLongPtrW(hwnd as _, GWLP_WNDPROC, window_proc as *const () as isize)
        };
        if prev == 0 {
            return false;
        }
        ORIG_PROC.store(prev, Ordering::SeqCst);
        // Before subclassing, the window was already built per winit's
        // NCCALCSIZE (client filling the whole window); our taken-over client
        // (inset by the frame width on left/right/bottom) only takes effect on
        // the next NCCALCSIZE — SWP_FRAMECHANGED forces the system to resend it
        // right away, so the border settles immediately.
        unsafe {
            use windows_sys::Win32::UI::WindowsAndMessaging::{
                SetWindowPos, SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE,
                SWP_NOZORDER,
            };
            SetWindowPos(
                hwnd as _,
                std::ptr::null_mut(),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED,
            );
        }
        true
    }
    #[cfg(not(windows))]
    {
        // Win+↑ / drag-to-top snap is provided by each window manager itself;
        // a frameless window's resize hit tests also bypass the wndproc, so
        // there is nothing to do here.
        false
    }
}

// ---------------------------------------------------------------------------
// The window procedure
// ---------------------------------------------------------------------------

#[cfg(windows)]
unsafe extern "system" fn window_proc(
    hwnd: windows_sys::Win32::Foundation::HWND,
    msg: u32,
    wparam: windows_sys::Win32::Foundation::WPARAM,
    lparam: windows_sys::Win32::Foundation::LPARAM,
) -> windows_sys::Win32::Foundation::LRESULT {
    // A panic must never unwind across this FFI boundary: unwinding into
    // `extern "system"` aborts the process outright. The panic hook has already
    // written crash.log; degrade to the default handling of this one message so the
    // window (and every document in it) survives.
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
        handle_window_msg(hwnd, msg, wparam, lparam)
    })) {
        Ok(r) => r,
        Err(_) => unsafe {
            // The hook already wrote crash.log (with file:line); this names the
            // degradation for diagnostics-on runs.
            crate::shell::debug_log("window_proc: panic swallowed, using DefWindowProc");
            windows_sys::Win32::UI::WindowsAndMessaging::DefWindowProcW(hwnd, msg, wparam, lparam)
        },
    }
}

/// The real message handling — split out so [`window_proc`] can firewall it
/// without swallowing the normal path.
#[cfg(windows)]
unsafe fn handle_window_msg(
    hwnd: windows_sys::Win32::Foundation::HWND,
    msg: u32,
    wparam: windows_sys::Win32::Foundation::WPARAM,
    lparam: windows_sys::Win32::Foundation::LPARAM,
) -> windows_sys::Win32::Foundation::LRESULT {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        WM_ENTERSIZEMOVE, WM_EXITSIZEMOVE, WM_MOVING, WM_NCCALCSIZE, WM_NCHITTEST,
    };

    match msg {
        // Revision three: take over the client rect computation — inset by one
        // native frame width on left/right/bottom (real non-client area; the
        // system natively answers resize/cursor/shadow/rounded corners), keep a
        // 1px shadow line at the top; maximize clamps to the work area. Do not
        // forward to winit (its handler is "client fills the whole window",
        // which is mutually exclusive with this scheme).
        WM_NCCALCSIZE => {
            if wparam != 0 {
                return nc_calc_size(hwnd as isize, lparam);
            }
        }
        WM_ENTERSIZEMOVE => begin(hwnd as isize),
        WM_MOVING => {
            let modified = moving(hwnd as isize, lparam as *mut _);
            let forwarded = forward(hwnd, msg, wparam, lparam);
            // The docs: return TRUE when the application processed WM_MOVING,
            // i.e. when it took over the drag rectangle.
            return if modified { 1 } else { forwarded };
        }
        WM_EXITSIZEMOVE => end(hwnd as isize),
        WM_NCHITTEST => {
            // The top-edge hand-drawn band (the client is flush with the top,
            // there is no top non-client area): answer with an HT* code on hit.
            if let Some(code) = top_band_code(hwnd as isize, lparam) {
                return code;
            }
            // Everything else goes to winit → DefWindowProc: left/right/bottom
            // are real non-client borders and the system natively returns
            // HTLEFT/… (cursor, NC gestures, Snap are all system behaviors); the
            // client returns HTCLIENT and the DOM still receives pointer events.
            // When the system answers with a resize code, arm RESIZE —
            // `WM_ENTERSIZEMOVE` only looks at KIND, and the native border path
            // does not go through our own hit codes, so the flag must be set
            // here.
            let r = forward(hwnd, msg, wparam, lparam);
            if crate::win::frameless::is_resize_ht(r) {
                arm_resize_gesture();
            }
            return r;
        }
        _ => {}
    }
    forward(hwnd, msg, wparam, lparam)
}

/// `WM_NCCALCSIZE` (`wparam != 0`): recompute the client rect — revision
/// three's native-border scheme.
///
/// * Normal: inset by one native frame width ([`frame_border_phys`]) on
///   left/right/bottom, so the system owns real non-client resize borders
///   (the Chrome / Windows Terminal recipe); the top `+1` keeps winit's DWM
///   shadow hairline (1px of non-client area; DWM draws the top line).
/// * Maximized: clamp to the work area of the monitor holding the window
///   (winit's original behavior; a native maximized frame already flares out
///   by one border width, so after clamping the client is exactly the work
///   area and the content is not cropped by the screen).
///
/// Returns 0 (the system adopts the rewritten `rgrc[0]` as the new client);
/// the caller **must not** forward to winit. When `wparam == 0` (a non-client
/// rect query), forward as usual.
#[cfg(windows)]
fn nc_calc_size(hwnd: isize, lparam: isize) -> isize {
    use windows_sys::Win32::Graphics::Gdi::{
        GetMonitorInfoW, MonitorFromRect, MONITORINFO, MONITOR_DEFAULTTONULL,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::NCCALCSIZE_PARAMS;

    let params = unsafe { &mut *(lparam as *mut NCCALCSIZE_PARAMS) };
    if is_zoomed(hwnd) {
        // Consistent with winit's original implementation: use MonitorFromRect
        // (not MonitorFromWindow) to pick the right monitor
        // (WebView2Feedback#2549).
        let monitor = unsafe { MonitorFromRect(&params.rgrc[0], MONITOR_DEFAULTTONULL) };
        if !monitor.is_null() {
            let mut info: MONITORINFO = unsafe { std::mem::zeroed() };
            info.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
            if unsafe { GetMonitorInfoW(monitor, &mut info) } != 0 {
                params.rgrc[0] = info.rcWork;
            }
        }
        return 0;
    }
    let frame = crate::win::frameless::frame_border_phys();
    params.rgrc[0].left += frame;
    params.rgrc[0].right -= frame;
    params.rgrc[0].bottom -= frame;
    // Top 1px: undecorated_shadow's DWM top line (winit's original behavior,
    // kept as-is)
    params.rgrc[0].top += 1;
    0
}

/// The top-edge hand-drawn hit band (after revision three, the only resize
/// band overlapping the client).
///
/// The client is flush with the top (only 1px of shadow non-client area up
/// there), so the top edge's resize grab is answered by us: a narrow band at
/// the client's outermost strip, as wide as the native border, → `HTTOP`;
/// its junction with the real left/right borders → `HTTOPLEFT/HTTOPRIGHT`;
/// hitting the title-bar button cluster (the close button) is always HTCLIENT.
/// All other cases return `None` (the message is forwarded as usual, letting
/// DefWindowProc answer left/right/bottom with the real non-client area).
#[cfg(windows)]
fn top_band_code(hwnd: isize, lparam: windows_sys::Win32::Foundation::LPARAM) -> Option<isize> {
    use windows_sys::Win32::UI::WindowsAndMessaging::GetCursorPos;

    use windows_sys::Win32::Foundation::POINT;

    if is_zoomed(hwnd) {
        return None; // No resize band when maximized (consistent with native Windows)
    }
    let ((ox, oy), (cw, _)) = client_box(hwnd)?;
    // The coordinates carried by the system-synthesized `WM_NCMOUSEMOVE`
    // after a touch are recorded with drift; correct them uniformly with
    // `GetCursorPos`; the screen coordinates in lParam serve only as a
    // backstop.
    let mut pt = POINT { x: 0, y: 0 };
    let (sx, sy) = if unsafe { GetCursorPos(&mut pt) } != 0 {
        (pt.x, pt.y)
    } else {
        (
            (lparam & 0xFFFF) as u16 as i16 as i32,
            ((lparam >> 16) & 0xFFFF) as u16 as i16 as i32,
        )
    };
    let scale = dpi_scale(hwnd);
    let (band_css, _) = crate::win::frameless::resize_band_css();
    let frame = (band_css * scale).round() as i32;
    // The top band is measured from the client top (the window top + the 1px
    // shadow line); y can be -1 (on the shadow line), which `top_band_hit`'s
    // `y < frame` check naturally covers.
    let x = sx - ox;
    let y = sy - oy;
    let dir = crate::win::frameless::with_hit_exempt(|ex| {
        crate::win::frameless::top_band_hit_arbitrated(x, y, cw, frame, scale, ex)
    })?;
    arm_resize_gesture();
    Some(crate::win::frameless::resize_ht_code(dir))
}

/// Hand the message to winit's window procedure — painting, input and the
/// sizing flags all live there.
#[cfg(windows)]
unsafe fn forward(
    hwnd: windows_sys::Win32::Foundation::HWND,
    msg: u32,
    wparam: windows_sys::Win32::Foundation::WPARAM,
    lparam: windows_sys::Win32::Foundation::LPARAM,
) -> windows_sys::Win32::Foundation::LRESULT {
    let orig = ORIG_PROC.load(Ordering::SeqCst);
    if orig == 0 {
        return 0;
    }
    let orig: windows_sys::Win32::UI::WindowsAndMessaging::WNDPROC = std::mem::transmute(orig);
    match orig {
        Some(f) => f(hwnd, msg, wparam, lparam),
        None => 0,
    }
}

// ---------------------------------------------------------------------------
// The three messages of a move/size loop
// ---------------------------------------------------------------------------

/// `WM_ENTERSIZEMOVE`: remember what the gesture started as.
#[cfg(windows)]
fn begin(hwnd: isize) {
    let kind = KIND.swap(NONE, Ordering::SeqCst);
    let cursor = cursor();
    let rect = window_rect(hwnd);
    let was_max = is_zoomed(hwnd);
    *lock() = Some(Gesture::new(
        // A caption drag we did not arm ourselves (none today, but the OS can
        // raise one) still counts as a move; only an armed edge strip does not.
        if kind == NONE { MOVE } else { kind },
        cursor,
        rect,
        was_max,
    ));
}

/// `WM_MOVING`: track the pointer and, where the system does not, decide where
/// the window belongs. Returns whether the drag rectangle was rewritten.
#[cfg(windows)]
fn moving(hwnd: isize, rect: *mut windows_sys::Win32::Foundation::RECT) -> bool {
    let cursor = cursor();
    let mut guard = lock();
    let Some(g) = guard.as_mut() else {
        return false;
    };

    let travel = (cursor.0 - g.start.0)
        .abs()
        .max((cursor.1 - g.start.1).abs());
    if travel > drag_threshold() && !g.moved {
        g.moved = true;
    }
    if !g.moved {
        // A click, not a drag: nothing to snap or to restore.
        return false;
    }

    g.snap_top = g.kind == MOVE && in_top_band(cursor.1, work_area_top(cursor));

    if g.was_max && !g.restored {
        // Dragging a maximized window away gives it back, keeping the pointer
        // at the same fraction of its width — where Windows lands a window it
        // restores mid-drag.
        if !restore(hwnd, g, cursor) {
            return false;
        }
        if !rect.is_null() {
            write_rect(rect, g, cursor);
        }
        return true;
    }

    if g.restored && !rect.is_null() {
        // The system's proposal still assumes the pre-restore window, so keep
        // the restored one glued to the pointer ourselves.
        write_rect(rect, g, cursor);
        return true;
    }
    false
}

/// `WM_EXITSIZEMOVE`: the pointer is up (or the drag was cancelled).
#[cfg(windows)]
fn end(hwnd: isize) {
    let Some(g) = lock().take() else {
        return;
    };
    // The gesture has ended: clear any arming that may have been left over
    // mid-loop (`arm_resize_gesture`'s `in_gesture` gate already blocks the
    // vast majority; this is the backstop).
    KIND.store(NONE, Ordering::SeqCst);
    let abort = aborted();
    let maximize = g.snap_top && g.moved && g.kind == MOVE && !abort;
    if !maximize {
        return;
    }
    unsafe {
        windows_sys::Win32::UI::WindowsAndMessaging::ShowWindow(
            hwnd as _,
            windows_sys::Win32::UI::WindowsAndMessaging::SW_MAXIMIZE,
        );
    }
}

// ---------------------------------------------------------------------------
// WM_NCHITTEST: the resize bands
// ---------------------------------------------------------------------------

/// Arm the next sizing loop as "resize" (no snap).
///
/// `WM_NCHITTEST` is dispatched repeatedly during the system's resize loop
/// (the resize cursor relies on it), while [`arm_resize`] should only arm
/// **at the moment of the actual press**: `KIND` is consumed by
/// `WM_ENTERSIZEMOVE`, and re-arming mid-loop would leave it behind for the
/// next gesture. The [`in_gesture`] gate below is what prevents that; the old
/// 500 ms debounce also swallowed the *next real press* on an edge (a second
/// top-edge resize within half a second was mislabelled as a move and
/// maximized the window on release).
fn arm_resize_gesture() {
    if in_gesture() {
        return;
    }
    arm_resize();
}

/// The client's origin (screen physical px) and size (physical px).
///
/// After revision three the client ≠ the outer frame: it is inset by one
/// native border width on left/right/bottom (`nc_calc_size`), and at the top
/// it differs from the outer frame by only the 1px shadow line. Every
/// "cursor screen coordinates → client physical coordinates" conversion
/// anchors here, consistent with the layout layer's client CSS coordinate
/// system.
#[cfg(windows)]
fn client_box(hwnd: isize) -> Option<((i32, i32), (i32, i32))> {
    use windows_sys::Win32::Foundation::{POINT, RECT};
    // windows-sys 0.59 puts ClientToScreen under Gdi rather than
    // WindowsAndMessaging
    use windows_sys::Win32::Graphics::Gdi::ClientToScreen;
    use windows_sys::Win32::UI::WindowsAndMessaging::GetClientRect;

    let mut origin = POINT { x: 0, y: 0 };
    if unsafe { ClientToScreen(hwnd as _, &mut origin) } == 0 {
        return None;
    }
    let mut r: RECT = unsafe { std::mem::zeroed() };
    if unsafe { GetClientRect(hwnd as _, &mut r) } == 0 {
        return None;
    }
    Some((
        (origin.x, origin.y),
        ((r.right - r.left).max(1), (r.bottom - r.top).max(1)),
    ))
}

/// The CSS→physical conversion ratio for the window's own DPI. Falls back to
/// 1.0 when the query fails, never to 0 — "no bandwidth" is exactly the root
/// cause of "no resize cursor on the edge".
#[cfg(windows)]
fn dpi_scale(hwnd: isize) -> f32 {
    use windows_sys::Win32::UI::HiDpi::GetDpiForWindow;

    let dpi = unsafe { GetDpiForWindow(hwnd as _) } as f32;
    if dpi > 0.0 {
        dpi / 96.0
    } else {
        1.0
    }
}

// ---------------------------------------------------------------------------
// Geometry helpers (pure — the tests cover them)
// ---------------------------------------------------------------------------

/// Is a pointer at `y` inside the snap band at the top of the work area?
/// Reused by the touch drag loop (`frameless::update_touch_gesture`'s
/// top-edge snap), sharing the same predicate as the system drag loop.
pub(crate) fn in_top_band(y: i32, work_top: i32) -> bool {
    y <= work_top + TOP_BAND
}

/// Where a restored window should land so the pointer keeps both its grab
/// offset and its *fraction* of the window's width.
///
/// `start` / `orig` describe the maximized window the gesture began on; `size`
/// is the size the window has after the restore.
///
/// `min_off_y` is a floor on the vertical grab offset, in physical pixels.
/// A maximized window's caption starts at the screen top, so the grab can sit
/// a pixel or two from the edge — inside the top resize strip the floating
/// window will show once restored. Landing the window with the pointer in that
/// strip paints a resize cursor over the caption right after leaving
/// fullscreen; the floor pushes the window up under the pointer far enough
/// that the grab lands on the caption itself, as Windows does.
fn restore_target(
    start: (i32, i32),
    orig: (i32, i32, i32, i32),
    cursor: (i32, i32),
    size: (i32, i32),
    min_off_y: i32,
) -> (i32, i32) {
    let orig_w = (orig.2 - orig.0).max(1) as f64;
    let frac = ((start.0 - orig.0) as f64 / orig_w).clamp(0.0, 1.0);
    let off_x = (frac * size.0 as f64).round() as i32;
    // The grab point sits inside the caption band; never more than a whole
    // window below the top, and never so close to the top that the pointer
    // lands in the restored window's resize strip.
    let off_y = (start.1 - orig.1).max(min_off_y).min(size.1.max(min_off_y));
    (cursor.0 - off_x, cursor.1 - off_y)
}

/// Write the followed position into the system's drag rectangle.
#[cfg(windows)]
fn write_rect(rect: *mut windows_sys::Win32::Foundation::RECT, g: &Gesture, cursor: (i32, i32)) {
    let (left, top) = restore_target(g.start, g.orig, cursor, g.restored_size, g.min_off_y);
    unsafe {
        *rect = windows_sys::Win32::Foundation::RECT {
            left,
            top,
            right: left + g.restored_size.0,
            bottom: top + g.restored_size.1,
        };
    }
}

// ---------------------------------------------------------------------------
// Small Win32 shims
// ---------------------------------------------------------------------------

#[cfg(windows)]
fn cursor() -> (i32, i32) {
    let mut pt = windows_sys::Win32::Foundation::POINT { x: 0, y: 0 };
    unsafe {
        windows_sys::Win32::UI::WindowsAndMessaging::GetCursorPos(&mut pt);
    }
    (pt.x, pt.y)
}

#[cfg(windows)]
fn window_rect(hwnd: isize) -> (i32, i32, i32, i32) {
    let mut r = windows_sys::Win32::Foundation::RECT {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };
    unsafe {
        windows_sys::Win32::UI::WindowsAndMessaging::GetWindowRect(hwnd as _, &mut r);
    }
    (r.left, r.top, r.right, r.bottom)
}

#[cfg(windows)]
fn is_zoomed(hwnd: isize) -> bool {
    unsafe { windows_sys::Win32::UI::WindowsAndMessaging::IsZoomed(hwnd as _) != 0 }
}

/// Top of the work area of the monitor holding `point` (reused by the touch
/// drag loop).
#[cfg(windows)]
pub(crate) fn work_area_top(point: (i32, i32)) -> i32 {
    use windows_sys::Win32::Graphics::Gdi::{
        GetMonitorInfoW, MonitorFromPoint, MONITORINFO, MONITOR_DEFAULTTONEAREST,
    };

    let pt = windows_sys::Win32::Foundation::POINT {
        x: point.0,
        y: point.1,
    };
    unsafe {
        let mon = MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST);
        if mon.is_null() {
            return 0;
        }
        let mut mi = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            rcMonitor: std::mem::zeroed(),
            rcWork: std::mem::zeroed(),
            dwFlags: 0,
        };
        if GetMonitorInfoW(mon, &mut mi) == 0 {
            return 0;
        }
        mi.rcWork.top
    }
}

/// Is the pointer holding Escape right now?
#[cfg(windows)]
fn aborted() -> bool {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_ESCAPE};

    unsafe { GetAsyncKeyState(VK_ESCAPE as i32) as u16 & 0x8000 != 0 }
}

/// Height of the top resize band in physical pixels for `hwnd`.
///
/// The band is the outermost strip of the window, measured in CSS pixels by
/// [`crate::win::frameless::resize_band_css`]; the gesture math here is physical,
/// so the band is scaled by the window's own DPI.
#[cfg(windows)]
fn top_strip_px(hwnd: isize) -> i32 {
    (crate::win::frameless::resize_band_css().0 * dpi_scale(hwnd)).round() as i32
}

/// Restore a maximized window and land it under the pointer.
///
/// Returns whether it is actually floating now — `ShowWindow(SW_RESTORE)` can
/// refuse (it posts to the window thread), and pretending it worked would mean
/// rewriting the rectangle of a window that is still maximized.
#[cfg(windows)]
fn restore(hwnd: isize, g: &mut Gesture, cursor: (i32, i32)) -> bool {
    let Some((_, size)) = restore_floating(hwnd, g.start, g.orig, cursor) else {
        return false;
    };
    g.restored_size = size;
    // The floor the landing used, so follow-up WM_MOVING steps land the same
    // way (`restore_target`'s `min_off_y`).
    g.min_off_y = top_strip_px(hwnd);
    g.restored = true;
    true
}

/// Restore a maximized window and land it under `cursor`, keeping the grab
/// fraction and offset the pointer had at `start` on the maximized rect `orig`
/// (see [`restore_target`]).
///
/// Shared by the system move loop ([`restore`]) and the touch drag loop
/// (`frameless::update_touch_gesture`'s drag-down-to-restore from maximized).
///
/// Returns `(new_frame_origin, floating_size)` or `None` when the restore was
/// refused — `ShowWindow(SW_RESTORE)` posts to the window thread, and
/// pretending it worked would mean positioning a window that is still
/// maximized.
#[cfg(windows)]
pub(crate) fn restore_floating(
    hwnd: isize,
    start: (i32, i32),
    orig: (i32, i32, i32, i32),
    cursor: (i32, i32),
) -> Option<((i32, i32), (i32, i32))> {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        SetWindowPos, ShowWindow, SWP_NOSIZE, SWP_NOZORDER, SW_RESTORE,
    };

    unsafe { ShowWindow(hwnd as _, SW_RESTORE) };
    if is_zoomed(hwnd) {
        return None;
    }
    let r = window_rect(hwnd);
    let size = ((r.2 - r.0).max(1), (r.3 - r.1).max(1));
    // Land it under the pointer right away: the grab offset keeps clear of the
    // top resize strip (see `restore_target`).
    let (left, top) = restore_target(start, orig, cursor, size, top_strip_px(hwnd));
    unsafe {
        SetWindowPos(
            hwnd as _,
            std::ptr::null_mut(),
            left,
            top,
            0,
            0,
            SWP_NOSIZE | SWP_NOZORDER,
        );
    }
    Some(((left, top), size))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// The band belongs to the *work* area: a taskbar pinned to the top raises
    /// it, and the work-area edge itself is inside the band (that is where the
    /// pointer lands when it is pushed against the top of the screen).
    #[test]
    fn the_top_band_is_where_the_work_area_starts() {
        assert!(in_top_band(0, 0));
        assert!(in_top_band(TOP_BAND, 0));
        assert!(!in_top_band(TOP_BAND + 1, 0));
        assert!(in_top_band(40, 40), "a top-pinned taskbar raises the band");
        assert!(!in_top_band(40 + TOP_BAND + 1, 40));
    }

    /// A restored window lands with the pointer at the same fraction of its
    /// width it had on the maximized one, and with the caption under it.
    #[test]
    fn restore_keeps_the_pointer_where_the_user_grabbed() {
        // maximized window spanning the full 1920 wide screen, grabbed a quarter in
        let orig = (0, 0, 1920, 1080);
        let start = (480, 30);
        let size = (1200, 800);

        let (left, top) = restore_target(start, orig, (480, 30), size, 0);
        assert_eq!(left, 480 - 300, "a quarter of 1200 stays under the pointer");
        assert_eq!(top, 0, "the grab offset keeps the caption under it");

        // the pointer travelled: the window follows
        let (left2, top2) = restore_target(start, orig, (900, 400), size, 0);
        assert_eq!(left2, 900 - 300);
        assert_eq!(top2, 400 - 30);
    }

    /// Grabbing the caption at the very top of a maximized window (its top is
    /// the screen edge) must not land the restored window with the pointer
    /// inside the top resize strip — that painted a resize cursor over the
    /// caption right after leaving fullscreen.
    #[test]
    fn restore_keeps_the_pointer_below_the_top_strip() {
        let orig = (0, 0, 1920, 1080);
        let size = (1200, 800);
        let strip = 9i32; // 6 CSS px at 150%

        // grabbed 2px from the screen top, pointer pulled down to y=300
        let (left, top) = restore_target((480, 2), orig, (480, 300), size, strip);
        assert_eq!(left, 480 - 300);
        assert_eq!(
            300 - top,
            strip,
            "the grab offset is floored at the strip height"
        );
        assert!(
            300 - top > strip - 1,
            "the pointer sits below the strip, on the caption"
        );

        // a grab already below the strip keeps its exact offset
        let unchanged = restore_target((480, 30), orig, (480, 300), size, strip);
        assert_eq!(unchanged.1, 300 - 30);
    }

    /// Clamp: a pointer at (or outside) either edge cannot push the restored
    /// window somewhere nonsensical.
    #[test]
    fn restore_offset_is_clamped_to_the_window() {
        let orig = (0, 0, 1920, 1080);
        let size = (1200, 800);

        let at_left = restore_target((-500, 30), orig, (0, 0), size, 0);
        assert_eq!(at_left.0, 0, "grabbed left of the window → flush left");

        let at_right = restore_target((5000, 30), orig, (1900, 0), size, 0);
        assert_eq!(at_right.0, 1900 - 1200, "grabbed right of it → flush right");

        let deep = restore_target((0, 5000), orig, (100, 100), size, 0);
        assert_eq!(deep.1, 100 - size.1, "never more than a window below");
    }

    /// A click on the caption stays under the travel threshold and must not
    /// trigger a snap or a restore — this is what `Gesture::moved` gates.
    #[test]
    fn a_click_is_not_a_drag() {
        let g = Gesture::new(MOVE, (100, 100), (10, 10, 1000, 800), true);

        // the same travel computation `moving()` performs, with typed coordinates
        let (dx, dy) = (102i32 - 100, 103i32 - 100);
        let click = dx.abs().max(dy.abs());
        assert!(
            click <= drag_threshold(),
            "a click stays under the threshold"
        );

        let (dx, dy) = (180i32 - 100, 100i32 - 100);
        let travel = dx.abs().max(dy.abs());
        assert!(travel > drag_threshold(), "real movement counts");
        assert!(!g.moved, "the gesture starts un-moved; only motion arms it");
    }
}
