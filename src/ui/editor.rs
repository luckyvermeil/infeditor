//! Top-level editor application: layout + context provider.
//!
//! Layout is fully absolute with **explicit pixel geometry**: the Blitz layout
//! engine does not resolve `bottom:` insets or `height:100%` inside a box whose
//! height comes from insets, so every panel is sized from the window client
//! size tracked in [`EditorState`].
//!
//! This is also the only place that talks to `winit` directly. That matters for
//! the right-click menu: `use_window_event` runs **before** `BlitzApplication`
//! gets the event, so it is the one hook that can block on a native modal popup
//! without a live borrow of the Blitz document.

use std::sync::Arc;

use crate::ui::{Fmt, Msg};
use dioxus_native::prelude::*;
use dioxus_native::use_window;
use dioxus_native::winit::data_transfer::TypeHint;
use dioxus_native::winit::dpi::{LogicalPosition, LogicalSize};
use dioxus_native::winit::event::{
    ButtonSource, Ime, MouseButton, PointerKind, PointerSource, WindowEvent,
};
use dioxus_native::winit::event_loop::DndAction;
use dioxus_native::winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
use dioxus_native::winit::window::{ImeRequest, ImeRequestData, Window};

use crate::highlight::Theme;
use crate::ui::actions::{self, editor_context_items};
use crate::ui::components::ctxmenu;
use crate::ui::components::mdws;
use crate::ui::components::{
    AppStyle, Composer, CtxMenuOverlay, DialogOverlay, EditorViewport, FindBar, FindPanel,
    KitStyle, MenuOverlay, MoreMenu, ResizeEdges, SideBar, StatusBar, TabBar, Tip, TipLayer,
    TitleBar, Toolbar,
};
use crate::ui::design::FONT_UI;
use crate::ui::shell::use_window_event;
use crate::ui::state::EditorState;

/// Status-bar hint shown while a drag hovers the window.
const DROP_HINT: i18n::Message = i18n::Message::Plain(Msg::StatusDropHint);

/// Window-level pointer source → the finger id matching the DOM's `pointer_id()`.
///
/// blitz passes winit's `Touch{finger_id}` into the DOM verbatim (the raw
/// value of `BlitzPointerId::Finger`); a pen has no separate id (always 0 at
/// the DOM layer); mouse/unknown sources have no finger (`None`). Touch-gesture
/// ledgers (window drag / content pan) store DOM ids, and window-level move
/// events filter on them: without the filter a second finger, a hovering pen,
/// or any mouse move is treated as the driving pointer, and the target
/// position gets yanked between pointers — the root cause of window jumping /
/// wild scrolling.
fn pointer_finger(source: &PointerSource) -> Option<i32> {
    match source {
        PointerSource::Touch { finger_id, .. } => Some(finger_id.into_raw() as i32),
        PointerSource::TabletTool { .. } => Some(0),
        _ => None,
    }
}

/// Same as [`pointer_finger`], derived from the `PointerLeft` pointer kind
/// (when the system cancels tracking there is no button event, so cleanup can
/// only rely on this).
fn pointer_kind_finger(kind: &PointerKind) -> Option<i32> {
    match kind {
        PointerKind::Touch(finger_id) => Some(finger_id.into_raw() as i32),
        PointerKind::TabletTool(_) => Some(0),
        _ => None,
    }
}

/// Structural CSS only.
///
/// There used to be `.mi:hover { background:#094771 }` and
/// `.cap:hover { background:#3c3c3c }` here. Both were wrong twice over: the
/// colors were hardcoded dark-theme values, so the **light** theme flashed dark
/// blue on hover; and a stylesheet rule cannot beat the inline `background` the
/// caption already sets, so the hover simply did not apply. Hover is now an
/// inline style driven by [`crate::ui::motion`] tokens, which fixes both.
const GLOBAL_CSS: &str = r#"
html, body { margin:0; padding:0; width:100%; height:100%; overflow:hidden; }
/* Caret blink. A `step-end` animation holds each keyframe until the next one,
   which is exactly the hard on/off blink a text caret wants (no fade). blitz
   drives the animation clock itself, so this redraws on its own. */
@keyframes caret-blink {
    0%   { opacity: 1; }
    50%  { opacity: 0; }
    100% { opacity: 1; }
}
/* Fluent-style dialog entrance: mask fades in, panel fades + scales in
   (96% → 100%, decelerate curve). The keyframes live only in the **main
   window's** GLOBAL_CSS — blitz self-continues redraws every frame for a
   window running animations, so animating in a popup child window would
   starve the main window's WM_PAINT (see the two iron rules in winshell
   controls.rs). In-page dialogs only live in the main window, so it is safe. */
@keyframes overlay-fade {
    from { opacity: 0; }
    to   { opacity: 1; }
}
@keyframes dialog-in {
    from { opacity: 0; transform: translate(-50%, -50%) scale(0.96); }
    to   { opacity: 1; transform: translate(-50%, -50%) scale(1); }
}
/* Markdown preview body: native blitz text selection works — by NOT setting
   user-select. It used to be pinned to none because blitz had two problems:
   ① the selection highlight was hardcoded opaque #b4d5ff, which washed light
   text out on the light background of the dark theme; ② hit_inner ignored
   overflow:hidden clipping, so after scrolling a screen the preview's hidden
   part covered the toolbar and ate clicks — at the time both symptoms were
   blamed on the selection state. Both are fixed at the root in the vendor
   patch: hit_inner returns None early outside the box for clipping containers
   (node.rs), and the selection highlight is translucent (SELECTION_COLOR in
   blitz-paint lib.rs).
   Another counter-intuitive trap: an explicit `user-select: text` actually
   BREAKS selection — blitz's drag-selection gate (pointer.rs
   handle_pointermove) only enters Selecting when the mousedown node's
   computed value is `Auto` (Auto then checks whether the direct parent is
   None; the `Text`/`All` branches are unhandled and fall through). So
   nothing must be set here, keeping every leaf produced by
   dangerous_inner_html at its initial Auto. */
/* No user-select rule for .md-body — see above. */
"#;

#[component]
pub fn EditorApp() -> Element {
    // First frame = boot placeholder (only peeks at the theme flag from
    // settings, one small IO): the window becomes visible before any content
    // exists; full loading (settings/font metrics/recovery scan/command-line
    // file) is deferred until after that first — flat, theme-colored — frame
    // (see the boot sequence in the RedrawRequested branch), so dark-theme
    // users never see a white first screen.
    let mut state = use_signal(EditorState::skeleton);
    use_context_provider(move || state);
    // Scrollbar drag's input-side thumb: a **dedicated** signal so a pointer
    // move during a drag re-renders only the thumb's scope instead of the
    // whole app — the decoupling that keeps the thumb glued to the pointer
    // (see tmp/scrollbar-lag-analysis.md). `sb_drag`/`sb_press` stay in
    // `EditorState`: they change once per gesture, not per move.
    let sb_live = use_signal(|| None::<crate::ui::scrollbar::SbLive>);
    use_context_provider(move || sb_live);
    // Tooltip state: a separate Signal (tooltip.rs); hovering re-renders only
    // the tip layer.
    use_context_provider(|| Signal::new(Tip::default()));

    // `use_window` consumes the window context, so it is called exactly once
    // here and the native handle is derived from the value it returns.
    let window = use_window();
    let hwnd = window_hwnd(&window);

    // The merged title bar's window buttons, drag strip and resize edges need
    // the window: the drags hand the gesture to the OS (`drag_window` /
    // `drag_resize_window`) and the context menu needs the raw `HWND`. Both are
    // parked in process-wide slots instead of being carried through the DOM
    // closures — the move/resize loops must never run while a `Signal` borrow
    // is alive, and a slot the closures read on the spot is the only way to
    // guarantee that.
    winshell::win::frameless::set_hwnd(hwnd);
    winshell::win::frameless::set_window(window.clone());

    // Observe the system's own move loop so the window snaps like a normal
    // frame does — see `winshell::win::snap` for why the shell does not do it for us.
    // Idempotent (this component body runs many times a second); it logs only
    // when it actually installs the subclass.
    winshell::win::snap::install();

    // Let background document loads (and anything else off-thread) ask for a
    // frame. `request_redraw` is thread-safe, `use_hook` keeps this to one
    // allocation for the life of the app.
    {
        let w = window.clone();
        use_hook(move || {
            let w = w.clone();
            state
                .write()
                .set_waker(std::sync::Arc::new(move || w.request_redraw()));
            // Markdown code-block coloring: wire the editor's lexer into
            // wysiwyg's `hl` hook (once, shared process-wide).
            crate::highlight::code::register();
            // Command-line file / recovery scan / workspace smoke all boot
            // with the real state (`EditorState::default` +
            // `boot_editor_state`), not during the skeleton phase.
            crate::ui::find::arm_smoke_from_env();
            crate::ui::prefswin::arm_smoke_from_env();
            crate::ui::numwin::arm_smoke_from_env();
        });
    }

    // Track the window client size in CSS pixels; everything else is derived.
    {
        let window = window.clone();
        use_effect(move || {
            let scale = window.scale_factor() as f32;
            if scale <= 0.0 {
                return;
            }
            let size = window.surface_size();
            state
                .write()
                .set_window_size(size.width as f32 / scale, size.height as f32 / scale);
        });
    }
    window_events(state, window.clone(), sb_live, hwnd);

    // Boot first screen: until the real state finishes booting, draw only a
    // flat theme-colored canvas (boot sequence in the RedrawRequested branch).
    // Placed after all use_* hooks — dioxus requires a constant hook order, so
    // conditional rendering can only happen after hook registration.
    if !state.read().booted {
        return skeleton_screen(state.read().theme());
    }

    let root_style = {
        let st = state.read();
        let t = st.theme();
        format!(
            "position:absolute; left:0; top:0; right:0; bottom:0;
             background:{}; color:{};
             font-family:{FONT_UI};",
            t.bg, t.menu_fg
        )
    };

    // Push the editor's state into the native find/replace and settings
    // windows.
    //
    // This is the app→dialog half of the bridge (see `ui::nativedlg`): the
    // window procedures cannot read a dioxus `Signal`, so every setter compares
    // against what the control already shows and does nothing when they agree.
    // Called here rather than in the redraw hook because it must happen *after*
    // this frame's commands have been applied.
    state.read().sync_dialogs();

    // Keep the OS title in step with the document. The drawn caption and the OS
    // title are two different things: the taskbar button, Alt+Tab and the
    // window's own system menu all read the latter, and without this every tab
    // showed the launch string.
    {
        let st = state.read();
        let name = st
            .tabs
            .get(st.active)
            .map(|t| t.doc.meta.display_name.clone())
            .unwrap_or_else(|| st.tr(Msg::DocUntitled).into_owned());
        let dirty = st
            .tabs
            .get(st.active)
            .map(|t| t.doc.is_dirty())
            .unwrap_or(false);
        let tabs = st.tabs.len();
        // Window title shape: `name[*] - InfEditor`, plus the tab count once
        // there is more than one, which is what makes a many-tab session
        // identifiable from the taskbar.
        let mut title = String::new();
        if dirty {
            title.push('*');
        }
        title.push_str(&name);
        if tabs > 1 {
            title.push_str(&format!(" [{}/{}]", st.active + 1, tabs));
        }
        title.push_str(" - InfEditor");
        winshell::win::frameless::set_title(&title);
    }

    rsx! {
        div {
            style: "{root_style}",
            // Keep the *keyboard* alive.
            //
            // blitz's click **default action** (`handle_click` in
            // `blitz-dom/events/pointer.rs`) walks up from the hit node looking
            // for something focusable; our chrome is nothing but plain `div`s,
            // so the walk fails and blitz used to call `doc.clear_focus()` —
            // blurring the IME composer, which is the editor's only keyboard
            // target. **vendor patch five** (see
            // vendor/blitz/INFEDITOR-PATCH.md) makes that fallback skip when
            // the current focus is a text input, so composer focus is no
            // longer interrupted by clicks — hence we **must not** cancel the
            // click default action here: dblclick synthesis lives exactly in
            // handle_click (the CSV table's double-click editing depends on
            // it), and a global cancel would break ondoubleclick across the
            // whole app.
            style { "{GLOBAL_CSS}" }
            // ui-kit's class stylesheet: button/checkbox tri-states are all
            // CSS pseudo-classes. Like GLOBAL_CSS, injected into the main
            // window DOM only (library components render only here).
            KitStyle { t: state.read().theme() }
            // App shell's class stylesheet (sidebar tree / status-bar toggles
            // / fold markers): likewise injected into the main window only;
            // tri-states are expressed via CSS pseudo-classes so components
            // do not re-render.
            AppStyle { t: state.read().theme() }
            // Tooltip layer (tooltip.rs): drawn outside every overflow; tip
            // state is a separate Signal — hovering re-renders only that one
            // component.
            TipLayer {}
            // one merged row: window controls + icon + title + whole menu tree
            TitleBar {}
            TabBar {}
            FindBar {}
            // CSV / JSON / JSONL / Markdown toolbars (appear automatically for
            // the matching document; inactive ones take no space)
            Toolbar {}
            // Workspace sidebar: directory tree of the open folder (viewport
            // yields space on the left, see `state::workspace` /
            // `EditorState::relayout`)
            SideBar {}
            EditorViewport {}
            FindPanel {}
            StatusBar {}
            // The dropdown is a **root-level** overlay, not a child of the menu
            // bar: the bar's `z-index` creates a stacking context that would
            // otherwise trap the panel below the find bar and results panel.
            MenuOverlay {}
            // Menu-bar overflow list (more button): same level as the
            // dropdown (root-level overlay)
            MoreMenu {}
            // In-page context menu (fallback for non-Windows native menus;
            // always empty on Windows, where right-click uses TrackPopupMenu
            // — see `open_context_menu`)
            CtxMenuOverlay {}
            // one permanent, transparent input surface; it owns DOM focus and is
            // parked over the caret or over the focused find box
            Composer {}
            // in-app modal dialogs (About/MD5/table view/Markdown preview) —
            // above the menu overlay
            DialogOverlay {}
            // Edge/corner resize handles for the frameless window. Windows
            // answers its real non-client hit test instead, so the component
            // renders nothing there; winit platforms have no native border to
            // grab and need these strips.
            ResizeEdges {}
        }
    }
}

/// Every window-level event in one place: resize/scale, pointer moves (touch pan,
/// window gestures, scrollbar drags), keyboard, IME, and file drops.
///
/// A plain function, not a component: `use_window_event` is a hook, and dioxus
/// identifies hooks by call position, so this has to run unconditionally from the
/// component body (custom-hook pattern).
fn window_events(
    mut state: Signal<EditorState>,
    window: Arc<dyn Window>,
    mut sb_live: Signal<Option<crate::ui::scrollbar::SbLive>>,
    hwnd: isize,
) {
    let window = window.clone();
    use_window_event(move |event, event_loop| match event {
        WindowEvent::SurfaceResized(size) => {
            let scale = window.scale_factor() as f32;
            if scale > 0.0 {
                state
                    .write()
                    .set_window_size(size.width as f32 / scale, size.height as f32 / scale);
                sync_maximized(&mut state);
            }
        }
        WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
            let scale = *scale_factor as f32;
            let size = window.surface_size();
            if scale > 0.0 {
                state
                    .write()
                    .set_window_size(size.width as f32 / scale, size.height as f32 / scale);
            }
        }
        // A drag that ends outside the viewport (over the tab strip, the
        // status bar, or another window) never reaches the viewport's own
        // `onmouseup`; without this the editor would keep extending the
        // selection on every subsequent mouse move.
        // A scrollbar thumb is dragged by tracking the pointer at the
        // window level: the drag leaves the 14px strip constantly, and the
        // bar's own `onmousemove` would stop firing mid-drag. (The press
        // itself is a DOM `onmousedown`, which is where the grab offset is
        // computed from the element coordinates.)
        WindowEvent::PointerMoved {
            position, source, ..
        } => {
            // Touch window gestures (title-bar drag / edge resize) are
            // diverted first: the follow loop is driven by
            // `winshell::win::frameless` via SetWindowPos, see
            // `begin_touch_move`. During a gesture, window-level
            // PointerMoved arrives from **all** pointers (winit funnels
            // every finger, hovering pen, and mouse move into one
            // window-level event stream); only the driving finger's moves
            // feed the follow loop — another pointer's coordinates would
            // yank the absolute target position to its touch point,
            // making the window jump between positions. Non-driving
            // pointer moves are always dropped and do not fall through
            // (scrollbar drags have their own pointer semantics and must
            // not be hijacked by a second finger either).
            if winshell::win::frameless::in_touch_gesture() {
                let moved = pointer_finger(source).is_some_and(|id| {
                    winshell::win::frameless::update_touch_gesture(id, position.x, position.y)
                });
                if moved && state.read().touch_press.is_some() {
                    // Once the window moves, this press is no longer a
                    // long-press candidate
                    state.write().touch_press = None;
                }
                return;
            }
            // Touch panning (body / preview / table): the gesture is
            // tracked by the corresponding view on press (`ui::touch`);
            // here the displacement is only converted into scroll deltas.
            // Deltas are enqueued for merging (see `ui::scroll`) and land
            // in one batch via the frame hook.
            //
            // Placed before long-press cancellation and scrollbar drags: a
            // finger slide can only be a pan — never a long-press
            // candidate, and it must not drive other drags at the same
            // time.
            {
                let scale = if window.scale_factor() > 0.0 {
                    window.scale_factor() as f32
                } else {
                    1.0
                };
                let (x, y) = (position.x / scale as f64, position.y / scale as f64);
                // Only the tracked finger's movement advances the pan (see
                // `pointer_finger`); mouse/unknown sources have no finger
                // id and do not participate in touch panning.
                let fed = pointer_finger(source).and_then(|id| crate::ui::touch::feed(id, x, y));
                if let Some((target, dx, dy)) = fed {
                    // Any slide means this press is no longer a
                    // long-press candidate
                    crate::ui::state::update_if(
                        &mut state,
                        |st| st.touch_press.is_some(),
                        |st| st.touch_press = None,
                    );
                    if crate::ui::touch::queue_scroll(target, dx, dy) {
                        state.read().request_frame();
                    }
                    return;
                }
            }
            // Long-press cancellation: once the finger moves past the
            // threshold it is no longer "held still". `touch_press` is
            // recorded only on touch press; mouse drags are unaffected.
            if state.read().touch_press.is_some() {
                let far = state.read().touch_press.is_some_and(|(_, px, py)| {
                    (position.x - px).abs() > LONG_PRESS_MOVE_PX
                        || (position.y - py).abs() > LONG_PRESS_MOVE_PX
                });
                if far {
                    state.write().touch_press = None;
                }
            }
            // `read` (not `write`) on the guard: a `write` marks the signal
            // dirty and would rebuild the whole viewport on every mouse
            // move even when nothing is being dragged.
            // The CSV table's scrollbar/row-column resize drags also use
            // window-level tracking: their pointermove/up live on the
            // table's root div, so once the pointer leaves the window the
            // DOM event stream cuts out and the drag hangs — the view kept
            // scrolling with the mouse (same history as the main scrollbar).
            let (sb, mm, csv, sel) = {
                let st = state.read();
                (
                    st.sb_drag.is_some(),
                    st.mm_drag.is_some(),
                    st.tab().csv.drag.is_some(),
                    st.drag || st.md_drag,
                )
            };
            let scale = if window.scale_factor() > 0.0 {
                window.scale_factor() as f32
            } else {
                1.0
            };
            // Drag selection: the viewport's own `pointermove` stops arriving
            // once the pointer leaves that element, which is exactly the
            // gesture "select past the last line" produces. Park the position
            // (no state write — the pointer moves every pixel) and let the
            // frame hook decide whether it is outside the text band and how far
            // to scroll; see `EditorState::drag_autoscroll`.
            if sel {
                crate::ui::scroll::drag_ptr_set(
                    position.x as f32 / scale,
                    position.y as f32 / scale,
                );
                // Only an out-of-band pointer needs a frame: inside the band the
                // viewport's own handler has already extended the selection.
                let due = {
                    let st = state.read();
                    st.drag_autoscroll_due() || st.wys_drag_due()
                };
                if due {
                    state.read().request_frame();
                }
            }
            if !sb && !mm && !csv {
                return;
            }
            let y = position.y as f32 / scale;
            if sb {
                // Scrollbar drag — **input side only**: the thumb's cheap
                // scope re-renders from the live signal (a pointer move
                // must not mark the whole app dirty), and the newest
                // sample is parked for the frame hook, which lands it in
                // `scroll_top` once per frame (see the lag analysis:
                // tmp/scrollbar-lag-analysis.md, H1/H2).
                let drag = state.read().sb_drag;
                let cx = position.x as f32 / scale;
                if let Some(d) = drag {
                    if let Some(live) = state.read().sb_live_off(d.vertical, cx, y) {
                        sb_live.set(Some(live));
                    }
                    crate::ui::scroll::sb_queue(crate::ui::scroll::SbPointer {
                        vertical: d.vertical,
                        cx,
                        cy: y,
                    });
                    state.read().request_frame();
                }
            }
            let needs_write = mm || csv;
            if needs_write {
                let mut st = state.write();
                if mm {
                    st.mm_pointer(y as f64);
                }
                if csv {
                    // Window logical coords = DOM client coords (frameless
                    // window, same space)
                    st.csv_drag_move(position.x as f32 / scale, position.y as f32 / scale);
                }
            }
        }
        WindowEvent::PointerButton {
            state: btn,
            button,
            position,
            primary,
            ..
        } => {
            if btn.is_pressed() {
                // Long-press (touch only): first finger held still →
                // editor context menu, mirroring Explorer's touchscreen
                // behavior. The expiry check piggybacks on redraw frames
                // (no async runtime/timers in this project), so here we
                // only record the candidate and make sure a frame will be
                // available when it expires: sleep until the deadline,
                // then request a redraw.
                if *primary && matches!(button, ButtonSource::Touch { .. }) {
                    let (px, py) = (position.x, position.y);
                    {
                        let mut st = state.write();
                        st.touch_press = Some((std::time::Instant::now(), px, py));
                    }
                    let w = window.clone();
                    std::thread::spawn(move || {
                        std::thread::sleep(std::time::Duration::from_millis(LONG_PRESS_MS));
                        w.request_redraw();
                    });
                }
                return;
            }
            {
                // both drags end on *any* release, wherever it happens.
                // The CSV table's drag gesture also ends at any release
                // point (see the PointerMoved branch comment: a DOM-level
                // pointerup exiting the window causes disconnect.)
                // Conditional write: an ordinary click release (no drag,
                // no long-press candidate) must not mark dirty — write()
                // unconditionally marks dirty, which would trigger a full
                // app re-render on every click.
                let (has_drag, has_press) = {
                    let st = state.read();
                    (
                        st.drag
                            || st.sb_drag.is_some()
                            || st.mm_drag.is_some()
                            || st.tab().csv.drag.is_some(),
                        st.touch_press.is_some(),
                    )
                };
                // A document-view drag whose release the component never saw (the
                // pointer left the element or the window, so no DOM `pointerup`
                // arrived): the editor would stay in pointer-owned mode — every later
                // selection would expand blocks by block range, and the floating
                // toolbar (hidden while `md_drag`) would never come back. Ends it
                // here, and the answer decides whether this release needs the state
                // write below (an ordinary click's WYS press sets `md_drag` too, so
                // "WYS is pressed" alone must not mark the app dirty).
                let wys_drag = crate::ui::components::mdws::end_wys_drag();
                if has_drag || has_press || wys_drag {
                    let mut st = state.write();
                    // Land the newest drag sample first, so the content
                    // matches the thumb the user let go of (the frame hook
                    // may not have run yet); sb_end then discards residue.
                    if let Some(p) = crate::ui::scroll::sb_take() {
                        st.sb_apply(p);
                    }
                    st.drag = false;
                    st.md_drag = false;
                    // The parked drag pointer belongs to the gesture that just
                    // ended; keeping it would let the next drag scroll before
                    // the pointer has moved at all.
                    crate::ui::scroll::drag_ptr_clear();
                    st.sb_end();
                    st.mm_end();
                    st.csv_end_drag();
                    // The long-press candidate is voided on release
                    st.touch_press = None;
                }
                // The thumb falls back to the content side. Guarded
                // write: `set` goes through `write()`, which marks the
                // thumb scopes dirty even when the value is unchanged —
                // an unconditional set here would re-render them on every
                // ordinary click release.
                if sb_live.read().is_some() {
                    sb_live.set(None);
                }
            }
            // The touch window gesture ends with it; a non-gesture
            // finger's release leaves it alone (window-level move events
            // carry no id, but release events have finger_id, which is
            // enough)
            match button {
                ButtonSource::Touch { finger_id, .. } => {
                    let id = finger_id.into_raw() as i32;
                    if winshell::win::frameless::touch_gesture_finger().is_none_or(|f| f == id) {
                        winshell::win::frameless::end_touch_gesture(false);
                    }
                    // Touch tap: a gesture that ends with **no slide** is
                    // a tap; place the caret (not on press — that press
                    // may turn into a slide). Moving the caret only on
                    // release is also mobile-editor convention: a
                    // mis-slide does not fling the caret to the start
                    // first.
                    if let Some((target, start, moved)) = crate::ui::touch::end(id) {
                        if !moved && target == crate::ui::touch::PanTarget::Doc {
                            let hit = state.write().touch_tap(start.0, start.1);
                            if hit {
                                // On touch devices, "tapping into the
                                // body" = wanting to type: summon the
                                // system touch keyboard (a no-op when
                                // already visible; a pure no-op on
                                // platforms without a touch keyboard).
                                winshell::win::softkb::show();
                            }
                        }
                    }
                }
                _ => {
                    winshell::win::frameless::end_touch_gesture(false);
                    // Mouse/pen never take the touch-tap path; any
                    // residual gesture is voided
                    crate::ui::touch::cancel();
                }
            }
            // Pop the editor context menu on right-button **release**
            // (Windows sends WM_CONTEXTMENU on button-up too).
            if matches!(button, ButtonSource::Mouse(MouseButton::Right)) {
                open_context_menu(state, &window, hwnd, position.x, position.y);
            }
        }
        // When the system cancels tracking of a pointer (palm rejection /
        // focus stolen / contact leaving the tracking area) winit sends
        // only PointerLeft, with no Released. Without cleanup the gesture
        // ledger hangs forever: TOUCH_ACTIVE blocks the next drag, and the
        // stale anchor makes later moves read as window teleports. A
        // normal release order is Released → PointerLeft, so by this point
        // both ledgers have already been consumed by the release path and
        // everything below is a no-op; only a **cancelled** pointer
        // actually reaches cleanup. A cancellation is not a tap: no caret.
        WindowEvent::PointerLeft { kind, .. } => {
            if let Some(id) = pointer_kind_finger(kind) {
                if winshell::win::frameless::touch_gesture_finger().is_none_or(|f| f == id) {
                    winshell::win::frameless::end_touch_gesture(true);
                }
                crate::ui::touch::cancel_id(id);
            }
        }
        // Anything that ran a command has had its effect applied and the
        // VDOM rebuilt by now, so this is the safe place to act on quit.
        // It is also where finished background opens are collected: a
        // worker thread finishes by calling `request_redraw`, so this event
        // arriving means the payload is already in the channel.
        // IME commit **no longer** writes the document here: vendor patch
        // seven rewrites `Ime::Commit` at the DOM layer into a synthetic
        // keydown (Key::Character, is_composing=false) dispatched to the
        // composer's onkeydown, which routes it by mode (dialog input /
        // CSV cell / find box / wys editor / document). Inserting directly
        // here + patch seven dispatching again = **every commit written
        // twice** (hit on real hardware: IME Han characters doubled in a
        // CSV cell). WM_CHAR during composition is explicitly ignored by
        // winit (the "probably IME" branch in keyboard.rs), so the
        // synthetic keydown is the only channel for committed text — half
        // a pinyin string can never leak into a keydown. blitz's default
        // action still inserts the committed text into the composer's
        // internal editor (invisible; prevented entirely when
        // prevent_default blocks it).
        // While composing (pinyin still in the candidate window): only
        // update the composition mirror (CSV cell / find box / wys
        // editor); plain text views do not render the composition — the
        // candidate window follows the system caret, and the pre-commit
        // intermediate state need not be shown.
        WindowEvent::Ime(Ime::Preedit(text, range)) => {
            // Feed the composing flag into the wys kernel
            // (`set_composing`) — keys during composition no longer enter
            // the wys editor (the Enter-split-block symptom caused by
            // uncommitted pinyin).
            wysiwyg::ui::set_composing(!text.is_empty());
            // Composition mirror: CSV cell editing first; next the focused
            // in-page find box (the composer textarea is invisible —
            // without mirroring, typing pinyin gives no feedback); last
            // the wys editor (rendered as a caret-anchored overlay, see
            // mdws::WysPreedit). Commits go through the insertion channel
            // via vendor patch seven's synthetic keydown; here we only
            // mirror. Conditional write (iron law): composition events
            // from plain document typing concern none of the three
            // consumers, and an unconditional state.write() must not dirty
            // the signal and trigger a full app re-render.
            let csv_editing = state.read().tab().csv.editing.is_some();
            if csv_editing {
                state.write().csv_edit_preedit(text);
            } else if state.read().find.captured() {
                if state.read().find.preedit != *text {
                    state.write().find.preedit = text.clone();
                }
            } else if state.read().tab().md_wys {
                // Residual cleanup: mirror the composition into wys
                // (including the caret viewport snapshot, cleared on
                // Commit/composition cancel). Also report the
                // scroll-compensated caret window coords to winit — the
                // candidate window anchor (scroll compensation can only be
                // computed during the event; at render time blitz holds a
                // mutable document borrow, so scroll_offset is
                // unreachable).
                mdws::set_preedit(text, *range);
                let view_y = state.read().view.y;
                if let Some((x, y, w, h)) = mdws::ime_anchor_win(view_y) {
                    let _ = window.request_ime_update(ImeRequest::Update(
                        ImeRequestData::default().with_cursor_area(
                            LogicalPosition::new(x as f64, y as f64).into(),
                            LogicalSize::new(w as f64, h as f64).into(),
                        ),
                    ));
                }
            }
        }
        WindowEvent::Ime(Ime::Commit(_)) => {
            // Composition ended: clear the composing flag and the wys
            // composition mirror (internally conditional writes)
            wysiwyg::ui::set_composing(false);
            mdws::set_preedit("", None);
            // Committed text is inserted by the synthetic keydown; the
            // composition string disappears with it. Conditional write:
            // When editing plain Chinese, both consumers are no-op (same
            // iron law as the adjacent Preedit branch) — no extra dirty
            // round for this.
            let needs = {
                let st = state.read();
                st.tab().csv.editing.is_some() || !st.find.preedit.is_empty()
            };
            if needs {
                let mut st = state.write();
                st.csv_edit_preedit("");
                st.find.preedit.clear();
            }
        }
        WindowEvent::Ime(_) => {}
        WindowEvent::Focused(f) => {
            // Window blur = any in-progress touch gesture gets no further
            // events (the release is swallowed by another window); the
            // ledger must be voided, or the next tap is blocked by the
            // "gesture already active" check.
            if !*f {
                crate::ui::touch::cancel();
                // Same history for the window gesture: after blur the
                // release is swallowed by another window, so its ledger
                // must be settled too (see the PointerLeft branch note).
                // Cancel semantics: if focus is lost while dragged to the
                // top band but before the finger lifts, do not maximize.
                winshell::win::frameless::end_touch_gesture(true);
            }
            // When clicking back into the document (main window regains
            // focus) hand the find popup's virtual focus back to the
            // document: the popup stays visible (non-modal), but typing
            // should go to the editor. F3/Shift+F3 keep working via the
            // shortcut table.
            if *f {
                let mut st = state.write();
                if st.find.child_win && st.find.open && st.find.captured() {
                    st.set_find_focus(crate::ui::state::FindFocus::Doc);
                }
            }
        }
        // ---- Close window --------------------------------------------
        //
        // WM_CLOSE / Alt+F4 / title-bar ×: the shell routes the main
        // window's CloseRequested here for **adjudication** (no direct
        // exit anymore). With unsaved documents, show the custom confirm
        // dialog (native rfd is invisible on a frameless window) — the
        // confirm command sets quit_requested and the RedrawRequested
        // branch exits; with no dirty documents, exit directly. This
        // branch neither exits nor hands off to blitz (its default
        // handling destroys the window); the window stays open.
        WindowEvent::CloseRequested => {
            let dirty = state.read().tabs.iter().any(|t| t.doc.is_dirty());
            if dirty {
                state.write().request_quit();
            } else {
                event_loop.exit();
            }
        }
        WindowEvent::RedrawRequested => {
            // Boot sequence: after the first (flat background) frame is drawn,
            // the next redraw synchronously builds the real state (all the
            // heavy lifting in `EditorState::default`: settings/font
            // metrics/recovery scan/command-line file) and swaps it in
            // wholesale — while boot blocks, the screen still shows that
            // background, so the user sees neither a white screen nor a gap.
            if !state.read().booted {
                static SKELETON_FRAMES: std::sync::atomic::AtomicU32 =
                    std::sync::atomic::AtomicU32::new(0);
                let n = SKELETON_FRAMES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if n == 0 {
                    // First frame: schedule one more frame; boot runs during
                    // that frame's event phase
                    window.request_redraw();
                } else {
                    boot_editor_state(&mut state, &window);
                }
            }
            // Land pending scroll deltas (wheel / touch pan / minimap
            // wheel). First, before everything: this frame's VDOM render
            // must see the new scroll_top, or scrolling lags a frame and
            // fast flicks show a "can't catch up" rubber band. Zero writes
            // when the probe is false.
            if crate::ui::scroll::pending() {
                state.write().apply_pending_scroll();
            }
            // Scrollbar drag: land the newest pointer sample into
            // `scroll_top` — the content follows at frame rate, while the
            // thumb itself renders from the input side (see the drag
            // branch of CursorMoved).
            if let Some(p) = crate::ui::scroll::sb_take() {
                state.write().sb_apply(p);
            }
            // Drag-selection auto-scroll: the pointer is held outside the text
            // band with the button down. Each step queues the next scroll and
            // re-arms the frame, so a pointer that sits still past the edge
            // keeps scrolling (the move event only armed the first frame).
            // `update_if` keeps an in-band drag at zero writes.
            crate::ui::state::update_if(
                &mut state,
                |st| st.drag_autoscroll_due(),
                |st| {
                    if st.drag_autoscroll() {
                        st.request_frame();
                    }
                },
            );
            // The document-view twin: blitz owns the content's scroll offset, so
            // the step goes through the kernel (scroll + extend, one row a
            // frame). Same shape, separate probe — see `mdws::drag_tick`.
            crate::ui::state::update_if(
                &mut state,
                |st| st.wys_drag_due(),
                |st| {
                    if crate::ui::components::mdws::drag_tick(st) {
                        st.request_frame();
                    }
                },
            );
            // Long-press expiry check (touch held 500ms → context menu).
            // The expiry popup is a modal Win32 loop and must run where no
            // Blitz document borrow is alive — this hook runs exactly
            // before Blitz receives the same event.
            maybe_long_press(state, &window, hwnd);
            // Soft-keyboard avoidance: on touch, clear the viewport
            // bottom when the keyboard appears.
            sync_keyboard_inset(state, &window);
            // The OS owns both of these flags — either can be changed by the
            // shell (taskbar, Win+Up, another window's menu) — and the title
            // bar renders from them, so refresh before this frame is built.
            sync_maximized(&mut state);
            sync_topmost(&mut state);
            // Idle polling family (background opens/search/external
            // changes/recovery log/wys debounce).
            //
            // There was a time every frame did an unconditional
            // `state.write()`: the guard drop unconditionally marks dirty ⇒
            // render marks dirty ⇒ poll ⇒ render again, pinning the whole
            // app in a ~40Hz self-sustaining loop (one core of idle CPU,
            // measured, see tmp/scroll_probe.py). Now `needs_idle_poll`
            // probes read-only first and we write only when there is work —
            // zero writes at steady-state idle, render Blink tick fire loop
            // (probe and correct early exit branch identically, see
            // opens.rs iron law comment).
            let now = std::time::Instant::now();
            crate::ui::state::update_if(
                &mut state,
                |st| st.needs_idle_poll(now),
                |st| {
                    st.poll_opens();
                    // A second launch (or a file dropped onto the exe while
                    // this instance runs) hands its command line over through
                    // `ui::single`; the frame only gets here when one is queued.
                    st.poll_remote_open_requests();
                    st.poll_index_jobs();
                    // Background search/replace progress and completion
                    // reaping (find.rs). There was a time only this
                    // Left document comment, never actually wired — after
                    // background task starts, status bar
                    // stuck at "scanning…" forever, results never
                    // applied, and "a search is already in progress"
                    // locked out all later find/replace (Esc could only
                    // stop the thread, never clear the job).
                    st.poll_search();
                    st.poll_external_change();
                    // Crash recovery: throttled session-log writes (see
                    // ui::recovery). Piggybacks on redraw frames because
                    // the project has no async runtime/timers.
                    st.recovery_tick(now);
                    // wys sync debounce flush when due (the timer thread
                    // armed by mdws::note_edit wakes here via
                    // request_redraw at the deadline; zero-overhead early
                    // return when the timer window has not filled).
                    crate::ui::components::mdws::flush_if_due(st);
                    // Caret blink phase: the global phase flipped by
                    // BlinkTick lands in state; each render site
                    // (viewport/findbar/csvtable/wys) reads it to swap
                    // phase.
                    st.caret_phase = winshell::shell::bridge::blink_on();
                },
            );
            // Frame-driven switch for caret blinking: frames are needed
            // only while the document or the find bar holds focus.
            // The CSS `caret-blink` animation was removed (blitz
            // Redraw every frame, see bridge module comment), blink driven by BlinkTick thread
            // scheduling a frame every 265ms, with the frame hook writing
            // the phase back.
            //
            // Smoke arming (INF_*_SMOKE, test processes only) also needs a
            // guaranteed frame supply: smoke timers are checked in the
            // frame hook, and once the render loop stops an idle window
            // has no frames, so the timer never fires (root cause of real
            // regressions in the wys GUI suite).
            {
                let st = state.read();
                let want_blink = crate::ui::smoke::any_env_armed()
                    || (st.caret_blink
                        && (st.doc_focused || st.find.focus != crate::ui::state::FindFocus::Doc));
                winshell::shell::bridge::main_blink_set(want_blink);
            }
            // wys overlay frame sync (hint candidate panel / selection
            // floating toolbar): mouse selection changes have no host
            // event, so this frame hook closes the loop
            // (RedrawRequested arrives before blitz borrows the document,
            // so geometry queries work). Returning true = caret sync still
            // has pending work (the focused block's inline layout is not
            // ready): another frame must be scheduled to retry, or the
            // caret stays pinned at its old position — blitz builds inline
            // layout later in this frame's render pass and is only
            // queryable next frame.
            let (wys_view, dragging) = {
                let st = state.read();
                (st.tab().md_wys, st.md_drag)
            };
            if crate::ui::components::mdws::overlays_frame(wys_view, dragging) {
                window.request_redraw();
            }
            if state.read().quit_requested {
                event_loop.exit();
            }
        }
        // ---- Drag-and-drop file open --------------------------------------------
        //
        // winit 0.31 dropped the old `DroppedFile`/`HoveredFile` events for
        // an async data-transfer flow. Blitz ignores all of it
        // (`blitz-shell/src/window.rs` stubs `DragEntered`/`Dropped`/`Left`
        // to `{}`), so the whole drop is handled here.
        //
        // `set_valid_dnd_actions` is not optional: winit's default is to
        // *reject* the drag, and the OS would never route the drop to us.
        WindowEvent::DragEntered { id, .. } => {
            let _ = event_loop.set_valid_dnd_actions(*id, &[DndAction::Copy]);
            state.write().status = Some(DROP_HINT.clone());
        }
        WindowEvent::DragPosition { id, .. } => {
            // keep accepting: some platforms drop the validity on move
            let _ = event_loop.set_valid_dnd_actions(*id, &[DndAction::Copy]);
        }
        WindowEvent::DragDropped { id, .. } => {
            // ask for the URI list; the payload arrives asynchronously in
            // `DataTransferReceived` below
            if let Err(e) = event_loop.fetch_data_transfer(*id, &TypeHint::UriList) {
                state.write().status = Some(Fmt::StatusDropFailed { err: e.to_string() }.into());
            }
        }
        WindowEvent::DragLeft { .. } => {
            if state.read().status.as_ref() == Some(&DROP_HINT) {
                state.write().status = None;
            }
        }
        WindowEvent::DataTransferReceived { value, .. } => {
            match value.try_as_file_paths() {
                Ok(paths) => {
                    // formatting and queueing live on `EditorState` so the
                    // behaviour can be unit-tested without an OLE drag source
                    let msg = { state.write().open_dropped(&paths) };
                    state.write().status = Some(msg);
                }
                Err(e) => {
                    state.write().status =
                        Some(Fmt::StatusDropParseFailed { err: e.to_string() }.into())
                }
            }
        }
        _ => {}
    });
}

/// Win32 `HWND` of the main window as an `isize`, or `0` when unavailable.
fn window_hwnd(window: &Arc<dyn Window>) -> isize {
    match window.window_handle() {
        Ok(h) => match h.as_raw() {
            RawWindowHandle::Win32(w) => w.hwnd.get(),
            _ => 0,
        },
        Err(_) => 0,
    }
}

// ---------------------------------------------------------------------------
// Boot skeleton screen and bootstrap
// ---------------------------------------------------------------------------

/// Replace the skeleton state wholesale with the real state (boot sequence in
/// the RedrawRequested branch).
///
/// [`EditorState::default`] does all the heavy lifting (settings + font
/// metrics + recovery scan + command-line file queueing); the live window
/// geometry already synced during the skeleton phase must be carried over —
/// otherwise the full UI's first frame lays out at factory size and flashes a
/// misaligned frame. The waker is dropped with the swap and re-attached:
/// background progressive opens rely on it to request frames.
fn boot_editor_state(state: &mut Signal<EditorState>, window: &Arc<dyn Window>) {
    let mut full = EditorState::default();
    {
        let sk = state.read();
        full.win.w = sk.win.w;
        full.win.h = sk.win.h;
        full.win.maximized = sk.win.maximized;
        full.win.topmost = sk.win.topmost;
    }
    let wake: crate::ui::state::WakeFn = {
        let w = window.clone();
        std::sync::Arc::new(move || w.request_redraw())
    };
    full.set_waker(wake);
    // INF_WS_SMOKE (GUI smoke): do not enter the working directory during the
    // skeleton phase; enter as usual once boot is done.
    if let Ok(p) = std::env::var("INF_WS_SMOKE") {
        if !p.is_empty() {
            full.ws_enter(std::path::PathBuf::from(&p));
            full.persist_settings();
        }
    }
    *state.write() = full;
}

/// Boot first screen (`EditorState::booted == false`): the theme's background
/// color as one flat fill.
///
/// This used to be a "skeleton": fake chrome (title bar / sidebar / a few text
/// lines / status bar) drawn to make the placeholder look like an editor. Boot
/// takes one or two frames, so the user saw those shapes appear and then vanish
/// again when the real UI replaced them — a flicker that never matched the
/// final layout anyway.
///
/// A single solid fill has the same job (nothing white, nothing empty) and no
/// transition to notice: it is exactly the color blitz already painted into the
/// window before the first frame (`WindowConfig::with_background`, InfEditor
/// patch 17), so the real UI simply appears where the background was.
///
/// Cost is the point: this frame has nothing else to show and must reach the
/// screen fast, so it stays one div with no content and no layout work.
fn skeleton_screen(t: &'static Theme) -> Element {
    rsx! {
        div {
            style: "position:absolute; inset:0; background:{t.bg}; overflow:hidden;",
            style { "{GLOBAL_CSS}" }
        }
    }
}

// ---------------------------------------------------------------------------
// Long-press (touch) → context menu
// ---------------------------------------------------------------------------

/// Hold duration (ms) for the long-press decision. Matches the Windows touch
/// long-press duration: shorter confuses it with "hold + nudge the
/// selection"; longer makes users think it is broken.
const LONG_PRESS_MS: u64 = 500;

/// Allowed finger drift (physical screen px) during a long press. Beyond it,
/// treat as a swipe/drag and cancel the long press — a menu popping up
/// mid-selection-drag is the most annoying mis-touch.
const LONG_PRESS_MOVE_PX: f64 = 12.0;

/// Whether the long-press candidate is due. Checks duration only; drift
/// cancellation happens in `PointerMoved` (which has the current coords), and
/// release/window gestures void the candidate in their own branches.
fn long_press_due(press: Option<(std::time::Instant, f64, f64)>, now: std::time::Instant) -> bool {
    press.is_some_and(|(t, _, _)| now.duration_since(t).as_millis() >= LONG_PRESS_MS as u128)
}

/// Pop the context menu when the long press is due. Called once per
/// `RedrawRequested`.
///
/// Why it piggybacks on redraw frames: the project has no async runtime and
/// no timers; while the finger holds, the `std::thread` spawned in the
/// `PointerButton` press branch sleeps until the deadline and calls
/// `request_redraw`, and the due check happens here. When no candidate exists
/// or it is not yet due, this is two cheap `Option` checks.
///
/// The menu is a modal Win32 loop (`TrackPopupMenu`), on the same path as the
/// right-button release (`open_context_menu`), likewise running in a hook
/// with no Blitz document borrow alive.
fn maybe_long_press(mut state: Signal<EditorState>, window: &Arc<dyn Window>, hwnd: isize) {
    let press = state.read().touch_press;
    if !long_press_due(press, std::time::Instant::now()) {
        return;
    }
    // `long_press_due` just confirmed a candidate exists, so unwrapping is safe
    let (_, x, y) = press.expect("long_press_due confirmed a candidate exists");
    state.write().touch_press = None;
    // The context menu is a modal Win32 loop: before it finishes, the finger
    // release may already have been swallowed by the menu, so void the touch
    // gesture ledger — otherwise that "release" after the menu closes is read
    // as a tap, moving the caret to the press position.
    crate::ui::touch::cancel();
    // What was recorded is physical screen coords — exactly the coordinate
    // system `open_context_menu` expects
    open_context_menu(state, window, hwnd, x, y);
}

// ---------------------------------------------------------------------------
// On-screen keyboard avoidance (touch)
// ---------------------------------------------------------------------------

/// Soft-keyboard avoidance: when the keyboard is visible, clear from the
/// viewport bottom the height the keyboard covers (CSS px).
///
/// The touch keyboard is a system top-level window: it floats above the app,
/// **does not** resize the window and **does not** change `SPI_GETWORKAREA`,
/// so we must probe it ourselves (`winshell::win::softkb`, throttled to
/// 120ms). The inset = window **client area** bottom − keyboard top,
/// converted back to CSS px and handed to
/// [`EditorState::set_keyboard_inset`]. After revision three the client area
/// ≠ outer frame (the bottom edge is inset by one native border width), so
/// the client bottom must come from `client_rect_screen` — using the outer
/// frame bottom overcounts one border and cuts an extra strip off the
/// viewport.
///
/// Probe failure / keyboard invisible → 0 (no avoidance): better to yield too
/// little than to permanently cut off the viewport. Platforms without a touch
/// keyboard (softkb always `None`) are naturally always 0; no cfg needed.
fn sync_keyboard_inset(mut state: Signal<EditorState>, window: &Arc<dyn Window>) {
    let want = match winshell::win::softkb::keyboard_top_screen_px() {
        Some(top) => {
            let scale = window.scale_factor() as f32;
            let scale = if scale > 0.0 { scale } else { 1.0 };
            match winshell::win::frameless::client_rect_screen() {
                // Client bottom (physical screen) = top + height
                Some((_l, t, _w, h)) => ((t + h - top) as f32 / scale).max(0.0),
                None => 0.0,
            }
        }
        None => 0.0,
    };
    // Write the signal only on a real change: `write` marks dirty and
    // rebuilds the whole VDOM, while this path runs on every redraw frame
    // (the probe is throttled to 120ms, but the frame rate is far higher).
    if (state.read().kb_inset - want).abs() >= 0.5 {
        state.write().set_keyboard_inset(want);
    }
}

/// Copy the OS's maximized state into the editor so the title bar's
/// maximize/restore glyph stays correct even when the window is maximized by
/// something other than our button (Win+Up, drag-to-top, the taskbar).
fn sync_maximized(state: &mut Signal<EditorState>) {
    let zoomed = winshell::win::frameless::is_zoomed();
    if state.read().win.maximized != zoomed {
        state.write().win.maximized = zoomed;
    }
}

/// Copy `WS_EX_TOPMOST` into the editor.
///
/// The flag can change from outside the app (a taskbar context menu, another
/// window's "always on top"), and the title-bar menu draws a check mark from it —
/// a stale check mark would offer to "pin" a window that is not pinned, or fail
/// to offer to unpin one that is.
fn sync_topmost(state: &mut Signal<EditorState>) {
    let top = winshell::win::frameless::is_topmost();
    if state.read().win.topmost != top {
        state.write().win.topmost = top;
    }
}

/// Raise the editor's context menu at the pointer.
///
/// Called from the winit hook, i.e. before Blitz handles the same event, so no
/// borrow of the document is alive while `TrackPopupMenu` runs its modal loop.
/// `raw_x` / `raw_y` are the pointer position in physical client pixels.
///
/// A right-click that landed on the **title bar** is not an editor click: it
/// gets the window's own system menu (restore / move / size / minimize /
/// maximize / always-on-top) instead, which is what a real caption does.
/// Without that split a right-click anywhere in the top 36px
/// popped up cut/copy/paste over the window controls.
///
/// Windows raises the native `TrackPopupMenu`; other platforms open the same
/// `Item` tree as the in-page overlay ([`CtxMenuOverlay`], flattened at open
/// time) — the only host that can show a menu extending past the window is
/// one with real popup windows.
fn open_context_menu(
    mut state: Signal<EditorState>,
    window: &Arc<dyn Window>,
    hwnd: isize,
    raw_x: f64,
    raw_y: f64,
) {
    // Platform choice is a runtime predicate (`winshell::native_ctx_menu`),
    // not cfg: both paths must compile and stay live; behavior differs by
    // capability. The native menu can extend past the window bounds and flips
    // automatically; the in-page overlay is the equivalent for other platforms
    // (submenus flattened into group headers).
    let native = winshell::native_ctx_menu();
    if native && hwnd == 0 {
        // Defensive branch: predicate true but the handle is not ready
        // (unreachable in theory — registered at the first frame).
        state.write().status = Some(Msg::StatusCtxMenuWindowsOnly.into());
        return;
    }
    // The in-page fallback does not depend on the handle; `hwnd` is only a
    // parameter of the native menu path.
    #[cfg(not(windows))]
    let _ = hwnd;

    let scale = window.scale_factor() as f32;
    let scale = if scale > 0.0 { scale } else { 1.0 };
    let css_x = raw_x as f32 / scale;
    let css_y = raw_y as f32 / scale;

    // ---- the title bar gets the window menu ---------------------------------
    if css_y < crate::ui::state::TITLEBAR_H {
        let (items, dark) = {
            let st = state.read();
            (actions::window_system_items(&st), st.dark)
        };
        // `dark` only feeds the native menu's theming; the in-page fallback
        // reads the theme off the state itself.
        if native {
            if let Some(cmd) = ctxmenu::show(&items, hwnd, raw_x as i32, raw_y as i32, dark, |c| {
                actions::is_checked(&state.read(), *c)
            }) {
                actions::run(&mut state.write(), cmd);
            }
        } else {
            // Anchor at the title bar's **bottom edge**: the native system
            // menu drops below the title bar without covering the bar that
            // invoked it (the in-page panel anchors its top-left at (x, y)).
            state
                .write()
                .open_ctx_menu(css_x, crate::ui::state::TITLEBAR_H, &items);
        }
        return;
    }

    // Move the caret unless the click landed inside the current selection —
    // that is what makes "copy" on a right-click
    // do the obvious thing. `dark` feeds the native menu's theming;
    // the in-page overlay reads the theme off the state itself.
    let dark = {
        let mut st = state.write();
        let v = st.view;
        let inside = css_x >= v.x && css_x <= v.x + v.w && css_y >= v.y && css_y <= v.y + v.h;
        if inside {
            if let Some(pos) = st.hit_test(css_x as f64, css_y as f64) {
                let keeps = st
                    .sel_range()
                    .map(|r| pos >= r.start && pos <= r.end)
                    .unwrap_or(false);
                if !keeps {
                    st.set_caret(pos, false);
                }
            }
        }
        st.dark
    };
    if !native {
        let _ = dark;
    }

    let items = editor_context_items(state.read().locale());
    if native {
        if let Some(cmd) = ctxmenu::show(&items, hwnd, raw_x as i32, raw_y as i32, dark, |c| {
            actions::is_checked(&state.read(), *c)
        }) {
            let mut st = state.write();
            actions::run(&mut st, cmd);
        }
    } else {
        state.write().open_ctx_menu(css_x, css_y, &items);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Long-press due decision: no candidate never fires, a just-pressed
    /// press cannot be due, and only holding past `LONG_PRESS_MS` fires
    /// (drift cancellation lives in the PointerMoved branch, not here).
    #[test]
    fn long_press_fires_only_after_the_hold_duration() {
        let now = std::time::Instant::now();
        assert!(!long_press_due(None, now), "no candidate means no menu");
        let fresh = Some((now, 100.0, 200.0));
        assert!(
            !long_press_due(fresh, now),
            "a just-pressed press cannot be due"
        );
        let later = now + std::time::Duration::from_millis(LONG_PRESS_MS + 50);
        assert!(
            long_press_due(fresh, later),
            "holding past the duration should be due"
        );
    }
}
