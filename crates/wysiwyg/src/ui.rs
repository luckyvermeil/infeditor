//! The `Wysiwyg` component: a controlled component; `editor: Signal<Editor>`
//! is held by the host (the demo's split source panel can therefore read the
//! same Signal directly).
//!
//! ## Event surface
//!
//! - Mouse: click to place the caret (clicking whitespace falls back to the
//!   document end); **drag selection is left to blitz native**
//!   (`DragMode::Selecting`, endpoints are always `(inline_root, layout byte)`,
//!   `geom::layout_to_plain` reverse-maps to the model `Pos`); double-click
//!   selects a word; Ctrl+click opens a link; clicking a task checkbox toggles
//!   it.
//! - Keyboard: all handled explicitly (Blitz has no contenteditable) — char
//!   insertion + block-level autocompletion, Enter/Backspace/Delete, arrow keys
//!   (Up/Down follow parley's visual lines, falling back to block boundaries
//!   across blocks), Home/End, Tab indent, Ctrl combos (A/C/X/V/Z/Y/B/I/K/D/E,
//!   1-6 headings).
//! - Selection highlight: **drawn natively by blitz** (`text_selection`), pushed
//!   back immediately when the model's selection changes (`apply_sel_to_blitz`);
//!   the DOM rebuild after a document change clears it, and the deferred sync
//!   task restores it next frame.
//! - IME: composition events are passed through to the default channel (input
//!   composition is taken over via a hidden input; the current div has no
//!   text_input_data so composed text is invisible — known limitation). Key
//!   presses **during** composition are intercepted by the flag maintained by
//!   [`set_composing`] (the vendor's `is_composing` is constantly false and
//!   dead code); the host drives this flag from winit's
//!   `WindowEvent::Ime` event.
//!
//! ## Render-phase discipline (design doc §4.3)
//!
//! - **Never touch the blitz document during the render phase**: dioxus
//!   component functions run inside blitz's resolve/paint flow, at which point
//!   `BaseDocument` is mutably borrowed and `try_doc()` is always `None`. All
//!   geometry computation must be computed in the **event handlers** and stored
//!   in a Signal; render only reads ready-made values.
//! - The render closure writes no Signal (no self-sustaining redraws).
//! - Derived values (content HTML) are computed as plain values before rsx!.

use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use dioxus_native::prelude::*;
use dioxus_native::NodeHandle;

use crate::edit::{Cmd, Editor, Move, WrapKind};
use crate::geom::{Geom, OverlayRect};
use crate::md;
use crate::model::{Doc, Pos, Sel};
use crate::render;

/// Triple-click window: the max gap between adjacent clicks.
const TRIPLE_WINDOW: Duration = Duration::from_millis(600);
/// Triple-click detection: the max displacement between adjacent clicks (px).
/// Aligned with touch slop — a fingertip's consecutive taps commonly drift
/// 3–6px, and 6px sits at the critical edge; mouse clicks are usually 0–2px
/// apart and unaffected.
const TRIPLE_DIST: f32 = 8.0;

/// Overlay state (content coordinates relative to `.wys-root`): caret + focused
/// cell highlight.
///
/// **Two rects in one signal**, because they always advance together in the
/// same focus change (see `Ctx::sync_overlays`) — splitting them into two
/// signals would make every call site responsible for two writes, and
/// `Signal::set` unconditionally marks dirty (render-phase discipline #1).
///
/// pub: the host (infeditor) path must hold it outside the component — key
/// presses on the composer route do not pass through the component's event
/// handlers, yet must still advance the caret overlay.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Overlays {
    /// The caret rect (`None` = not yet positioned).
    pub caret: Option<OverlayRect>,
    /// The whole-cell highlight when the focused block is a table cell.
    pub cell: Option<OverlayRect>,
}

/// An event-phase utility bundle: Signal references + NodeHandle. Constructed by
/// each event handler.
struct Ctx<'a> {
    editor: &'a mut Signal<Editor>,
    /// The caret/cell overlay (shared by the component's render and the host's
    /// composer key path).
    overlays: &'a mut Signal<Overlays>,
    handle: Rc<NodeHandle>,
    /// Mouse-edit notification (see [`Wysiwyg`]'s parameter of the same name).
    on_edit: Option<EventHandler<()>>,
    /// Mouse-caret notification (see [`Wysiwyg`]'s `on_caret`).
    on_caret: Option<EventHandler<()>>,
    /// Whether this component may grab DOM focus. A standalone demo must grab
    /// (it is its own only focus anchor; without grabbing, key events have no
    /// target); the host (infeditor) must **not** grab — the only real global
    /// focus is the host's composer textarea; grabbing triggers the host's
    /// onblur→swap-node→autofocus loop (re-mounting the composer on every key
    /// press).
    grab: bool,
}

impl Ctx<'_> {
    fn geom(&self) -> Geom {
        Geom::new(self.handle.clone())
    }

    /// Set DOM focus to wys-root. **Required**: blitz clears focus after a
    /// non-link/non-input click (`handle_click`'s `!matched → clear_focus`);
    /// the keydown event chain looks for a dioxus id by walking up from the
    /// focused node — with focus empty it lands on the root element and never
    /// reaches our onkeydown. A document change rebuilds nodes, invalidating
    /// the focus id, so focus must be restored after editing too.
    fn focus_root(&self) {
        if !self.grab {
            return;
        }
        let mut doc = self.handle.doc_mut();
        let ok = doc.set_focus_to(self.handle.node_id());
        wys_log(&format!("focus_root ok={ok}"));
    }

    /// Model Sel → overlay (event-phase geometry computation; the mirror
    /// operation of the render-phase iron rule).
    /// A single entry point advances both the caret and the focused-cell
    /// highlight — both are bound to the same focus change, keeping all call
    /// sites (component mouse/keyboard, host bridge) on a single sync path.
    ///
    /// When the caret's geometry cannot be found (the inline layout is
    /// momentarily missing after a rebuild) the **old value is kept**: clearing
    /// it makes the caret disappear entirely (observed: after typing in a code
    /// block the caret vanished and the hint panel was misjudged as drifting).
    /// The signal is only written when the value truly changed —
    /// `Signal::set` unconditionally marks dirty.
    fn sync_overlays(&mut self, g: &Geom) {
        let sel = self.editor.read().sel;
        let mut next = *self.overlays.peek();
        if let Some(r) = caret_overlay(g, sel) {
            next.caret = Some(r);
        }
        next.cell = self.cell_overlay(g, sel.focus.block);
        let cur = *self.overlays.peek();
        if next != cur {
            self.overlays.set(next);
        }
    }

    /// The focused block is a table cell (`Td`) → that cell's border-box rect;
    /// otherwise None (cleared).
    fn cell_overlay(&self, g: &Geom, block: usize) -> Option<OverlayRect> {
        let is_td = self
            .editor
            .read()
            .doc
            .blocks
            .get(block)
            .is_some_and(|b| b.is_td());
        is_td.then(|| cell_overlay(g, block)).flatten()
    }

    /// Home/End: the start/end of the current **visual line** (the line at a
    /// soft wrap, not the whole block).
    /// Returns false when the geometry is unavailable (an empty block / a
    /// zero-line layout / the document cannot be borrowed during render), and
    /// the caller falls back to the block start/end.
    fn move_line_edge(&mut self, to_start: bool, extend: bool) -> bool {
        let p = self.editor.read().sel.focus;
        let target = {
            let ed = self.editor.read();
            self.geom().line_edge(p, &ed.doc, to_start)
        };
        let Some(target) = target else {
            return false;
        };
        // Already at the line start/end: consume the key but do not move
        // (a browser's End does not jump to the paragraph end at a wrap).
        // With a **live selection** and no extend, though, the key still has an
        // effect: it collapses the selection onto that edge. Returning early
        // used to keep the selection alive — pressing End after Shift+End then
        // typed a character *replacing the whole selection* (the selection sat
        // there invisible-until-typed, and the status column jumped backwards).
        if target == p {
            if extend || self.editor.read().sel.collapsed() {
                return true;
            }
            let sel = Sel::caret(p);
            {
                let mut ed = self.editor.write();
                ed.sel = sel;
                ed.h_pos = None;
            }
            self.sync_overlays(&self.geom());
            self.apply_sel_to_blitz(sel);
            return true;
        }
        {
            let mut ed = self.editor.write();
            if extend {
                ed.sel.focus = target;
            } else {
                ed.sel = Sel::caret(target);
            }
            ed.h_pos = None; // Home/End rebuilds the vertical column memory
        }
        let g = self.geom();
        self.sync_overlays(&g);
        let sel = self.editor.read().sel;
        self.apply_sel_to_blitz(sel);
        caret_into_view(&self.handle, target);
        true
    }

    /// Model Sel → blitz's native selection (effective immediately; exact while
    /// the DOM is unchanged, restored by the deferred sync after a change).
    fn apply_sel_to_blitz(&self, sel: Sel) {
        push_selection(&self.handle, sel);
    }

    /// blitz's native selection → model Sel (read back during a drag select).
    /// anchor keeps the drag start passed in by the caller (blitz's endpoints
    /// have no direction semantics), focus = the other end. Returns None when
    /// there is no selection.
    fn read_blitz_sel(&self, _g: &Geom, anchor: Pos) -> Option<Sel> {
        let doc = self.handle.try_doc()?;
        let ranges = doc.get_text_selection_ranges();
        if ranges.is_empty() {
            return None;
        }
        let (first_node, s0, _) = ranges[0];
        let (last_node, _, e1) = ranges[ranges.len() - 1];
        let bid_of = |doc: &blitz_dom::BaseDocument, nid: blitz_dom::NodeId| -> Option<usize> {
            let node = doc.get_node(nid)?;
            let el = node.data.downcast_element()?;
            let a = el.attrs().iter().find(|a| &*a.name.local == "data-bid")?;
            a.value.parse().ok()
        };
        let b0 = crate::geom::layout_to_plain(&doc, first_node, s0)?;
        let b1 = crate::geom::layout_to_plain(&doc, last_node, e1)?;
        let bid0 = bid_of(&doc, first_node);
        let bid1 = bid_of(&doc, last_node);
        drop(doc);
        let p0 = Pos::new(bid0?, b0);
        let p1 = Pos::new(bid1?, b1);
        // The moving end is whichever endpoint is farther from the drag start.
        // The anchor can be a position blitz cannot express (an empty block, a
        // formula's virtual bytes, the "click past the end" fallback), in which
        // case *neither* endpoint equals it and the old "if p0 == anchor" test
        // picked the wrong end — the model selection then covered a different
        // range than the on-screen highlight (copy/cut read the model one).
        let key = |p: Pos| (p.block, p.off);
        let focus = if key(anchor) <= key(p0) {
            p1
        } else if key(anchor) >= key(p1) {
            p0
        } else if p0 == anchor {
            p1
        } else {
            p0
        };
        Some(Sel { anchor, focus })
    }

    /// The unified entry point for applying a command: write the model →
    /// recompute the caret CSS → (document unchanged) push the selection back
    /// immediately; document changed → restore the caret/selection one frame
    /// later.
    /// Returns **whether the document actually changed** (the host schedules the
    /// write-back accordingly; pure caret moves / invalid commands return false,
    /// avoiding incidental format normalization / dirtying).
    fn apply_cmd(&mut self, cmd: Cmd) -> bool {
        // The selection snapshot must be taken unconditionally **before**
        // apply() — with logging off, the old implementation only snapshotted
        // inside the log branch and the `sel_before` fallback below read
        // **after** apply, so it always equaled sel_now, `sel_now != sel_before`
        // was forever false, and the selection push-back (apply_sel_to_blitz)
        // never ran.
        let sel_before = self.editor.read().sel;
        // The len statistics are two full-document plain_text traversals, done
        // only when logging is on: `format!` arguments are evaluated before
        // wys_log's internal check — on a 500-block document that is 2×500 heap
        // allocations per key feeding a log nobody reads (the same defect as in
        // shell.rs).
        let len_before = log_enabled().then(|| total_plain_len(&self.editor.read().doc));
        let changed = self.editor.write().apply(cmd.clone());
        if let Some(len_before) = len_before {
            let sel_now = self.editor.read().sel;
            wys_log(&format!(
                "apply_cmd {cmd:?} sel_before={sel_before:?} len_before={len_before} \
                 sel_now={sel_now:?} len_now={}",
                total_plain_len(&self.editor.read().doc)
            ));
        }
        let sel_now = self.editor.read().sel;
        let g = self.geom();
        self.sync_overlays(&g);
        // Pure selection/move commands do not touch the document: the old DOM
        // is still valid, push back immediately.
        // Wrap/Link/SetHead/Toggle* change the fragment HTML → DOM rebuild, so
        // deferred_sync is required: the old block nodes are invalidated by the
        // rebuild, and pushing back immediately would draw the caret/selection
        // on destroyed nodes (aligned with Editor::apply's changed semantics; a
        // hand-maintained whitelist once drifted from the real changed).
        let doc_unchanged = matches!(
            cmd,
            Cmd::Move(_) | Cmd::MoveExtend(_) | Cmd::SelectAll | Cmd::SelectWord | Cmd::SelectBlock
        );
        // Mouse edits (checking a task box, etc.) only happen inside this
        // component: notify the host to write back and mark dirty, otherwise the
        // change only lives in the wys model and gets overwritten by the piece
        // table on save / tab switch.
        if !doc_unchanged && changed {
            if let Some(cb) = self.on_edit.as_ref() {
                cb.call(());
            }
        }
        if doc_unchanged {
            if sel_now != sel_before {
                self.apply_sel_to_blitz(sel_now);
            }
            caret_into_view(&self.handle, sel_now.focus);
        } else {
            self.deferred_sync(sel_now);
        }
        changed
    }

    /// Deferred sync after a document change: wait one frame for the DOM rebuild
    /// to finish, then restore the caret CSS and selection.
    ///
    /// **Borrowing iron rule**: dioxus's `poll()` holds `inner.borrow_mut()`
    /// (the MutationWriter) during `render_immediate`, while render internally
    /// polls this component's spawned tasks. If a task resumes during render:
    /// any `doc()`/`doc_mut()` panics with `RefCell already borrowed`; and
    /// spinning on the borrow with `std::thread::sleep` **deadlocks the render**
    /// (the borrow is only released when render returns). So an async Delay must
    /// **truly suspend** (letting render continue); the next poll round after
    /// the borrow is released naturally succeeds.
    fn deferred_sync(&self, sel: Sel) {
        // Host mode (grab=false): set the pending flag and hand it to
        // [`host_flush_resync`] on RedrawRequested. Async polling in the host
        // would never obtain a borrowable document (whenever the task is polled
        // render holds &mut) and would only spin until giving up.
        if !self.grab {
            RESYNC_TRIES.store(0, std::sync::atomic::Ordering::Relaxed);
            NEEDS_RESYNC.store(true, std::sync::atomic::Ordering::Relaxed);
            return;
        }
        let handle = self.handle.clone();
        let mut overlays = *self.overlays;
        // Whether the focused block is a table cell is taken at **event time**
        // (the DOM is rebuilt after a document change, so re-querying the model
        // asynchronously is not guaranteed consistent); after the rebuild redraw
        // the whole-cell highlight by the same block number.
        let cell_block = sel.focus.block;
        let cell_is_td = self
            .editor
            .read()
            .doc
            .blocks
            .get(cell_block)
            .is_some_and(|b| b.is_td());
        let grab = self.grab;
        spawn(async move {
            // Wait until the document is borrowable (= render has finished;
            // 2ms polling). An iteration cap as a backstop: after the component
            // unmounts try_doc is always None, and an uncapped loop would spin
            // forever at 2ms intervals.
            let mut tries = 0u32;
            loop {
                if handle.try_doc().is_some() {
                    break;
                }
                tries += 1;
                if tries > 500 {
                    wys_log("deferred_sync: give up after 500 tries (unmounted?)");
                    return;
                }
                futures_timer::Delay::new(std::time::Duration::from_millis(2)).await;
            }
            // The rebuilt DOM: the focus id is invalidated, re-focus the root (the
            // lifeline for keyboard input).
            // Host mode (grab=false) does not grab focus: the host composer
            // textarea is the only
            // real focus anchor; the rebuild does not pass through it, so there
            // is no need (and no way) to grab on its behalf.
            if grab {
                let mut doc = handle.doc_mut();
                doc.set_focus_to(handle.node_id());
            }
            let g = Geom::new(handle.clone());
            let mut next = *overlays.peek();
            if let Some(css) = caret_overlay(&g, sel) {
                next.caret = Some(css);
            }
            next.cell = cell_overlay(&g, cell_block).filter(|_| cell_is_td);
            let cur = *overlays.peek();
            if next != cur {
                overlays.set(next);
            }
            // After typing in long text / splitting a block the caret can fall
            // outside the viewport: follow-scroll immediately.
            caret_into_view(&handle, sel.focus);
            if !sel.collapsed() {
                push_selection(&handle, sel);
            }
        });
    }
}

/// Scroll the caret into the wys-root viewport (keep it visible when the
/// content overflows — caret following for Ctrl+End / page turns / typing in
/// long text). Only scrolls when it crosses 24px beyond the top/bottom edge;
/// clicking visible text does not trigger jumping.
///
/// Borrowing: first `try_doc` to compute the target (may fail = render phase /
/// component unmounted), then `doc_mut` to actually scroll. Safe to call both
/// in event phase and in the deferred task (the latter has awaited try_doc).
fn caret_into_view(handle: &Rc<NodeHandle>, pos: Pos) {
    /// The minimum margin between the caret and the viewport's top/bottom edge.
    const VIEW_MARGIN: f32 = 24.0;
    let Some(doc) = handle.try_doc() else { return };
    let Some(node) = doc.get_node(handle.node_id()) else {
        return;
    };
    let vp_h = node.final_layout().size.height;
    if vp_h <= 0.0 {
        return;
    }
    let scroll_y = node.scroll_offset().y as f32;
    let g = Geom::new(handle.clone());
    let Some(r) = g.caret_rect(pos) else { return };
    let Some((_, oy)) = g.root_origin() else {
        return;
    };
    let top = r.y - oy;
    let bottom = top + r.h;
    let mut target = scroll_y;
    if top - VIEW_MARGIN < scroll_y {
        target = top - VIEW_MARGIN;
    } else if bottom + VIEW_MARGIN > scroll_y + vp_h {
        target = bottom + VIEW_MARGIN - vp_h;
    }
    if (target - scroll_y).abs() < 0.5 {
        return;
    }
    drop(doc);
    let mut doc = handle.doc_mut();
    doc.scroll_to(
        handle.node_id(),
        0.0,
        target.max(0.0) as f64,
        blitz_dom::ScrollBehavior::Instant,
    );
}

/// Pending DOM sync to complete after a document change (host mode). Mutating
/// the DOM/geometry during event phase is unreliable: blitz holds the document
/// borrow while dispatching events, and tasks spawned during render are only
/// ever polled during render (`try_doc` always None) — async Delay polling is a
/// **dead end** here (observed: still waiting 5 seconds after Ctrl+End). The
/// host instead calls [`host_flush_resync`] on `RedrawRequested` (before blitz
/// borrows the document) to do the same thing.
static NEEDS_RESYNC: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Retry count after a geometry query failed / the layout was not ready: on the
/// first frame after a document change the DOM may not yet be rebuilt/laid out,
/// and the caret rect either comes back empty or falls into the empty-block
/// fallback. Keep the pending flag so the next frame retries; the cap prevents
/// pathological documents from spinning every frame (retries are driven by the
/// caller's request_redraw).
static RESYNC_TRIES: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

/// The host frame hook: finishes "push the caret/selection back after a
/// document change + scroll it into view". Zero cost when nothing is pending
/// (one atomic). Called in `RedrawRequested`.
///
/// Returns whether work is **still pending**: true = not finished this time
/// (the focused block's layout is not ready); the caller should
/// `request_redraw` to schedule another frame, otherwise the retry never
/// happens (no one requests → no next frame).
///
/// ## Why "the query succeeded" can still be untrustworthy (observed: the
/// caret pinned to the block start after IME / heading / formula)
///
/// In the host pipeline, the DOM tree update (dioxus poll) and the **inline
/// layout construction** (blitz's render-phase deferred task) are two steps:
/// this hook runs on `RedrawRequested` (before blitz resolves), when a freshly
/// rebuilt focused block's `inline_layout_data` is guaranteed `None`.
/// `caret_rect` then falls into the empty-block fallback (the content box's
/// top-left) everywhere — the query returns `Some`, but the position is the
/// **block start**, and it also clears the retry pending flag, so the caret is
/// pinned to the block start from then on (the faster you type the more obvious:
/// IME commits, `# ` converting to a heading, closing a formula — all the same).
/// So when the focused block's layout is not ready (`None`) the overlay is **not
/// overwritten** and the pending flag is kept to retry next frame — next frame
/// blitz has built the layout and the `Some` value is the true position (this
/// includes the zero-line fallback: a constructed zero-line is a hard fact and
/// is trustworthy).
pub fn host_flush_resync(
    handle: Rc<NodeHandle>,
    editor: &mut Signal<Editor>,
    overlays: &mut Signal<Overlays>,
) -> bool {
    if !NEEDS_RESYNC.swap(false, std::sync::atomic::Ordering::Relaxed) {
        return false;
    }
    let sel = editor.read().sel;
    let cell_block = sel.focus.block;
    let cell_is_td = editor
        .read()
        .doc
        .blocks
        .get(cell_block)
        .is_some_and(|b| b.is_td());
    let g = Geom::new(handle.clone());
    let mut next = *overlays.peek();
    // When the geometry cannot be found (the inline layout is momentarily
    // missing after a rebuild) **keep** the old caret: clearing it makes the
    // caret disappear entirely (observed: after typing in a code block the
    // caret vanished and the hint panel was misjudged as drifting).
    // Also keep the pending flag to retry next frame — on the first frame after
    // a document change the DOM may not yet be rebuilt; giving up after a single
    // failure would leave the caret at the old position (most obvious when Enter
    // creates an empty line).
    let hit = caret_overlay(&g, sel);
    // A freshly rebuilt block's inline layout is guaranteed unbuilt (see the
    // function doc); a `hit` of Some right now is the empty-block fallback's
    // fake block-start value — do not overwrite; wait for the real layout next
    // frame. Exception: the model block **was already empty** (an Enter new line /
    // `# ` converting to a heading) — an empty block never has an inline layout
    // (blitz does not build one for an empty paragraph), so waiting would only
    // burn retries; the empty-block fallback's content-box top-left is the
    // correct caret position (on the first frame, when the taffy box is not yet
    // laid out, the fallback's extrapolation branch gives an approximation;
    // next frame it is exact).
    let ready = focus_layout_ready(&handle, sel.focus.block)
        || editor
            .read()
            .doc
            .blocks
            .get(sel.focus.block)
            .is_some_and(|b| b.plain_text().is_empty());
    if log_enabled() {
        // Diagnostics: whether the DOM queried by the frame hook already
        // contains the new text (a len mismatch = old DOM), and whether the
        // focused block's inline layout is built (a hit with ready=false is
        // untrustworthy).
        let dom_len = handle.try_doc().and_then(|doc| {
            let container = crate::geom::fragment_container(&doc, handle.node_id());
            crate::geom::node_for_bid(&doc, container, sel.focus.block)
                .and_then(|nid| crate::geom::layout_to_plain(&doc, nid, usize::MAX))
        });
        // The image data state of the first svg in the focused block
        // (Some(true)=drawable / Some(false)=parse missing / None=no svg).
        let svg_state = handle.try_doc().and_then(|doc| {
            let container = crate::geom::fragment_container(&doc, handle.node_id());
            crate::geom::node_for_bid(&doc, container, sel.focus.block)
                .and_then(|nid| find_svg_image(&doc, nid))
        });
        wys_log(&format!(
            "flush_resync sel={sel:?} hit={hit:?} ready={ready} dom_block_len={dom_len:?} \
             model_len={:?} tries={} svg={svg_state:?}",
            editor
                .read()
                .doc
                .blocks
                .get(sel.focus.block)
                .map(|b| b.plain_text().len()),
            RESYNC_TRIES.load(std::sync::atomic::Ordering::Relaxed)
        ));
    }
    let settled = if let Some(r) = hit.filter(|_| ready) {
        next.caret = Some(r);
        RESYNC_TRIES.store(0, std::sync::atomic::Ordering::Relaxed);
        true
    } else if RESYNC_TRIES.load(std::sync::atomic::Ordering::Relaxed) < 3 {
        RESYNC_TRIES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        NEEDS_RESYNC.store(true, std::sync::atomic::Ordering::Relaxed);
        false
    } else {
        // Give up after the retry cap: accept the last query (in a truly
        // pathological case the block start is still better than a vanished
        // caret).
        if let Some(r) = hit {
            next.caret = Some(r);
        }
        RESYNC_TRIES.store(0, std::sync::atomic::Ordering::Relaxed);
        true
    };
    if settled {
        // Same reasoning as the caret: a freshly rebuilt block's taffy box is
        // also unresolved, so querying the cell now returns None → the table
        // focus highlight flickers for a frame. Keep the old value and refresh
        // next frame (when settled).
        next.cell = cell_overlay(&g, cell_block).filter(|_| cell_is_td);
    }
    let cur = *overlays.peek();
    if next != cur {
        overlays.set(next);
    }
    if settled && !sel.collapsed() {
        push_selection(&handle, sel);
    }
    if settled {
        caret_into_view(&handle, sel.focus);
    }
    !settled
}

/// The image data state of the first `<svg>` element in a subtree (diagnostics):
/// Some(true)=ImageData::Svg (drawable), Some(false)=the svg element exists but
/// the image data is missing, None=the subtree has no svg.
fn find_svg_image(doc: &blitz_dom::BaseDocument, id: blitz_dom::NodeId) -> Option<bool> {
    let node = doc.get_node(id)?;
    if let Some(el) = node.data.downcast_element() {
        if el.name.local.to_string() == "svg" {
            let has_image = matches!(
                &el.special_data,
                blitz_dom::node::SpecialElementData::Image(img)
                    if matches!(**img, blitz_dom::node::ImageData::Svg(_))
            );
            return Some(has_image);
        }
    }
    for &c in &node.children {
        if let Some(found) = find_svg_image(doc, c) {
            return Some(found);
        }
    }
    None
}

/// Whether the focused block's inline layout has been built (the trust
/// criterion for [`host_flush_resync`]). When the block node cannot be found /
/// the element is abnormal, treat it as ready (do not turn a defensive branch
/// into a permanent retry).
fn focus_layout_ready(handle: &NodeHandle, block: usize) -> bool {
    let Some(doc) = handle.try_doc() else {
        return true;
    };
    let container = crate::geom::fragment_container(&doc, handle.node_id());
    let Some(nid) = crate::geom::node_for_bid(&doc, container, block) else {
        return true;
    };
    let Some(node) = doc.get_node(nid) else {
        return true;
    };
    node.data
        .downcast_element()
        .map(|el| el.inline_layout_data.is_some())
        .unwrap_or(true)
}

/// Model Sel → the caret overlay rect (pure function): document coordinates
/// minus the root origin.
fn caret_overlay(g: &Geom, sel: Sel) -> Option<OverlayRect> {
    let r = g.caret_rect(sel.focus)?;
    let (ox, oy) = g.root_origin()?;
    Some(OverlayRect {
        x: r.x - ox,
        y: r.y - oy,
        w: r.w,
        h: r.h,
    })
}

/// Block → the whole-cell highlight overlay rect (pure function): the block's
/// border box minus the root origin.
/// Returns None when the geometry is unavailable (out of bounds / node
/// missing) — the overlay is cleared rather than drawn blindly.
fn cell_overlay(g: &Geom, block: usize) -> Option<OverlayRect> {
    let (x, y, w, h) = g.block_border_rect(block)?;
    if w <= 0.0 || h <= 0.0 {
        return None;
    }
    let (ox, oy) = g.root_origin()?;
    Some(OverlayRect {
        x: x - ox,
        y: y - oy,
        w,
        h,
    })
}

/// Model Sel → blitz's native selection. Silently skipped when there is no
/// document borrow / the node is missing / the DOM has been rebuilt (the caller
/// is responsible for re-pushing). All selection-pushing paths (immediate /
/// deferred / the host frame hook) share the same locating logic — it used to
/// be written three times over, and the byte conversion silently drifted.
fn push_selection(handle: &NodeHandle, sel: Sel) -> bool {
    let Some(doc) = handle.try_doc() else {
        return false;
    };
    let (start, end) = sel.range();
    let container = crate::geom::fragment_container(&doc, handle.node_id());
    let (Some(sn), Some(en)) = (
        crate::geom::node_for_bid(&doc, container, start.block),
        crate::geom::node_for_bid(&doc, container, end.block),
    ) else {
        return false;
    };
    let (Some(s), Some(e)) = (
        crate::geom::plain_to_layout(&doc, sn, start.off),
        crate::geom::plain_to_layout(&doc, en, end.off),
    ) else {
        return false;
    };
    drop(doc);
    let mut doc = handle.doc_mut();
    doc.set_text_selection(sn, s, en, e);
    true
}
/// WYSIWYG Markdown editor.
///
/// - `editor`: the controlled editor state (the host may `write()` to replace
///   the whole document / undo, etc.).
/// - `font_size`: the content font size in px (default 16).
/// - `on_open_link`: Ctrl+click link callback (the host decides how to open).
/// - `overlays`: the caret/focused-cell overlay (also advanced when the host's
///   composer routes key presses, so the same signal must be shared; if not
///   passed the component creates one — used by the demo).
/// - `grab_focus`: whether to grab DOM focus onto wys-root after an event
///   (default true). The host passes false (see the doc on [`Ctx::grab`]).
/// - `on_ready`: callback with the NodeHandle after the root element mounts
///   (the host stores it for the composer path's geometry queries; ignored if
///   not passed).
/// - `img_base`: the directory the document lives in — relative-path images
///   resolve against it into file:// URLs; pass `None` for an unsaved new
///   document (relative images fall back to the alt placeholder, no broken
///   images).
/// - `on_drag`: press/release notifications for drag-selection auto-scroll
///   (see the parameter doc).
#[component]
pub fn Wysiwyg(
    mut editor: Signal<Editor>,
    font_size: Option<i32>,
    on_open_link: Option<EventHandler<String>>,
    overlays: Option<Signal<Overlays>>,
    grab_focus: Option<bool>,
    on_ready: Option<EventHandler<Rc<NodeHandle>>>,
    /// Notification after a **mouse** edit inside the component (checking a task
    /// box, etc.): the host uses it to write the wys content back to the piece
    /// table and mark it dirty. The keyboard path is driven by the host itself
    /// and does not go through here.
    on_edit: Option<EventHandler<()>>,
    /// Notification after a **mouse** caret/selection move inside the component:
    /// the host uses it to align the hidden text caret to the same position
    /// (status bar row/column, selection anchor). Without the notification the
    /// status bar stays at the old row/column after a click until the next
    /// keyboard move (found in practice).
    ///
    /// Also fires when a primary press is consumed without moving the caret
    /// (task-checkbox click, Ctrl+click link): the host closes its menu-bar
    /// dropdown on every press into the content, exactly like the composer's
    /// pointer handler does — a menu must not outlive a click elsewhere.
    on_caret: Option<EventHandler<()>>,
    /// The content foreground color (the fill for list/table text and formula
    /// SVGs). Defaults to DEFAULT_FG (dark text on a light theme) — the host
    /// should pass the theme's fg, otherwise formulas are invisible on a dark
    /// theme.
    fg: Option<String>,
    /// The base for resolving relative images (the document's directory); see
    /// the parameter doc above.
    img_base: Option<std::path::PathBuf>,
    /// Notification when a press inside the content starts / ends a potential
    /// drag selection (`true` = a press landed, `false` = it was released).
    ///
    /// The host needs it because blitz's native drag stops at the element
    /// boundary: once the pointer leaves the content, no `pointermove` reaches
    /// this component, so the selection neither grows nor scrolls. The host
    /// tracks the window-level pointer and calls [`host_drag_extend`] +
    /// [`host_scroll_by`] on each frame while the button is down.
    on_drag: Option<EventHandler<bool>>,
) -> Element {
    let mut handle_sig: Signal<Option<Rc<NodeHandle>>> = use_signal(|| None);
    let own: Signal<Overlays> = use_signal(Overlays::default);
    let mut overlays = overlays.unwrap_or(own);
    let grab = grab_focus.unwrap_or(true);
    let fs = font_size.unwrap_or(16);
    let fg = fg.unwrap_or_else(|| render::DEFAULT_FG.to_string());

    // Triple-click select-block rhythm counter: (consecutive count, last x,
    // last y, last instant).
    let mut clicks: Signal<(u32, f32, f32, Option<Instant>)> = use_signal(|| (0, 0.0, 0.0, None));

    // Sharded rendering: one fragment per region (render::region_fragments),
    // display:contents wrappers are layout-transparent (blitz's box-construction
    // hoisting). Under dioxus's positional diffing, an unchanged fragment's
    // String is unchanged → dangerous_inner_html unchanged → zero re-parse /
    // zero re-layout — a keystroke in a large document only re-lays-out the
    // current block (no key: keys shift as fragment count changes, forcing a
    // full re-comparison).
    // img_base is passed to the render layer to resolve relative images (same
    // safe_image_url as the preview).
    // Formulas render at the current body font size (fs is configurable; a
    // fixed 16 would be inconsistent with 14px body text)
    let frags =
        render::region_fragments_sized(&editor.read().doc, &fg, img_base.as_deref(), fs as f32);

    rsx! {
        style { {render::SHELL_CSS} }
        div {
            class: "wys-root",
            // tabindex: makes blitz's click default action (vendor patch six)
            // treat this element as a focusable target — clicking the edit area
            // focuses rather than defocuses, so keyboard input has a target.
            // **Host mode (grab=false) must not set it**: with it, patch six pins
            // focus to wys-root, key presses go through the component's own
            // onkeydown (the host bridge stops working, content never lands on
            // disk — proven by a live smoke test); without it a click goes
            // through clear_focus and the host composer's
            // onblur→swap-node→autofocus takes focus back, so key presses are
            // always routed via the host bridge.
            tabindex: grab.then_some("0"),
            onmounted: move |evt: MountedEvent| async move {
                // NodeHandle: the front door for synchronous access to
                // BaseDocument (design doc §1).
                if let Some(h) = evt.data.downcast::<NodeHandle>() {
                    let h = Rc::new(h.clone());
                    let g = Geom::new(h.clone());
                    let sel = editor.read().sel;
                    let is_td = editor
                        .read()
                        .doc
                        .blocks
                        .get(sel.focus.block)
                        .is_some_and(|b| b.is_td());
                    // At mount there is usually no caret (the host seeded but
                    // has not positioned it); do a defensive sync once.
                    let next = Overlays {
                        caret: caret_overlay(&g, sel),
                        cell: cell_overlay(&g, sel.focus.block).filter(|_| is_td),
                    };
                    let cur = *overlays.peek();
                    if next != cur {
                        overlays.set(next);
                    }
                    handle_sig.set(Some(h.clone()));
                    if let Some(cb) = on_ready.as_ref() {
                        cb.call(h);
                    }
                }
            },
            // The pointer family rather than the mouse family: blitz only
            // synthesizes MouseDown/MouseUp for a mouse; touch (fingers) only
            // dispatches pointer/touch events — an onmousedown handler would
            // **never receive clicks** on a touchscreen: tapping the body does
            // nothing and the caret does not land. Pointer events cover mouse,
            // finger and stylus alike.
            onpointerdown: move |evt: PointerEvent| {
                let Some(h) = handle_sig.read().as_ref().cloned() else { return };
                // Right/middle clicks do not move the selection: the context
                // menu's "Copy" reads exactly the current selection (Sel);
                // collapsing it into a caret on press (the original had no
                // button check) would make the menu never copy what was just
                // selected. Browser behavior: right-click keeps the existing
                // selection.
                if evt.data.trigger_button() != Some(dioxus_elements::input_data::MouseButton::Primary) {
                    return;
                }
                let c = evt.data.client_coordinates();
                let (x, y) = (c.x as f32, c.y as f32);
                let g = Geom::new(h.clone());
                wys_log(&format!("mousedown at ({x},{y})"));
                // Triple-click rhythm: consecutive when within 600ms and
                // displacement < 6px; the 3rd click triggers block selection
                // (checkbox/link hits take priority; detection only accounts, the
                // trigger comes after click positioning).
                let now = Instant::now();
                let (cnt, lx, ly, lt) = *clicks.read();
                let chained = lt
                    .map(|t| now.duration_since(t) < TRIPLE_WINDOW)
                    .unwrap_or(false)
                    && (x - lx).powi(2) + (y - ly).powi(2) < TRIPLE_DIST * TRIPLE_DIST;
                let triple = chained && cnt >= 2;
                clicks.set(if chained {
                    (if triple { 0 } else { cnt + 1 }, x, y, Some(now))
                } else {
                    (1, x, y, Some(now))
                });
                let mut ctx = Ctx { editor: &mut editor, overlays: &mut overlays, handle: h, on_edit, on_caret, grab };
                // Checkbox hit takes priority (it is outside data-bid, so
                // pos_at cannot reach it)
                if let Some(bid) = g.task_at(x, y) {
                    ctx.apply_cmd(Cmd::ToggleTaskChecked(bid));
                    // demo (grab=true) re-focuses the root so you can type right
                    // after clicking the box; host mode is a no-op (the
                    // composer's onblur→autofocus takes it back).
                    ctx.focus_root();
                    // A press consumed by the box is still a press into the
                    // content: the host closes its menu on it (see `on_caret`).
                    if let Some(cb) = ctx.on_caret.as_ref() {
                        cb.call(());
                    }
                    return;
                }
                // Click positioning: an empty block (no text node) hits by
                // geometry first, otherwise find_text_position; neither hits
                // (whitespace in the content area / below the document end) →
                // document end. When pos_at misses data-bid (the
                // marker/checkbox hanging area) it falls to the nearest block by
                // geometry instead of being thrown to the document end by the
                // end-of-document fallback. Geometry queries need &Doc, so the
                // short-read guard only lives until p is computed (the
                // end-of-document fallback also uses it; no second read, no
                // deep-cloning the whole document).
                let p = {
                    let ed = ctx.editor.read();
                    g.empty_block_at(x, y, &ed.doc)
                        .map(|bid| Pos::new(bid, 0))
                        .or_else(|| g.math_at(x, y, &ed.doc))
                        .or_else(|| g.pos_at(x, y, &ed.doc))
                        .unwrap_or_else(|| {
                            let last = ed.doc.blocks.len().saturating_sub(1);
                            let off = ed
                                .doc
                                .blocks
                                .last()
                                .map(|b| b.plain_text().len())
                                .unwrap_or(0);
                            Pos::new(last, off)
                        })
                };
                // Ctrl+click on a link opens it outside the app. Looked up in
                // the **model** at the position the click resolved to (the DOM
                // link hit-test and the caret positioning disagreed on
                // coordinates — the model is the ledger the caret rides on).
                // Deliberately checked **before** `sel` is written: following a
                // link is navigation, not editing, so the caret stays on the
                // word the user was on (`Edit` still applies to it). The text
                // view behaves the same way. The URL passes the
                // `safe_link_url` whitelist before the callback, same source as
                // the preview; a rejected URL falls through to plain caret
                // placement.
                if evt.data.modifiers().ctrl() {
                    let url = ctx.editor.read().link_at_pos(p);
                    if let Some(url) = url {
                        if let Some(target) = open_link_target(&url) {
                            if let Some(cb) = on_open_link.as_ref() {
                                cb.call(target);
                            }
                        }
                        ctx.focus_root();
                        // Same as the checkbox: the menu must not outlive the
                        // click.
                        if let Some(cb) = ctx.on_caret.as_ref() {
                            cb.call(());
                        }
                        return;
                    }
                }
                {
                    let mut ed = ctx.editor.write();
                    ed.sel = Sel::caret(p);
                    ed.h_pos = None; // A click rebuilds the vertical column memory
                }
                ctx.sync_overlays(&g);
                ctx.focus_root();
                // A press landed in the content: the host starts tracking the
                // window-level pointer so a drag that leaves this element keeps
                // extending (blitz's native drag stops at the boundary).
                if let Some(cb) = on_drag.as_ref() {
                    cb.call(true);
                }
                if triple {
                    ctx.apply_cmd(Cmd::SelectBlock);
                }
                if let Some(cb) = ctx.on_caret.as_ref() {
                    cb.call(());
                }
                // Drag selection is taken over by blitz native (user-select
                // defaults to Auto); the drag start = the caret just set, read
                // back on pointerup.
            },
            onpointerup: move |_evt: PointerEvent| {
                if let Some(cb) = on_drag.as_ref() {
                    cb.call(false);
                }
                let Some(h) = handle_sig.read().as_ref().cloned() else { return };
                let g = Geom::new(h.clone());
                let mut ctx = Ctx { editor: &mut editor, overlays: &mut overlays, handle: h, on_edit, on_caret, grab };
                let anchor = ctx.editor.read().sel.anchor;
                if let Some(sel) = ctx.read_blitz_sel(&g, anchor) {
                    if !sel.collapsed() {
                        let mut ed = ctx.editor.write();
                        ed.sel = sel;
                        ed.h_pos = None; // drag select ended, vertical column memory invalid
                        drop(ed);
                        ctx.sync_overlays(&g);
                        if let Some(cb) = ctx.on_caret.as_ref() {
                            cb.call(());
                        }
                    }
                }
            },
            ondoubleclick: move |_evt: MouseEvent| {
                let Some(h) = handle_sig.read().as_ref().cloned() else { return };
                let mut ctx = Ctx { editor: &mut editor, overlays: &mut overlays, handle: h, on_edit, on_caret, grab };
                // Table cell: double-click = select the whole cell's content
                // (aligned with CSV double-click in-place editing — typing
                // directly overwrites the cell). Non-table blocks keep
                // double-click word selection.
                let block = ctx.editor.read().sel.focus.block;
                let is_td = ctx
                    .editor
                    .read()
                    .doc
                    .blocks
                    .get(block)
                    .is_some_and(|b| b.is_td());
                ctx.apply_cmd(if is_td {
                    Cmd::SelectBlock
                } else {
                    Cmd::SelectWord
                });
                if let Some(cb) = ctx.on_caret.as_ref() {
                    cb.call(());
                }
            },
            onkeydown: move |evt: KeyboardEvent| {
                let Some(h) = handle_sig.read().as_ref().cloned() else { return };
                let mut ctx = Ctx { editor: &mut editor, overlays: &mut overlays, handle: h, on_edit, on_caret, grab };
                handle_keydown(&mut ctx, &evt, &on_open_link);
            },
            // Focused cell highlight: rendered **before** the content
            // (`.wys-content` is a relatively positioned element, so content
            // later in DOM order paints on top of it), serving as a cell
            // background color underneath; text/borders are unaffected.
            // pointer-events:none does not intercept clicks.
            if let Some(r) = overlays().cell {
                div { class: "wys-cell", style: "{r.css()}" }
            }
            div {
                class: "wys-content",
                font_size: "{fs}px",
                for frag in frags {
                    div { style: "display:contents;", dangerous_inner_html: "{frag}" }
                }
            }
            if let Some(r) = overlays().caret {
                div { class: "wys-caret", style: "{r.css()}" }
            }
        }
    }
}

/// Keyboard dispatch (a standalone function: complex logic does not belong in
/// an rsx closure body).
/// Log switch (OnceLock cached: even one env query per key is hot-path cost).
fn log_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("INF_WYS_LOG").is_ok())
}

/// Whole-document plain-text length (for diagnostic logs; called only when
/// log_enabled).
fn total_plain_len(doc: &Doc) -> usize {
    doc.blocks.iter().map(|b| b.plain_text().len()).sum()
}

fn wys_log(msg: &str) {
    if log_enabled() {
        eprintln!("[wys] {msg}");
    }
}

// ---- IME composition state ----------------------------------------------------
//
// The vendor's `KeyboardEvent::is_composing` is hardcoded false (dead code), and
// dioxus-native does not expose `WindowEvent::Ime`'s Preedit to components — the
// component cannot perceive composition start/end itself, and key presses
// during composition (including Enter) would go straight to editing commands
// and split blocks. The **host** can only collect Ime events at the winit layer
// and call the public entry points below to drive the flag:
// `Ime::Preedit(s, Some(_))` with s non-empty → `set_composing(true)`;
// `Ime::Preedit(_, None)` / `Ime::Commit(_)` → `set_composing(false)`.

/// The IME-composing flag (process-global: only one editing surface composes at
/// a time).
static COMPOSING: AtomicBool = AtomicBool::new(false);

/// Set/clear the "IME composing" flag. The host calls it from winit
/// `WindowEvent::Ime`'s Preedit/Commit branches (see the module doc above);
/// the component's keyboard guard consumes it via [`is_composing`].
pub fn set_composing(on: bool) {
    COMPOSING.store(on, Ordering::Relaxed);
}

/// Whether an IME composition is in progress (for the `handle_keydown` guard).
pub fn is_composing() -> bool {
    COMPOSING.load(Ordering::Relaxed)
}

/// The choke point for Ctrl+click link opening: passes the same
/// `safe_link_url` whitelist as the preview (http/https/mailto/relative/drive
/// paths; rejects control characters and javascript:/data: etc.).
/// Returns `None` for anything off the whitelist; the caller silently does not
/// open it — edit and preview are two outlets that must share one policy.
fn open_link_target(url: &str) -> Option<String> {
    crate::preview::safe_link_url(url)
}

fn handle_keydown(
    ctx: &mut Ctx<'_>,
    evt: &KeyboardEvent,
    on_open_link: &Option<EventHandler<String>>,
) {
    let key = evt.data.key();
    let mods = evt.data.modifiers();
    wys_log(&format!(
        "keydown key={:?} ctrl={} shift={}",
        key,
        mods.ctrl(),
        mods.shift()
    ));
    // IME composing: not handled (the hidden input-method channel takes over).
    // The vendor's `evt.data.is_composing()` is hardcoded false (dead code);
    // the composition state is driven by the host via [`set_composing`]
    // (Ime::Preedit/Commit). Check both, and let through if either is true —
    // otherwise key presses during composition go straight to Cmd::Enter and
    // split blocks.
    if is_composing() || evt.data.is_composing() {
        return;
    }
    let ctrl = mods.ctrl() || mods.meta();
    let shift = mods.shift();

    // ---- Ctrl combos ----
    if ctrl {
        if let Key::Character(ch) = &key {
            let c = ch.to_lowercase();
            match c.as_str() {
                "a" => {
                    evt.prevent_default();
                    ctx.apply_cmd(Cmd::SelectAll);
                }
                "c" => {
                    evt.prevent_default();
                    clipboard_copy(ctx.editor);
                }
                "x" => {
                    evt.prevent_default();
                    clipboard_copy(ctx.editor);
                    ctx.apply_cmd(Cmd::DeleteSelection);
                }
                "v" => {
                    evt.prevent_default();
                    if let Some(t) = clipboard_paste() {
                        ctx.apply_cmd(Cmd::Paste(t));
                    }
                }
                "z" => {
                    evt.prevent_default();
                    if shift {
                        ctx.editor.write().redo();
                    } else {
                        ctx.editor.write().undo();
                    }
                    let sel = ctx.editor.read().sel;
                    ctx.deferred_sync(sel);
                    let g = ctx.geom();
                    ctx.sync_overlays(&g);
                }
                "y" => {
                    evt.prevent_default();
                    ctx.editor.write().redo();
                    let sel = ctx.editor.read().sel;
                    ctx.deferred_sync(sel);
                    let g = ctx.geom();
                    ctx.sync_overlays(&g);
                }
                "b" => {
                    evt.prevent_default();
                    ctx.apply_cmd(Cmd::Wrap(WrapKind::Bold));
                }
                "i" => {
                    evt.prevent_default();
                    ctx.apply_cmd(Cmd::Wrap(WrapKind::Italic));
                }
                "k" => {
                    evt.prevent_default();
                    ctx.apply_cmd(Cmd::Link(String::new()));
                }
                "d" => {
                    evt.prevent_default();
                    ctx.apply_cmd(Cmd::Wrap(WrapKind::Del));
                }
                "e" => {
                    evt.prevent_default();
                    ctx.apply_cmd(Cmd::Wrap(WrapKind::Code));
                }
                "1" | "2" | "3" | "4" | "5" | "6" => {
                    evt.prevent_default();
                    ctx.apply_cmd(Cmd::SetHead(c.parse().unwrap_or(1)));
                }
                _ => {}
            }
            // Ctrl+char: bound ones handled, unbound ones are not inserted as
            // text either.
            return;
        }
        // Non-character Ctrl combos do **not** return here: Ctrl+←/→/Backspace/
        // Delete/Home/End must fall through to the navigation/editing keys
        // below. The old implementation unconditionally returned here, swallowing
        // those keys whole (Ctrl+Left/Right did nothing at all, Ctrl+Backspace
        // did not delete a word).
    }

    // ---- Navigation/editing keys ----
    match &key {
        Key::Enter => {
            evt.prevent_default();
            // Shift+Enter = soft line break within a block;
            // Enter's block-splitting semantics stay.
            ctx.apply_cmd(if shift { Cmd::SoftBreak } else { Cmd::Enter });
        }
        Key::Backspace => {
            evt.prevent_default();
            ctx.apply_cmd(if ctrl {
                Cmd::DeleteBackWord
            } else {
                Cmd::DeleteBack
            });
        }
        Key::Delete => {
            evt.prevent_default();
            ctx.apply_cmd(if ctrl {
                Cmd::DeleteForwardWord
            } else {
                Cmd::DeleteForward
            });
        }
        Key::Tab => {
            evt.prevent_default();
            ctx.apply_cmd(if shift { Cmd::Outdent } else { Cmd::Indent });
        }
        Key::Home | Key::End => {
            evt.prevent_default();
            let home = matches!(key, Key::Home);
            if ctrl {
                // Ctrl+Home/End = document start/end.
                let m = if home { Move::DocStart } else { Move::DocEnd };
                ctx.apply_cmd(if shift {
                    Cmd::MoveExtend(m)
                } else {
                    Cmd::Move(m)
                });
            } else if !ctx.move_line_edge(home, shift) {
                // Visual line geometry unavailable (empty block, etc.): fall
                // back to block start/end.
                let m = if home {
                    Move::BlockStart
                } else {
                    Move::BlockEnd
                };
                ctx.apply_cmd(if shift {
                    Cmd::MoveExtend(m)
                } else {
                    Cmd::Move(m)
                });
            }
        }
        Key::ArrowLeft | Key::ArrowRight => {
            evt.prevent_default();
            let left = matches!(key, Key::ArrowLeft);
            let m = match (left, ctrl) {
                (true, false) => Move::Left,
                (false, false) => Move::Right,
                (true, true) => Move::WordLeft,
                (false, true) => Move::WordRight,
            };
            ctx.apply_cmd(if shift {
                Cmd::MoveExtend(m)
            } else {
                Cmd::Move(m)
            });
        }
        Key::ArrowUp | Key::ArrowDown => {
            evt.prevent_default();
            let up = matches!(key, Key::ArrowUp);
            move_visual_cmd(ctx, up, shift);
        }
        Key::Character(text) => {
            if text.is_empty() {
                return;
            }
            evt.prevent_default();
            ctx.apply_cmd(Cmd::InsertText(text.clone()));
        }
        _ => {}
    }
    let _ = on_open_link;
}

/// Visual-line movement (↑/↓; parley geometry + in-block top/bottom fallback
/// across block boundaries). Shared by the component's onkeydown and the host
/// bridge [`host_move_visual`].
///
/// The target column (h_pos) is chained through `Editor.h_pos`: while
/// continuously pressing ↑/↓ the column is kept across short lines / empty
/// paragraphs; any other command (`Editor::apply`) and clicks clear it. On
/// landing in an empty block (no lines, all geometry queries None) the old
/// value is kept — continuing to press ↑/↓ still uses the original column.
fn move_visual_cmd(ctx: &mut Ctx<'_>, up: bool, extend: bool) {
    let p = ctx.editor.read().sel.focus;
    let saved = ctx.editor.read().h_pos;
    // ↑/↓ are hot keys; the old implementation cloned the whole Doc (deep
    // copying all block Strings) just to get the block count and the fallback
    // target — changed to scalars + a short-read guard, borrowing &Doc only for
    // the fallback.
    let (nblocks, is_td) = {
        let ed = ctx.editor.read();
        (
            ed.doc.blocks.len(),
            ed.doc.blocks.get(p.block).is_some_and(|b| b.is_td()),
        )
    };
    // The target column used for this move: the chained memory first, otherwise
    // take it from the current caret geometry.
    let g = ctx.geom();
    // A live selection collapses to its edge instead of moving one visual line
    // further: the kernel's `move_caret` does this for ←/→/Ctrl+←/→, and ↑/↓ must
    // match (they never reach `move_caret`). Moving on from the focus with the
    // anchor kept threw the selection away from the end the user was looking at.
    // A table cell is the exception: Tab selects the whole cell, and the first ↓
    // must still walk down the column (Excel-like), not collapse onto the cell's
    // last character.
    if !extend && !is_td {
        let sel = ctx.editor.read().sel;
        if !sel.collapsed() {
            let (start, end) = sel.range();
            let target = Sel::caret(if up { start } else { end });
            {
                let mut ed = ctx.editor.write();
                ed.h_pos = None;
                ed.sel = target;
            }
            ctx.sync_overlays(&g);
            ctx.apply_sel_to_blitz(target);
            return;
        }
    }
    let h = saved.or_else(|| g.h_pos(p));
    // Up/Down inside a table follow **column semantics**: move across rows by
    // column first; at the table boundary exit the table and land on the
    // before/after block. Must not use parley's visual lines — adjacent blocks
    // in the same table are the next cell of the same row, so visual movement
    // treats them as "the next block" (observed: pressing ↓ in cell b landed on
    // c instead of the same-column d).
    let td_target = if is_td {
        let ed = ctx.editor.read();
        crate::edit::table_cell_vertical(&ed.doc, p, up)
    } else {
        None
    };
    let in_table_nav = td_target.is_some();
    let target = if let Some(t) = td_target {
        Some(t)
    } else {
        g.move_visual(p, up, h).or_else(|| {
            // Td: exiting the cell moves across rows by column — an adjacent
            // block in the same table is the previous cell of the same row, so
            // using its geometry is column-semantic misalignment; hand it to
            // cross_block_fallback's Td special case.
            if is_td {
                return None;
            }
            // Cross-block: land on the previous block's last line / next
            // block's first line **in the same column** (h_pos) — the old
            // implementation jumped straight to the previous block's end / next
            // block's start, losing the column position (review residual).
            // Document boundaries still stand still via cross_block_fallback.
            let h = h?;
            if up {
                if p.block == 0 {
                    return None;
                }
                g.move_visual_cross(p.block - 1, true, h)
            } else {
                if p.block + 1 >= nblocks {
                    return None;
                }
                g.move_visual_cross(p.block + 1, false, h)
            }
        })
    };
    let target = target.unwrap_or_else(|| {
        let ed = ctx.editor.read();
        cross_block_fallback(p, &ed.doc, up)
    });
    if target == p {
        return;
    }
    // Remember the target column: remember **the h used for the move** (the
    // landing point may be further left due to a short line, or may sit on an
    // empty block whose geometry is unqueryable — using the landing column
    // would break the chain into 0). When h is None (all geometry unavailable)
    // keep the old value; it is rebuilt from the landing point next time.
    if !in_table_nav {
        if let Some(h) = h {
            ctx.editor.write().h_pos = Some(h);
        }
    }
    if extend {
        ctx.editor.write().sel.focus = target;
    } else {
        ctx.editor.write().sel = Sel::caret(target);
    }
    ctx.sync_overlays(&g);
    let sel = ctx.editor.read().sel;
    ctx.apply_sel_to_blitz(sel);
}

/// The cross-block fallback target (pure function, unit-testable): when
/// geometry queries are unavailable, land on the **previous block's end / next
/// block's start** (the normal path is same-column cross-block, see
/// `Geom::move_visual_cross`).
/// Document boundaries must stand still — the first block has no block above
/// it and the last block none below; the old code unconditionally computed
/// `p.block ± 1` then clamped, so ↑ on block 0 yielded "block 0's block end"
/// and the caret jumped to the paragraph end out of nowhere (found in review).
///
/// Td special case: up/down inside a table move **across rows by column**, not
/// to "the previous block's end / next block's start" — an adjacent block in
/// the same table is the previous cell of the same row, and column semantics
/// would be lost.
fn cross_block_fallback(p: Pos, d: &Doc, up: bool) -> Pos {
    if let Some(t) = crate::edit::table_cell_vertical(d, p, up) {
        return t;
    }
    if up {
        if p.block == 0 {
            return p;
        }
        let prev = p.block - 1;
        Pos::new(prev, d.blocks.get(prev).map_or(0, |b| b.plain_text().len()))
    } else {
        if p.block + 1 >= d.blocks.len() {
            return p;
        }
        Pos::new(p.block + 1, 0)
    }
}

// ---- Host bridge (the host's composer routes key presses) -------------------------------
//
// The host's only global DOM focus is the composer's transparent textarea (see
// the module doc of the host's components::composer); key presses are rerouted
// to the wys editor via the host's keyboard routing.
// The three entry points below are **verbatim-semantics-identical** to the
// component's own onkeydown branches: all are called at event time (the
// document is borrowable so the synchronous geometry is exact), and document
// changes go through deferred_sync for deferred restore.

/// Host path: apply a command (equivalent to the component onkeydown's
/// apply_cmd branch).
pub fn host_apply_cmd(
    handle: Rc<NodeHandle>,
    editor: &mut Signal<Editor>,
    overlays: &mut Signal<Overlays>,
    cmd: Cmd,
) -> bool {
    let mut ctx = Ctx {
        editor,
        overlays,
        handle,
        on_edit: None,
        on_caret: None,
        grab: false,
    };
    ctx.apply_cmd(cmd)
}

/// Host path: visual-line movement (↑/↓). `extend` = Shift held (extend the
/// selection).
pub fn host_move_visual(
    handle: Rc<NodeHandle>,
    editor: &mut Signal<Editor>,
    overlays: &mut Signal<Overlays>,
    up: bool,
    extend: bool,
) {
    let mut ctx = Ctx {
        editor,
        overlays,
        handle,
        on_edit: None,
        on_caret: None,
        grab: false,
    };
    move_visual_cmd(&mut ctx, up, extend);
    let h = ctx.handle.clone();
    let focus = ctx.editor.read().sel.focus;
    caret_into_view(&h, focus);
}

/// Host path: the outline's document-view jump — place the caret at the start
/// of `block`, push the selection back and scroll the block into view.
///
/// Writing the model `sel` alone is invisible: without the sync the caret
/// overlay never moves, blitz's native selection is untouched and the viewport
/// stays where it was — an outline click then looks dead (the report that led
/// here). Same tail as every kernel move: sync → push → scroll.
pub fn host_goto_block(
    handle: Rc<NodeHandle>,
    editor: &mut Signal<Editor>,
    overlays: &mut Signal<Overlays>,
    block: usize,
) -> bool {
    if editor.read().doc.blocks.get(block).is_none() {
        return false;
    }
    let target = Pos::new(block, 0);
    let mut ctx = Ctx {
        editor,
        overlays,
        handle,
        on_edit: None,
        on_caret: None,
        grab: false,
    };
    {
        let mut ed = ctx.editor.write();
        ed.sel = Sel::caret(target);
        ed.h_pos = None;
    }
    let g = ctx.geom();
    ctx.sync_overlays(&g);
    let sel = ctx.editor.read().sel;
    ctx.apply_sel_to_blitz(sel);
    caret_into_view(&ctx.handle, target);
    true
}

/// Host path: scroll the wys content by `dy` px (positive = reveal **later**
/// rows, i.e. the content moves up). Returns whether the offset actually moved
/// — `false` at either end of the document.
///
/// The component's root scrolls natively (`.wys-content` has `overflow:auto`,
/// blitz owns the offset), so a programmatic scroll has to go through the same
/// API `caret_into_view` uses.
pub fn host_scroll_by(handle: Rc<NodeHandle>, dy: f64) -> bool {
    let Some(doc) = handle.try_doc() else {
        return false;
    };
    let node_id = handle.node_id();
    let before = doc.get_node(node_id).map(|n| n.scroll_offset().y);
    drop(doc);
    let Some(before) = before else { return false };
    let mut doc = handle.doc_mut();
    doc.scroll_by(node_id, 0.0, dy, blitz_dom::ScrollBehavior::Instant);
    let after = doc
        .get_node(node_id)
        .map(|n| n.scroll_offset().y)
        .unwrap_or(before);
    (after - before).abs() > 0.5
}

/// Host path: move a **drag selection's** moving end to `(x, y)` (client CSS
/// px, already clamped into the content box by the caller).
///
/// The anchor — the press position — is left alone: this is the extension half
/// of a drag, and the model `sel` is the single source of truth (the host reads
/// blitz's selection back only on release; see the component's `onpointerup`).
/// The whole kernel tail runs (sync → push), so the caret overlay and blitz's
/// native highlight both follow.
pub fn host_drag_extend(
    handle: Rc<NodeHandle>,
    editor: &mut Signal<Editor>,
    overlays: &mut Signal<Overlays>,
    x: f32,
    y: f32,
) -> bool {
    let mut ctx = Ctx {
        editor,
        overlays,
        handle,
        on_edit: None,
        on_caret: None,
        grab: false,
    };
    let p = {
        let ed = ctx.editor.read();
        let g = Geom::new(ctx.handle.clone());
        match g.pos_at(x, y, &ed.doc) {
            Some(p) => p,
            None => return false,
        }
    };
    ctx.editor.write().sel.focus = p;
    let g = ctx.geom();
    ctx.sync_overlays(&g);
    let sel = ctx.editor.read().sel;
    ctx.apply_sel_to_blitz(sel);
    true
}

/// Host path: Home/End's visual line start/end. Returns false = geometry
/// unavailable; the caller falls back to block start/end (consistent with the
/// component onkeydown's branch).
pub fn host_move_line_edge(
    handle: Rc<NodeHandle>,
    editor: &mut Signal<Editor>,
    overlays: &mut Signal<Overlays>,
    to_start: bool,
    extend: bool,
) -> bool {
    let mut ctx = Ctx {
        editor,
        overlays,
        handle,
        on_edit: None,
        on_caret: None,
        grab: false,
    };
    ctx.move_line_edge(to_start, extend)
}

/// Host path: sync after operations like undo/redo that **change the doc
/// directly without going through apply_cmd** (the document has changed:
/// deferred caret/selection restore + immediate overlay recompute — consistent
/// with the component's Ctrl+Z branch).
pub fn host_resync(
    handle: Rc<NodeHandle>,
    editor: &mut Signal<Editor>,
    overlays: &mut Signal<Overlays>,
) {
    let mut ctx = Ctx {
        editor,
        overlays,
        handle,
        on_edit: None,
        on_caret: None,
        grab: false,
    };
    let sel = ctx.editor.read().sel;
    ctx.deferred_sync(sel);
    let g = ctx.geom();
    ctx.sync_overlays(&g);
}

/// Clipboard copy (model selection → Markdown source).
///
/// Copying the *source* keeps the formatting when the text lands in another
/// Markdown document: a table stays a table, `**bold**` stays bold, a display
/// formula stays `$$…$$`. A partial inline selection is re-emitted too (see
/// [`Editor::selected_markdown`]); only a selection with nothing structural falls
/// back to plain text.
fn clipboard_copy(editor: &Signal<Editor>) {
    let text = editor.read().selected_markdown();
    if text.is_empty() {
        return;
    }
    let _clip = platform::clipboard::lock();
    if let Ok(mut cb) = arboard::Clipboard::new() {
        let _ = cb.set_text(text);
    }
}

/// Clipboard paste.
fn clipboard_paste() -> Option<String> {
    let _clip = platform::clipboard::lock();
    arboard::Clipboard::new().ok()?.get_text().ok()
}

/// Convenience API: parse Markdown and use it as the controlled initial
/// document.
pub fn parse_doc(markdown: &str) -> Doc {
    md::parse(markdown)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Sel;

    #[test]
    fn cross_block_fallback_respects_document_edges() {
        let d = crate::md::parse("第一段\n\n第二段\n\n第三段");
        assert_eq!(d.blocks.len(), 3);
        let first_mid = Pos::new(0, 1);
        let last_mid = Pos::new(2, 1);
        // The first block at the top: ↑ stands still (used to jump to the block
        // end)
        assert_eq!(cross_block_fallback(first_mid, &d, true), first_mid);
        // The last block at the bottom: ↓ stands still (used to jump to the block
        // start)
        assert_eq!(cross_block_fallback(last_mid, &d, false), last_mid);
        // A middle block crosses normally: ↑ lands at the previous block's end,
        // ↓ lands at the next block's start
        let mid = Pos::new(1, 0);
        assert_eq!(
            cross_block_fallback(mid, &d, true),
            Pos::new(0, "第一段".len())
        );
        assert_eq!(cross_block_fallback(mid, &d, false), Pos::new(2, 0));
        // Single-block document: both directions stand still
        let one = crate::md::parse("only");
        let p = Pos::new(0, 2);
        assert_eq!(cross_block_fallback(p, &one, true), p);
        assert_eq!(cross_block_fallback(p, &one, false), p);
        // Unrelated field conservation: the return value only moves (block, off)
        let _ = Sel::caret(p);
    }

    #[test]
    fn cross_block_fallback_lands_in_empty_paragraphs() {
        // Empty paragraphs are legitimate docking points for vertical movement:
        // ↓ passing through an empty paragraph must stop in it (the fallback
        // lands at the next block's start = off 0); ↑ from an empty paragraph
        // lands at the previous block's end (an empty block's off is 0).
        // When the target block's geometry queries all returned None, the
        // fallback was also this pure-logic path; here we lock its behavior on
        // an empty-block document.
        let d = crate::md::parse("above");
        let mut d = d;
        d.blocks.insert(1, crate::model::Block::Para(vec![]));
        d.blocks.push(crate::model::Block::Para(vec![]));
        let above_end = Pos::new(0, 5);
        assert_eq!(cross_block_fallback(above_end, &d, false), Pos::new(1, 0));
        assert_eq!(cross_block_fallback(Pos::new(1, 0), &d, true), above_end);
        assert_eq!(
            cross_block_fallback(Pos::new(1, 0), &d, false),
            Pos::new(2, 0)
        );
        // Document-tail empty paragraph: ↓ stands still
        assert_eq!(
            cross_block_fallback(Pos::new(2, 0), &d, false),
            Pos::new(2, 0)
        );
    }

    /// Up/down inside a table move across rows by column (not "the previous
    /// block's end / next block's start" — an adjacent block in the same table
    /// is the previous cell of the same row); at the table boundary, exit the
    /// table and land on the before/after block.
    #[test]
    fn cross_block_fallback_in_table_moves_by_column() {
        let d = crate::md::parse("pre\n\n| h1 | h2 |\n|---|---|\n| a | b |\n| c | d |\n\npost");
        // Block order: 0=pre, 1..=6=table (h1 h2 / a b / c d), 7=post
        let td_count = d.blocks.iter().filter(|b| b.is_td()).count();
        assert_eq!(td_count, 6, "2 columns × 3 rows: {:?}", d.blocks);
        // Cell b (row1 col1) ↑ → h2 (same column, previous row), ↓ → d (same
        // column, next row)
        let b = Pos::new(4, 1);
        assert_eq!(cross_block_fallback(b, &d, true), Pos::new(2, 1));
        assert_eq!(cross_block_fallback(b, &d, false), Pos::new(6, 1));
        // ↑ from the table's first row exits to the previous block's end; ↓ from
        // the table's last row exits to the next block's start
        assert_eq!(
            cross_block_fallback(Pos::new(1, 0), &d, true),
            Pos::new(0, "pre".len())
        );
        assert_eq!(
            cross_block_fallback(Pos::new(6, 0), &d, false),
            Pos::new(7, 0)
        );
    }

    #[test]
    fn editor_h_pos_chain_lifecycle() {
        // Vertical column memory: apply (any non-vertical command) clears it;
        // clone preserves it.
        let mut ed = crate::edit::Editor::new(crate::md::parse("hello"));
        ed.h_pos = Some(12.0);
        ed.apply(crate::edit::Cmd::Move(crate::edit::Move::Right));
        assert!(ed.h_pos.is_none());
        ed.h_pos = Some(7.5);
        let cloned = ed.clone();
        assert_eq!(cloned.h_pos, Some(7.5));
        ed.set_doc(crate::md::parse("world"));
        assert!(ed.h_pos.is_none());
    }

    /// The whitelist choke point before Ctrl+click opens a link —
    /// http/https/mailto/relative/drive paths pass, javascript:/data:/vbscript:
    /// and control-character bypasses (`java\tscript:`) are rejected.
    #[test]
    fn open_link_target_enforces_scheme_whitelist() {
        assert_eq!(
            open_link_target("https://x.y/a").as_deref(),
            Some("https://x.y/a")
        );
        assert_eq!(
            open_link_target("http://x.y").as_deref(),
            Some("http://x.y")
        );
        assert_eq!(
            open_link_target("mailto:a@b.c").as_deref(),
            Some("mailto:a@b.c")
        );
        assert_eq!(open_link_target("./rel.md").as_deref(), Some("./rel.md"));
        assert!(open_link_target("javascript:alert(1)").is_none());
        assert!(open_link_target("JAVASCRIPT:alert(1)").is_none());
        assert!(open_link_target("data:text/html,x").is_none());
        assert!(open_link_target("vbscript:x").is_none());
        assert!(open_link_target("java\tscript:alert(1)").is_none());
    }

    /// The composing flag is driven by the host (winit Ime events) and consumed
    /// by the keyboard guard.
    #[test]
    fn composing_flag_roundtrip() {
        set_composing(true);
        assert!(is_composing());
        set_composing(false);
        assert!(!is_composing());
    }
}
