//! Multi-window Blitz shell: the main window + Blitz-rendered child windows
//! created at runtime (find/replace).
//!
//! dioxus_native's built-in `DioxusNativeApplication` only supports a single
//! window at startup, and `WindowEventHandlers` / `DioxusNativeDocument` are
//! both `pub(crate)`, impossible to extend externally. Here we implement our
//! own `ApplicationHandler` directly on top of
//! `blitz_shell::BlitzApplication`:
//!
//! * The main window completes the same context injection as the official
//!   launch in `can_create_surfaces` (window / renderer / shell provider /
//!   document);
//! * Popups are lazily created upon receiving `ShellEvt::PopupShow`: an owned
//!   window in **system-dialog shape** — a normal top-level window (system
//!   title bar + border) gets an owner attached after creation via
//!   `win32::attach_owner`, with content still rendered by blitz — that is,
//!   "the shape of a system dialog, painted by Blitz". The system title bar
//!   (drag, `×`, Alt+F4, system menu) and resize border are provided by the
//!   OS, with only the `×` button; the ownership semantics come for free:
//!   z-order always above the owner, auto-hidden when the owner minimizes, no
//!   taskbar entry; a popup can be dragged out of the main window, resized,
//!   and holds its own keyboard focus and IME composer (see `findwin.rs`'s
//!   `FindComposer`).

pub mod bridge;
pub mod controls;
pub mod motion;

/// Win32-exclusive owned popup glue (see the module doc); other platforms have
/// no popups.
#[cfg(windows)]
mod win32;
#[cfg(windows)]
use win32::place_show;

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::OnceLock;

use blitz_shell::{
    create_default_event_loop, BlitzApplication, BlitzShellEvent, BlitzShellProxy, View,
    WindowConfig,
};
use dioxus_core::{provide_context, ScopeId};
use dioxus_native::prelude::*;
use dioxus_native::use_window;
use dioxus_native::winit;
use dioxus_native::{
    CompositeAlphaMode, DioxusDocument, DioxusNativeWindowRenderer, DocumentConfig, RendererOptions,
};
use winit::application::ApplicationHandler;
use winit::event::{StartCause, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop};
use winit::window::{ImeCapabilities, ImeEnableRequest, ImeRequest, ImeRequestData, WindowId};

use crate::{Popup, PopupId};
// popup_by_id is only used in the Windows PopupShow arm (non-Windows has no
// popups).
#[cfg(windows)]
use crate::popup_by_id;

/// The document's [`blitz_traits::shell::ShellProvider`]: forwards the
/// document's own requests to the winit window.
///
/// The default provider is a no-op, which is why the app never changed the
/// mouse cursor: every region showed the plain arrow — no I-beam over text, no
/// hand over buttons, and no resize arrows on the frameless edges (the
/// non-Windows edge strips added for window resizing rely on this). Repaint
/// requests made by the document itself (image arrival, style invalidation)
/// were dropped too; the app's own waker masked most of that, but forwarding
/// them is what the document expects.
///
/// IME cursor-area and window-title requests keep the trait's defaults: the
/// app positions the composer itself and owns the title.
struct WindowShellProvider {
    window: std::sync::Arc<dyn winit::window::Window>,
}

impl blitz_traits::shell::ShellProvider for WindowShellProvider {
    fn request_redraw(&self) {
        self.window.request_redraw();
    }

    fn set_cursor(&self, icon: Option<winit::cursor::CursorIcon>) {
        match icon {
            Some(icon) => {
                self.window.set_cursor_visible(true);
                self.window.set_cursor(winit::cursor::Cursor::Icon(icon));
            }
            // `cursor:none` — the only case that hides the pointer.
            None => self.window.set_cursor_visible(false),
        }
    }
}

// ---------------------------------------------------------------------------
// UI → shell communication
// ---------------------------------------------------------------------------

/// Window-management events sent from UI code to the shell (transported via
/// `BlitzShellEvent::Embedder`).
#[derive(Debug, Clone)]
pub enum ShellEvt {
    /// Create (first time) or show a popup ([`PopupId`]), placed at screen
    /// logical coordinates `(x, y)` with size `(w, h)` (logical pixels).
    /// `parent` is the main window's HWND (the owned popup's owner).
    PopupShow {
        id: PopupId,
        parent: isize,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
    },
    /// Hide (do not destroy).
    PopupHide { id: PopupId },
    /// The popup caret blink beat (posted by the timer thread started by
    /// `bridge::popup_blink_start`; see the `bridge` module doc — why not CSS
    /// animations).
    BlinkTick,
    /// Diagnostics: inject a synthetic winit touch-event sequence (coordinates
    /// are window client physical pixels). Only goes through the blitz → DOM
    /// pipeline; used to reproduce/verify touch behavior without touch
    /// hardware.
    SimTouchTap { x: f64, y: f64 },
    SimTouchDrag {
        x0: f64,
        y0: f64,
        x1: f64,
        y1: f64,
        steps: usize,
    },
    /// Request closing the main window (issued by the non-Windows
    /// `frameless::close` hook). The shell synthesizes it into the main
    /// window's `CloseRequested`, going through the exact same app-layer
    /// verdict path as a real `×` / Alt+F4 (dirty-document confirmation /
    /// clean immediate exit; the event is not handed to blitz — its default
    /// handling would destroy the window). Windows does not need it:
    /// `PostMessageW(WM_CLOSE)` already produces a real CloseRequested.
    CloseMain,
}

static PROXY: OnceLock<BlitzShellProxy> = OnceLock::new();

// ---------------------------------------------------------------------------
// App-side injection points (the framework knows nothing of business:
// network images, diagnostic log path, menu-bar anchor measurement)
// ---------------------------------------------------------------------------

/// The document's `NetProvider` (taken once when `launch` builds the
/// `DocumentConfig`).
static NET_PROVIDER: OnceLock<std::sync::Arc<dyn blitz_traits::net::NetProvider>> = OnceLock::new();

/// The diagnostic log path provider (called on every log write; when not
/// injected, writes `shell.log` under cwd).
static LOG_PATH: OnceLock<fn() -> Option<std::path::PathBuf>> = OnceLock::new();

/// Main-window layout anchor measurement (like the menu title's left edge):
/// called once per idle round; skipped when not injected.
static ANCHOR_PROBE: OnceLock<fn(&blitz_dom::BaseDocument)> = OnceLock::new();

/// Inject the document's network resource fetcher (the MD preview's <img>
/// etc.; `main` calls it once before `launch`).
pub fn set_net_provider(p: std::sync::Arc<dyn blitz_traits::net::NetProvider>) {
    let _ = NET_PROVIDER.set(p);
}

/// Inject the diagnostic log path provider (writes `shell.log` under cwd when
/// not injected).
pub fn set_log_path(f: fn() -> Option<std::path::PathBuf>) {
    let _ = LOG_PATH.set(f);
}

/// Inject the main-window layout anchor measurement callback (the menu title
/// anchor belongs to business logic and is injected via this callback; called
/// once per idle round on the main window's document).
pub fn set_anchor_probe(f: fn(&blitz_dom::BaseDocument)) {
    let _ = ANCHOR_PROBE.set(f);
}

/// Whether the shell event bridge is ready (true after `launch`). When not
/// ready, find falls back to the in-page bar.
pub fn bridge_ready() -> bool {
    PROXY.get().is_some()
}

/// Send an event to the shell from arbitrary (main-thread) code.
pub fn send(evt: ShellEvt) {
    if let Some(p) = PROXY.get() {
        p.send_event(BlitzShellEvent::embedder_event(evt));
    }
}

// ---------------------------------------------------------------------------
// winit event handlers (a local replica of dioxus_native::use_window_event)
// ---------------------------------------------------------------------------

/// Raw winit event callbacks registered by components. dioxus_native's
/// mechanism of the same name is `pub(crate)`; we substitute our own type
/// here with identical semantics: the callback runs synchronously **before**
/// blitz handles the event and may write Signals (which then trigger a
/// repaint).
type EvtCallback = Box<dyn FnMut(&WindowEvent, &dyn ActiveEventLoop)>;

#[derive(Default)]
pub struct WinEvtHandlers {
    handlers: RefCell<Vec<(WindowId, EvtCallback)>>,
}

impl WinEvtHandlers {
    fn apply(&self, window_id: WindowId, event: &WindowEvent, el: &dyn ActiveEventLoop) {
        for (id, h) in self.handlers.borrow_mut().iter_mut() {
            if *id == window_id {
                dispatch_in_window_runtime(*id, || h(event, el));
            }
        }
    }
}

thread_local! {
    static EVT_HANDLERS: Rc<WinEvtHandlers> = Rc::new(WinEvtHandlers::default());
}

// ---------------------------------------------------------------------------
// Window runtime registry (Dioxus runtime guards for raw winit callbacks)
// ---------------------------------------------------------------------------

thread_local! {
    /// Each window vdom's Dioxus runtime: registered at window creation (the
    /// main window in `can_create_surfaces`, a popup in `open_child`); windows
    /// are only hidden, never destroyed, so lifetimes match the process.
    /// Accounted per `WindowId` — a popup is its own vdom, `GlobalSignal`s are
    /// isolated per runtime, and using the wrong runtime would write the
    /// popup's global state onto the main window.
    static WINDOW_RUNTIMES: RefCell<Vec<(WindowId, Rc<dioxus_core::Runtime>)>> =
        const { RefCell::new(Vec::new()) };
}

/// Register a window vdom's runtime (re-registering the same id overwrites
/// the old value).
fn note_window_runtime(id: WindowId, rt: Rc<dioxus_core::Runtime>) {
    WINDOW_RUNTIMES.with(|slots| {
        let mut slots = slots.borrow_mut();
        match slots.iter_mut().find(|(w, _)| *w == id) {
            Some(slot) => slot.1 = rt,
            None => slots.push((id, rt)),
        }
    });
}

/// A window's registered runtime (`None` when unregistered).
fn window_runtime(id: WindowId) -> Option<Rc<dioxus_core::Runtime>> {
    WINDOW_RUNTIMES.with(|slots| {
        slots
            .borrow()
            .iter()
            .find(|(w, _)| *w == id)
            .map(|(_, rt)| Rc::clone(rt))
    })
}

/// Run `f` inside the window's own Dioxus runtime (run as-is if the current
/// thread is already inside a runtime).
///
/// **Iron rule**: callbacks registered via `use_window_event` are **raw winit
/// callbacks**, called directly by `window_event` — unlike DOM events
/// (`onclick`/`onkeydown`, dispatched by dioxus inside the runtime), they are
/// natively **outside** the Dioxus runtime. The host's event-phase hooks
/// (like `mdws::overlays_frame`'s candidate panel / floating toolbar state)
/// read and write `GlobalSignal`s and panic without a guard — "Must be called
/// from inside a Dioxus runtime" — crashing on the very first frame at
/// startup (reproduced live 2026-10-17). Every app winit callback is
/// dispatched from here; do not add guards at each call site individually.
///
/// Nesting-safe: `RuntimeGuard` is stack semantics; when already inside a
/// runtime (a DOM event path triggering a winit callback in turn), it does
/// not push again, avoiding shadowing the inner runtime.
fn dispatch_in_window_runtime<R>(id: WindowId, f: impl FnOnce() -> R) -> R {
    if dioxus_core::Runtime::try_current().is_some() {
        return f();
    }
    let Some(rt) = window_runtime(id) else {
        // An unregistered window (called bare in unit tests) keeps the old
        // behavior: the caller's mistake is exposed by the caller.
        return f();
    };
    let _guard = dioxus_core::RuntimeGuard::new(rt);
    f()
}

/// Register a winit event callback for the current window (see
/// [`WinEvtHandlers`]).
pub fn use_window_event(handler: impl FnMut(&WindowEvent, &dyn ActiveEventLoop) + 'static) {
    let window = use_window();
    let handlers = EVT_HANDLERS.with(Rc::clone);
    use_hook(move || {
        let id = window.id();
        handlers.handlers.borrow_mut().push((id, Box::new(handler)));
    });
}

// ---------------------------------------------------------------------------
// ShellApp
// ---------------------------------------------------------------------------

/// A popup child window's routing table.
///
/// The shell knows nothing of each popup's snapshot/command types; it only
/// uses the [`Popup`] trait's function slots to finish up: `flush` flushes the
/// shared mirror into the popup's Signal, `on_focused` finishes up when the
/// popup gets OS focus, and `dark` tells the system title bar which theme to
/// pick. Instances are registered at app startup
/// ([`crate::register_popups`]) and looked up by [`PopupId`].
///
/// Bookkeeping for soft-keyboard avoidance: `(original position, last moved-
/// to position)`, both in screen physical pixels.
///
/// The popup's original position is recorded when the keyboard shoves it up;
/// when the keyboard collapses, only a window the user has not manually
/// re-dragged (its current position still equals the last moved-to position)
/// is put back.
type KbSave = ((i32, i32), (i32, i32));

struct ChildWin {
    id: WindowId,
    popup: &'static dyn Popup,
    /// Pending-flush flag (set by window_event, consumed by poll_children)
    focus_pending: std::cell::Cell<bool>,
    /// Bookkeeping for soft-keyboard avoidance:
    /// `Some((original position, last moved-to position))`.
    ///
    /// Only a popup that was moved up while the keyboard was visible has a
    /// value; when the keyboard collapses, only a window the user has not
    /// manually re-dragged (its current position still equals the last
    /// moved-to position) is put back.
    #[cfg_attr(not(windows), allow(dead_code))] // Only consumed by avoid_soft_keyboard
    kb_saved: std::cell::Cell<Option<KbSave>>,
}

pub struct ShellApp {
    inner: BlitzApplication<DioxusNativeWindowRenderer>,
    evt: Rc<WinEvtHandlers>,
    main_cfg: Option<WindowConfig<DioxusNativeWindowRenderer>>,
    children: Vec<ChildWin>,
    /// The main window's winit id (recorded at can_create_surfaces; for touch
    /// simulation).
    main_id: Option<WindowId>,
    /// The application's background color, handed to blitz as the pre-first-frame
    /// paint (`WindowConfig::with_background`, InfEditor patch 17) for every
    /// window the shell creates — the main window in `launch`, popups here.
    /// Without it a window is composited before its first frame: the white
    /// flash this patch exists to prevent.
    bg: Option<[u8; 3]>,
}

/// Whether IME is turned off in the test environment (`INFEDITOR_IME=0`).
///
/// Any place in the component layer that **actively** does
/// `ImeRequest::Enable` must ask here first: a window-level enable is a
/// one-shot action, but CSV cell editing re-enables on every entry into
/// editing — guarding only the window level means typing one letter in a test
/// gets composed by the real IME into a CJK character (observed: "x,y"
/// became a CJK character followed by ",y", and subsequent keys stayed in
/// the composition string, never committing).
pub fn ime_disabled() -> bool {
    std::env::var_os("INFEDITOR_IME").is_some_and(|v| v == "0")
}

/// Turn IME on at **window level** after window creation. Skipped when
/// `INFEDITOR_IME=0` (see the function doc).
///
/// Why it must be enabled explicitly here: the winit fork fixes
/// `set_ime_allowed(false)` at window creation, while blitz's only enable
/// point — `node.focus()` detecting the target is a TextInput — happens at
/// mutation flush, **before the layout attaches the TextInput data**: the
/// autofocus call is bound to miss, and afterwards there is no reopening
/// path, so IME is dead for the whole app (round 26's diagnosis, proven by a
/// vendor log probe: `node.focus tag=textarea text_input=false`).
fn enable_ime(window: &dyn winit::window::Window) {
    // Escape hatch for GUI live testing: tests inject bare SendInput virtual
    // keys, while a real Chinese IME intercepts letters into a composition
    // string and eats Enter for confirming the composition (observed: five
    // keys "grape" got composed into one Ime::Commit("grape"); the commit key
    // never reached the editor), so every editing GUI test failed. Setting 0
    // restores the old "IME dead" behavior; a real person's Chinese input is
    // unaffected (which is also why this switch only appears in test
    // environments).
    if ime_disabled() {
        return;
    }
    // The `with_cursor_area()` capability is mandatory: only then does the
    // win32 backend accept candidate-window anchor updates (the component
    // layer's request_ime_update(Update)); with an empty capability set they
    // are silently dropped (winit-win32: "discarding IME cursor area update
    // without capability enabled"). `ImeEnableRequest::new` requires
    // capabilities and initial values to be declared in pairs (XOR check), so
    // an initial anchor starting at 0,0 is given as well; the component layer
    // refreshes it to the real position on render.
    // A rejected request (should be impossible: the capabilities/values are paired by
    // construction) must degrade to "no IME anchor" — it must not panic on startup.
    if let Some(req) = ImeEnableRequest::new(
        ImeCapabilities::new().with_cursor_area(),
        ImeRequestData::default().with_cursor_area(
            winit::dpi::LogicalPosition::new(0.0, 0.0).into(),
            winit::dpi::LogicalSize::new(0.0, 0.0).into(),
        ),
    ) {
        let _ = window.request_ime_update(ImeRequest::Enable(req));
    }
}

// ---------------------------------------------------------------------------
// Popup IME composition string (Preedit) mirror
// ---------------------------------------------------------------------------
//
// blitz renders the IME composition string inside the (invisible) composer
// textarea, which the popup's visible layer cannot see — typing pinyin gives
// the input field no feedback at all, looking exactly like "Chinese input is
// not supported". The shell captures a copy of the composition string in the
// event callback (before blitz handles it) into this thread-local, and the
// popup's flush (like findwin's `flush_snap`) reads it and renders a visible
// composition span. Cleared after a Commit — the committed text goes through
// the normal insertion channel via vendor patch seven's synthetic keydown and
// is not re-inserted here.

thread_local! {
    static POPUP_PREEDIT: std::cell::RefCell<String> = const { std::cell::RefCell::new(String::new()) };
}

/// Read the current composition string when a popup component flushes (same
/// thread; the shell writes it in the winit callback).
pub fn popup_preedit() -> String {
    POPUP_PREEDIT.with(|p| p.borrow().clone())
}

/// Shell side: write/clear the composition string.
fn set_popup_preedit(text: &str) {
    POPUP_PREEDIT.with(|p| {
        if p.borrow().as_str() != text {
            *p.borrow_mut() = text.to_string();
        }
    });
}

impl ShellApp {
    pub fn new(
        proxy: BlitzShellProxy,
        event_queue: std::sync::mpsc::Receiver<BlitzShellEvent>,
        main_cfg: WindowConfig<DioxusNativeWindowRenderer>,
        bg: Option<[u8; 3]>,
    ) -> Self {
        let _ = PROXY.set(proxy.clone());
        ShellApp {
            inner: BlitzApplication::new(proxy, event_queue),
            evt: EVT_HANDLERS.with(Rc::clone),
            main_cfg: Some(main_cfg),
            children: Vec::new(),
            main_id: None,
            bg,
        }
    }

    fn handle_shell_evt(&mut self, el: &dyn ActiveEventLoop, evt: ShellEvt) {
        match evt {
            ShellEvt::SimTouchTap { x, y } => {
                self.inject_touch(el, &TouchSeq::Tap { x, y });
            }
            ShellEvt::SimTouchDrag {
                x0,
                y0,
                x1,
                y1,
                steps,
            } => {
                self.inject_touch(
                    el,
                    &TouchSeq::Drag {
                        x0,
                        y0,
                        x1,
                        y1,
                        steps,
                    },
                );
            }
            ShellEvt::PopupShow {
                id,
                parent,
                x,
                y,
                w,
                h,
            } => {
                #[cfg(windows)]
                {
                    if let Some(c) = self.children.iter().find(|c| c.popup.id() == id) {
                        if let Some(v) = self.inner.windows.get_mut(&c.id) {
                            // The minimum size follows the content mode (find/
                            // replace have different row counts).
                            let (min_w, min_h) = c.popup.min_size();
                            v.window.set_min_surface_size(Some(
                                winit::dpi::LogicalSize::new(min_w, min_h).into(),
                            ));
                            let becoming_visible = !v.is_visible;
                            place_show(&v.window, parent, x, y, w, h, c.popup.dark());
                            v.is_visible = true;
                            // The blink count only follows "hidden → visible"
                            // transitions: a repeated PopupShow (re-pushed while
                            // the popup is already visible, e.g. switching tabs
                            // inside the find popup re-calls show_find_child)
                            // would also count, and POPUPS_OPEN would only
                            // grow, leaving the blink thread spinning forever.
                            if becoming_visible {
                                crate::shell::bridge::popup_blink_start();
                            }
                            v.request_redraw();
                        }
                    } else if let Some(popup) = popup_by_id(id) {
                        crate::shell::bridge::popup_blink_start();
                        self.open_child(el, popup, parent, [x, y, w, h]);
                    }
                }
                #[cfg(not(windows))]
                {
                    // Popups (owned windows) only exist on Windows: owner
                    // attachment and taskbar suppression are Win32 semantics.
                    // The app layer on other platforms has already fallen back
                    // to the in-page dialog before sending (see each
                    // `open_*_window`'s cfg gating); this is the silent
                    // backstop.
                    let _ = (id, parent, x, y, w, h);
                }
            }
            ShellEvt::PopupHide { id } => {
                crate::shell::bridge::popup_blink_stop();
                // Popup hidden: the composition mirror is voided (the next
                // Show starts from an empty string)
                set_popup_preedit("");
                if let Some(c) = self.children.iter_mut().find(|c| c.popup.id() == id) {
                    // The popup may be reopened elsewhere; a saved pre-keyboard
                    // position from this session must not be restored later
                    // (it would yank the new window back to the old spot).
                    c.kb_saved.set(None);
                    if let Some(v) = self.inner.windows.get_mut(&c.id) {
                        v.window.set_visible(false);
                        v.is_visible = false;
                    }
                }
            }
            ShellEvt::BlinkTick => {
                // Flip the blink phase, push the new phase into every
                // **visible** popup snapshot and poll-render immediately. A
                // user event is always dispatched in the message pump (unlike
                // WM_PAINT, which can be starved by another window's repaint
                // stream); this is the caret blink's driving source — see the
                // `bridge` module doc. Hidden windows are skipped: a phase flip
                // is meaningless for an invisible DOM, and popup_blink_start
                // resets the phase when shown again.
                crate::shell::bridge::blink_toggle();
                for i in 0..self.children.len() {
                    let id = self.children[i].id;
                    self.children[i].popup.flush();
                    if let Some(v) = self.inner.windows.get_mut(&id) {
                        if v.is_visible {
                            v.poll();
                        }
                    }
                }
                // Main window caret: schedule only one frame — the new
                // phase is written back to state by the frame hook
                // (`caret_phase`, see `opens.rs::needs_idle_poll`), and the
                // next render naturally flips the phase. Exactly 2 frames per
                // 530ms at idle steady state.
                if crate::shell::bridge::main_blink_on() {
                    if let Some(main_id) = self.main_id {
                        if let Some(v) = self.inner.windows.get_mut(&main_id) {
                            v.request_redraw();
                        }
                    }
                }
            }
            ShellEvt::CloseMain => {
                // Synthesize the main window's CloseRequested, going through
                // the same path as a real × / Alt+F4: `evt.apply` hands it to
                // the app layer's verdict (dirty-document confirmation / clean
                // immediate `event_loop.exit()`). The event is **not** handed
                // to blitz (see the CloseRequested branch's doc in
                // `window_event`).
                if let Some(id) = self.main_id {
                    let evt = WindowEvent::CloseRequested;
                    self.evt.apply(id, &evt, el);
                }
            }
        }
    }
}

impl ApplicationHandler for ShellApp {
    fn can_create_surfaces(&mut self, event_loop: &dyn ActiveEventLoop) {
        // Main window: the same context injection as dioxus_native's launch.
        if let Some(cfg) = self.main_cfg.take() {
            let mut view = View::init(cfg, event_loop, &self.inner.proxy);
            // Nothing to do about visibility here: blitz shows the window itself
            // right away, already painted with the background color the app
            // passed in (`WindowConfig::with_background`, InfEditor patch 17), so
            // there is no white first screen to cover. The old DWM cloak that
            // used to hide the window for the whole cold start is gone — startup
            // paints instead of hiding, and the shutdown path does the same (see
            // `Drop`).
            timing("window+renderer");
            let winit_window = std::sync::Arc::clone(&view.window);
            // The composer is the only keyboard/IME landing point, always on at
            // window level (see enable_ime's long doc).
            enable_ime(&*winit_window);
            let renderer = view.renderer.clone();
            let window_id = view.window_id();
            let doc = view.downcast_doc_mut::<DioxusDocument>();
            // Install the real shell provider before it is cloned into the
            // Dioxus context below: the document uses it for cursor changes and
            // its own repaint requests (see `WindowShellProvider`).
            doc.inner
                .borrow_mut()
                .set_shell_provider(std::sync::Arc::new(WindowShellProvider {
                    window: std::sync::Arc::clone(&winit_window),
                }));
            // Runtime accounting for raw winit callbacks (see
            // dispatch_in_window_runtime).
            note_window_runtime(window_id, doc.vdom.runtime());
            doc.vdom.in_scope(ScopeId::ROOT, || {
                let shared: Rc<dyn document::Document> = Rc::new(document::NoOpDocument);
                provide_context(shared);
            });
            let shell_provider = doc.inner.borrow().shell_provider.clone();
            doc.vdom
                .in_scope(ScopeId::ROOT, move || provide_context(shell_provider));
            doc.vdom
                .in_scope(ScopeId::ROOT, move || provide_context(renderer));
            doc.vdom
                .in_scope(ScopeId::ROOT, move || provide_context(winit_window));
            doc.initial_build();
            timing("initial_build");
            view.request_redraw();
            self.main_id = Some(window_id);
            self.inner.windows.insert(window_id, view);
        }
        // Restore the render surfaces of all windows (including registered
        // pending ones).
        self.inner.can_create_surfaces(event_loop);
        timing("surfaces_ready");
    }

    fn resumed(&mut self, event_loop: &dyn ActiveEventLoop) {
        self.inner.resumed(event_loop);
        // Surface creation / swapchain reconfiguration in the first resumed is
        // part of the startup segment (the GPU side often first truly touches
        // the swapchain here rather than in the renderer's constructor).
        timing_once("resumed");
    }

    fn suspended(&mut self, event_loop: &dyn ActiveEventLoop) {
        self.inner.suspended(event_loop);
    }

    fn destroy_surfaces(&mut self, event_loop: &dyn ActiveEventLoop) {
        self.inner.destroy_surfaces(event_loop);
    }

    fn new_events(&mut self, event_loop: &dyn ActiveEventLoop, cause: StartCause) {
        timing_once("first_new_events");
        self.inner.new_events(event_loop, cause);
    }

    fn about_to_wait(&mut self, event_loop: &dyn ActiveEventLoop) {
        timing_once("first_about_to_wait");
        // The window rects queued by the touch follow loop land here in one
        // shot (see `crate::win::frameless::flush_touch_gesture`): the window
        // moves once per event batch instead of once per PointerMoved.
        crate::win::frameless::flush_touch_gesture();
        self.inner.about_to_wait(event_loop);
        self.poll_sim_touch(event_loop);
        // Soft-keyboard avoidance (popup side): when the keyboard shows, move
        // each occluded popup window up as a whole. Runs before poll_children so
        // the Moved events produced by the move are handled in the same pass.
        self.avoid_soft_keyboard();
        // Measure the menu-caption anchors (used by Alt+letter to open a
        // dropdown). Layout is resolved on every render, so what we read here is
        // always the previous frame's fresh value.
        self.measure_caption_anchors();
        // Child windows (the find popup) have no continuous winit event stream:
        // blitz only queues a Poll after a window event, and DioxusDocument::poll's
        // wait_for_work future is dropped as soon as it returns Pending (the waker
        // dies with it), so an external Signal write (findwin::flush_snap) would
        // never be woken. Poll once here on every idle pass as a fallback.
        self.poll_children();
        // Accessibility tree (accesskit → UIA: screen readers / automation).
        //
        // blitz only ever builds the tree once, at `InitialTreeRequested` (see the
        // forwarding of `BlitzShellEvent::Accessibility`); after that the document
        // can change and nobody pushes again, so a screen reader would get the
        // snapshot from startup. Refresh once per idle pass here: with no client
        // attached `update_if_active` is a cheap no-op (the tree-building closure
        // never runs) — the cost is paid only when a UIA client is really there.
        self.refresh_accessibility();
        // DOM dump (diagnostics): with INF_DOM_DUMP=1, once any popup has
        // appeared, write the DOM (tags + attributes) of every window into the
        // shell log in one go — used to compare the two windows' actual DOM.
        if std::env::var("INF_DOM_DUMP").is_ok()
            && !DOM_DUMPED.swap(true, std::sync::atomic::Ordering::Relaxed)
        {
            // Decide by "a created popup child window is visible" (an ASCII
            // technical id: the PopupId from the popup registry) instead of
            // comparing localized title strings — titles change with the language,
            // so a hard-coded Chinese title would never match in an English UI
            // (silently disabling the dump).
            let popup_ready = self
                .children
                .iter()
                .any(|c| self.inner.windows.get(&c.id).is_some_and(|v| v.is_visible));
            if popup_ready {
                for (wid, view) in self.inner.windows.iter() {
                    append_log(&format!(
                        "==== DOM window {:?} title={:?} ====",
                        wid,
                        view.window.title()
                    ));
                    dom_walk(0, &view.doc.inner(), view.doc.inner().root_element().id);
                }
            } else {
                // The popup has not appeared yet: try again on the next idle pass
                DOM_DUMPED.store(false, std::sync::atomic::Ordering::Relaxed);
            }
        }
    }

    fn window_event(
        &mut self,
        event_loop: &dyn ActiveEventLoop,
        window_id: WindowId,
        event: WindowEvent,
    ) {
        // The diagnostic log line gets formatted **before** the switch is
        // checked (once per RedrawRequested / PointerMoved, plus a DOM focus query
        // for keyboard events) — in a release build that is two string allocations
        // per frame. Cache the check behind a OnceLock first: zero cost when off.
        if shell_log_on() {
            // blitz's default CloseRequested handling destroys the window (a
            // closed child can never be reopened) or even turns the whole app into
            // a zombie (main window closed while children are still in the map —
            // the event loop never exits), so both kinds of close are intercepted
            // here.
            let child_idx = self.children.iter().position(|c| c.id == window_id);
            let is_child = child_idx.is_some();
            slog(&format!(
                "winit evt (child={is_child}): {:?}{}",
                match &event {
                    WindowEvent::RedrawRequested => "RedrawRequested".to_string(),
                    WindowEvent::SurfaceResized(s) => format!("SurfaceResized({s:?})"),
                    WindowEvent::Focused(f) => format!("Focused({f})"),
                    WindowEvent::KeyboardInput { .. } => "KeyboardInput".to_string(),
                    WindowEvent::PointerMoved {
                        position, source, ..
                    } => format!(
                        "PointerMoved @{:?} src={source:?}",
                        (position.x, position.y)
                    ),
                    WindowEvent::PointerButton {
                        position,
                        state,
                        button,
                        primary,
                        ..
                    } => format!(
                        "PointerButton {state:?} btn={button:?} primary={primary} @{:?}",
                        (position.x, position.y)
                    ),
                    WindowEvent::PointerEntered {
                        position,
                        kind,
                        primary,
                        ..
                    } => format!(
                        "PointerEntered kind={kind:?} primary={primary} @{:?}",
                        (position.x, position.y)
                    ),
                    WindowEvent::PointerLeft {
                        position,
                        kind,
                        primary,
                        ..
                    } => format!(
                        "PointerLeft kind={kind:?} primary={primary} @{:?}",
                        position.map(|p| (p.x, p.y))
                    ),
                    other => format!("{other:?}"),
                },
                // For keyboard events, append the currently focused DOM node
                // (diagnosing the composer focus chain)
                if let WindowEvent::KeyboardInput { event, .. } = &event {
                    let focus = self
                        .inner
                        .windows
                        .get(&window_id)
                        .map(|v| match v.doc.inner().get_focussed_node_id() {
                            Some(id) => match v.doc.inner().get_node(id) {
                                Some(node) => {
                                    let tag = node
                                        .data
                                        .downcast_element()
                                        .map(|e| e.name.local.to_string())
                                        .unwrap_or_else(|| "<non-element>".into());
                                    let dx_id = node
                                        .element_data()
                                        .and_then(|e| {
                                            e.attrs
                                                .iter()
                                                .find(|a| a.name.local == *"data-dioxus-id")
                                        })
                                        .map(|a| a.value.clone())
                                        .unwrap_or_else(|| "<none>".into());
                                    format!("{id:?} tag={tag} dx_id={dx_id}")
                                }
                                None => format!("{id:?} <no node>"),
                            },
                            None => "<no focus>".to_string(),
                        })
                        .unwrap_or_default();
                    let text = format!("{:?}", event.text);
                    format!(" text={text} focus_node={focus}")
                } else {
                    String::new()
                }
            ));
        }
        let child_idx = self.children.iter().position(|c| c.id == window_id);
        // Interleave the child window's IME composition string (see the
        // POPUP_PREEDIT comment): composing / clearing / committing all sync to the
        // mirror, and the popup's visible layer renders the composition string from
        // it. Flush + poll **immediately** after intercepting (same safe-point shape
        // as BlinkTick: we are before blitz handles this event, nothing is borrowed
        // from the document) — if we only write without flushing, the composition
        // string waits for the next BlinkTick (≤265ms) and per-key typing always
        // lags by one beat.
        if let Some(idx) = child_idx {
            let ime_touched = match &event {
                winit::event::WindowEvent::Ime(winit::event::Ime::Preedit(text, _)) => {
                    set_popup_preedit(text);
                    true
                }
                winit::event::WindowEvent::Ime(
                    winit::event::Ime::Commit(_) | winit::event::Ime::Disabled,
                ) => {
                    set_popup_preedit("");
                    true
                }
                _ => false,
            };
            if ime_touched {
                let id = self.children[idx].id;
                self.children[idx].popup.flush();
                if let Some(v) = self.inner.windows.get_mut(&id) {
                    if v.is_visible {
                        v.poll();
                    }
                }
            }
        }
        if matches!(event, WindowEvent::CloseRequested) {
            if let Some(idx) = child_idx {
                // System `×`: hide the popup and run the normal close flow
                // (store geometry, give focus back).
                if let Some(v) = self.inner.windows.get_mut(&window_id) {
                    v.window.set_visible(false);
                    v.is_visible = false;
                }
                self.children[idx].popup.request_close();
            } else {
                // Main window close: **hand it to the application layer for a
                // decision first** — with unsaved files the application layer shows
                // a confirmation dialog (no exit, no window destruction), and after
                // confirming it calls `event_loop.exit()` itself (see the host's
                // RedrawRequested consumption of quit_requested). The event never
                // reaches blitz: its default handling destroys the window, and both
                // an orphaned child and a resurrected main window are dead ends (see
                // the original comment below).
                self.evt.apply(window_id, &event, event_loop);
            }
            return;
        }
        // A child window got OS focus: set the pending-flush flag. We cannot flush
        // directly here — a Signal write inside a winit window_event callback does
        // not enter the popup vdom's scheduling (measured: rendering is postponed
        // indefinitely), so poll_children does the flush at a safe point instead.
        if let Some(idx) = child_idx {
            if matches!(event, WindowEvent::Focused(true)) {
                self.children[idx].focus_pending.set(true);
            }
        }
        self.evt.apply(window_id, &event, event_loop);
        // First window event: the gap between `initial_build` and it is the event
        // loop's first iteration (winit init + touching the first surface/
        // swapchain); it belongs to no application code, so this is the only point
        // where "waiting for events" and "rendering" can be measured separately.
        timing_once("first_window_event");
        // While a touch window gesture (title-bar drag / edge resize) is active,
        // main-window pointer moves only enter the follow loop (the `evt.apply`
        // above has already diverted them to `win32::update_touch_gesture`) and are
        // no longer handed to blitz: per-event hit-tests / hovers during the gesture
        // would trigger whole-tree re-render storms (the source of drag flicker),
        // and the DOM nodes involved may already be unmounted so the target is dead.
        // Presses still reach blitz (the gesture starts when it dispatches DOM
        // events); the release ends the gesture in `evt.apply` first, so by the time
        // we check we are no longer in a gesture and it is delivered normally —
        // blitz's key state does not get stuck.
        let gesture_move = matches!(event, WindowEvent::PointerMoved { .. })
            && self.main_id == Some(window_id)
            && crate::win::frameless::in_touch_gesture();
        let main_redraw =
            matches!(event, WindowEvent::RedrawRequested) && self.main_id == Some(window_id);
        if main_redraw {
            timing_once("first_redraw");
            frame_tick();
        }
        if !gesture_move {
            self.inner.window_event(event_loop, window_id, event);
        }
        if main_redraw {
            timing_once("first_frame_painted");
        }
    }

    fn proxy_wake_up(&mut self, event_loop: &dyn ActiveEventLoop) {
        while let Ok(event) = self.inner.event_queue.try_recv() {
            match event {
                BlitzShellEvent::Embedder(e) => {
                    slog(&format!("embedder evt: {e:?}"));
                    if let Some(evt) = e.downcast_ref::<ShellEvt>() {
                        let evt = evt.clone();
                        self.handle_shell_evt(event_loop, evt);
                    }
                    // The remaining embedder events (official CreateHeadElement
                    // etc.) are unused by this application
                }
                other => {
                    slog(&format!("shell evt: {other:?}"));
                    self.inner.handle_blitz_shell_event(event_loop, other)
                }
            }
        }
        self.poll_children();
    }
}

impl Drop for ShellApp {
    fn drop(&mut self) {
        // Shutdown, handled the same way as startup: repaint the windows with
        // their background color instead of hiding them.
        //
        // A window whose GPU surface is gone but whose HWND is still visible
        // gets the Win32 fallback surface composed — white. The teardown order
        // is fixed inside blitz (`View::drop` releases the surface first — "a
        // GPU surface must not outlive the window" — and only then do the
        // window/document fields drop), so the gap cannot be closed by
        // reordering; it is not short either (dropping the VDOM + DOM tree takes
        // a visible amount of time).
        //
        // Painting the client area with the same color the window started with
        // (blitz patch 17, `View::paint_background`) makes that window show the
        // theme background instead of white. The window keeps its size and
        // position until the process exits, exactly like the startup side, where
        // the window is painted rather than hidden.
        //
        // The old DWM cloak (hide every window during teardown) was removed on
        // request: it made the window *disappear* before the process was gone,
        // which is the same "the window goes away" complaint the startup cloak
        // drew.
        #[cfg(windows)]
        for v in self.inner.windows.values() {
            v.paint_background();
        }
    }
}

impl ShellApp {
    /// Refresh every window's accessibility tree (see the notes in
    /// `about_to_wait`).
    fn refresh_accessibility(&mut self) {
        for w in self.inner.windows.values_mut() {
            w.build_accessibility_tree();
        }
    }

    /// Soft-keyboard avoidance (popup side).
    ///
    /// The main window yields the bottom of the viewport via `kb_inset` (content
    /// reflow, see `ui::softkb`); popups are fixed-pixel small panels (find bar /
    /// settings dialog) where reflow is pointless — the whole window just moves
    /// up. Both share the same detection (120ms throttle), checked on every idle
    /// pass of the event loop:
    ///
    /// * keyboard visible and the popup's bottom is occluded → remember the
    ///   original position, move the whole window above the keyboard (leaving a
    ///   [`KB_GAP`] gap); afterwards only re-move when the target changes (the
    ///   keyboard height varies);
    /// * keyboard dismissed → put "windows we moved and the user has not manually
    ///   dragged since" back where they were (verifying before the move that the
    ///   current position still equals where we last moved it).
    ///
    /// Only `children` (popups) are walked; the main window is not among them — it
    /// has the `kb_inset` path, and moving it whole would push the title bar off
    /// screen.
    #[cfg(windows)]
    fn avoid_soft_keyboard(&mut self) {
        /// Gap left between a popup's bottom and the keyboard's top (physical px)
        const KB_GAP: i32 = 8;
        let top = match crate::win::softkb::keyboard_top_screen_px() {
            Some(t) => t,
            None => {
                // Keyboard dismissed: put back the windows we moved (leave
                // user-dragged ones alone)
                for c in self.children.iter_mut() {
                    let Some(((sx, sy), (nx, ny))) = c.kb_saved.take() else {
                        continue;
                    };
                    let Some(v) = self.inner.windows.get(&c.id) else {
                        continue;
                    };
                    if !v.is_visible {
                        continue;
                    }
                    if let Ok(p) = v.window.outer_position() {
                        if (p.x, p.y) == (nx, ny) {
                            crate::win::frameless::move_window_screen(v.window.as_ref(), sx, sy);
                        }
                    }
                }
                return;
            }
        };
        for c in self.children.iter_mut() {
            let Some(v) = self.inner.windows.get(&c.id) else {
                continue;
            };
            if !v.is_visible {
                continue;
            }
            let Ok(pos) = v.window.outer_position() else {
                continue;
            };
            let h = v.window.outer_size().height as i32;
            if pos.y + h <= top {
                continue; // not occluded by the keyboard
            }
            let nudged = (pos.x, (top - h - KB_GAP).max(0));
            if nudged.1 == pos.y {
                // Already at the target position (e.g. keyboard very tall, popup
                // can only hug the screen top): just update the bookkeeping
                let orig = c.kb_saved.get().map(|(o, _)| o).unwrap_or((pos.x, pos.y));
                c.kb_saved.set(Some((orig, nudged)));
                continue;
            }
            let orig = c.kb_saved.get().map(|(o, _)| o).unwrap_or((pos.x, pos.y));
            c.kb_saved.set(Some((orig, nudged)));
            crate::win::frameless::move_window_screen(v.window.as_ref(), nudged.0, nudged.1);
        }
    }

    #[cfg(not(windows))]
    fn avoid_soft_keyboard(&mut self) {}

    /// Write each menu caption's **true** left edge from the main window's layout
    /// into the titlebar's measurement slot.
    ///
    /// Opening a dropdown with <kbd>Alt</kbd>+letter has no pointer event, so the
    /// anchor can only come from the layout itself. Every caption renders with
    /// `inf-cap-{idx}` (see `titlebar::caption_dom_id`); blitz records the id
    /// attribute in its `nodes_to_id` map, so `get_element_by_id` reaches it in one
    /// hop; `get_client_bounding_rect` returns the same coordinate space as
    /// `client_coordinates()` (CSS px, relative to the root element), identical to
    /// the click path's `client_x - element_x` — window size, DPI and font changes
    /// are followed automatically, with no more estimates.
    ///
    /// Called once per idle pass: two hash lookups plus one rect read, far cheaper
    /// than a frame of rendering. Skips the write when the id is not found (the
    /// menu bar has not rendered yet) or there is no layout rect; the estimate
    /// remains the fallback.
    fn measure_caption_anchors(&mut self) {
        use blitz_dom::BaseDocument;

        let Some(probe) = ANCHOR_PROBE.get() else {
            return;
        };
        let Some(main_id) = self.main_id else { return };
        let Some(view) = self.inner.windows.get(&main_id) else {
            return;
        };
        // `Document::inner`'s `DocGuard` Derefs to `BaseDocument` — all the query
        // APIs live on it, no downcast needed.
        let doc: &BaseDocument = &view.doc.inner();
        probe(doc);
    }

    /// Poll every popup's vdom (see the notes in `about_to_wait`).
    fn poll_children(&mut self) {
        static CALLS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        for i in 0..self.children.len() {
            let c = &self.children[i];
            // Flush the pending focus flag (see the note in window_event): must be
            // written before the poll, so the same pass pushes the focus generation
            // into the popup vdom and remounts the composer.
            if c.focus_pending.replace(false) {
                c.popup.on_focused();
                c.popup.flush();
            }
            let id = c.id;
            if let Some(v) = self.inner.windows.get_mut(&id) {
                let polled = v.poll();
                if polled {
                    let focus = v
                        .doc
                        .inner()
                        .get_focussed_node_id()
                        .map(|id| {
                            let tag = v
                                .doc
                                .inner()
                                .get_node(id)
                                .and_then(|n| n.data.downcast_element())
                                .map(|e| e.name.local.to_string())
                                .unwrap_or_default();
                            format!("{id:?}({tag})")
                        })
                        .unwrap_or_else(|| "<none>".into());
                    slog(&format!(
                        "poll_child call#{n} {:?} rendered=true focus={focus}",
                        self.children[i].popup.id()
                    ));
                } else if n % 500 == 0 {
                    slog(&format!("poll_child call#{n} rendered=false"));
                }
            }
        }
    }
}

/// Diagnostic log entry point (for all modules to call; writes shell.log when
/// `INF_SHELL_LOG=1`).
pub fn debug_log(msg: &str) {
    if shell_log_on() {
        slog(&format!("[dbg] {msg}"));
    }
}

/// Whether diagnostic logging is enabled. Cached: `std::env::var` on every
/// call was a per-event syscall on the hot path (RedrawRequested, pointer
/// moves), and `debug_log(format!(...))` callers still paid the formatting.
pub fn shell_log_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    // Test the value, not the presence: `INF_SHELL_LOG=0` means "off" (by
    // convention people pass 0 to disable diagnostics; the old is_ok() check
    // instead turned everything on and, once cached by the OnceLock, could not be
    // corrected within the process).
    *ON.get_or_init(|| std::env::var("INF_SHELL_LOG").is_ok_and(|v| v != "0"))
}

// ---------------------------------------------------------------------------
// Startup phase timing (`INF_TIMING=1`)
//
// The first step of startup optimization is "measure first, don't guess": font
// enumeration and wgpu init both live in vendor, and without application-side
// instrumentation you are flying blind. This provides zero-cost phase markers
// (just one OnceLock lookup when the env var is unset), printed to stderr:
//
//   [timing] main: 0.00ms
//   [timing] vdom: 1.20ms
//   [timing] document: 13.80ms
//   [timing] window+renderer: 120.40ms
//   [timing] initial_build: 180.10ms
//   [timing] first_redraw: 205.30ms
//
// All values are relative to the first `timing()` call in the process (the top
// of `main`).
// ---------------------------------------------------------------------------

static TIMING_ORIGIN: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();

fn timing_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("INF_TIMING").is_some())
}

/// Print one startup phase (no-op when `INF_TIMING` is unset).
pub fn timing(label: &str) {
    if !timing_on() {
        return;
    }
    let t0 = TIMING_ORIGIN.get_or_init(std::time::Instant::now);
    let line = format!(
        "[timing] {label}: {:.2}ms",
        t0.elapsed().as_secs_f64() * 1000.0
    );
    eprintln!("{line}");
    // Also into shell.log: GUI automation discards the child process's stderr in
    // the terminal, so the log file is the assertable oracle (see the tests_py
    // driver).
    if shell_log_on() {
        append_log(&line);
    }
}

/// A phase printed only once (for high-frequency events like
/// `RedrawRequested`).
pub fn timing_once(label: &'static str) {
    static SEEN: std::sync::Mutex<Vec<&'static str>> = std::sync::Mutex::new(Vec::new());
    if !timing_on() {
        return;
    }
    if let Ok(mut seen) = SEEN.lock() {
        if seen.contains(&label) {
            return;
        }
        seen.push(label);
    }
    timing(label);
}

/// Frame-interval statistics (`INF_FRAME_LOG=1`, writes shell.log with the
/// `[frame]` prefix).
///
/// Outputs p50/p95/max every 120 frames: the only trustworthy measure of
/// scrolling performance is "the gap between adjacent RedrawRequested";
/// `viewport::build`'s microbenchmark can only prove line construction is not
/// slow, not that the whole frame is not slow (19 components share one Signal,
/// the re-render cost lives elsewhere).
pub fn frame_tick() {
    use std::cell::{Cell, RefCell};
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if !*ON.get_or_init(|| std::env::var_os("INF_FRAME_LOG").is_some()) {
        return;
    }
    thread_local! {
        static LAST: Cell<Option<std::time::Instant>> = const { Cell::new(None) };
        static BUF: RefCell<Vec<f32>> = const { RefCell::new(Vec::new()) };
    }
    let now = std::time::Instant::now();
    let dt = LAST.with(|l| {
        let prev = l.replace(Some(now));
        prev.map(|p| now.duration_since(p).as_secs_f32() * 1000.0)
    });
    let Some(dt) = dt else { return };
    BUF.with(|b| {
        let mut b = b.borrow_mut();
        b.push(dt);
        if b.len() < 120 {
            return;
        }
        let mut v = std::mem::take(&mut *b);
        v.sort_by(|a, c| a.partial_cmp(c).unwrap_or(std::cmp::Ordering::Equal));
        let p = |q: f32| v[((v.len() - 1) as f32 * q) as usize];
        let line = format!(
            "[frame] n={} p50={:.2}ms p95={:.2}ms max={:.2}ms",
            v.len(),
            p(0.50),
            p(0.95),
            v[v.len() - 1]
        );
        drop(b);
        append_log(&line);
    });
}

/// Write one line to the diagnostic log file (does not require
/// `INF_SHELL_LOG`; callers check the switch themselves).
fn append_log(line: &str) {
    use std::io::Write;
    let path = match LOG_PATH.get().and_then(|f| f()) {
        Some(p) => p,
        None => std::path::PathBuf::from("shell.log"),
    };
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(f, "{line}");
    }
}

/// UI geometry probe switch (`INF_UI_PROBE=1`): lets GUI automation read the
/// true layout of custom-drawn widgets instead of guessing from colors/pixels.
/// Kept separate from `INF_SHELL_LOG` so the probe alone does not force on
/// per-event logging.
pub fn ui_probe_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("INF_UI_PROBE").is_some())
}

/// Write one probe log line (regardless of `INF_SHELL_LOG`; the caller is
/// responsible for checking `ui_probe_on()`).
pub fn probe_log(line: &str) {
    append_log(line);
}

/// The **labels** of the current dialog's action buttons (same source as the
/// geometry probe).
///
/// Geometry (the center of `inf-dlg-btn-N`) can only tell a test "which button
/// by index", but a button's meaning depends on its label: "Cancel" and
/// "Confirm discard" sit in the same box, and clicking the wrong one deletes the
/// user's only rescue snapshot. Tests therefore click by label, not by guessing
/// an index.
static DIALOG_LABELS: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

/// The component layer registers the labels when rendering the action row (a
/// no-op when the probe is off, to avoid taking the lock for nothing).
pub fn set_dialog_labels(labels: Vec<String>) {
    if !ui_probe_on() {
        return;
    }
    if let Ok(mut cur) = DIALOG_LABELS.lock() {
        if *cur != labels {
            *cur = labels;
        }
    }
}

/// The reader end of [`set_dialog_labels`]; an empty `Vec` when no dialog is
/// open.
pub fn dialog_labels() -> Vec<String> {
    DIALOG_LABELS.lock().map(|v| v.clone()).unwrap_or_default()
}

fn slog(msg: &str) {
    if shell_log_on() {
        // log into the config dir: a cwd-relative path scattered shell.log
        // into whatever directory the editor happened to be started from
        append_log(msg);
    }
}

/// One-shot gate for the DOM dump (diagnostics).
static DOM_DUMPED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Recursively print a DOM subtree (elements print tag + all attributes, text
/// truncated) via the shell log.
fn dom_walk(indent: usize, doc: &blitz_dom::BaseDocument, id: blitz_dom::NodeId) {
    use blitz_dom::NodeData;
    let Some(node) = doc.get_node(id) else {
        return;
    };
    let pad: String = " ".repeat(indent);
    match &node.data {
        NodeData::Element(el) => {
            let mut line = format!("{pad}<{} id={:?}>", el.name.local, id);
            for a in el.attrs.iter() {
                line.push_str(&format!(" {}={:?}", a.name.local, a.value));
            }
            append_log(&line);
        }
        NodeData::Text(t) => {
            let c = t.content.trim();
            if !c.is_empty() {
                let head: String = c.chars().take(24).collect();
                append_log(&format!("{pad}#text {id}: {head:?}"));
            }
        }
        _ => append_log(&format!("{pad}#{:?}", std::mem::discriminant(&node.data))),
    }
    for child in node.children.iter() {
        dom_walk(indent + 1, doc, *child);
    }
}

// ---------------------------------------------------------------------------
// Touch simulation (diagnostics): synthetic winit touch event sequences that go
// through the exact same ShellApp::window_event → BlitzApplication → blitz DOM
// pipeline as real input.
// With INF_TOUCH_SIM=1 it polls sim_touch.txt in the cwd, one command per line:
//   tap x y
//   jtap x y [dx dy]   A tap with jitter: after down it drifts by (dx,dy)
//                      (default 5,3) before up, to reproduce blitz's "drift
//                      >2px enters Panning and swallows the Click".
//   drag x0 y0 x1 y1 [steps]
// Coordinates of tap/jtap/rpress/mtap are physical pixels in the main window's
// client area; drag's (x0,y0)→(x1,y1) is the finger's physical screen trajectory.
// The script debug_touch.py generates this file automatically.
// ---------------------------------------------------------------------------

enum TouchSeq {
    Tap {
        x: f64,
        y: f64,
    },
    /// A tap on a popup child window: the coordinates are physical pixels of the
    /// **first visible child window's** client area. Used to verify the touch path
    /// (real touch goes through the same winit → blitz pipeline).
    /// Optional (dx,dy) = drift before lift-off (default 0; pass 5,3 to reproduce
    /// finger jitter).
    PopupTap {
        x: f64,
        y: f64,
        dx: f64,
        dy: f64,
    },
    JTap {
        x: f64,
        y: f64,
        dx: f64,
        dy: f64,
    },
    MouseTap {
        x: f64,
        y: f64,
    },
    RightTap {
        x: f64,
        y: f64,
    },
    Drag {
        x0: f64,
        y0: f64,
        x1: f64,
        y1: f64,
        steps: usize,
    },
}

impl ShellApp {
    fn poll_sim_touch(&mut self, el: &dyn ActiveEventLoop) {
        if std::env::var("INF_TOUCH_SIM").is_err() {
            return;
        }
        let Ok(txt) = std::fs::read_to_string("sim_touch.txt") else {
            return;
        };
        let _ = std::fs::remove_file("sim_touch.txt");
        for line in txt.lines() {
            let mut it = line.split_whitespace();
            let seq = match it.next() {
                Some("tap") => {
                    let p: Vec<f64> = it.filter_map(|s| s.parse().ok()).collect();
                    if p.len() >= 2 {
                        Some(TouchSeq::Tap { x: p[0], y: p[1] })
                    } else {
                        None
                    }
                }
                Some("ptap") => {
                    let p: Vec<f64> = it.filter_map(|s| s.parse().ok()).collect();
                    if p.len() >= 2 {
                        Some(TouchSeq::PopupTap {
                            x: p[0],
                            y: p[1],
                            dx: p.get(2).copied().unwrap_or(0.0),
                            dy: p.get(3).copied().unwrap_or(0.0),
                        })
                    } else {
                        None
                    }
                }
                Some("jtap") => {
                    let p: Vec<f64> = it.filter_map(|s| s.parse().ok()).collect();
                    if p.len() >= 2 {
                        Some(TouchSeq::JTap {
                            x: p[0],
                            y: p[1],
                            dx: p.get(2).copied().unwrap_or(5.0),
                            dy: p.get(3).copied().unwrap_or(3.0),
                        })
                    } else {
                        None
                    }
                }
                Some("rpress") => {
                    let p: Vec<f64> = it.filter_map(|s| s.parse().ok()).collect();
                    if p.len() >= 2 {
                        Some(TouchSeq::RightTap { x: p[0], y: p[1] })
                    } else {
                        None
                    }
                }
                Some("mtap") => {
                    let p: Vec<f64> = it.filter_map(|s| s.parse().ok()).collect();
                    if p.len() >= 2 {
                        Some(TouchSeq::MouseTap { x: p[0], y: p[1] })
                    } else {
                        None
                    }
                }
                Some("drag") => {
                    let p: Vec<f64> = it.filter_map(|s| s.parse().ok()).collect();
                    if p.len() >= 4 {
                        Some(TouchSeq::Drag {
                            x0: p[0],
                            y0: p[1],
                            x1: p[2],
                            y1: p[3],
                            steps: p.get(4).copied().unwrap_or(10.0) as usize,
                        })
                    } else {
                        None
                    }
                }
                _ => None,
            };
            if let Some(seq) = seq {
                self.inject_touch(el, &seq);
            }
        }
    }

    fn inject_touch(&mut self, el: &dyn ActiveEventLoop, seq: &TouchSeq) {
        use winit::dpi::PhysicalPosition;
        use winit::event::{
            ButtonSource, ElementState, FingerId, Force, PointerKind, PointerSource,
        };
        let Some(id) = self.main_id else { return };
        let fid = FingerId::from_raw(42usize);
        let kind = PointerKind::Touch(fid);
        let feed = |this: &mut Self, ev: WindowEvent| {
            // Go through the full ShellApp::window_event pipeline (including the
            // pointer swallowing during gestures) — identical to real input.
            this.window_event(el, id, ev);
        };
        let button = |state, position| WindowEvent::PointerButton {
            device_id: None,
            state,
            position,
            primary: true,
            button: ButtonSource::Touch {
                finger_id: fid,
                force: Some(Force::Normalized(0.5)),
            },
            is_macos_activation_click: false,
        };
        let move_to = |position| WindowEvent::PointerMoved {
            device_id: None,
            primary: true,
            position,
            source: PointerSource::Touch {
                finger_id: fid,
                force: Some(Force::Normalized(0.5)),
            },
        };
        // Screen physical coordinates → coordinates relative to the current
        // window's **client area** (winit interprets PointerMoved in client
        // coordinates): the follow loop moves the window itself, so every step must
        // convert using the real client origin at that moment (after revision 3 the
        // client area ≠ the outer frame; ClientToScreen is required).
        let to_window_relative = |sx: f64, sy: f64| {
            let (l, t) = crate::win::frameless::client_origin_physical().unwrap_or((0, 0));
            PhysicalPosition::new(sx - l as f64, sy - t as f64)
        };
        match seq {
            TouchSeq::RightTap { x, y } => {
                use winit::event::MouseButton;
                let p = PhysicalPosition::new(*x, *y);
                feed(
                    self,
                    WindowEvent::PointerEntered {
                        device_id: None,
                        primary: true,
                        position: p,
                        kind: PointerKind::Mouse,
                    },
                );
                feed(
                    self,
                    WindowEvent::PointerButton {
                        device_id: None,
                        state: ElementState::Pressed,
                        position: p,
                        primary: true,
                        button: ButtonSource::Mouse(MouseButton::Right),
                        is_macos_activation_click: false,
                    },
                );
                feed(
                    self,
                    WindowEvent::PointerButton {
                        device_id: None,
                        state: ElementState::Released,
                        position: p,
                        primary: true,
                        button: ButtonSource::Mouse(MouseButton::Right),
                        is_macos_activation_click: false,
                    },
                );
                feed(
                    self,
                    WindowEvent::PointerLeft {
                        device_id: None,
                        primary: true,
                        position: Some(p),
                        kind: PointerKind::Mouse,
                    },
                );
            }
            TouchSeq::MouseTap { x, y } => {
                use winit::event::MouseButton;
                let p = PhysicalPosition::new(*x, *y);
                feed(
                    self,
                    WindowEvent::PointerEntered {
                        device_id: None,
                        primary: true,
                        position: p,
                        kind: PointerKind::Mouse,
                    },
                );
                feed(
                    self,
                    WindowEvent::PointerButton {
                        device_id: None,
                        state: ElementState::Pressed,
                        position: p,
                        primary: true,
                        button: ButtonSource::Mouse(MouseButton::Left),
                        is_macos_activation_click: false,
                    },
                );
                feed(
                    self,
                    WindowEvent::PointerButton {
                        device_id: None,
                        state: ElementState::Released,
                        position: p,
                        primary: true,
                        button: ButtonSource::Mouse(MouseButton::Left),
                        is_macos_activation_click: false,
                    },
                );
                feed(
                    self,
                    WindowEvent::PointerLeft {
                        device_id: None,
                        primary: true,
                        position: Some(p),
                        kind: PointerKind::Mouse,
                    },
                );
            }
            TouchSeq::Tap { x, y } => {
                let p = PhysicalPosition::new(*x, *y);
                feed(
                    self,
                    WindowEvent::PointerEntered {
                        device_id: None,
                        primary: true,
                        position: p,
                        kind,
                    },
                );
                feed(self, button(ElementState::Pressed, p));
                feed(self, button(ElementState::Released, p));
                feed(
                    self,
                    WindowEvent::PointerLeft {
                        device_id: None,
                        primary: true,
                        position: Some(p),
                        kind,
                    },
                );
            }
            // A tap with jitter: a real finger almost inevitably drifts a few
            // pixels between down and up, and blitz treats drifts over 2px as a
            // "pan gesture" and swallows the Click (unless the target element's
            // `touch-action` forbids panning). Used to verify touch-action:none in
            // the chrome.
            TouchSeq::JTap { x, y, dx, dy } => {
                let p0 = PhysicalPosition::new(*x, *y);
                let p1 = PhysicalPosition::new(*x + *dx, *y + *dy);
                feed(
                    self,
                    WindowEvent::PointerEntered {
                        device_id: None,
                        primary: true,
                        position: p0,
                        kind,
                    },
                );
                feed(self, button(ElementState::Pressed, p0));
                feed(self, move_to(p1));
                feed(self, button(ElementState::Released, p1));
                feed(
                    self,
                    WindowEvent::PointerLeft {
                        device_id: None,
                        primary: true,
                        position: Some(p1),
                        kind,
                    },
                );
            }
            // A tap on a popup child window (coordinates = physical pixels of the
            // child window's client area). Touch-path verification: real touch goes
            // through the same winit → blitz pipeline; the only difference between
            // child and main window is which window_id is fed. The optional drift
            // (dx,dy) reproduces finger jitter — with `touch-action:none` on the
            // target the click is not swallowed.
            TouchSeq::PopupTap { x, y, dx, dy } => {
                let Some(cid) = self
                    .children
                    .iter()
                    .map(|c| c.id)
                    .find(|id| self.inner.windows.get(id).is_some_and(|v| v.is_visible))
                else {
                    return;
                };
                let p0 = PhysicalPosition::new(*x, *y);
                let p1 = PhysicalPosition::new(*x + *dx, *y + *dy);
                let feed_child = |this: &mut Self, ev: WindowEvent| {
                    this.window_event(el, cid, ev);
                };
                feed_child(
                    self,
                    WindowEvent::PointerEntered {
                        device_id: None,
                        primary: true,
                        position: p0,
                        kind,
                    },
                );
                feed_child(self, button(ElementState::Pressed, p0));
                if *dx != 0.0 || *dy != 0.0 {
                    feed_child(self, move_to(p1));
                }
                feed_child(self, button(ElementState::Released, p1));
                feed_child(
                    self,
                    WindowEvent::PointerLeft {
                        device_id: None,
                        primary: true,
                        position: Some(p1),
                        kind,
                    },
                );
            }
            TouchSeq::Drag {
                x0,
                y0,
                x1,
                y1,
                steps,
            } => {
                let steps = (*steps).max(1);
                // The whole drag command is a **screen** physical trajectory
                // ((x0,y0)→(x1,y1)); the press point must also be converted to
                // window-relative coordinates using the current window origin:
                // otherwise the press anchor (interpreted in client coordinates)
                // and the movement (interpreted in screen coordinates) use
                // inconsistent frames, and the window first jumps away then slides
                // back.
                let p0 = to_window_relative(*x0, *y0);
                feed(
                    self,
                    WindowEvent::PointerEntered {
                        device_id: None,
                        primary: true,
                        position: p0,
                        kind,
                    },
                );
                feed(self, button(ElementState::Pressed, p0));
                // The finger's **screen** trajectory (x0,y0)→(x1,y1). Every step
                // is converted back to window-relative coordinates using the
                // **current** window origin: the follow loop moves the window itself,
                // so only screen space is the reference frame that does not move with
                // the window; when the window follows 1:1 the relative coordinates
                // stay constant after the first step.
                for i in 1..=steps {
                    let t = i as f64 / steps as f64;
                    let (sx, sy) = (x0 + (x1 - x0) * t, y0 + (y1 - y0) * t);
                    let p = to_window_relative(sx, sy);
                    feed(self, move_to(p));
                }
                let pr = to_window_relative(*x1, *y1);
                feed(self, button(ElementState::Released, pr));
                feed(
                    self,
                    WindowEvent::PointerLeft {
                        device_id: None,
                        primary: true,
                        position: Some(pr),
                        kind,
                    },
                );
            }
        }
    }
}

/// The DocumentConfig for our own shell: wire in a real HTML parser.
///
/// `DocumentConfig::default()`'s `html_parser_provider` is `None`, falling back
/// to `DummyHtmlParserProvider` — `dangerous_inner_html` (in-place Markdown
/// preview, HTML dialogs) would be silently dropped and render blank.
/// dioxus-native's own launch wires in `blitz_html::HtmlProvider` under the
/// `html` feature; we do not use its launch, so we must wire it in ourselves
/// (see the `blitz-html` dependency in Cargo.toml).
fn document_config() -> DocumentConfig {
    let provider = std::sync::Arc::new(blitz_html::HtmlProvider)
        as std::sync::Arc<dyn blitz_dom::HtmlParserProvider>;
    DocumentConfig {
        html_parser_provider: Some(provider),
        // Resource fetching for the <img> tags in MD previews (a NetProvider
        // injected by the application side, e.g. local file:// and http/https
        // images; None when not injected, blitz's default behavior takes over)
        net_provider: NET_PROVIDER.get().cloned(),
        // A parseable fallback base for relative URLs (prevents blitz's
        // resolve_url from panicking when joining against a base like
        // about:blank; local images in the preview are already turned into
        // absolute file:// URLs at render time)
        base_url: Some("file:///".to_string()),
        ..DocumentConfig::default()
    }
}

/// Equivalent of the official launch (minus the net / hot-reload assembly this
/// application does not use).
pub fn launch(
    app: fn() -> Element,
    attrs: winit::window::WindowAttributes,
    base_color: dioxus_native::Color,
) {
    // "Request main window close" hook: Windows' `frameless::close` posts
    // WM_CLOSE and never reads it; other platforms have no message to post, so
    // the hook forwards the request as ShellEvt::CloseMain, and the shell
    // synthesizes a CloseRequested for the main window that goes to the
    // application layer for a decision (see handle_shell_evt). Injected
    // unconditionally — platform differences stay inside frameless.
    crate::win::frameless::set_close_hook(|| send(ShellEvt::CloseMain));
    // `0xRRGGBB` form of the same clear color — blitz paints the window with it
    // before the first frame (see `with_background` below).
    let rgba = base_color.to_rgba8();
    let bg = [rgba.r, rgba.g, rgba.b];
    let event_loop: EventLoop = create_default_event_loop();
    let winit_proxy = event_loop.create_proxy();
    let (proxy, event_queue) = BlitzShellProxy::new(winit_proxy);
    let vdom = dioxus_core::VirtualDom::new(app);
    timing("vdom");
    let doc = DioxusDocument::new(vdom, document_config());
    timing("document");
    let cfg = WindowConfig::with_attributes(
        Box::new(doc) as _,
        // The clear color is passed in by the application according to the saved
        // theme: the window becomes visible before the first frame, and the
        // renderer's default white clear is exactly what causes the "dark theme
        // flashes a white screen at startup".
        DioxusNativeWindowRenderer::with_options(RendererOptions {
            base_color: Some(base_color),
            // Opaque compositing. With the default (`Auto`) the renderer picks
            // a *premultiplied* alpha mode whenever the surface supports it
            // (it sorts PreMultiplied first), so DWM composites the window
            // per-pixel: any frame the app has not painted — the first present,
            // a swapchain reconfiguration — arrives with alpha 0 and the whole
            // window shows the desktop through it. This UI has no translucent
            // surfaces, so the surface is declared opaque.
            alpha_mode: Some(CompositeAlphaMode::Opaque),
            ..RendererOptions::default()
        }),
        attrs,
    )
    // Same color, painted into the client area before the first frame exists
    // (blitz patch 17, `WindowConfig::with_background`): the window appears
    // immediately, already in the theme's background color, and the renderer's
    // first frame (which clears with exactly this color) replaces it without a
    // visible seam. This is what removes the white first screen — no DWM cloak,
    // no delayed show.
    .with_background(bg);
    let application = ShellApp::new(proxy, event_queue, cfg, Some(bg));
    timing("app");
    // An event-loop error (driver/backend failure) used to `unwrap` — i.e. panic on
    // the way out, which looks exactly like a crash with no message. Log it instead;
    // `main` still gets to clean up the recovery session afterwards.
    if let Err(err) = event_loop.run_app(application) {
        debug_log(&format!("event loop exited with error: {err}"));
    }
}

#[cfg(test)]
mod runtime_guard_tests {
    //! Runtime guard for raw winit callbacks (2026-10-17 crash regression).
    //!
    //! Scene: an in-event hook inside `RedrawRequested` reads and writes a
    //! `GlobalSignal` (wys candidate panel / floating toolbar state), while the
    //! `use_window_event` callback is invoked directly by
    //! `winit::window_event` — there is no Dioxus runtime on that thread, so
    //! `Runtime::current()` panics and the very first frame crashes the app.
    use super::*;

    fn empty_app() -> Element {
        rsx! {}
    }

    #[test]
    fn raw_winit_callback_can_touch_global_signals() {
        static HITS: GlobalSignal<i32> = Signal::global(|| 7);
        let vdom = dioxus_core::VirtualDom::new(empty_app);
        let id = WindowId::from_raw(9001);
        note_window_runtime(id, vdom.runtime());
        // The dispatch entry must put the callback inside that window's runtime:
        // reading and writing global signals then does not panic.
        assert_eq!(dispatch_in_window_runtime(id, || *HITS.read()), 7);
        dispatch_in_window_runtime(id, || *HITS.signal().write() = 9);
        assert_eq!(dispatch_in_window_runtime(id, || *HITS.read()), 9);
    }

    #[test]
    fn nested_dispatch_reuses_the_current_runtime() {
        static HITS: GlobalSignal<i32> = Signal::global(|| 3);
        let vdom = dioxus_core::VirtualDom::new(empty_app);
        let id = WindowId::from_raw(9002);
        note_window_runtime(id, vdom.runtime());
        // A dispatch triggered inside a runtime (a winit callback arriving from
        // inside a DOM event): do not push the runtime twice, and do not shadow
        // the inner runtime either.
        let got = dispatch_in_window_runtime(id, || {
            dioxus_core::Runtime::try_current().expect("must be inside a runtime in the guard");
            dispatch_in_window_runtime(id, || *HITS.read())
        });
        assert_eq!(got, 3);
    }

    #[test]
    fn runtime_registry_is_keyed_by_window() {
        let a = dioxus_core::VirtualDom::new(empty_app);
        let b = dioxus_core::VirtualDom::new(empty_app);
        let (wa, wb) = (WindowId::from_raw(9101), WindowId::from_raw(9102));
        note_window_runtime(wa, a.runtime());
        note_window_runtime(wb, b.runtime());
        let (ra, rb) = (
            window_runtime(wa).expect("A registered"),
            window_runtime(wb).expect("B registered"),
        );
        assert!(
            Rc::ptr_eq(&ra, &a.runtime()),
            "A got someone else's runtime"
        );
        assert!(
            Rc::ptr_eq(&rb, &b.runtime()),
            "B got someone else's runtime"
        );
        // Registering the same id again overwrites the old value (popup rebuild)
        note_window_runtime(wa, b.runtime().clone());
        assert!(Rc::ptr_eq(&window_runtime(wa).unwrap(), &b.runtime()));
        assert!(window_runtime(WindowId::from_raw(9199)).is_none());
    }
}
