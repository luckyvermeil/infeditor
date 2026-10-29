//! The common popup skeleton: two-way state bridge + focus epoch + caret
//! blink accounting.
//!
//! Every Blitz popup (find/replace `findwin`, preferences `prefswin`, insert
//! number `numwin`…) follows the same shape (see `shell.rs`):
//!
//! * **Window**: a normal top-level window with an owner attached via
//!   [`crate::win::frameless::attach_owner`] (system title bar, draggable,
//!   `×`/Esc closes, system border resize, no taskbar entry);
//! * **State bridge** [`Bridge`]: the main window → popup display mirror
//!   (`shared`, deduped before advancing the popup vdom's `Signal`) plus the
//!   popup → main window commands (an mpsc channel; the main side applies them
//!   uniformly in `poll_dialogs`).
//!
//! Each popup module instantiates a [`Bridge`] in its own `thread_local` (the
//! types being its own snapshot/command types) and exports two function
//! pointers for the shell to route:
//!
//! * `flush()` — flush the shared mirror into the popup's Signal;
//! * `on_focused()` — finish up when the popup gets OS focus (the shell
//!   backfills a flush at a safe point).

use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::{mpsc, Mutex, OnceLock};

use dioxus_native::prelude::*;

/// The composer re-mount epoch: incremented by one every time a popup gets OS
/// focus (the shell side does the incrementing), used via `autofocus` to
/// rebuild the DOM focus chain.
///
/// One global copy is enough (at most one popup holds the keyboard at a time);
/// each popup folds the epoch into its own composer's re-mount key.
static FOCUS_EPOCH: AtomicU32 = AtomicU32::new(0);

/// Shell side: called when a popup gets OS focus. The next `Bridge::flush`
/// pushes the new epoch into the popup vdom.
pub fn bump_focus_epoch() {
    FOCUS_EPOCH.fetch_add(1, Ordering::Relaxed);
}

/// The current focus epoch (used by `Bridge::flush` to inject into the
/// snapshot).
pub fn focus_epoch() -> u32 {
    FOCUS_EPOCH.load(Ordering::Relaxed)
}

// ---------------------------------------------------------------------------
// The caret's blink phase (shared by popup input fields + the main window
// editor)
//
// **Why not CSS animations**: blitz self-sustains `request_redraw` every frame
// for a window with a running animation (`View::redraw`'s `is_animating`
// branch) — a window with an infinite animation repaints forever at 40–60fps
// and keeps one CPU core resident while idle (2026-10 round seven's live
// measurement: `tmp/scroll_probe.py --idle`'s 120-frame batch never
// stopped). In earlier years the main window's `@keyframes caret-blink` also
// masked another problem: a repaint stream saturating winit's message pump
// starves the main window's WM_PAINT (measured on winit 0.31-beta: 60
// cross-window `request_redraw`s with zero delivery and `about_to_wait`
// stalling); popup commands relied entirely on the main window's repaint
// frames, so the result was "typing in a popup echoes nothing".
//
// Now everything is **event-driven**: a low-frequency thread posts a
// `ShellEvt::BlinkTick` user event every 265ms; the shell flips the phase
// ([`blink_toggle`]) then flushes each popup's snapshot (the phase field goes
// into the Snap, so dedup never matches) and polls the child window vdom; for
// the main window, [`main_blink_on`] triggers a `request_redraw` and the
// frame hook writes the new phase back to state (`caret_phase`) driving the
// re-render. Each blink period is only 2 renders + 2 repaints, no flooding.
// ---------------------------------------------------------------------------

/// The blink phase (true = caret visible).
static BLINK_ON: AtomicBool = AtomicBool::new(true);
/// Count of open popups. The timer thread only posts events while the count
/// is > 0.
static POPUPS_OPEN: AtomicUsize = AtomicUsize::new(0);
/// The main window caret blink switch (the app side syncs
/// `caret_blink && (doc_focused || find bar focused)` every frame).
///
/// After the idle render loop goes dormant, the main window's caret blinking
/// is driven by this flag: the timer thread posts `ShellEvt::BlinkTick` every
/// 265ms, the shell does a `request_redraw` for the main window, and the frame
/// hook writes the new phase back to state (see `opens.rs::needs_idle_poll`'s
/// blink arm). It used to be a CSS infinite animation: blitz self-sustains
/// per-frame repaints for a window with a running animation, so the whole app
/// ran forever at 40–60fps — the dominant idle-CPU cost located by 2026-10
/// round seven's live measurements.
static MAIN_BLINK: AtomicBool = AtomicBool::new(false);
/// The blink thread's started marker (`OnceLock` guarantees exactly one spawn
/// for the whole run).
static BLINK_THREAD: OnceLock<()> = OnceLock::new();

/// The current blink phase (read when a popup component renders the caret).
pub fn blink_on() -> bool {
    BLINK_ON.load(Ordering::Relaxed)
}

/// The app side syncs the main window's blink demand every frame (writes only
/// on change, avoiding pointless atomic-write contention).
pub fn main_blink_set(on: bool) {
    if main_blink_on() != on {
        MAIN_BLINK.store(on, Ordering::Relaxed);
        if on {
            ensure_blink_thread();
        }
    }
}

/// Lazy start of the resident blink thread (shared by [`popup_blink_start`]
/// and [`main_blink_set`]).
fn ensure_blink_thread() {
    // The thread is **resident**: no "exit when the count reaches zero"
    // lifecycle — there is a race window between the exit path and the
    // re-start path (spawning on fetch_add 0→1): an interleaving of clearing
    // the flag before exit and a new session racing for the flag would leave
    // one popup session unserved. A resident thread loads one atomic per
    // period, negligible; when the process exits, main's return terminates it
    // directly without blocking shutdown.
    BLINK_THREAD.get_or_init(|| {
        std::thread::spawn(|| loop {
            std::thread::sleep(std::time::Duration::from_millis(265));
            // A panic in one tick must not kill the resident thread (and with it the
            // whole process): swallow it and keep blinking.
            let _ = std::panic::catch_unwind(|| {
                if POPUPS_OPEN.load(Ordering::Relaxed) > 0 || MAIN_BLINK.load(Ordering::Relaxed) {
                    crate::shell::send(crate::shell::ShellEvt::BlinkTick);
                }
            });
        });
    });
}

/// Whether the main window's blinking needs frame driving (read by the
/// `ShellEvt::BlinkTick` handler).
pub fn main_blink_on() -> bool {
    MAIN_BLINK.load(Ordering::Relaxed)
}

/// Shell side (`ShellEvt::BlinkTick`): flip the phase.
pub fn blink_toggle() {
    BLINK_ON.store(!BLINK_ON.load(Ordering::Relaxed), Ordering::Relaxed);
}

/// Shell side: called when a popup **transitions from hidden to visible**
/// (`PopupShow` while previously hidden / first creation). Count +1, phase
/// reset to visible (the caret is there the moment the focus box appears),
/// and the timer thread is ensured running.
///
/// A repeated PopupShow (the popup already visible, e.g. switching tabs inside
/// the find popup re-pushes) is not counted — otherwise `POPUPS_OPEN` only
/// grows and the thread never stops spinning.
pub fn popup_blink_start() {
    BLINK_ON.store(true, Ordering::Relaxed);
    if POPUPS_OPEN.fetch_add(1, Ordering::Relaxed) == 0 {
        ensure_blink_thread();
    }
}

/// Shell side: called when a popup hides (`PopupHide`). After the count
/// reaches zero the thread turns to silent idling (one atomic load per
/// period).
pub fn popup_blink_stop() {
    let prev = POPUPS_OPEN.fetch_sub(1, Ordering::Relaxed);
    if prev == 0 {
        // Defensive: more Hides than Shows (theoretically impossible; if
        // pairing gets scrambled, do not let the count wrap into a huge value
        // that locks the thread in a spinning state).
        POPUPS_OPEN.store(0, Ordering::Relaxed);
    }
}

/// The two-way state bridge (one instance per popup module).
///
/// * The main side writes [`Bridge::set_shared`]; shell/main side calls
///   [`Bridge::flush`] to advance the popup vdom (**accounted only when actually
///   written into a Signal**: a popup vdom is created lazily, and a flush
///   before registration must pass through unchanged, otherwise the first
///   push after registration would be swallowed by dedup and the popup would be
///   stuck at `S::default()` forever — the find popup once "could not type"
///   because of this);
/// * The popup side [`Bridge::send`]s commands, and the main side
///   [`Bridge::drain`]s them for uniform application.
pub struct Bridge<S, C> {
    shared: RefCell<S>,
    last: RefCell<Option<S>>,
    sig: OnceLock<Signal<S>>,
    tx: OnceLock<mpsc::Sender<C>>,
    rx: OnceLock<Mutex<mpsc::Receiver<C>>>,
}

impl<S, C> Default for Bridge<S, C>
where
    S: Clone + PartialEq + Default + 'static,
    C: Clone + 'static,
{
    fn default() -> Self {
        Self::new()
    }
}

impl<S, C> Bridge<S, C>
where
    S: Clone + PartialEq + Default + 'static,
    C: Clone + 'static,
{
    pub fn new() -> Self {
        Bridge {
            shared: RefCell::new(S::default()),
            last: RefCell::new(None),
            sig: OnceLock::new(),
            tx: OnceLock::new(),
            rx: OnceLock::new(),
        }
    }

    /// Main side: replace the shared mirror wholesale.
    pub fn set_shared(&self, s: S) {
        // Do not replace when equal: the main window's per-frame sync_shared
        // constructs a fresh mirror each time, and an unconditional overwrite
        // makes flush's dedup happen after a whole batch of allocations (a
        // frame with unchanged state pays ~35 wasted heap allocations).
        // S: PartialEq is already guaranteed by flush's comparison.
        let mut b = self.shared.borrow_mut();
        if *b == s {
            return;
        }
        *b = s;
    }

    /// Read the shared mirror (the popup component's first-frame initial
    /// value, and main-side queries).
    pub fn shared(&self) -> S {
        self.shared.borrow().clone()
    }

    /// The popup component registers its render Signal (called once in
    /// `use_hook`).
    pub fn register(&self, sig: Signal<S>) {
        let _ = self.sig.set(sig);
    }

    /// Shell/main side: flush the shared mirror into the popup Signal.
    ///
    /// `inject` lets the caller synthesize fields maintained only by the shell
    /// (like the focus epoch) before pushing. The dedup comparison includes the
    /// injected value; no change costs nothing. Not accounted when the signal
    /// is unregistered.
    pub fn flush(&self, inject: impl FnOnce(&mut S)) {
        let mut next = self.shared();
        inject(&mut next);
        let same = self.last.borrow().as_ref() == Some(&next);
        if same {
            return;
        }
        let Some(mut sig) = self.sig.get().copied() else {
            // The popup vdom is not built yet: not accounted; the first flush
            // after registration backfills the push.
            return;
        };
        *self.last.borrow_mut() = Some(next.clone());
        sig.set(next);
    }

    /// Popup side: send a command to the main window (channel created
    /// lazily).
    pub fn send(&self, c: C) {
        let tx = self.tx.get_or_init(|| {
            let (tx, rx) = mpsc::channel();
            let _ = self.rx.set(Mutex::new(rx));
            tx
        });
        let _ = tx.send(c);
    }

    /// Main side: take all queued commands.
    pub fn drain(&self) -> Vec<C> {
        let Some(rx) = self.rx.get() else {
            return Vec::new();
        };
        let rx = rx.lock().unwrap_or_else(|e| e.into_inner());
        let mut out = Vec::new();
        while let Ok(c) = rx.try_recv() {
            out.push(c);
        }
        out
    }
}
