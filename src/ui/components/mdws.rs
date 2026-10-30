//! Markdown WYSIWYG mode: the **host wiring** of the [`crates/wysiwyg`](wysiwyg) component.
//!
//! Design notes live in `crates/wysiwyg/README.md`: infeditor's md Tab routes to the
//! `Wysiwyg` component with `set_value`/`value` as the bridge. This module is that bridge:
//!
//! - **Model side**: the wys editor (block tree + undo + caret) is the content authority in wys
//!   mode, living in the global [`WYS`] signal. **After an edit** the content is emitted as
//!   Markdown and written back to the piece table — dirty flags, autosave, recovery logs and
//!   Ctrl+S all keep working, with zero migration on leaving the mode. The write-back goes through
//!   [`note_edit`] / [`flush_pending`] with **idle debouncing** (key bursts fuse into one
//!   emit+diff per input run; save / mode switch / tab switch force a flush), and cross-tab
//!   switches are re-seeded under [`guard_seed`]'s validation, so another tab's content can never
//!   be synced into this document.
//! - **Keyboard side**: infeditor's single global DOM focus is the composer's transparent
//!   textarea (see the `components::composer` module comment); key presses reroute through the
//!   composer here ([`wys_chord_key`] / [`wys_key`]) and then through the `wysiwyg::host_*`
//!   entries to apply commands — word-for-word the same semantics as the component's own
//!   onkeydown. The component mounts with `grab_focus: false` and **must not** steal focus, or
//!   every key press would trigger a host onblur → swap node → autofocus loop.
//! - **Mouse side**: click-to-position / drag-select / double-click word / checkbox stay with the
//!   component's own events (the composer textarea lets the mouse through to the wys view).
//!
//! The caret/cell overlay (`OVERLAYS`) is **shared** by the component and the composer path:
//! both advance it.

use std::rc::Rc;

use dioxus_native::prelude::*;
use dioxus_native::NodeHandle;

use std::path::Path;

use wysiwyg::{Cmd as WysCmd, Editor as WysEditor, Move as WysMove, WrapKind};

use super::{mdhint, mdselbar};
use crate::ui::state::EditorState;
use crate::ui::{Fmt, Msg};

/// The wys editor (model + undo + caret). A V1 global singleton; surviving a tab switch falls
/// back on [`MdWys`]'s mount-time consistency guard (re-seed whenever the document or content
/// changed).
pub static WYS: GlobalSignal<WysEditor> = Signal::global(WysEditor::default);

/// Identity of the document currently owned by [`WYS`].
///
/// Content equality alone is **not** document identity: two tabs can hold identical text while
/// their undo stacks differ. Without this id, switching between such tabs kept the previous
/// tab's undo history, and a Ctrl+Z could write an old snapshot into the new document. `0` means
/// "no session has been seeded yet".
static WYS_DOC_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// The wys root element's NodeHandle: the composer keyboard path's front door for geometry
/// queries (↑/↓ visual rows, pushing the selection back). Written by the component's `on_ready`
/// callback.
static HANDLE: GlobalSignal<Option<Rc<NodeHandle>>> = Signal::global(|| None);

/// The caret / focused-cell overlay (content-coordinate rects relative to `.wys-root`). Shared
/// by the component's render and the composer keyboard path; the two always advance together
/// inside one focus change, so they are one signal (see `wysiwyg::Overlays`).
static OVERLAYS: GlobalSignal<wysiwyg::Overlays> = Signal::global(wysiwyg::Overlays::default);

/// Failed-image set version: blitz has no image load event, so `overlays_frame` pulls failed
/// URLs from the document's `failed_images` into the render layer (alt placeholders) and bumps
/// this signal to make `MdWys` re-render the fragment.
static IMG_EPOCH: GlobalSignal<u64> = Signal::global(|| 0u64);

/// Host-edit version: the host keyboard bridge mutates the kernel's `WYS` global signal, and in
/// some paths the component's subscription does not mark `MdWys` dirty immediately (in practice:
/// after Enter the new block lingered outside the DOM). Each host edit bumps this signal
/// explicitly and `MdWys` reads it while rendering — guaranteeing the next frame re-renders the
/// wys subtree.
static HOST_EPOCH: GlobalSignal<u64> = Signal::global(|| 0u64);

/// A host edit awaits re-render (set in the event phase, consumed by the frame hook). Bumping
/// the signal **directly in the event phase does not work**: writes inside winit's event
/// dispatch are cleared when that dispatch round ends, and the next frame's dioxus poll never
/// sees the dirty scope (observed: after Enter the new block never rendered). Writing it in
/// `overlays_frame` (RedrawRequested, before blitz's polling) is what works.
static NEEDS_RENDER: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// The signature of the last synced failed-image set (see `Document::failed_image_sig`).
///
/// A count cannot serve as change detection: two tabs failing on the same number of images with
/// different URLs is routine, and after switching tabs the render layer would keep the previous
/// document's failure set, drawing images that are successfully loading as alt placeholders.
static IMG_FAILED_SIG: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

// ---- model bridge (seed / sync) ---------------------------------------------

/// Seed the active tab's document text into the global wys editor (called when entering wys
/// mode, in the event phase). Caret and undo stack reset — the V1 trade-off.
pub fn seed_from_doc(st: &EditorState) {
    // Re-seeding replaces the source of truth: a pending write-back from the previous
    // document must not fire afterwards and overwrite what we just seeded.
    PENDING.store(false, std::sync::atomic::Ordering::Relaxed);
    let text = active_text(st);
    let mut ed = WysEditor::new(wysiwyg::parse_doc(&text));
    restore_caret(st, &text, &mut ed);
    // The source expansion's block-height floors were measured on the **previous**
    // document; they are indexed by block number, so carrying them over pads blocks of
    // the new one (a short paragraph under a tall table kept the table's height).
    // `Editor::set_doc` clears them, but a tab switch replaces the editor wholesale.
    wysiwyg::render::clear_height_floors();
    *WYS.signal().write() = ed;
    // Bind the session to the document identity, not to its current text: another tab may
    // legitimately hold the same text with a different undo history.
    WYS_DOC_ID.store(active_doc_id(st), std::sync::atomic::Ordering::Relaxed);
    // Undo/redo availability resets along with it (host-side counting, see [`can_redo`]).
    REDO_DEPTH.store(0, std::sync::atomic::Ordering::Relaxed);
    // The overlays (hint candidate panel / selection floating toolbar / cell highlight) reset
    // with the seed — a mode or tab switch never carries old panel state into the new session.
    mdhint::close();
    mdselbar::reset();
    *OVERLAYS.signal().write() = wysiwyg::Overlays::default();
}

/// Map the host caret into the freshly seeded wys model.
///
/// Entering wys used to start from `sel = (0, 0)` — the "caret resets on entry" V1
/// trade-off. The visible symptom: type something (a trailing space is enough),
/// switch to the text view to check, switch back and the next keystroke lands at
/// the beginning of the document. `block_offsets` is the same emit-coordinate map
/// [`align_caret`] walks in the opposite direction.
fn restore_caret(st: &EditorState, src: &str, ed: &mut WysEditor) {
    let blocks = ed.doc.blocks.len();
    if blocks == 0 {
        return;
    }
    let caret = (st.caret().pos as usize).min(src.len());
    // Doc bytes → emit bytes: a CRLF in the piece table is one `\n` in emit (and in
    // the block offsets).
    let mut emit_pos = caret;
    let head = &src.as_bytes()[..caret];
    if head.contains(&b'\r') {
        emit_pos -= head
            .windows(2)
            .filter(|w| w[0] == b'\r' && w[1] == b'\n')
            .count();
    }
    let offsets = wysiwyg::md::block_offsets(&ed.doc);
    // The block whose start is nearest before the caret. A caret sitting on a block
    // separator line (or past the end after normalization) snaps to the previous
    // block's end.
    let block = offsets
        .iter()
        .rposition(|o| *o <= emit_pos)
        .unwrap_or(0)
        .min(blocks - 1);
    let plain = ed.doc.blocks[block].plain_text();
    let local = emit_pos.saturating_sub(offsets.get(block).copied().unwrap_or(0));
    let off = wysiwyg::model::floor_boundary(&plain, local);
    ed.sel = wysiwyg::model::Sel::caret(wysiwyg::model::Pos::new(block, off));
}

/// Emit the wys editor's current content as Markdown (the external-read entry for save /
/// recovery and friends).
pub fn emit_markdown() -> String {
    wysiwyg::md::emit(&WYS.read().doc)
}

/// Cross-tab consistency guard: the active tab is in wys mode while the global wys singleton's
/// content does not match the document (a stale singleton left from another tab) → re-seed.
/// **Called after tab activation changes** (`EditorState::set_active_tab`); MdWys's `on_ready`
/// guard at mount is its second layer (the key forces a remount).
pub fn guard_seed(st: &mut EditorState) {
    if !st.tabs.get(st.active).is_some_and(|t| t.md_wys) {
        return;
    }
    // A tab switch can land on a document with the same text but a different undo stack.
    // The session identity is the document id, never the text.
    if WYS_DOC_ID.load(std::sync::atomic::Ordering::Relaxed) != active_doc_id(st) {
        seed_from_doc(st);
        return;
    }
    if content_eq(&active_text(st), &emit_markdown()) {
        return;
    }
    seed_from_doc(st);
}

/// Whether the live wys session belongs to the active document.
pub fn session_matches_doc(st: &EditorState) -> bool {
    WYS_DOC_ID.load(std::sync::atomic::Ordering::Relaxed) == active_doc_id(st)
}

/// Logical equality: emit's `\n` and the document's `\r\n` count as the same shape. Shared by the
/// cross-tab guard / mount guard / write-back reconciliation — otherwise a CRLF document would be
/// judged "content mismatched" on every tab switch and remount, throwing away the undo stack and
/// caret for nothing.
fn content_eq(doc_text: &str, md: &str) -> bool {
    md == doc_text || crlf_eq(doc_text, md)
}

// ---- sync debouncing --------------------------------------------------------
//
// Emitting + materializing the whole document + diffing the whole document on every keystroke is
// two O(N) copies per key on multi-MB documents (unusable above 10MB). The keystroke path only
// accounts ([`note_edit`]); [`SYNC_DEBOUNCE_MS`] of idleness later a one-shot timer thread wakes
// (`wake` = request_redraw), and the actual write-back happens in `RedrawRequested` via
// [`flush_if_due`]. Paths that **must** hit disk — save / mode switch / tab switch / close — go
// through [`flush_pending`], which flushes unconditionally, not bounded by the window.

/// The idle debounce window (ms): write back to the piece table only this long after typing
/// stops.
const SYNC_DEBOUNCE_MS: u64 = 250;

/// A wys edit has not yet been written back to the piece table.
static PENDING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// The moment of the most recent edit (epoch ms).
static LAST_EDIT_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// The timer wake thread is armed (at most one at a time).
static ARMED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// Host-side count of the available redo depth (the kernel exposes no `can_redo`; undo/redo only
/// report success — see the accounting in [`undo`] / [`redo`]).
static REDO_DEPTH: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Debounce-due check (pure function, unit-testable): write back only after a full **idle**
/// window since the last edit.
/// A key burst only postpones `last` and never fires early — that is how several edits fuse into
/// one sync.
fn debounce_due(now: u64, last: u64) -> bool {
    now.saturating_sub(last) >= SYNC_DEBOUNCE_MS
}

/// The throttled entry after a wys keystroke (replacing per-key [`sync_to_doc`]): record the
/// edit time and arm the one-shot timer thread. The thread steps in 50ms increments until it has
/// been **idle** for a full window since the last edit, then wakes — what it waits for is
/// *idleness*, not a fixed delay, so it postpones itself automatically during a key burst.
pub fn note_edit(st: &EditorState) {
    use std::sync::atomic::Ordering;
    PENDING.store(true, Ordering::Relaxed);
    LAST_EDIT_MS.store(now_ms(), Ordering::Relaxed);
    if ARMED.swap(true, Ordering::Relaxed) {
        return;
    }
    let Some(wake) = st.wake.clone() else {
        // No waker (headless test environment): arm no thread; rely on the caller's forced flush
        // as the fallback.
        ARMED.store(false, Ordering::Relaxed);
        return;
    };
    std::thread::spawn(move || {
        // A panic in the wake path must not kill the process; the arm flag is reset
        // outside the guard so the debounce can re-arm even after a failed run.
        crate::ui::guard::worker("ui::mdws", move || {
            for _ in 0..400 {
                std::thread::sleep(std::time::Duration::from_millis(50));
                if !PENDING.load(std::sync::atomic::Ordering::Relaxed) {
                    break;
                }
                let last = LAST_EDIT_MS.load(std::sync::atomic::Ordering::Relaxed);
                if debounce_due(now_ms(), last) {
                    wake();
                    break;
                }
            }
        });
        ARMED.store(false, std::sync::atomic::Ordering::Relaxed);
    });
}

/// Drop a pending write-back without touching either document.
///
/// Used when the tab set is rebuilt underneath the global singleton (session
/// restore, in-place reload): the singleton still holds the *old* document's
/// content, so flushing it would overwrite the freshly restored/reloaded text.
/// The caller then lets [`guard_seed`] re-seed from the new document.
pub fn discard_pending() {
    PENDING.store(false, std::sync::atomic::Ordering::Relaxed);
}

/// **Force**-flush the wys content back to the piece table (called before save / mode switch /
/// tab switch / close).
///
/// It ignores `PENDING`: mouse edits (checkbox checks etc., handled by the component itself) do
/// not pass through [`note_edit`], so with the flag false WYS may still lead the piece table —
/// the disk path must reconcile unconditionally (when content matches [`sync_to_doc`] returns
/// with zero writes, costing a single emit).
pub fn flush_pending(st: &mut EditorState) {
    PENDING.store(false, std::sync::atomic::Ordering::Relaxed);
    if st.tabs.get(st.active).is_some_and(|t| t.md_wys) {
        sync_to_doc(st);
    }
}

/// The debounce-due check inside `RedrawRequested`: flush only when **idle for a full window**.
pub fn flush_if_due(st: &mut EditorState) {
    if !PENDING.load(std::sync::atomic::Ordering::Relaxed) {
        return;
    }
    let last = LAST_EDIT_MS.load(std::sync::atomic::Ordering::Relaxed);
    if !debounce_due(now_ms(), last) {
        return;
    }
    flush_pending(st);
}

/// The read-only probe of [`flush_if_due`] (probe/fix early-exit branches are word-for-word
/// identical, the iron rule of `state::heal`): the frame hook uses it to decide whether taking a
/// `Signal::write()` is worth it. In steady state (no pending changes) it is always false — so
/// idle frames do zero writes and zero dirty marks.
pub fn flush_pending_due() -> bool {
    if !PENDING.load(std::sync::atomic::Ordering::Relaxed) {
        return false;
    }
    let last = LAST_EDIT_MS.load(std::sync::atomic::Ordering::Relaxed);
    debounce_due(now_ms(), last)
}

/// Flush only when a debounced change is pending (the cheap gate before an external command
/// reads the piece table — find / copy-all / word count must see the current content). Mouse
/// edits skip [`note_edit`], so disk paths still use the unconditional [`flush_pending`].
pub fn flush_if_pending(st: &mut EditorState) {
    if PENDING.load(std::sync::atomic::Ordering::Relaxed) {
        flush_pending(st);
    }
}

/// Write the wys content back to the piece table (returning with zero writes when unchanged) and
/// align the hidden caret to the wys focus (`block_offsets + sel.focus.off`, feeding only the
/// status bar / scroll restoration).
/// **Called at the end of every edit run (when the debounce is due) / on disk paths.**
///
/// The write-back is a **minimal-diff replace**: only the middle span between the common
/// prefix/suffix goes to the piece table — so the text view's undo granularity steps into line
/// with wys edits (replacing the whole document would turn every edit into one giant undo step).
/// `old` must be materialized in full: it is the reconciliation baseline for the common prefix /
/// suffix, and the "skip materializing when lengths differ" fast path never existed (a former
/// untrue comment).
///
/// Line endings are handled by `crlf_eq` / `preserve_eol`: the piece table preserves `\r\n` /
/// `\r` byte-for-byte while emit always sends `\n` — reconciling by raw bytes would treat
/// "equal but different shape" as an edit.
pub fn sync_to_doc(st: &mut EditorState) {
    let md = emit_markdown();
    let total = st.active_doc().total_bytes();
    // Equal lengths are a prerequisite for a zero diff: if the byte-by-byte reconciliation
    // agrees, return immediately (saving the replace_range).
    let old = st.active_doc().text_range(0..total);
    if md == old {
        align_caret(st, false);
        return;
    }
    // The piece table preserves bytes exactly (`\r\n` in a CRLF file), while emit always sends
    // `\n`. Judging equality by bytes alone would make the first sync of a "pure caret move" write
    // the entire document back as an "equal-but-different-shape" edit — the user would see
    // "modified" with identical content, and CRLF would be flattened to LF. EOL differences are
    // therefore excluded from what counts as an edit.
    if crlf_eq(&old, &md) {
        align_caret(st, false);
        return;
    }
    // A real edit: restore emit's `\n` into **each line's original ending** (only changed lines
    // take the document's dominant ending), then do the minimal-diff replace — so unmodified
    // lines in CRLF / CR / mixed-ending files stay byte-identical and only genuinely changed lines
    // take part in the replace.
    let new = if old.contains('\r') {
        preserve_eol(&old, &md, st.active_doc().newline())
    } else {
        md
    };
    if new == old {
        align_caret(st, false);
        return;
    }
    apply_sync(st, &new, &old, total);
}

/// Whether `old` equals `md` byte for byte after replacing its line endings with emit's `\n`
/// (allocates nothing).
///
/// - `\r\n` drops the `\r`;
/// - an isolated `\r` (old-Mac ending) counts as a line ending matching emit's `\n` — the
///   markdown parser treats CR as a terminator too, so this is the same logical content;
/// - every other byte compares literally.
fn crlf_eq(old: &str, md: &str) -> bool {
    if !old.as_bytes().contains(&b'\r') {
        return false;
    }
    let ob = old.as_bytes();
    let nb = md.as_bytes();
    let mut j = 0usize;
    for (i, &b) in ob.iter().enumerate() {
        let want = if b == b'\r' {
            if ob.get(i + 1) == Some(&b'\n') {
                continue; // the CRLF \r: two ending bytes folded into one
            }
            b'\n' // an isolated CR = a line ending
        } else {
            b
        };
        if nb.get(j) != Some(&want) {
            return false;
        }
        j += 1;
    }
    j == nb.len()
}

/// One line = (content, terminator). The terminator may be `\r\n` / `\n` / `\r` (old Mac) or empty
/// (the last line has no newline). Line breaks inside HTML block content are split into lines the
/// same way — emit passes them through verbatim, and per-line processing cannot mangle them (the
/// sliced content concatenates back to the original bytes).
fn split_lines(s: &str) -> Vec<(&str, &str)> {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut start = 0usize;
    let mut i = 0usize;
    while i < b.len() {
        match b[i] {
            b'\n' => {
                out.push((&s[start..i], &s[i..i + 1]));
                i += 1;
                start = i;
            }
            b'\r' => {
                let end = if b.get(i + 1) == Some(&b'\n') {
                    i + 2
                } else {
                    i + 1
                };
                out.push((&s[start..i], &s[i..end]));
                i = end;
                start = i;
            }
            _ => i += 1,
        }
    }
    // The last line (possibly empty content, no terminator) must join the comparison too: emit
    // always ends with a newline, and without this item a trailing "file ends with newline" would
    // be misjudged as a content diff.
    out.push((&s[start..], ""));
    out
}

/// Restore emit's output (whose endings are always `\n`) to the original's ending shape:
///
/// - lines whose content matches `old` **as-is** (the line-level common prefix/suffix) keep
///   `old`'s own terminator;
/// - changed/new lines take the document's dominant ending `default` (`CrLf` → `\r\n`, `Cr` →
///   `\r`);
/// - whether the file ends with a newline follows emit's shape (`md` ending with a newline adds
///   one dominant ending) — part of "normalizing the format on save": it must not follow the old
///   tail, or it would never converge.
///
/// This way the minority endings in a mixed-ending file are never conveniently unified, and a
/// lone-CR file does not turn into LF wholesale after an edit. Only when `old` contains `\r` is
/// this path needed at all (a pure-LF document has the same shape as emit and dereferences
/// directly, saving a full split — the large-file path).
fn preserve_eol(old: &str, md: &str, default: crate::buffer::Newline) -> String {
    // The trailing ("", "") is a synthetic entry for "the file ends with a newline", not line
    // content: keeping it would let empty lines mismatch against it (eating the blank line
    // between blocks when a segment is added). Strip it from both sides first; the final tail is
    // settled by emit's rules at the end.
    fn body(s: &str) -> Vec<(&str, &str)> {
        let mut v = split_lines(s);
        if v.last() == Some(&("", "")) {
            v.pop();
        }
        v
    }
    let o = body(old);
    let m = body(md);
    let min = o.len().min(m.len());
    let mut p = 0usize;
    while p < min && o[p].0 == m[p].0 {
        p += 1;
    }
    let mut s = 0usize;
    while s < min - p && o[o.len() - 1 - s].0 == m[m.len() - 1 - s].0 {
        s += 1;
    }
    let term = default.terminator();
    let mut out = String::with_capacity(md.len() + md.len() / 16);
    for (c, t) in &o[..p] {
        out.push_str(c);
        out.push_str(t);
    }
    // Changed/new lines: content comes from emit, endings take the document's dominant ending
    // (the source file's own CRLF / CR is never dragged along by emit's `\n`).
    for (c, t) in &m[p..m.len() - s] {
        out.push_str(c);
        if !t.is_empty() {
            out.push_str(term);
        }
    }
    for (c, t) in &o[o.len() - s..] {
        out.push_str(c);
        out.push_str(t);
    }
    // Whether the file ends with a newline is decided by emit (part of "normalizing the format
    // on save") and never follows the old tail — otherwise an original file without a trailing
    // newline could never converge to emit's shape.
    if md.ends_with('\n') && !out.ends_with('\n') && !out.ends_with('\r') {
        out.push_str(term);
    }
    out
}

fn apply_sync(st: &mut EditorState, md: &str, old: &str, total: u64) {
    let (prefix, suffix) = common_span(old, md);
    let mid = md[prefix..md.len() - suffix].to_string();
    st.tab_mut()
        .doc
        .replace_range(prefix as u64..(total - suffix as u64), &mid);
    align_caret(st, true);
}

/// Align the hidden caret to the wys focus (`block_offsets + sel.focus.off`, feeding only the
/// status bar / scroll restoration). `clear_anchor` is set only when content was really replaced —
/// a pure move never touches the selection.
///
/// The zero-write path must call it too: before the CRLF fix every sync wrote back and aligned
/// the caret as a side effect; after the fix a pure move writes nothing, so the status bar's
/// line/col would freeze at the old position (it once looked "correct by side effect").
fn align_caret(st: &mut EditorState, clear_anchor: bool) {
    let (focus, offsets, plain_len) = {
        let wys = WYS.read();
        (
            wys.sel.focus,
            wysiwyg::md::block_offsets(&wys.doc),
            wys.doc
                .blocks
                .get(wys.sel.focus.block)
                .map(|b| b.plain_text().len())
                .unwrap_or(0),
        )
    };
    let pos = offsets
        .get(focus.block)
        .copied()
        .unwrap_or(0)
        .saturating_add(focus.off.min(plain_len));
    let total = st.active_doc().total_bytes();
    // The offset means a byte position in the **emit text**: in a CRLF document (its `\r\n` in
    // the piece table is one byte longer than emit's `\n`) everything shifts forward by a few
    // bytes and may land mid multi-byte character. The status bar slicing by bytes would panic (in
    // practice: clicking a Chinese title crashed), so `snap` falls back to the nearest character
    // boundary here; position_info has another defense.
    let snapped = st.active_doc().snap(pos as u64);
    let c = st.caret_mut();
    if clear_anchor {
        c.anchor = None;
    }
    c.pos = snapped.min(total);
}

/// Align the host's hidden caret after a mouse move (status bar line/col, selection anchor; zero
/// document writes).
/// The component's mouse path does not pass through the host keyboard bridge, so
/// `Wysiwyg::on_caret` must notify proactively.
pub fn align_status(st: &mut EditorState) {
    align_caret(st, false);
}

/// Open a link target outside the app (browser for http/mailto, the file's
/// default handler for local paths — `explorer <path>` deals with spaces where
/// `start` would not).
///
/// One choke point for all three link outlets — the preview's whitelist rule,
/// the document view's Ctrl+click and the text view's Ctrl+click — they must
/// not diverge: `safe_link_url` rejects control characters and
/// `javascript:`/`data:`/`ftp:` etc. A scheme-less target is a path, resolved
/// against the document's directory (the same rule as relative images).
/// Returns false when the URL is off the whitelist (the caller falls back to
/// normal click semantics); open failures land in the status bar.
pub fn open_external_link(st: &mut EditorState, url: &str, img_base: Option<&Path>) -> bool {
    let Some(target) = wysiwyg::safe_link_url(url) else {
        return false;
    };
    let res = match crate::ui::commands::markdown::link_path_target(&target, img_base) {
        Some(p) => platform::reveal_in_file_manager(&p, false),
        None => platform::open_url(&target),
    };
    if let Err(e) = res {
        st.status = Some(Fmt::StatusBrowserFailed { err: e.to_string() }.into());
    }
    true
}

/// Document-view outline jump: move the wys caret to the start of `block` and
/// scroll it into view (the visible half of `outline_wys_goto`). Returns false
/// when the wys view is not mounted (another view is active) or the block is
/// out of range.
pub fn goto_block(block: usize) -> bool {
    let Some(h) = handle() else {
        return false;
    };
    let mut editor = WYS.signal();
    let mut overlays = OVERLAYS.signal();
    wysiwyg::ui::host_goto_block(h, &mut editor, &mut overlays, block)
}

/// One auto-scroll step of a **document-view** drag selection.
///
/// The text view gets the same behaviour from `EditorState::drag_autoscroll`
/// (it owns its scroll offset); here the content scrolls inside blitz, so the
/// step goes through the kernel (`host_scroll_by` for the offset,
/// `host_drag_extend` for the moving end of the selection, both in the same
/// frame so the highlight and the viewport never disagree).
///
/// One row per frame — the doc view's rows are much taller than a code line's,
/// so the overshoot-proportional ramp the text view uses would be too coarse.
/// Returns whether anything moved; the caller re-arms the frame, which is what
/// keeps a stationary pointer scrolling.
pub fn drag_tick(st: &mut EditorState) -> bool {
    if !st.md_drag {
        return false;
    }
    let Some((px, py)) = crate::ui::scroll::drag_ptr_peek() else {
        return false;
    };
    let (top, bottom) = (st.view.y, st.view.y + st.view.h);
    // Row height from the wys caret overlay (the kernel's own line height);
    // the font-size fallback only applies before the first caret was placed.
    let row = OVERLAYS
        .read()
        .caret
        .map(|r| r.h)
        .unwrap_or(st.font_size * 1.6)
        .max(6.0);
    // Sign: blitz's `scroll_by` **adds** the delta to the content's scroll
    // offset (positive = later rows), the opposite convention of the text
    // view's `scroll_top`-based `scroll_by` — mixing the two scrolled the wrong
    // way and read as "cannot scroll".
    let (dy, cy) = if py < top {
        (-f64::from(row), top + row * 0.5)
    } else if py > bottom {
        (f64::from(row), bottom - row * 0.5)
    } else {
        return false;
    };
    let Some(h) = handle() else {
        return false;
    };
    if !wysiwyg::ui::host_scroll_by(h.clone(), dy) {
        return false; // an end of the document: nothing to extend either
    }
    let cx = px.clamp(st.view.x + 1.0, st.view.x + st.view.w - 1.0);
    let mut editor = WYS.signal();
    let mut overlays = OVERLAYS.signal();
    wysiwyg::ui::host_drag_extend(h, &mut editor, &mut overlays, cx, cy);
    // The status bar follows the moving end, same as a DOM drag does through
    // `on_caret` — otherwise it would keep reporting the press position while
    // the selection grows past the viewport.
    align_status(st);
    true
}

/// End a WYS pointer drag whose release the component never saw (the pointer left the
/// window, so the DOM `pointerup` went elsewhere): the pointer-owned source
/// reveal is dropped, and a one-line selection falls back to typeset exactly as it
/// does on a normal release.
///
/// Returns whether a drag was live. The window-level release handler uses that to
/// skip the state write for an ordinary click (the WYS press sets `md_drag` on
/// every press, not only on drags).
pub fn end_wys_drag() -> bool {
    let Some(h) = HANDLE.read().as_ref().cloned() else {
        return false;
    };
    let mut editor = WYS.signal();
    let mut overlays = OVERLAYS.signal();
    wysiwyg::ui::host_drag_end(h, &mut editor, &mut overlays)
}

/// Common prefix/suffix lengths (bytes), both falling back to character boundaries of the two
/// texts (UTF-8-safe slicing).
/// Prefix and suffix never overlap: the suffix search is bounded by `min(len) - prefix`.
fn common_span(old: &str, new: &str) -> (usize, usize) {
    let ob = old.as_bytes();
    let nb = new.as_bytes();
    let min_len = ob.len().min(nb.len());
    let mut prefix = 0;
    while prefix < min_len && ob[prefix] == nb[prefix] {
        prefix += 1;
    }
    while prefix > 0 && (!old.is_char_boundary(prefix) || !new.is_char_boundary(prefix)) {
        prefix -= 1;
    }
    let mut suffix = 0;
    while suffix < min_len - prefix && ob[ob.len() - 1 - suffix] == nb[nb.len() - 1 - suffix] {
        suffix += 1;
    }
    while suffix > 0
        && (!old.is_char_boundary(old.len() - suffix) || !new.is_char_boundary(new.len() - suffix))
    {
        suffix -= 1;
    }
    (prefix, suffix)
}

fn active_text(st: &EditorState) -> String {
    let doc = st.active_doc();
    doc.text_range(0..doc.total_bytes())
}

fn active_doc_id(st: &EditorState) -> u64 {
    st.tabs.get(st.active).map_or(0, |t| t.doc.id)
}

/// The wys caret anchor (for positioning the IME candidate window): (x, y, height), all in the
/// **content coordinates** of `.wys-root` (scroll excluded — the caret div lives inside the
/// scrolling container and moves with the content).
/// Reads the overlay's rect fields directly (the kernel already provides structured coordinates;
/// no more parsing CSS strings).
/// `None` = the caret is not ready (never positioned yet).
///
/// Known residual: this function is called during render (the composer reports it every frame
/// as a fallback), but during render `scroll_offset` cannot be queried (blitz's
/// `render_immediate` holds a mutable document borrow, so `try_doc` is always `None`), and
/// scrolling does not trigger a re-render — the anchor drifts after a scroll. During
/// composition the candidate window's anchor is instead reported by the Ime event-phase
/// [`ime_anchor_win`] (with scroll compensation); this function is only the fallback.
pub fn ime_anchor() -> Option<(f32, f32, f32)> {
    let r = OVERLAYS.read().caret?;
    Some((r.x, r.y, r.h))
}

// ---- IME preedit mirror -----------------------------------------------------

/// The wys preedit mirror state: preedit text + a caret viewport snapshot at update time.
#[derive(Default, Clone, PartialEq, Debug)]
pub struct PreeditMirror {
    /// The preedit string (empty = nothing composing is shown).
    pub text: String,
    /// The draw origin `(x, y, row height)` (relative to the `.wys-root` viewport, see
    /// [`caret_view`]).
    /// Keys during composition are swallowed and the caret does not move, so this snapshot simply
    /// "follows the caret".
    pub pos: Option<(f32, f32, f32)>,
}

/// The wys preedit mirror: the text of winit's `Ime::Preedit`, rendered by [`WysPreedit`] as a
/// caret-hugging overlay (font size/color at the caret + underline). The model content never
/// changes during composition.
static PREEDIT: GlobalSignal<PreeditMirror> = Signal::global(PreeditMirror::default);

/// winit `Ime::Preedit` → mirror text (pure function, unit-testable): `(_, None)` (winit's
/// semantics for cancel/clear) and the empty string both clear; otherwise mirror the preedit
/// verbatim (the `Some` range is the candidate cursor's position inside the string — it only
/// affects the system candidate window, not the mirrored text).
fn preedit_text(text: &str, range: Option<(usize, usize)>) -> String {
    if range.is_none() || text.is_empty() {
        String::new()
    } else {
        text.to_string()
    }
}

/// Update the preedit mirror (called by `editor.rs`'s `Ime::Preedit` branch, event phase): pass
/// `("", None)` or `("", Some(_))` on cancel/end to clear. Writes only on change (the iron rule —
/// ordinary typing in a document must not mark dirty for this).
pub fn set_preedit(text: &str, range: Option<(usize, usize)>) {
    let text = preedit_text(text, range);
    let pos = if text.is_empty() { None } else { caret_view() };
    let next = PreeditMirror { text, pos };
    if *PREEDIT.read() != next {
        *PREEDIT.signal().write() = next;
    }
}

/// The wys-root's current scroll offset. **Queried in the event phase** (during render blitz's
/// `render_immediate` holds a mutable document borrow and `try_doc` is always `None`); falls
/// back to `(0, 0)` on failure.
fn root_scroll() -> (f32, f32) {
    let Some(h) = handle() else {
        return (0.0, 0.0);
    };
    let Some(doc) = h.try_doc() else {
        return (0.0, 0.0);
    };
    let Some(n) = doc.get_node(h.node_id()) else {
        return (0.0, 0.0);
    };
    (n.scroll_offset().x as f32, n.scroll_offset().y as f32)
}

/// The wys caret's viewport position `(x, y, height)` (relative to the `.wys-root` border box =
/// the host root div, scroll already subtracted). **Called in the event phase** (see
/// `root_scroll`).
pub fn caret_view() -> Option<(f32, f32, f32)> {
    let (x, y, h) = ime_anchor()?;
    let (sx, sy) = root_scroll();
    Some((x - sx, y - sy, h))
}

/// The candidate window anchor's **window coordinates** `(x, y, w, h)` (including wys-root scroll
/// compensation plus `view_y`'s wys-root window offset). Reported to winit during the `Ime` event
/// phase (the fix path for the candidate anchor drifting after scrolls); scroll cannot be queried
/// during render — see `root_scroll`.
pub fn ime_anchor_win(view_y: f32) -> Option<(f32, f32, f32, f32)> {
    let (x, y, h) = caret_view()?;
    Some((x, y + view_y, 2.0, h.max(1.0)))
}

// ---- keyboard bridge (composer routing → host_* entries) ---------------------

fn handle() -> Option<Rc<NodeHandle>> {
    HANDLE.read().clone()
}

/// Apply a command (event phase), returning whether it succeeded (no handle = failure, and the
/// caller treats it as unconsumed).
fn apply(h: Rc<NodeHandle>, cmd: WysCmd) -> bool {
    let mut editor = WYS.signal();
    let mut overlays = OVERLAYS.signal();
    // Editing commands clear the kernel's redo stack (push_undo) — the host's redo count
    // resets to zero in step (consecutive InsertText inside the typing-merge window does not
    // clear the stack, so a spurious clear only greys the button for an instant).
    // The table Tab chain is a pure cursor move (excluded by the kernel's may_edit, no redo
    // clear), so the count does not move; the redo clear for "Tab in the last cell adds a row"
    // is caught up by the next editing command (a button-level inaccuracy).
    if !matches!(
        cmd,
        WysCmd::Move(_)
            | WysCmd::MoveExtend(_)
            | WysCmd::SelectAll
            | WysCmd::SelectWord
            | WysCmd::SelectBlock
            | WysCmd::Table(wysiwyg::TableCmd::CellNext)
            | WysCmd::Table(wysiwyg::TableCmd::CellPrev)
    ) {
        REDO_DEPTH.store(0, std::sync::atomic::Ordering::Relaxed);
    }
    wysiwyg::host_apply_cmd(h, &mut editor, &mut overlays, cmd)
}

fn resync(h: Rc<NodeHandle>) {
    let mut editor = WYS.signal();
    let mut overlays = OVERLAYS.signal();
    wysiwyg::host_resync(h, &mut editor, &mut overlays);
}

/// In read-only mode every wys edit key is swallowed (the mode entry already blocks read-only
/// documents; this is the fallback for "switching to read-only after entering" — typing must
/// not quietly modify a read-only document).
fn readonly_blocked(state: &mut Signal<EditorState>) -> bool {
    if state.read().read_only {
        state.write().status = Some(crate::ui::commands::READONLY_MSG.into());
        return true;
    }
    false
}
/// IME composing: keys during composition (Enter/letters …) never reach the editor. The kernel's
/// `is_composing` guard only covers the component's own onkeydown; the host bridge must block one
/// more layer here, or an Enter before pinyin commits would hit `Cmd::Enter` directly and split
/// the block (host side).
///
/// The preedit is mirrored at the caret as an overlay by [`WysPreedit`] (state maintained by
/// [`set_preedit`]); the model content never changes during composition; `editor.rs`'s
/// `Ime::Preedit/Commit` wires the composing flag to
/// [`wysiwyg::ui::set_composing`].
fn composing_blocked() -> bool {
    wysiwyg::ui::is_composing()
}

/// Ctrl chord keys. `handle_key` calls this **after reserved keys and before the shortcut table**
/// — in wys mode Ctrl+B/Z… must reroute to the wys editor instead of falling into the piece
/// table's command table.
/// Returns `true` = consumed.
pub fn wys_chord_key(
    state: &mut Signal<EditorState>,
    data: &KeyboardData,
    ctrl: bool,
    shift: bool,
) -> bool {
    if !ctrl {
        return false;
    }
    if composing_blocked() {
        return true;
    }
    let Key::Character(s) = data.key() else {
        return false;
    };
    // Read-only mode: swallow everything except select-all/copy (the mode entry already blocks
    // read-only documents; this is the fallback for "switching to read-only after entering" —
    // typing must not quietly modify a read-only document).
    let mutating = !matches!(s.to_lowercase().as_str(), "a" | "c");
    if mutating && readonly_blocked(state) {
        return true;
    }
    let Some(h) = handle() else {
        return false;
    };
    // Hint candidate panel: any Ctrl chord closes the panel — shortcuts take priority over
    // completion (Ctrl+B etc. must not land on the candidates).
    mdhint::close();
    let mut consumed = true;
    // Editing commands go through the debounce ([`note_edit`]); pure selection only aligns the
    // hidden caret (**no document write** — sync_to_doc would normalize trailing blank lines /
    // block spacing, and arrow keys must not modify the file); undo/redo may change the
    // document, so they take the full sync.
    let mut edit_sync = true;
    let mut full_sync = true;
    match s.to_lowercase().as_str() {
        "a" => {
            consumed = apply(h, WysCmd::SelectAll);
            edit_sync = false;
            full_sync = false;
        }
        "c" => {
            copy_selection();
            edit_sync = false;
            full_sync = false;
        }
        "x" => {
            copy_selection();
            consumed = apply(h, WysCmd::DeleteSelection);
        }
        "v" => {
            // Rich paste: HTML/URL/image/plain text all go through [`paste_cmd`]
            if let Some(c) = paste_cmd() {
                consumed = apply(h, c);
            }
        }
        // undo/redo are unified through [`undo`] / [`redo`] (same entry point as the toolbar
        // buttons).
        "z" => {
            if shift {
                redo(&mut state.write());
            } else {
                undo(&mut state.write());
            }
            edit_sync = false;
        }
        "y" => {
            redo(&mut state.write());
            edit_sync = false;
        }
        "b" => consumed = apply(h, WysCmd::Wrap(WrapKind::Bold)),
        "i" => consumed = apply(h, WysCmd::Wrap(WrapKind::Italic)),
        // Pop an input box to collect the URL (the selection text is no longer treated as the
        // URL); an empty URL cancels.
        "k" => {
            open_link_dialog(&mut state.write());
            edit_sync = false;
        }
        "d" => consumed = apply(h, WysCmd::Wrap(WrapKind::Del)),
        "e" => consumed = apply(h, WysCmd::Wrap(WrapKind::Code)),
        "1" | "2" | "3" | "4" | "5" | "6" => {
            let n: u8 = s.parse().unwrap_or(1);
            consumed = apply(h, WysCmd::SetHead(n));
        }
        _ => return false,
    }
    if consumed {
        let st = &mut state.write();
        if edit_sync {
            note_edit(st);
        } else if full_sync {
            sync_to_doc(st);
        } else {
            align_caret(st, false);
        }
        log_caret("chord");
    }
    if consumed {
        // Aligned with wys_key: chord edits (Ctrl+V/B/I…) also changed the WYS model, so next
        // frame we bump HOST_EPOCH to trigger an MdWys re-render (otherwise the DOM lags the
        // model).
        NEEDS_RENDER.store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(wake) = state.read().wake.clone() {
            wake();
        }
    }
    consumed
}

/// Non-Ctrl keys (arrows/Enter/Backspace/typing…). `edit_key` is called after the find bar's
/// capture decision; returns `true` = consumed.
pub fn wys_key(
    state: &mut Signal<EditorState>,
    data: &KeyboardData,
    ctrl: bool,
    shift: bool,
) -> bool {
    if composing_blocked() {
        return true;
    }
    let Some(h) = handle() else {
        return false;
    };
    let mutating = matches!(
        data.key(),
        Key::Character(_) | Key::Enter | Key::Backspace | Key::Delete | Key::Tab
    );
    if mutating && readonly_blocked(state) {
        return true;
    }
    // Hint auto-complete: while the panel is open, navigation/confirm/close keys belong to it;
    // typing and backspace go into the editor as usual (the trailing [`hint_probe`] recomputes
    // the candidates from the new text); the rest (arrows/delete/…) close the panel and take
    // the normal edit path. wys-mode only — text mode does not go through this bridge.
    if mdhint::open() {
        match data.key() {
            Key::ArrowUp | Key::ArrowDown => {
                mdhint::nav(data.key() == Key::ArrowUp);
                return true;
            }
            // Enter/Tab confirms (while the panel is open these take priority over split-block /
            // indent; Shift+Enter likewise)
            Key::Enter | Key::Tab => {
                match mdhint::selected_index() {
                    Some(i) => hint_confirm(&mut state.write(), i),
                    None => mdhint::close(),
                }
                return true;
            }
            Key::Escape => {
                mdhint::close();
                return true;
            }
            Key::Character(_) | Key::Backspace => {}
            _ => mdhint::close(),
        }
    }
    let mut consumed = true;
    // Same as [`wys_chord_key`]: editing commands are debounced; move/select only align the
    // hidden caret (zero writes); undo/redo take the full sync.
    let edit_sync: bool;
    let full_sync: bool;
    match data.key() {
        // Shift+Enter = soft line break inside the block; Enter keeps its split-block semantics
        Key::Enter => {
            let changed = apply(
                h,
                if shift {
                    WysCmd::SoftBreak
                } else {
                    WysCmd::Enter
                },
            );
            consumed = true;
            // Enter is a structural edit (split block / create a blank line): write back
            // **immediately** and trigger a whole-tree re-render. With debouncing, the empty
            // block would wait 250ms+ for a wake frame to reach the DOM; meanwhile the caret
            // stays on the previous line and repeated Enter presses show no new blank line
            // (on real hardware).
            edit_sync = false;
            full_sync = changed;
        }
        // Backspace/Delete **always belong to wys** (consumed here; otherwise the composer
        // would delete the original text a second time at the hidden text caret and tables
        // would be ripped apart — on real hardware, backspace at the start of a cell shredded
        // `| a | b |` into loose blocks). Only schedule a write-back if the model really
        // changed the document; pure moves (backspace at cell start = move to the previous
        // cell) take the alignment path, avoiding an opportunistic format normalization.
        Key::Backspace => {
            let changed = apply(
                h,
                if ctrl {
                    WysCmd::DeleteBackWord
                } else {
                    WysCmd::DeleteBack
                },
            );
            consumed = true;
            edit_sync = changed;
            full_sync = changed;
        }
        Key::Delete => {
            let changed = apply(
                h,
                if ctrl {
                    WysCmd::DeleteForwardWord
                } else {
                    WysCmd::DeleteForward
                },
            );
            consumed = true;
            edit_sync = changed;
            full_sync = changed;
        }
        Key::Tab => {
            // Table focus: the Tab chain moves between cells (Tab in the last cell adds a row);
            // it must not fall into list indenting. Other focuses keep list Indent/Outdent.
            if focus_cell().is_some() {
                let before = WYS.read().doc.blocks.len();
                consumed = apply(
                    h,
                    if shift {
                        WysCmd::Table(wysiwyg::TableCmd::CellPrev)
                    } else {
                        WysCmd::Table(wysiwyg::TableCmd::CellNext)
                    },
                );
                // Pure moves take the alignment path (zero writes, no format normalization);
                // **Tab in the last cell automatically adds a row** (block count changes) —
                // that one must take the write-back, or the new row only lives in the model
                // and is lost on save.
                let appended = WYS.read().doc.blocks.len() != before;
                edit_sync = appended;
                full_sync = appended;
            } else {
                let changed = apply(
                    h,
                    if shift {
                        WysCmd::Outdent
                    } else {
                        WysCmd::Indent
                    },
                );
                // Tab belongs to wys: even when it cannot indent (first item / outdent at
                // top level) it must be consumed, so the host does not treat Tab as focus
                // switching / inserting a tab character.
                consumed = true;
                edit_sync = changed;
                full_sync = changed;
            }
        }
        // Ctrl+Home/End = document start/end (the kernel's Move has no DocStart/DocEnd).
        Key::Home if ctrl => {
            consumed = move_doc_edge(h, true, shift);
            edit_sync = false;
            full_sync = false;
        }
        Key::End if ctrl => {
            consumed = move_doc_edge(h, false, shift);
            edit_sync = false;
            full_sync = false;
        }
        // Home/End: visual line start/end (soft-wrap break points); when geometry is
        // unavailable (empty block etc.) fall back to block start/end. Same branch semantics
        // as the component's onkeydown.
        Key::Home => {
            let target = WysMove::BlockStart;
            let mut editor = WYS.signal();
            let mut overlays = OVERLAYS.signal();
            consumed =
                wysiwyg::host_move_line_edge(h.clone(), &mut editor, &mut overlays, true, shift)
                    || move_to(h, target, shift);
            edit_sync = false;
            full_sync = false;
        }
        Key::End => {
            let target = WysMove::BlockEnd;
            let mut editor = WYS.signal();
            let mut overlays = OVERLAYS.signal();
            consumed =
                wysiwyg::host_move_line_edge(h.clone(), &mut editor, &mut overlays, false, shift)
                    || move_to(h, target, shift);
            edit_sync = false;
            full_sync = false;
        }
        Key::ArrowLeft => {
            let m = if ctrl {
                WysMove::WordLeft
            } else {
                WysMove::Left
            };
            consumed = move_to(h, m, shift);
            edit_sync = false;
            full_sync = false;
        }
        Key::ArrowRight => {
            let m = if ctrl {
                WysMove::WordRight
            } else {
                WysMove::Right
            };
            consumed = move_to(h, m, shift);
            edit_sync = false;
            full_sync = false;
        }
        // ↑/↓ use parley's visual line geometry (same-source logic as the component's
        // onkeydown)
        Key::ArrowUp | Key::ArrowDown => {
            let mut editor = WYS.signal();
            let mut overlays = OVERLAYS.signal();
            wysiwyg::host_move_visual(
                h,
                &mut editor,
                &mut overlays,
                matches!(data.key(), Key::ArrowUp),
                shift,
            );
            edit_sync = false;
            full_sync = false;
        }
        // PageUp/PageDown = visual line movement by viewport-line count (geometry same as
        // ↑/↓). Previously these fell to `return false`, so the key moved the text view
        // hidden underneath wys instead.
        Key::PageUp | Key::PageDown => {
            let up = data.key() == Key::PageUp;
            let lines = page_lines(state);
            let mut editor = WYS.signal();
            let mut overlays = OVERLAYS.signal();
            for _ in 0..lines {
                wysiwyg::host_move_visual(h.clone(), &mut editor, &mut overlays, up, shift);
            }
            edit_sync = false;
            full_sync = false;
        }
        Key::Character(s) if !ctrl => {
            let changed = apply(h, WysCmd::InsertText(s.clone()));
            consumed = true;
            edit_sync = changed;
            full_sync = changed;
        }
        // Everything else (Escape, F-keys, no handle) is not consumed: falls through to the
        // host's own default handling
        _ => return false,
    }
    if consumed {
        let st = &mut state.write();
        if edit_sync {
            note_edit(st);
        } else if full_sync {
            sync_to_doc(st);
        } else {
            align_caret(st, false);
        }
        // Hint trigger/filter: only typing/backspace change the trigger string (`:x`, the
        // language name at the start of a code block) — recompute the candidate panel from
        // the new text; the other keys above already closed the panel.
        if matches!(data.key(), Key::Character(_) | Key::Backspace) {
            hint_probe();
        }
        log_caret("key");
    }
    // Explicit wake: wys's WYS signal write happens inside a winit input event; dioxus only
    // mark_dirty's and does not guarantee anyone request_redraw's — without the wake, new
    // blocks/new text wait for the next unrelated event to reach the DOM (on real hardware:
    // pressing Enter repeatedly shows no new blank line, caret stays on the previous line).
    if consumed {
        NEEDS_RENDER.store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(wake) = state.read().wake.clone() {
            wake();
        }
    }
    consumed
}

/// Visual lines per page (PageUp/PageDown step): viewport height / line height (1.5 ×
/// font size).
fn page_lines(state: &Signal<EditorState>) -> usize {
    let st = state.read();
    let lh = (st.font_size * 1.5).max(12.0);
    ((st.view.h.max(lh) / lh).floor() as usize).max(1)
}

/// Ctrl+Home / Ctrl+End: caret to document start/end (Shift = extend selection).
/// The kernel's `Move` has no DocStart/DocEnd variant — place the logical caret directly, then
/// [`resync`] to restore geometry (same semantics as the post-undo sync).
fn move_doc_edge(h: Rc<NodeHandle>, start: bool, shift: bool) -> bool {
    let mut editor = WYS.signal();
    {
        let mut e = editor.write();
        let target = if start {
            wysiwyg::Pos::new(0, 0)
        } else {
            let last = e.doc.blocks.len().saturating_sub(1);
            wysiwyg::Pos::new(last, e.doc.blocks[last].plain_text().len())
        };
        e.sel = if shift {
            wysiwyg::Sel {
                anchor: e.sel.anchor,
                focus: target,
            }
        } else {
            wysiwyg::Sel::caret(target)
        };
    }
    resync(h);
    true
}

/// Diagnostics (written to shell.log when `INF_WYS_LOG=1`): wys caret CSS and geometry
/// breakdown.
fn log_caret(tag: &str) {
    if std::env::var("INF_WYS_LOG").is_err() {
        return;
    }
    let Some(h) = handle() else { return };
    let pos = WYS.read().sel.focus;
    let g = wysiwyg::geom::Geom::new(h.clone());
    // Diagnostic supplement: when geom=None, locate the missing link (document borrow /
    // node present or not / inline layout present or not). Block-number lookup goes through
    // the editor's same structural-generation index.
    let probe = h.clone().try_doc().and_then(|doc| {
        let container = wysiwyg::geom::fragment_container(&doc, h.node_id());
        wysiwyg::geom::node_for_bid(&doc, container, pos.block).map(|nid| {
            let has_layout = doc
                .get_node(nid)
                .and_then(|n| n.data.downcast_element())
                .and_then(|el| el.inline_layout_data.as_ref())
                .is_some();
            format!("node={nid:?} inline_layout={has_layout}")
        })
    });
    crate::ui::shell::debug_log(&format!(
        "[wys-caret] {tag} focus={pos:?} overlays={:?} geom={:?} probe={probe:?}",
        *OVERLAYS.read(),
        g.caret_debug(pos)
    ));
}

fn move_to(h: Rc<NodeHandle>, m: WysMove, shift: bool) -> bool {
    apply(
        h,
        if shift {
            WysCmd::MoveExtend(m)
        } else {
            WysCmd::Move(m)
        },
    )
}

fn copy_selection() {
    // Markdown source, not flattened text: a copied table / formula / bold run
    // must survive a paste into another Markdown document.
    let text = WYS.read().selected_markdown();
    if text.is_empty() {
        return;
    }
    let _clip = platform::clipboard::lock();
    if let Ok(mut cb) = arboard::Clipboard::new() {
        let _ = cb.set_text(text);
    }
}

// ---- hint auto-complete (pure logic and candidate panel in [`mdhint`] ) ------------------

/// Cursor context for hint trigger detection: `(in-code-block, text before cursor, focused
/// block's full text, cursor byte offset)`.
fn hint_context() -> Option<(bool, String, String, usize)> {
    let wys = WYS.read();
    let focus = wys.sel.focus;
    let b = wys.doc.blocks.get(focus.block)?;
    let is_code = matches!(b.core(), wysiwyg::Block::Code { .. });
    let text = b.plain_text();
    let mut off = focus.off.min(text.len());
    while off > 0 && !text.is_char_boundary(off) {
        off -= 1;
    }
    Some((is_code, text[..off].to_string(), text, off))
}

/// Post-keystroke trigger/filter probe (event phase): recompute the candidate panel from the
/// cursor context and refresh the cursor anchor snapshot. Pure logic lives in
/// [`mdhint::detect`] / [`mdhint::step`].
fn hint_probe() {
    let det = hint_context()
        .and_then(|(is_code, before, code, off)| mdhint::detect(is_code, &before, &code, off));
    let next = mdhint::step(mdhint::state().as_ref(), det, caret_view(), root_scroll());
    mdhint::set_state(next);
}

/// Confirm a hint candidate (keyboard Enter/Tab and mouse click share the entry point). Model
/// operations = `DeleteBack` × trigger-string length + (emoji) `InsertText` (pure description
/// in [`mdhint::confirm_for`]); a language candidate additionally writes the code block's
/// lang — the kernel's `Cmd` set has no language variant (`edit.rs` cannot be modified), so the
/// host writes the block metadata directly and it does not enter undo separately (the next
/// edit's snapshot naturally carries it; the prefix deletion still enters undo as usual).
pub fn hint_confirm(st: &mut EditorState, idx: usize) {
    if readonly_blocked_sig(st) {
        return;
    }
    let Some(s) = mdhint::state() else {
        return;
    };
    let Some(c) = mdhint::confirm_for(s.kind, &s.prefix, idx) else {
        mdhint::close();
        return;
    };
    let Some(h) = handle() else {
        return;
    };
    for _ in 0..c.delete {
        apply(h.clone(), WysCmd::DeleteBack);
    }
    if !c.insert.is_empty() {
        apply(h, WysCmd::InsertText(c.insert));
    }
    if let Some(lang) = c.lang {
        set_focus_code_lang(lang);
    }
    mdhint::close();
    sync_to_doc(st);
    log_caret("hint");
}

/// Write the focused code block's lang (the metadata part of hint language confirmation; see
/// [`hint_confirm`]).
fn set_focus_code_lang(lang: &str) {
    let mut sig = WYS.signal();
    let mut e = sig.write();
    let bid = e.sel.focus.block;
    if let Some(wysiwyg::Block::Code { lang: slot, .. }) =
        e.doc.blocks.get_mut(bid).map(wysiwyg::Block::core_mut)
    {
        *slot = Some(lang.to_string());
    }
}

// ---- overlay frame sync (hint panel / selection floating toolbar) ------------------------

/// Overlay frame sync. Called by `editor.rs`'s `RedrawRequested` **in the event phase** (blitz
/// has not borrowed the document yet and geometry queries are available — the render-phase
/// iron rule is in the top comment of `wysiwyg::ui`):
///
/// - **Selection floating toolbar**: mouse drag-select / double-click word-select are handled
///   inside the wys kernel component and the host has no event to hook — visibility and
///   positioning are all unified in this frame hook (pops up on the **next frame** after the
///   selection changes). State machine in [`mdselbar::step`].
/// - **Hint panel**: scroll drift / caret clicked away → close the panel (the anchor is a
///   snapshot from typing time; leaving it open after drift points to the wrong place).
///
/// Returns true = caret/selection sync **still has pending work** (the focused block's inline
/// layout is not ready): the caller must `request_redraw` to schedule another frame, or the
/// retry never arrives (no next frame without a request) and the caret stays at its old
/// position.
///
/// Both are conditional writes (iron rule: only write on change, never self-sustain redraws).
pub fn overlays_frame(wys: bool, dragging: bool) -> bool {
    // Re-render after host edits (a signal written during the frame phase is seen by this
    // round's poll; see NEEDS_RENDER)
    if NEEDS_RENDER.swap(false, std::sync::atomic::Ordering::Relaxed) {
        *HOST_EPOCH.signal().write() += 1;
    }
    if !wys {
        mdhint::close();
        mdselbar::reset();
        // Leaving the wys view: clear the focused cell highlight (otherwise a colored cell
        // patch lingers when switching back to the text view)
        let stale_cell = OVERLAYS.signal().peek().cell.is_some();
        if stale_cell {
            OVERLAYS.signal().write().cell = None;
        }
        return false;
    }
    // The kernel component is not mounted yet (first frame / remount window): overlays have no
    // positioning basis, so drop them — leaving them would draw the last session's panel at the
    // wrong place.
    let Some(h0) = handle() else {
        mdhint::close();
        mdselbar::reset();
        return false;
    };
    // Image load failure → alt placeholder: when a resource fails blitz records the URL into
    // the document's `failed_images` (vendor patch); sync it to the render layer here and bump
    // the version number.
    //
    // Change detection uses a **set signature** rather than a count: when switching tabs the
    // two documents may have the same failure count with completely different URLs, and
    // comparing only the count would leave the render layer holding the previous document's
    // failed set. The signature is computed inside the document borrow (no allocation); the
    // URL list is only cloned + sorted when the signature really changed.
    if let Some((sig, Some(urls))) = h0.clone().try_doc().map(|d| {
        let sig = d.failed_image_sig();
        let changed = sig != IMG_FAILED_SIG.load(std::sync::atomic::Ordering::Relaxed);
        (sig, changed.then(|| d.failed_image_urls()))
    }) {
        IMG_FAILED_SIG.store(sig, std::sync::atomic::Ordering::Relaxed);
        wysiwyg::render::set_failed_images(urls);
        *IMG_EPOCH.signal().write() += 1;
    }
    // Push back the caret/selection after a document change + scroll into view: async tasks
    // cannot borrow the document while polling in the host (see wysiwyg::ui::host_flush_resync
    // comments), so this frame hook unifies the work.
    let mut editor = WYS.signal();
    let mut overlays = OVERLAYS.signal();
    let resync_pending = wysiwyg::ui::host_flush_resync(h0, &mut editor, &mut overlays);
    if mdhint::open() {
        let round = |p: Option<(f32, f32, f32)>| {
            p.map(|(x, y, _)| ((x * 10.0).round(), (y * 10.0).round()))
        };
        let cur = round(caret_view());
        let at = round(mdhint::state().and_then(|s| s.pos));
        // Only close the panel when both sides can be located and there really is drift: the
        // caret geometry may not be filled in yet this frame (the caret rect is temporarily
        // missing after a rebuild), and treating None as drift would close the panel
        // immediately (on real hardware: the code block's language candidates flashed and
        // vanished).
        if let (Some(cur), Some(at)) = (cur, at) {
            if cur != at {
                mdhint::close();
            }
        }
    }
    let sel = WYS.read().sel;
    // While a drag is in progress the floating toolbar stays hidden: it sits above the
    // selection (right under the moving pointer) and would swallow the pointer moves that
    // extend the drag — the widget owns the drag now (see `wysiwyg::ui` onpointermove).
    let next = mdselbar::step(
        &mdselbar::current(),
        sel,
        wysiwyg::ui::is_composing() || dragging,
        root_scroll(),
        sel_start_view(sel),
        std::time::Instant::now(),
    );
    mdselbar::set_state(next);
    resync_pending
}

/// Viewport coordinates of the selection start (floating toolbar positioning): the kernel's
/// `Geom` has no selection-rect API, so the caret rect at the **selection start** approximates
/// the "first rect" (`sel.range().0` is the top-left of the first line's selected range); when
/// not found, fall back to the focused caret ([`OVERLAYS`]). Called in the event phase.
fn sel_start_view(sel: wysiwyg::Sel) -> Option<(f32, f32)> {
    let h = handle()?;
    let g = wysiwyg::geom::Geom::new(h.clone());
    let (sx, sy) = root_scroll();
    if let (Some((ox, oy)), Some(r)) = (g.root_origin(), g.caret_rect(sel.range().0)) {
        return Some((r.x - ox - sx, r.y - oy - sy));
    }
    ime_anchor().map(|(x, y, _)| (x - sx, y - sy))
}

/// Plain-URL test (pasting a URL over a selection makes a link): the whole string is a
/// **single** http/https/file URL (no whitespace).
fn is_plain_url(text: &str) -> bool {
    let t = text.trim();
    let Some((scheme, rest)) = t.split_once("://") else {
        return false;
    };
    matches!(scheme, "http" | "https" | "file")
        && !rest.is_empty()
        && !t.chars().any(char::is_whitespace)
}

/// Image clipboard → PNG saved to the project `tmp` directory (project rule: temp files go in
/// tmp), producing an `![](file:///…)` template text (same `InsertText` template semantics as
/// the image button). Falls back to a placeholder template if the save fails.
fn clip_image_md(img: &arboard::ImageData<'_>) -> String {
    let path = (|| {
        let dir = std::env::current_dir().ok()?.join("tmp");
        std::fs::create_dir_all(&dir).ok()?;
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let path = dir.join(format!("paste-{}-{stamp}.png", std::process::id()));
        image::save_buffer(
            &path,
            img.bytes.as_ref(),
            img.width as u32,
            img.height as u32,
            image::ColorType::Rgba8,
        )
        .ok()?;
        Some(path)
    })();
    match path {
        Some(p) => {
            let url = format!("file:///{}", p.display().to_string().replace('\\', "/"));
            format!("![]({url})")
        }
        None => "![](url)".to_string(),
    }
}

/// Rich paste: clipboard → wys command. The consumption order is fixed as follows:
/// 1. Focus in a Code block → paste the text **as-is** (no parsing, no link — the kernel's
///    `Cmd::Paste` already inserts literally for a Code focus; the Html downgrade only applies
///    to non-Code focuses);
/// 2. There is a selection and the clipboard is a plain URL → wrap the selection in a link
///    (`Cmd::Link`, not Paste);
/// 3. HTML clipboard (Word/web pages) → sanitize and convert to Markdown
///    ([`wysiwyg::html_to_markdown`]; sanitizing is a security requirement);
/// 4. Image clipboard → save a PNG and insert an `![](file:///…)` template;
/// 5. Fall back to plain text.
fn paste_cmd() -> Option<WysCmd> {
    let _clip = platform::clipboard::lock();
    let mut cb = arboard::Clipboard::new().ok()?;
    let text = cb.get_text().ok();
    if focus_state().code {
        if let Some(t) = text {
            return Some(WysCmd::Paste(t));
        }
        if let Ok(img) = cb.get_image() {
            return Some(WysCmd::InsertText(clip_image_md(&img)));
        }
        return cb.get().html().ok().map(WysCmd::Paste);
    }
    let has_sel = !WYS.read().selected_text().is_empty();
    if has_sel {
        if let Some(t) = text.as_deref().map(str::trim).filter(|t| is_plain_url(t)) {
            return Some(WysCmd::Link(t.to_string()));
        }
    }
    if let Some(md) = cb
        .get()
        .html()
        .ok()
        .map(|h| wysiwyg::html_to_markdown(&h))
        .filter(|m| !m.trim().is_empty())
    {
        return Some(WysCmd::Paste(md));
    }
    if let Ok(img) = cb.get_image() {
        return Some(WysCmd::InsertText(clip_image_md(&img)));
    }
    text.map(WysCmd::Paste)
}

// ---- WYSIWYG toolbar (actions::Cmd::WysTool entry) --------------------------------

/// Inline wrap counts at the caret (the `on` state of the toolbar's inline format buttons).
#[derive(Default, Clone, Copy, PartialEq, Eq, Debug)]
pub struct InlineMarks {
    pub bold: bool,
    pub italic: bool,
    pub strike: bool,
    pub code: bool,
    pub link: bool,
}

/// State summary of the focused block (for the toolbar buttons' `on`/dropdown state).
#[derive(Default)]
pub struct WysFocus {
    /// The focused block is a heading → its level.
    pub head: Option<u8>,
    /// The focused block is a list item → its ordered-ness.
    pub ordered: Option<bool>,
    /// The focused block is a task list item.
    pub task: bool,
    /// The focused block is inside a quote.
    pub quote: bool,
    /// The focused block is a fenced code block.
    pub code: bool,
    /// The focused block is a table cell (display/enable condition for the table operation
    /// group).
    pub td: bool,
    /// The current alignment of the column containing the focused cell (encoded like `Td.align`:
    /// 0 = default, 1 = left, 2 = center, 3 = right).
    /// The table alignment key's `on` state lights up from it (`TableCmd::Align` applies per
    /// column and a column always holds one value).
    pub td_align: Option<u8>,
    /// The inline marks effective at the caret.
    pub inline: InlineMarks,
}

/// The block's inline content reference (Code/Html/Hr have no inline tree; returns empty).
fn inlines_of(b: &wysiwyg::Block) -> &[wysiwyg::Inline] {
    use wysiwyg::Block;
    match b {
        Block::Para(i)
        | Block::Head { inlines: i, .. }
        | Block::Li { inlines: i, .. }
        | Block::Quote(i)
        | Block::Td { inlines: i, .. } => i,
        _ => &[],
    }
}

/// The inline wraps effective at the caret (pure function, unit-testable).
///
/// Walk the inline tree recording byte ranges in `plain_text` order; a container node
/// (Strong/Em/Del/Link) that covers the probe point lights up its mark and recurses into
/// nesting (recurse only when covering, to avoid misjudging a caret outside the container as
/// relative offset 0 inside a child). The test is "touching any inner boundary": a caret
/// immediately before/after bold text counts as inside (general editor intuition).
pub fn marks_at(inlines: &[wysiwyg::Inline], off: usize) -> InlineMarks {
    let mut m = InlineMarks::default();
    walk_marks(inlines, off, &mut m);
    m
}

fn covers(start: usize, end: usize, off: usize) -> bool {
    end > start && ((start <= off && off < end) || (start < off && off <= end))
}

fn walk_marks(inlines: &[wysiwyg::Inline], off: usize, m: &mut InlineMarks) {
    use wysiwyg::Inline;
    let mut at = 0usize;
    for inl in inlines {
        let end = at + inl.plain().len();
        let hit = covers(at, end, off);
        match inl {
            Inline::Code(_) => {
                if hit {
                    m.code = true;
                }
            }
            Inline::Strong(c) => {
                if hit {
                    m.bold = true;
                    walk_marks(c, off - at, m);
                }
            }
            Inline::Em(c) => {
                if hit {
                    m.italic = true;
                    walk_marks(c, off - at, m);
                }
            }
            Inline::Del(c) => {
                if hit {
                    m.strike = true;
                    walk_marks(c, off - at, m);
                }
            }
            Inline::Link { inlines: c, .. } => {
                if hit {
                    m.link = true;
                    walk_marks(c, off - at, m);
                }
            }
            Inline::Text(_)
            | Inline::Image { .. }
            | Inline::Math { .. }
            | Inline::SoftBreak
            | Inline::HardBreak => {}
        }
        at = end;
    }
}

/// Read the focused block's state of the global wys editor (called during toolbar render,
/// read-only).
pub fn focus_state() -> WysFocus {
    let wys = WYS.read();
    let focus = wys.sel.focus;
    let b = wys.doc.blocks.get(focus.block);
    let (head, ordered, task, quote, code, td) = match b {
        Some(wysiwyg::Block::Head { level, .. }) => {
            (Some(*level), None, false, false, false, false)
        }
        Some(wysiwyg::Block::Li { ordered, task, .. }) => {
            (None, Some(*ordered), task.is_some(), false, false, false)
        }
        Some(wysiwyg::Block::Quote(_)) => (None, None, false, true, false, false),
        Some(wysiwyg::Block::Code { .. }) => (None, None, false, false, true, false),
        Some(b) if b.is_td() => (None, None, false, false, false, true),
        _ => (None, None, false, false, false, false),
    };
    let inline = b
        .filter(|b| !matches!(b, wysiwyg::Block::Code { .. } | wysiwyg::Block::Hr))
        .map(|b| marks_at(inlines_of(b), focus.off))
        .unwrap_or_default();
    // Focused column alignment (on state): read the focused cell's own align — Align writes
    // the same value down the column
    let td_align = match b.map(|b| b.core()) {
        Some(wysiwyg::Block::Td { align, .. }) => Some(*align),
        _ => None,
    };
    WysFocus {
        head,
        ordered,
        task,
        quote,
        code,
        td,
        td_align,
        inline,
    }
}

/// The focused block's heading level (the "Body" item: clicking the same level again restores
/// a paragraph — set_head's toggle semantics).
pub fn focus_head_level() -> Option<u8> {
    focus_state().head
}

/// Whether the wys undo stack can step back (toolbar undo greying).
pub fn can_undo() -> bool {
    WYS.read().can_undo()
}

/// Whether wys redo is available (toolbar redo greying). The kernel does not expose `can_redo`
/// and the undo stack field is private — the host maintains a depth counter from the success
/// return values of undo/redo (edits clear the redo stack; see the bookkeeping in `apply`).
pub fn can_redo() -> bool {
    REDO_DEPTH.load(std::sync::atomic::Ordering::Relaxed) > 0
}

/// Emoji panel (a common emoji subset; the toolbar grid lays them out in 9 columns).
pub const EMOJIS: [&str; 72] = [
    "😀", "😃", "😄", "😁", "😆", "😅", "🤣", "😂", "🙂", "🙃", "😉", "😊", "😇", "🥰", "😍", "🤩",
    "😘", "😗", "😚", "😙", "😋", "😛", "😜", "🤪", "🤨", "🧐", "🤓", "😎", "🥳", "😏", "😒", "😞",
    "😔", "😟", "😕", "🙁", "😣", "😖", "😫", "😩", "🥺", "😢", "😭", "😤", "😠", "😡", "🤬", "🤯",
    "😳", "🥵", "🥶", "😱", "😨", "😰", "😥", "😓", "🤗", "🤔", "🤭", "🤫", "🤥", "😶", "😐", "😑",
    "😬", "🙄", "😯", "😦", "😧", "😮", "😲", "🥱",
];

// ---- URL input dialog -------------------------------------------------------------

/// Insert link: pop an input box to collect the URL — the selection text is **no longer**
/// treated as the URL (it used to produce `[click here](click here)`). Confirm goes through
/// [`apply_link`]; an empty URL cancels.
pub fn open_link_dialog(st: &mut EditorState) {
    use crate::ui::actions::Cmd;
    use crate::ui::state::Dialog;
    st.show_dialog(
        Dialog::text(Msg::WysLinkTitle, Msg::WysLinkPrompt)
            .with_input(
                Fmt::CommonRaw {
                    text: "https://".into(),
                },
                Cmd::WysLinkSubmit,
            )
            .primary(Msg::CommonOk, Some(Cmd::WysLinkSubmit))
            .button(Msg::CommonCancel, None),
    );
}

/// Insert image: same URL input box as the link, producing an `![](url)` template (same
/// semantics as the text view's "Image" button; an empty URL inserts a placeholder template).
pub fn open_image_dialog(st: &mut EditorState) {
    use crate::ui::actions::Cmd;
    use crate::ui::state::Dialog;
    st.show_dialog(
        Dialog::text(Msg::WysImageTitle, Msg::WysImagePrompt)
            .with_input(
                Fmt::CommonRaw {
                    text: "https://".into(),
                },
                Cmd::WysImageSubmit,
            )
            .primary(Msg::CommonOk, Some(Cmd::WysImageSubmit))
            .button(Msg::CommonCancel, None),
    );
}

/// Submit for [`open_link_dialog`] (`Cmd::WysLinkSubmit`): an empty URL cancels.
pub fn apply_link(st: &mut EditorState, url: &str) {
    let url = url.trim();
    if url.is_empty() {
        st.status = Some(Msg::WysLinkCancelled.into());
        return;
    }
    let Some(h) = handle() else {
        return;
    };
    apply(h, WysCmd::Link(url.to_string()));
    sync_to_doc(st);
    log_caret("link");
}

/// Submit for [`open_image_dialog`] (`Cmd::WysImageSubmit`): insert a **structured**
/// `Inline::Image` node (`WysCmd::Image`).
///
/// Do not insert the literal `![](url)` template: that is a plain text node and emit would
/// escape the brackets into `!\[\](url)` — after saving / re-parsing it comes back as literal
/// text, not an image (verified on real hardware).
pub fn apply_image(st: &mut EditorState, url: &str) {
    let url = url.trim();
    let Some(h) = handle() else {
        return;
    };
    apply(h, WysCmd::Image(url.to_string()));
    sync_to_doc(st);
    log_caret("image");
}

/// Toolbar command entry: expand the Copy subset [`WysTool`](crate::ui::actions::WysTool)
/// into a `wysiwyg::Cmd`, apply it to the wys editor, then write back to the piece table.
/// Read-only documents are blocked here (same wording as the keyboard path).
pub fn toolbar_apply(st: &mut EditorState, t: crate::ui::actions::WysTool) {
    use crate::ui::actions::WysTool as Wt;
    if readonly_blocked_sig(st) {
        return;
    }
    match t {
        // Link/image pop the URL input box (not inserted here directly).
        Wt::Link => return open_link_dialog(st),
        Wt::Image => return open_image_dialog(st),
        _ => {}
    }
    let Some(cmd) = tool_cmd(t) else {
        return;
    };
    if let WysCmd::Table(wysiwyg::TableCmd::Align(a)) = &cmd {
        st.status = Some(
            match a {
                2 => Msg::WysTableAlignCenter,
                3 => Msg::WysTableAlignRight,
                _ => Msg::WysTableAlignLeft,
            }
            .into(),
        );
    }
    let Some(h) = handle() else {
        return;
    };
    apply(h, cmd);
    sync_to_doc(st);
    log_caret("toolbar");
}

/// **Pure mapping** for [`toolbar_apply`] (unit-testable): `WysTool` → wys kernel command.
/// `None` = no command to send (link/image go through a dialog; emoji index out of bounds).
fn tool_cmd(t: crate::ui::actions::WysTool) -> Option<WysCmd> {
    use crate::ui::actions::WysTool as Wt;
    Some(match t {
        Wt::Wrap(k) => WysCmd::Wrap(k),
        Wt::SetHead(n) => WysCmd::SetHead(n),
        Wt::ToggleList { ordered } => WysCmd::ToggleList { ordered },
        Wt::ToggleQuote => WysCmd::ToggleQuote,
        Wt::ToggleTask => WysCmd::ToggleTask,
        Wt::ToggleCodeBlock => WysCmd::ToggleCodeBlock,
        Wt::InsertHr => WysCmd::InsertHr,
        Wt::InsertBefore => WysCmd::InsertBefore,
        Wt::InsertAfter => WysCmd::InsertAfter,
        Wt::InsertTable { rows, cols } => WysCmd::InsertTable { rows, cols },
        Wt::Table(tc) => WysCmd::Table(tc),
        // Alignment goes through the kernel's `TableCmd::Align` (applies to the focused column,
        // enters the undo stack); the old host path that directly mutated `Td.align` (not in
        // undo) has been deleted.
        Wt::Align(a) => WysCmd::Table(wysiwyg::TableCmd::Align(a)),
        Wt::Indent => WysCmd::Indent,
        Wt::Outdent => WysCmd::Outdent,
        Wt::Emoji(i) => WysCmd::InsertText((*EMOJIS.get(i as usize)?).to_string()),
        Wt::Link | Wt::Image => return None,
    })
}

/// The focused cell's `(table_seq, row, column index)`; the column index = the focused cell's
/// ordinal among its row in the same table (same rule as the kernel's `InsertColAfter` scan).
/// `None` when the focused block is not a Td.
fn focus_cell() -> Option<(u64, usize, usize)> {
    let wys = WYS.read();
    let focus = wys.sel.focus;
    let (seq, row) = match wys.doc.blocks.get(focus.block)? {
        wysiwyg::Block::Td { table_seq, row, .. } => (*table_seq, *row),
        _ => return None,
    };
    let mut col = 0;
    for b in &wys.doc.blocks[..focus.block] {
        if let wysiwyg::Block::Td {
            table_seq: s,
            row: r,
            ..
        } = b
        {
            if *s == seq && *r == row {
                col += 1;
            }
        }
    }
    Some((seq, row, col))
}

/// Undo (**the single entry point** for keyboard Ctrl+Z / the toolbar button).
pub fn undo(st: &mut EditorState) {
    let Some(h) = handle() else {
        return;
    };
    let ok = WYS.signal().write().undo();
    if ok {
        REDO_DEPTH.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
    resync(h);
    sync_to_doc(st);
}

/// Redo (the single entry point for keyboard Ctrl+Y / Ctrl+Shift+Z / the toolbar button).
pub fn redo(st: &mut EditorState) {
    let Some(h) = handle() else {
        return;
    };
    let ok = WYS.signal().write().redo();
    if ok {
        redo_depth_take_one();
    }
    resync(h);
    sync_to_doc(st);
}

/// Takes one redone step off [`REDO_DEPTH`], saturating at zero.
///
/// Written as its own function so the `allow(deprecated)` covers exactly one
/// call: `fetch_update` does what we want, but toolchains newer than this
/// crate's 1.85 MSRV deprecate it in favour of `try_update` — a name that does
/// not exist on the MSRV, so the call cannot be renamed yet. Do not "fix" this
/// by bumping the number by hand: a plain `fetch_sub` would wrap 0 to
/// `usize::MAX` and the `> 0` probe would then never read false again.
#[allow(deprecated)]
fn redo_depth_take_one() {
    let _ = REDO_DEPTH.fetch_update(
        std::sync::atomic::Ordering::Relaxed,
        std::sync::atomic::Ordering::Relaxed,
        |v| Some(v.saturating_sub(1)),
    );
}

/// Toolbar undo (same entry point as Ctrl+Z, [`undo`]).
pub fn toolbar_undo(st: &mut EditorState) {
    undo(st);
}

/// Toolbar redo (same entry point as Ctrl+Y, [`redo`]).
pub fn toolbar_redo(st: &mut EditorState) {
    redo(st);
}

// ---- menu / context-menu rerouting (the wys-side targets of write-authority unification) ----

/// `Cmd::Copy` reroutes in wys mode: copy the wys selection.
pub fn wys_copy(_st: &mut EditorState) {
    copy_selection();
}

/// `Cmd::Cut` reroutes in wys mode: copy + delete the selection.
pub fn wys_cut(st: &mut EditorState) {
    copy_selection();
    let Some(h) = handle() else {
        return;
    };
    apply(h, WysCmd::DeleteSelection);
    sync_to_doc(st);
}

/// `Cmd::Paste` reroutes in wys mode: rich paste into the wys model (same entry point as
/// Ctrl+V, `paste_cmd`).
pub fn wys_paste(st: &mut EditorState) {
    let Some(c) = paste_cmd() else {
        return;
    };
    let Some(h) = handle() else {
        return;
    };
    apply(h, c);
    sync_to_doc(st);
}

/// `Cmd::Delete` reroutes in wys mode: delete the wys selection.
pub fn wys_delete_sel(st: &mut EditorState) {
    let Some(h) = handle() else {
        return;
    };
    apply(h, WysCmd::DeleteSelection);
    sync_to_doc(st);
}

/// `Cmd::SelectAll` reroutes in wys mode: select the whole wys document.
pub fn wys_select_all(st: &mut EditorState) {
    let Some(h) = handle() else {
        return;
    };
    apply(h, WysCmd::SelectAll);
    sync_to_doc(st);
}

/// The Signal-free version of `readonly_blocked` (the keyboard path holds `&mut Signal`, here
/// there is only `&mut EditorState` — same wording, write the state directly).
fn readonly_blocked_sig(st: &mut EditorState) -> bool {
    if st.read_only {
        st.status = Some(crate::ui::commands::READONLY_MSG.into());
        return true;
    }
    false
}

/// more▾ → export HTML: the wys content is rendered into a standalone HTML file (formulas /
/// code highlighting inlined; relative images resolve against the document's **containing
/// directory** — it used to pass the file path as the base, which made relative images parse
/// as `a.md\pic.png` and all break).
pub fn export_html(st: &mut EditorState) {
    let html = {
        let wys = WYS.read();
        let tab = st.tabs.get(st.active);
        let fg = st.theme().menu_fg;
        let base = tab
            .and_then(|t| t.doc.meta.path.clone())
            .and_then(|p| p.parent().map(ToOwned::to_owned));
        wysiwyg::preview_html(&wys.doc, fg, base.as_deref())
    };
    save_export(st, &html, "html");
}

/// more▾ → export Markdown: the wys content is emitted into a .md file.
pub fn export_md(st: &mut EditorState) {
    let md = emit_markdown();
    save_export(st, &md, "md");
}

/// Export file base name: strip only **one** extension layer (`a.b.md` → `a.b`).
/// `trim_end_matches('.')` used to eat all trailing dots ("backup.." → "backup"), and
/// stripping `.md` twice was confusing for `a.md.md` — delegate to
/// [`std::path::Path::file_stem`].
fn export_base_name(display_name: &str) -> String {
    let name = display_name.trim();
    let stem = std::path::Path::new(name)
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let base = if stem.is_empty() {
        name.to_string()
    } else {
        stem
    };
    if base.is_empty() {
        "export".to_string()
    } else {
        base
    }
}

/// Export save dialog + write to disk (does not touch the document itself; success only
/// updates the status bar).
fn save_export(st: &mut EditorState, content: &str, ext: &str) {
    let base = st
        .tabs
        .get(st.active)
        .map(|t| export_base_name(&t.doc.meta.display_name))
        .unwrap_or_else(|| "export".to_string());
    let dlg = winshell::dialog::owned(
        rfd::FileDialog::new()
            .set_title(st.tr(Msg::DlgExportTitle))
            .set_file_name(format!("{base}.{ext}"))
            .add_filter(ext, &[ext]),
    );
    let Some(p) = dlg.save_file() else {
        return;
    };
    match std::fs::write(&p, content) {
        Ok(_) => {
            st.status = Some(
                Fmt::WysExported {
                    path: p.display().to_string(),
                }
                .into(),
            )
        }
        Err(e) => st.status = Some(Fmt::WysExportFailed { err: e.to_string() }.into()),
    }
}

// ---- view components -----------------------------------------------------------

/// Composition-string mirror: render the IME preedit hugging the caret overlay — font size /
/// color at the caret + an underline (Windows IME convention); the position follows the caret
/// viewport snapshot taken at [`set_preedit`] time (during composition keys are swallowed and
/// the caret does not move). A separate component: a composition-string change only re-renders
/// it, not touching wys shard rendering. The model content never changes during composition
/// ([`composing_blocked`] swallows keys; Commit puts text on screen).
///
/// Not covered by the render layer's headless tests — manual verification: on Windows enter
/// wys mode and type pinyin; the composition string appears underlined at the caret, the
/// candidate window pops up at the caret, and after committing the composition string
/// disappears and the text lands in one shot.
#[component]
fn WysPreedit() -> Element {
    let state = use_context::<Signal<EditorState>>();
    let (text, pos) = {
        let p = PREEDIT.read();
        (p.text.clone(), p.pos)
    };
    let Some((x, y, h)) = pos else {
        return rsx! {};
    };
    // Font size / color at the caret (same source as the wys content: MdWys passes the same
    // fg/font_size to the kernel)
    let (fg, fs) = {
        let st = state.read();
        (st.theme().menu_fg, st.font_size)
    };
    let style = format!(
        "position:absolute; left:{x:.1}px; top:{y:.1}px; height:{h:.1}px; \
         line-height:{h:.1}px; font-size:{fs:.0}px; color:{fg}; \
         text-decoration:underline; white-space:pre; pointer-events:none;"
    );
    rsx! {
        div { style: "{style}", "{text}" }
    }
}

/// Document view (WYSIWYG): replaces the editor text box wholesale.
///
/// Consistency guard on mount: if the global wys editor's content does not match the current
/// document (a stale singleton left across tabs), reseed it — never letting another tab's
/// content be synced into this document.
#[component]
pub fn MdWys() -> Element {
    let mut state = use_context::<Signal<EditorState>>();
    // Subscribe to the image-failure version: the failed URLs are already written into the
    // render layer by overlays_frame; this read makes the component re-render the shard after
    // a bump (`<img>` replaced by the alt placeholder).
    let _img_epoch = IMG_EPOCH();
    // Host keyboard-edit version (see HOST_EPOCH): reading subscribes; after an edit it must
    // re-render.
    let _host_epoch = HOST_EPOCH();

    // Pure read snapshot (render-phase discipline: read-only, no signal writes, no touching
    // the blitz document)
    let (bg, fg, caret_color, fs, vw, vh, vtop, vx, blink, caret_phase) = {
        let st = state.read();
        let t = st.theme();
        (
            t.bg,
            t.menu_fg,
            t.caret,
            st.font_size,
            st.view.w.max(200.0),
            st.view.h.max(40.0),
            st.view.y,
            st.view.x,
            st.caret_blink,
            st.caret_phase,
        )
    };
    // Theme variables: content foreground/caret color follow the theme (wys's SHELL_CSS picks
    // up var(--wys-fg/--wys-caret), defaulting to the light theme's dark text); the wys-root
    // has overflow:auto (content scrolling is taken over by blitz; the container has definite
    // pixel width/height and does not rely on the flex chain that misbehaves in the demo).
    // Caret blinking uses a phase variable (`--wys-caret-op`; the BlinkTick thread flips it
    // every 265ms and the frame hook writes back `caret_phase`): a CSS infinite animation
    // **must not** be used — blitz self-sustains a redraw every frame for windows with running
    // animations (see the winshell bridge module comments).
    let caret_op = if blink && !caret_phase { 0.0 } else { 1.0 };
    let blink_vars = format!("--wys-caret-op:{caret_op};");
    // Line wash of the source expansion (a revealed block is tinted so the reader can
    // see which lines the sweep turned into source): the theme accent at 10%, so it
    // works on both the light and the dark surface.
    let tint = mdselbar::rgba(state.read().theme().accent, 0.10);
    let root_style = format!(
        "position:absolute; top:{vtop}px; left:{vx}px; width:{vw}px; height:{vh}px; \
         background:{bg}; overflow:hidden; --wys-fg:{fg}; --wys-caret:{caret_color}; \
         --wys-tint:{tint}; {blink_vars}"
    );
    // Relative images resolve against the document's **containing directory** (same rule as
    // export_html/preview); an unsaved new document has no path → None (alt placeholder, no
    // broken image). The same base resolves relative *link* targets on Ctrl+click.
    let img_base = {
        let st = state.read();
        st.tabs
            .get(st.active)
            .and_then(|t| t.doc.meta.path.clone())
            .and_then(|p| p.parent().map(ToOwned::to_owned))
    };
    let link_img_base = img_base.clone();

    rsx! {
        div {
            style: "{root_style}",
            wysiwyg::Wysiwyg {
                editor: WYS.signal(),
                overlays: OVERLAYS.signal(),
                grab_focus: false,
                font_size: Some(fs as i32),
                fg: Some(fg.to_string()),
                img_base,
                // Ctrl+click on a rendered link opens it outside the app (the
                // whitelist and the relative resolution live in
                // `open_external_link`; without this wiring the callback was
                // None and the click did nothing at all).
                on_open_link: move |url: String| {
                    let mut st = state.write();
                    open_external_link(&mut st, &url, link_img_base.as_deref());
                },
                on_ready: move |h: Rc<NodeHandle>| {
                    *HANDLE.signal().write() = Some(h);
                    // Consistency guard (event phase): content not matching the document = a
                    // stale singleton
                    let st = state.read();
                    let text = active_text(&st);
                    drop(st);
                    if !session_matches_doc(&state.read()) || !content_eq(&text, &emit_markdown()) {
                        seed_from_doc(&state.read());
                    }
                },
                // Mouse edits (task-box checking etc.): the component does not go through the
                // host's keyboard bridge and must notify actively, or the change only lives in
                // the wys model (lost on save, the title is not dirtied).
                on_edit: move |_| {
                    let st = state.read();
                    note_edit(&st);
                },
                // Mouse click / drag-select / double-click: align the hidden caret to the wys
                // selection so the status bar's line/column follows immediately (without the
                // notification it would wait for the next keyboard move).
                on_caret: move |_| {
                    let mut st = state.write();
                    // A press into the content is an outside click for the menus:
                    // the menu-bar dropdown (and the in-page context menu) must
                    // close, same as the composer's pointer handler. The wys view
                    // used to leave the menu hanging — the panel is a positioned
                    // div with no backdrop, so a click here reached nothing that
                    // closed it.
                    st.menu_close();
                    align_status(&mut st);
                },
                // Press/release around a possible drag selection: the frame hook
                // needs to know a drag is live to auto-scroll past the edges
                // (blitz's native drag stops at the element boundary — see the
                // prop's doc).
                on_drag: move |down: bool| {
                    state.write().md_drag = down;
                },
            }
            // Composition-string mirror: after wys-root = floating above it; pointer
            // pass-through lets clicks reach the wys content as usual.
            WysPreedit {}
            // Hint auto-complete candidate panel: below the caret overlay
            mdhint::WysHint {}
            // Selection floating toolbar: above the selection's first rect
            mdselbar::WysSelBar {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::common_span;

    /// Reconstruct by the minimal-diff three segments (prefix + new middle + suffix); should
    /// restore to new.
    fn apply(old: &str, new: &str) -> String {
        let (p, s) = common_span(old, new);
        let mut o = old[..p].to_string();
        o.push_str(&new[p..new.len() - s]);
        o.push_str(&old[old.len() - s..]);
        o
    }

    #[test]
    fn common_span_reconstructs_new() {
        for (old, new) in [
            ("hello", "hello world"),
            ("hello world", "hello"),
            ("para one\n\npara two", "para one!\n\npara two"),
            ("# 标题\n\n正文", "# 标题改\n\n正文"),
            ("abc", "xyz"),
            ("", "new"),
            ("old", ""),
        ] {
            assert_eq!(apply(old, new), new, "old={old:?} new={new:?}");
        }
    }

    #[test]
    fn common_span_utf8_safe() {
        let old = "汉a字";
        let new = "汉b字";
        let (p, s) = common_span(old, new);
        assert!(old.is_char_boundary(p) && new.is_char_boundary(p));
        assert!(old.is_char_boundary(old.len() - s) && new.is_char_boundary(new.len() - s));
        assert_eq!(apply(old, new), new);
    }

    // ---- export file name strips only one extension layer ---------------------

    #[test]
    fn export_base_name_strips_only_one_extension() {
        use super::export_base_name;
        assert_eq!(export_base_name("report.md"), "report");
        // Only one layer: "a.b.md" → "a.b" (used to be confused by trim_end_matches('.')
        // semantics)
        assert_eq!(export_base_name("a.b.md"), "a.b");
        // Trailing dots must not all be eaten
        assert_eq!(export_base_name("备份.."), "备份.");
        assert_eq!(export_base_name("备份."), "备份");
        // No extension / empty name fallback
        assert_eq!(export_base_name("README"), "README");
        assert_eq!(export_base_name(""), "export");
    }

    // ---- inline format counts at the caret -------------------------------------

    #[test]
    fn marks_at_reports_inline_wraps_at_caret() {
        use super::{marks_at, InlineMarks};
        use wysiwyg::Inline;
        // "ab**cd**e": ab=Text, cd=Strong, e=Text
        let inlines = vec![
            Inline::Text("ab".into()),
            Inline::Strong(vec![Inline::Text("cd".into())]),
            Inline::Text("e".into()),
        ];
        // Caret inside ab (0/1): no format
        assert_eq!(marks_at(&inlines, 0), InlineMarks::default());
        assert_eq!(marks_at(&inlines, 1), InlineMarks::default());
        // Caret inside cd (including both inner boundaries 2/4): bold
        for off in [2, 3, 4] {
            assert!(marks_at(&inlines, off).bold, "off={off}");
        }
        // Caret after the trailing e: not lit
        assert!(!marks_at(&inlines, 5).bold);
        // Nesting: `*[a`code`]*` → em + code
        let nested = vec![Inline::Em(vec![
            Inline::Text("a".into()),
            Inline::Code("code".into()),
        ])];
        assert!(marks_at(&nested, 2).italic && marks_at(&nested, 2).code);
        assert!(!marks_at(&nested, 2).bold);
        // Link lights up link
        let linked = vec![Inline::Link {
            url: "u".into(),
            inlines: vec![Inline::Text("L".into())],
        }];
        assert!(marks_at(&linked, 0).link);
    }

    // ---- IME composition-string mirror: state update/clear ---------------------

    #[test]
    fn preedit_mirror_updates_and_clears() {
        use super::{preedit_text, PreeditMirror};
        // Normal composition: mirror the original text (the Some range is the candidate caret's
        // position inside the string and does not affect the text)
        assert_eq!(preedit_text("zhong", Some((0, 5))), "zhong");
        assert_eq!(preedit_text("拼", Some((1, 1))), "拼");
        // Clearing rules: winit's `(_, None)` = composition canceled; empty string =
        // composition finished
        assert_eq!(preedit_text("zhong", None), "");
        assert_eq!(preedit_text("", Some((0, 0))), "");
        // State machine: while composing, carry the caret snapshot (mirror rendering); after
        // clearing, back to default (empty text + pos None → WysPreedit renders nothing)
        let composing = PreeditMirror {
            text: preedit_text("ni", Some((0, 2))),
            pos: Some((10.0, 20.0, 16.0)),
        };
        assert_eq!(composing.text, "ni");
        assert!(composing.pos.is_some());
        let cleared = PreeditMirror {
            text: preedit_text("", None),
            pos: None,
        };
        assert_eq!(cleared, PreeditMirror::default());
    }

    // ---- debounce merging --------------------------------------------------

    #[test]
    fn debounce_merges_edit_burst_until_idle() {
        use super::{debounce_due, SYNC_DEBOUNCE_MS};
        let t0 = 1_000_000u64;
        // Burst typing: each edit only postpones last, so it never expires inside the window
        assert!(
            !debounce_due(t0 + 50, t0),
            "must not fire inside the window"
        );
        assert!(
            !debounce_due(t0 + 200, t0 + 150),
            "must not fire after postponing"
        );
        // Only write back after one full idle window (a single merge)
        assert!(debounce_due(t0 + SYNC_DEBOUNCE_MS, t0));
        assert!(debounce_due(t0 + SYNC_DEBOUNCE_MS + 1, t0));
        // A rewound clock does not misfire
        assert!(!debounce_due(t0.saturating_sub(100), t0));
    }

    // ---- line endings (CRLF documents) must not be dirtied / rewritten by pure moves --------

    /// Logical equivalence: `\r\n` and `\n` are treated as the same shape; an isolated `\r` is
    /// not stripped.
    #[test]
    fn crlf_eq_ignores_newline_representation_only() {
        use super::crlf_eq;
        assert!(crlf_eq("a\r\nb\r\n", "a\nb\n"));
        assert!(crlf_eq("a\r\nb", "a\nb"));
        // A real content difference still reports unequal
        assert!(!crlf_eq("a\r\nb\r\n", "a\nb!\n"));
        assert!(!crlf_eq("a\r\nb\r\n", "a\nb"));
        // A pure-LF document has no `\r`: unequal directly (the equivalence does not relax)
        assert!(!crlf_eq("a\nb\n", "a\nb\n"));
        // Isolated CR (old-Mac line ending): also a line terminator in markdown, equivalent to
        // `\n`
        assert!(crlf_eq("a\rb\r", "a\nb\n"));
        // But a line mixing CRLF and CR internally is still a "content difference" and reports
        // unequal
        assert!(!crlf_eq("a\rb\r", "a\nb"));
    }

    /// `split_lines`: all three terminators + the last line are cut correctly; joining them back
    /// reproduces the original byte for byte.
    #[test]
    fn split_lines_roundtrips_all_terminators() {
        use super::split_lines;
        let rebuild = |s: &str| {
            split_lines(s)
                .iter()
                .map(|(c, t)| format!("{c}{t}"))
                .collect::<String>()
        };
        for s in [
            "a\nb\n",
            "a\r\nb\r\n",
            "a\rb\r",
            "a\r\nb\nc\r",
            "",
            "no-eol",
            "混合\r\n行尾\n都\r在",
        ] {
            assert_eq!(rebuild(s), s, "rebuild restores original bytes: {s:?}");
        }
        assert_eq!(
            split_lines("a\r\nb\r"),
            vec![("a", "\r\n"), ("b", "\r"), ("", "")]
        );
    }

    /// emit output restores the original line endings: unmodified lines keep their own
    /// terminator, modified lines use the dominant one.
    #[test]
    fn preserve_eol_keeps_untouched_lines_verbatim() {
        use super::preserve_eol;
        use crate::buffer::Newline;
        // CRLF document: new blocks use CRLF
        assert_eq!(
            preserve_eol("a\r\nb\r\n", "a\nb\nc\n", Newline::CrLf),
            "a\r\nb\r\nc\r\n"
        );
        // Isolated-CR (old Mac) document: new blocks are CR, not the whole text converted to LF
        assert_eq!(
            preserve_eol("a\rb\r", "a\nb\nc\n", Newline::Cr),
            "a\rb\rc\r"
        );
        // Mixed endings: line b is LF and unmodified → kept as-is; only the modified line c
        // uses the dominant ending
        assert_eq!(
            preserve_eol("a\r\nb\nc\r\n", "a\nb\nC\n", Newline::CrLf),
            "a\r\nb\nC\r\n"
        );
        // Deleting a line: the remaining lines keep their own endings
        assert_eq!(
            preserve_eol("a\r\nb\nc\r\n", "a\nc\n", Newline::CrLf),
            "a\r\nc\r\n"
        );
        // Old text with no trailing newline: the md's trailing newline follows emit's form,
        // not guessed
        assert_eq!(
            preserve_eol("a\r\nb", "a\nb\n", Newline::CrLf),
            "a\r\nb\r\n"
        );
    }

    /// Regression: a CRLF document entering WYS and only moving the caret → not dirtied, text
    /// unchanged byte for byte;
    /// a real edit → only the content changes, line endings stay CRLF.
    /// Regression: `wys → text → wys` used to reset the caret to the document start,
    /// so the next keystroke landed at the beginning (reported as "type a space,
    /// check the text view, come back → the caret position is wrong"). The seed must
    /// carry the host caret into the wys model in emit coordinates.
    #[test]
    fn wys_seed_restores_caret_from_the_host_document() {
        use dioxus_native::prelude::dioxus_core::{RuntimeGuard, VirtualDom};
        fn empty_app() -> dioxus_native::prelude::Element {
            use dioxus_native::prelude::*;
            rsx! {}
        }
        let vdom = VirtualDom::new(empty_app);
        let _guard = RuntimeGuard::new(vdom.runtime());
        use super::{seed_from_doc, WYS};
        use crate::ui::testutil::st_of;
        use dioxus_native::prelude::{ReadableExt, WritableExt};

        let mut st = st_of("第一段\n\n第二段 abc\n");
        st.set_language_by_name("Markdown");
        // Caret at the end of the second paragraph (before the trailing newline).
        st.caret_mut().pos = st.tab().doc.total_bytes() - 1;
        seed_from_doc(&st);
        {
            let w = WYS.read();
            assert_eq!(
                w.sel.focus.block, 1,
                "the caret must stay in the second block"
            );
            assert_eq!(
                w.sel.focus.off, 13,
                "the caret must stay at the paragraph end"
            );
            assert_eq!(w.doc.blocks[1].plain_text(), "第二段 abc");
        }
        // The next keystroke appends at that caret instead of landing at the top.
        WYS.signal()
            .write()
            .apply(wysiwyg::edit::Cmd::InsertText(" ".into()));
        assert_eq!(WYS.read().doc.blocks[1].plain_text(), "第二段 abc ");
    }

    #[test]
    fn wys_sync_preserves_crlf_and_ignores_pure_move() {
        use dioxus_native::prelude::dioxus_core::{RuntimeGuard, VirtualDom};
        fn empty_app() -> dioxus_native::prelude::Element {
            use dioxus_native::prelude::*;
            rsx! {}
        }
        let vdom = VirtualDom::new(empty_app);
        let _guard = RuntimeGuard::new(vdom.runtime());

        use super::{seed_from_doc, sync_to_doc, WYS};
        use crate::ui::testutil::st_of;
        use dioxus_native::prelude::WritableExt;
        let src = "# t\r\n\r\n- a\r\n- b\r\n";
        let mut st = st_of(src);
        st.set_language_by_name("Markdown");
        if !st.tab().md_wys {
            st.md_toggle_wys();
        }
        seed_from_doc(&st);
        let text = |st: &crate::ui::state::EditorState| {
            let d = st.tab().doc.text_range(0..st.tab().doc.total_bytes());
            (d, st.tab().doc.is_dirty())
        };
        // Pure move (End/arrow path): equivalent but different shape → zero writes, return
        // directly
        sync_to_doc(&mut st);
        let (t, dirty) = text(&st);
        assert_eq!(t, src, "pure move rewrote the text");
        assert!(!dirty, "pure move dirtied the document");
        // Real edit (adding a block = emit inserts a new line): content changes, endings stay
        // CRLF
        *WYS.signal().write() =
            super::WysEditor::new(wysiwyg::parse_doc("# t\r\n\r\n- a\r\n- b\r\n\r\nnew\r\n"));
        sync_to_doc(&mut st);
        let (t, dirty) = text(&st);
        assert!(dirty, "a real edit must dirty the document");
        assert_eq!(
            t, "# t\r\n\r\n- a\r\n- b\r\n\r\nnew\r\n",
            "line endings were rewritten to LF"
        );
    }

    /// Isolated-CR (old Mac) and mixed-ending documents: unmodified lines stay byte-identical
    /// after an edit.
    #[test]
    fn wys_sync_preserves_cr_and_mixed_eol() {
        use dioxus_native::prelude::dioxus_core::{RuntimeGuard, VirtualDom};
        fn empty_app() -> dioxus_native::prelude::Element {
            use dioxus_native::prelude::*;
            rsx! {}
        }
        let vdom = VirtualDom::new(empty_app);
        let _guard = RuntimeGuard::new(vdom.runtime());
        use super::{seed_from_doc, sync_to_doc, WYS};
        use crate::ui::testutil::st_of;
        use dioxus_native::prelude::WritableExt;

        for (src, edited, want) in [
            // Isolated CR: pure move writes nothing; new lines after an edit are also CR
            ("# t\r\r- a\r", "# t\r\r- a\r\rnew\r", "# t\r\r- a\r\rnew\r"),
            // Mixed endings: only the modified first line participates in the replacement; the
            // `para` line stays LF
            (
                "# t\r\n\r\npara\n\r\n- a\r\n",
                "# T\r\n\r\npara\n\r\n- a\r\n",
                "# T\r\n\r\npara\n\r\n- a\r\n",
            ),
        ] {
            let mut st = st_of(src);
            st.set_language_by_name("Markdown");
            if !st.tab().md_wys {
                st.md_toggle_wys();
            }
            seed_from_doc(&st);
            sync_to_doc(&mut st);
            let doc_text = |st: &crate::ui::state::EditorState| {
                st.tab().doc.text_range(0..st.tab().doc.total_bytes())
            };
            assert_eq!(doc_text(&st), src, "pure move rewrote the text: {src:?}");
            assert!(
                !st.tab().doc.is_dirty(),
                "pure move dirtied the doc: {src:?}"
            );
            *WYS.signal().write() = super::WysEditor::new(wysiwyg::parse_doc(edited));
            sync_to_doc(&mut st);
            assert!(
                st.tab().doc.is_dirty(),
                "a real edit must dirty the doc: {src:?}"
            );
            assert_eq!(
                doc_text(&st),
                want,
                "line ending shape was rewritten: {src:?}"
            );
        }
    }

    // ---- cross-tab pollution + three-state switching (same test serially shares the WYS
    // singleton) ---------

    /// Content equality is not session identity: two tabs can hold the same Markdown while
    /// their undo stacks differ. Switching must start a fresh WYS session for the new document,
    /// otherwise Ctrl+Z in tab B can replay tab A's history into B.
    #[test]
    fn same_content_tab_switch_does_not_inherit_the_previous_undo_stack() {
        use dioxus_native::prelude::dioxus_core::{RuntimeGuard, VirtualDom};
        fn empty_app() -> dioxus_native::prelude::Element {
            use dioxus_native::prelude::*;
            rsx! {}
        }
        let vdom = VirtualDom::new(empty_app);
        let _guard = RuntimeGuard::new(vdom.runtime());

        use super::{emit_markdown, seed_from_doc, session_matches_doc, WysCmd, WYS};
        use crate::buffer::TextDocument;
        use crate::ui::commands::testutil::st_of;
        use crate::ui::state::Tab;
        // Reading the global signal needs the signal trait in scope (the parent
        // module gets it through `dioxus_native::prelude::*`).
        use dioxus_native::prelude::ReadableExt;

        let mut st = st_of("# Same\n");
        st.set_language_by_name("Markdown");
        st.tab_mut().md_wys = true;
        seed_from_doc(&st);
        {
            let mut ed = WYS.write();
            ed.apply(WysCmd::InsertText("Z".into()));
            ed.apply(WysCmd::DeleteBack);
            assert!(ed.can_undo(), "the first tab must have a real undo history");
        }
        assert_eq!(emit_markdown(), "# Same\n");

        // Same text, different document id.
        let mut second = TextDocument::from("# Same\n");
        second.meta.display_name = "second.md".into();
        let mut second_tab = Tab::with_spec(second, st.spec());
        second_tab.md_wys = true;
        st.tabs.push(second_tab);
        st.set_active_tab(1);

        assert!(
            session_matches_doc(&st),
            "the WYS session must be bound to the new document id"
        );
        assert!(
            !WYS.read().can_undo(),
            "the previous tab's undo stack leaked into a same-content document"
        );
        assert_eq!(st.active_doc().read_all(), "# Same\n");
        assert!(!st.active_doc().is_dirty());
    }

    #[test]
    fn cross_tab_guard_reseeds_and_three_states_are_exclusive() {
        // The WYS global singleton is a GlobalSignal; reading/writing it needs the dioxus
        // runtime — borrow the runtime with a VirtualDom of an empty component (`Runtime::new`
        // is crate-private; the only public entry is VirtualDom).
        use dioxus_native::prelude::dioxus_core::{RuntimeGuard, VirtualDom};
        fn empty_app() -> dioxus_native::prelude::Element {
            use dioxus_native::prelude::*;
            rsx! {}
        }
        let vdom = VirtualDom::new(empty_app);
        let _guard = RuntimeGuard::new(vdom.runtime());
        use super::emit_markdown;
        use crate::buffer::TextDocument;
        use crate::ui::commands::testutil::st_of;
        let mut st = st_of("# A\n");
        st.set_language_by_name("Markdown");
        // Second tab (also entering wys): the singleton is seeded as B
        st.new_doc();
        st.tabs[st.active].doc = TextDocument::from("# B\n");
        st.set_language_by_name("Markdown");
        st.md_toggle_wys();
        assert!(st.tab().md_wys, "B enters wys");
        let b_md = emit_markdown();
        assert!(b_md.contains('B'), "{b_md}");
        // Simulate A having entered wys too; switch back to A: the guard must reseed the
        // singleton as A
        st.tabs[0].md_wys = true;
        st.set_active_tab(0);
        let a_md = emit_markdown();
        assert!(
            a_md.contains('A') && !a_md.contains('B'),
            "after switching back to A the singleton is still B: {a_md}"
        );
        // Two-state mutual exclusion: wys → text (MdText) → wys
        st.md_show_text();
        assert!(!st.tab().md_wys, "text view: the flag is off");
        st.md_toggle_wys();
        assert!(st.tab().md_wys);
    }

    // ---- table alignment command routing (toolbar → kernel `TableCmd::Align`) ----------------

    #[test]
    fn toolbar_align_routes_to_kernel_table_cmd() {
        use super::{tool_cmd, WysCmd};
        use crate::ui::actions::WysTool as Wt;
        // Toolbar left/center/right and the generic table entry all converge on the kernel's
        // Align (entering the undo stack)
        for a in 1..=3u8 {
            let want = Some(WysCmd::Table(wysiwyg::TableCmd::Align(a)));
            assert_eq!(tool_cmd(Wt::Align(a)), want, "Wt::Align({a})");
            assert_eq!(
                tool_cmd(Wt::Table(wysiwyg::TableCmd::Align(a))),
                want,
                "Wt::Table(Align({a}))"
            );
        }
        // Link/image use the URL input box, emoji index out of bounds: no command to send
        assert_eq!(tool_cmd(Wt::Link), None);
        assert_eq!(tool_cmd(Wt::Image), None);
        assert_eq!(tool_cmd(Wt::Emoji(u8::MAX)), None);
    }

    // ---- plain-URL test for pasting a URL over a selection into a link --------------------

    #[test]
    fn is_plain_url_accepts_http_https_file_only() {
        use super::is_plain_url;
        assert!(is_plain_url("https://example.com/a?b=1"));
        assert!(is_plain_url(" http://x.y "));
        assert!(is_plain_url("file:///C:/tmp/a.png"));
        assert!(!is_plain_url("ftp://x.y"), "scheme outside the whitelist");
        assert!(!is_plain_url("javascript:alert(1)"), "dangerous scheme");
        assert!(
            !is_plain_url("https://x.y/z w"),
            "contains whitespace, not a plain URL"
        );
        assert!(!is_plain_url("not a url"));
        assert!(!is_plain_url("https://"), "empty path");
    }
}

#[cfg(test)]
mod overlays_frame_tests {
    //! Headless regression for the event-phase overlay frame hook (2026-10-17 startup crash).
    //!
    //! `overlays_frame` is driven by `RedrawRequested` (a raw winit callback) and reads/writes
    //! a pile of `GlobalSignal`s. The host enters the runtime through winshell's window runtime
    //! guard; here the same posture (`RuntimeGuard`) reproduces it, verifying that neither
    //! branch panics and that the non-wys view really closes the panels — this path had no
    //! headless coverage at all before, which is how the crash leaked all the way to real
    //! hardware.
    use super::*;
    use dioxus_native::prelude::dioxus_core::{RuntimeGuard, VirtualDom};

    fn empty_app() -> Element {
        rsx! {}
    }

    fn hint_state(prefix: &str) -> mdhint::HintState {
        mdhint::HintState {
            kind: mdhint::HintKind::Emoji,
            prefix: prefix.to_string(),
            sel: 0,
            pos: Some((12.0, 24.0, 18.0)),
            scroll: (0.0, 0.0),
        }
    }

    #[test]
    fn overlays_frame_clears_overlays_outside_wys() {
        let vdom = VirtualDom::new(empty_app);
        let _guard = RuntimeGuard::new(vdom.runtime());
        mdhint::set_state(Some(hint_state("smi")));
        assert!(mdhint::open(), "precondition: panel open");
        overlays_frame(false, false);
        assert!(
            !mdhint::open(),
            "non-wys view must close the candidate panel"
        );
        assert!(
            !mdselbar::current().show,
            "floating toolbar likewise resets"
        );
        // Idempotence: calling again after everything is closed writes nothing (conditional
        // write iron rule)
        overlays_frame(false, false);
        assert!(!mdhint::open());
    }

    #[test]
    fn overlays_frame_tolerates_missing_wys_handles() {
        // wys view but the kernel handle is not ready yet (`HANDLE` = None, first frame /
        // remount window): every geometry query misses; the hook must return safely, not panic.
        let vdom = VirtualDom::new(empty_app);
        let _guard = RuntimeGuard::new(vdom.runtime());
        assert!(handle().is_none(), "precondition: no handle");
        mdhint::set_state(Some(hint_state("ru")));
        overlays_frame(true, false);
        // Caret anchor not found → inconsistent with the panel snapshot → close the panel (not
        // left at the wrong position)
        assert!(!mdhint::open(), "lost anchor must close the panel");
    }
}
