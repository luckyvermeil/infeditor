//! Application state for the editor: documents, carets, scrolling, metrics.
//!
//! The type is one flat `EditorState`; its behaviour lives in focused files:
//! `dialogs` (in-app modals), `find_state` (find bar), [`menu`] (menu
//! bar), `opens` (background opens & native-dialog pump) and `docs`
//! (tabs & file actions), plus the command modules under
//! [`crate::ui::commands`].

mod dialogs;
mod docs;
mod find_state;
pub mod heal;
mod hidden;
pub mod json;
pub mod jsonl;
mod marks;
pub mod menu;
mod opens;
pub mod outline;
mod popups;
pub mod workspace;
pub mod wrap;

pub mod csv;

pub use dialogs::{Dialog, DialogBody, DialogButton, DialogInput};
pub use find_state::{FindFocus, FindState};
pub use hidden::HiddenCache;
pub use marks::Mark;
pub use opens::{
    human_bytes, opening_status, IndexJob, OpenIntent, OpenProgress, PendingOpen, WakeFn,
};
pub use outline::{OutlineCache, OutlineItem};
pub use popups::{AboutWinState, NumField, NumWinState, PrefsWinState};
pub use workspace::{WsNode, WsRow};
pub use wrap::WrapIndex;

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::rc::Rc;

use crate::buffer::TextDocument;
use crate::highlight::{for_path_and_first_line, Highlighter, LangSpec, Theme};
use crate::ui::metrics::{metrics_for_family, Metrics};
use crate::ui::{Fmt, Msg};

// ---------------------------------------------------------------------------
// Conditional write helper
// ---------------------------------------------------------------------------

/// Conditional write: probe read-only first, take the write lock only when a
/// change is needed.
///
/// Why this must replace a bare `Signal::write()`: this project's Signal write
/// guard unconditionally calls mark_dirty on drop, so even a "changed nothing"
/// write triggers a full app re-render. High-frequency event handlers
/// (pointermove / pointerup / wheel / mouseleave / render-time) must `read()`
/// first and only then decide to `write()`. Two review rounds fixed the same
/// pattern; this funnels it into one entry point so new code stops hand-rolling it.
///
/// ```ignore
/// update_if(&mut state, |st| st.drag, |st| st.drag = false);
/// ```
pub fn update_if(
    state: &mut dioxus_native::prelude::Signal<EditorState>,
    needs_change: impl FnOnce(&EditorState) -> bool,
    change: impl FnOnce(&mut EditorState),
) {
    use dioxus_native::prelude::*; // read/write on Signal need the ext trait in scope
    if needs_change(&state.read()) {
        change(&mut state.write());
    }
}

/// Legacy gutter floor (px). No longer used by [`EditorState::gutter_w`]
/// (which now hugs the real line-number width); kept because several older
/// geometry tests still reference it.
pub const GUTTER: f32 = 40.0;
/// Top padding of the text area (px).
pub const PAD_TOP: f32 = 6.0;
/// Left padding of the text area inside its column (px).
pub const PAD_LEFT: f32 = 6.0;
/// Default tab width in columns.
pub const TAB_SIZE: usize = 4;
/// Height of the merged title+menu bar (px).
///
/// One bar that carries the window controls, the app icon, the
/// document title and the whole menu tree. It replaced the old 24px menu bar
/// that sat *below* a native OS title bar.
pub const TITLEBAR_H: f32 = 36.0;
/// Width of one window-control button (minimize / maximize / close).
pub const WINBTN_W: f32 = 46.0;
/// Height of the tab bar (px).
pub const TABS_H: f32 = 28.0;
/// Height of the status bar (px).
pub const STATUS_H: f32 = 22.0;
/// Vertical space used by the chrome above the editor viewport.
pub const CHROME_TOP: f32 = TITLEBAR_H + TABS_H;
/// Thickness of the editor's own scrollbars (px). Mirrors
/// [`crate::ui::scrollbar::BAR`]; it lives here because the viewport geometry
/// (`EditorState`) needs it and `state` must not depend on `components`.
pub const SCROLLBAR_W: f32 = 14.0;
/// Width of the minimap column (px). Mirrors [`crate::ui::minimap::MM_W`]; it
/// lives here for the same reason `SCROLLBAR_W` does — the viewport geometry
/// needs it and `state` must not depend on the view modules.
pub const MINIMAP_W: f32 = 84.0;

/// Width of the fold-marker column (px): a vertical slot to the **right** of
/// the line numbers where the JSON tree / JSONL fold buttons are drawn
/// uniformly (VS Code's folding gutter aligns on the same side of the full
/// column, so deeply indented trees never push the buttons far away). Added to
/// the line-number slot width when a foldable view is active; see
/// [`EditorState::gutter_w`] for the drawn width and
/// [`EditorState::fold_marker_row_at`] for the click test over that slot.
pub const MARKER_W: f32 = 16.0;
/// Height of the CSV/Markdown toolbar (px). Docked under the tab strip
/// (below the find bar when that is open) and subtracted from the viewport
/// by [`EditorState::relayout`].
pub const TOOLBAR_H: f32 = 32.0;
/// Workspace sidebar width (px). Fixed width (no drag-resizing); claimed from
/// the viewport width by [`EditorState::relayout`]; see the [`workspace`]
/// module.
pub const SIDEBAR_W: f32 = 220.0;
/// Line numbers are shown verbatim up to this many digits; beyond it the
/// gutter switches to scientific notation ([`format_line_no`]).
const MAX_PLAIN_DIGITS: u64 = 10_000_000_000;
/// Height of one row of the find/replace bar (px).
pub const FIND_ROW_H: f32 = 30.0;
/// Height of the "find all" results panel (px).
pub const FIND_PANEL_H: f32 = 150.0;
/// Max number of matches collected by Find All / Replace All.
pub const FIND_LIMIT: usize = 50_000;

/// Number format for the line-number gutter: shown verbatim up to 10 digits,
/// then switches to scientific notation (e.g. line 12 345 678 901 renders as
/// `1.23e10`). The gutter width is sized from this accordingly.
pub fn format_line_no(n: u64) -> String {
    if n < MAX_PLAIN_DIGITS {
        n.to_string()
    } else {
        // two decimal places: `1.23e10`
        format!("{:.2e}", n as f64)
    }
}

/// The **character count** [`format_line_no`] produces (pure arithmetic,
/// zero allocation).
///
/// Gutter width = char count × advance, and it sits on high-frequency paths
/// (pointer movement / relayout). Counting chars used to `format!` the whole
/// string and `.chars().count()` it — two heap allocations per frame. The
/// same formula is now computed directly:
/// - `n < 10^10`: decimal digit count;
/// - otherwise scientific `d.deXX`: 1 integer digit + `.` + 2 decimals + `e`
///   + the exponent's digit count.
pub fn line_no_digits(n: u64) -> usize {
    let mut digits = 1usize;
    let mut v = n / 10;
    while v > 0 {
        digits += 1;
        v /= 10;
    }
    if n < MAX_PLAIN_DIGITS {
        digits
    } else {
        // exponent = digits - 1 (floor(log10)), then count the exponent's digits
        let mut exp_digits = 1usize;
        let mut e = (digits - 1) / 10;
        while e > 0 {
            exp_digits += 1;
            e /= 10;
        }
        1 + 1 + 2 + 1 + exp_digits
    }
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct Caret {
    /// byte offset of the caret within the document
    pub pos: u64,
    /// selection anchor (`Some` while selecting); `pos != anchor` = selection
    pub anchor: Option<u64>,
}

impl Caret {
    /// The selected byte range, if any.
    pub fn selection(&self) -> Option<std::ops::Range<u64>> {
        let a = self.anchor.filter(|a| *a != self.pos)?;
        if a < self.pos {
            Some(a..self.pos)
        } else {
            Some(self.pos..a)
        }
    }
}

/// One open tab: the document plus everything that lives *with* it — its
/// caret, its detected language and its highlighter.
///
/// Bundling these four values is the load-bearing architectural decision of
/// the tab model: they used to be four parallel `Vec`s on [`EditorState`] that
/// every open/close/reload had to keep in lock-step by hand (with `if i <
/// self.specs.len()` guards to paper over the desyncs that caused). Here a tab
/// is born, moved and dies as one value, and the caret lookup is a field
/// access instead of an O(n) scan by document id on every keystroke.
pub struct Tab {
    pub doc: TextDocument,
    pub caret: Caret,
    pub spec: &'static LangSpec,
    pub hl: Highlighter,
    /// Virtual-line state of the JSONL preview (enabled by default only for
    /// "JSON Lines" documents)
    pub jsonl: jsonl::JsonlView,
    /// Whole-document JSON tree view preview (**on by default for a valid,
    /// in-cap .json document** — see [`json::JsonView::for_doc`]; oversized or
    /// unparseable documents fall back to text; toggled from the toolbar /
    /// view menu)
    pub json: json::JsonView,
    /// CSV toolbar + table view state (on by default for csv/tsv documents,
    /// see [`csv::CsvView`])
    pub csv: csv::CsvView,
    /// Markdown document view (WYSIWYG): the view routes to the `Wysiwyg`
    /// component in `crates/wysiwyg` (**on by default for Markdown documents
    /// within the visual-default cap** — oversized files open as source;
    /// toggled from the toolbar "document view" switch; see
    /// [`crate::ui::components::mdws`]). Content is owned by the wys editor
    /// and written back to the piece table via `mdws::sync_to_doc` **after
    /// every edit** (so dirty flags / autosave / recovery keep working).
    pub md_wys: bool,
    /// Document outline cache (data for the sidebar "Outline" panel,
    /// invalidated by the gen key; see [`outline`]).
    pub outline: Option<OutlineCache>,
}

impl Tab {
    /// A tab from a freshly loaded document: language detected from the path
    /// and first line, highlighter built for it, caret at the start.
    pub fn new(doc: TextDocument) -> Self {
        // There is only one field-initialization chain (`with_spec`): missing
        // a new per-tab field on either constructor path makes "opened files"
        // and "new files" behave differently.
        let spec = detect_spec(&doc);
        Self::with_spec(doc, spec)
    }

    /// A tab with an explicit language (new documents are pinned to plain
    /// text; the syntax menu only changes [`EditorState::set_language`], it
    /// does not rebuild the tab).
    pub fn with_spec(doc: TextDocument, spec: &'static LangSpec) -> Self {
        let csv = csv::CsvView::for_doc(spec.name, doc.meta.csv_mode, doc.meta.path.as_deref());
        let json = json::JsonView::for_doc(spec.name, &doc);
        let md_wys = spec.name == "Markdown" && visual_default_ok(&doc);
        Self {
            hl: Highlighter::new(spec),
            caret: Caret::default(),
            doc,
            spec,
            jsonl: jsonl::JsonlView::for_spec(spec.name),
            json,
            csv,
            md_wys,
            outline: None,
        }
    }
}

/// Whether a document may open straight into a visual view by default
/// (currently the Markdown WYSIWYG default; the JSON tree caps itself inside
/// [`json::JsonView::for_doc`]). Both views materialize the whole document
/// into their model, so past the JSON preview's byte cap
/// ([`json::MAX_DOC_BYTES`]) the default stays the text view — opening a file
/// must never turn into a parse/materialize hang. Switching by hand is still
/// possible; only the *default* is capped.
fn visual_default_ok(doc: &TextDocument) -> bool {
    (doc.total_bytes() as usize) <= json::MAX_DOC_BYTES
}

/// Viewport rectangle in client (window content) coordinates.
/// The editor's running measurement of the widest line in the active document.
///
/// There is no soft wrap and no stored per-line width, so the content width a
/// horizontal scrollbar needs is the maximum over *every* line — and the
/// document generation changes on every keystroke, so it cannot be recomputed
/// from scratch on edit. Instead the scan walks a bounded slice of lines per
/// frame and remembers both the widest line found so far and where to resume.
///
/// The consequence is deliberate: right after opening a huge file the extent is
/// an *underestimate* and grows as the scan advances (the bar's thumb shrinks
/// and the reachable range widens). An over-estimate would be worse — it would
/// make the bar flicker away and back on every keystroke, moving the text.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct HExtent {
    /// document this measurement belongs to
    pub doc_id: u64,
    /// generation it was taken at; a change means "rescan"
    pub gen: u64,
    /// next line to measure
    pub next: u64,
    /// widest line measured so far, px (excluding padding)
    pub width: f32,
    /// every line has been measured at this generation
    pub done: bool,
    /// Range `(doc_id, first, last)` last folded into the visible lines: the
    /// horizontal scrollbar must always reach what is on screen, so after a
    /// scroll the new visible range is folded in; this record is the
    /// read-only probe key for "has the fold been done" (used by pre-render
    /// healing, see `state::heal`).
    pub visible: Option<(u64, u64, u64)>,
}

#[derive(Clone, Copy, Debug)]
pub struct ViewRect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Default for ViewRect {
    fn default() -> Self {
        // bootstrap geometry until the window size is known; the editor layout
        // is expressed in explicit pixels because the Blitz layout engine does
        // not resolve `bottom:` insets / `height:100%` inside an auto-height box.
        ViewRect {
            x: 0.0,
            y: CHROME_TOP,
            w: 1280.0,
            h: 860.0 - CHROME_TOP - STATUS_H,
        }
    }
}

/// Shape of the text caret, the classic cursor options.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum CaretStyle {
    /// thin vertical bar — the default
    #[default]
    Line,
    /// filled rectangle covering the character under the caret
    Block,
    /// short underline at the baseline of the cursor line
    Underline,
}

impl CaretStyle {
    /// `None` when the label is only used to surface the current value
    /// The caret style as a **key**; callers render it in their own locale.
    /// `label` formerly returned Chinese directly and froze it in the prefswin dialog snapshot.
    pub fn msg(self) -> Msg {
        match self {
            CaretStyle::Line => Msg::CaretLine,
            CaretStyle::Block => Msg::CaretBlock,
            CaretStyle::Underline => Msg::CaretUnderline,
        }
    }

    /// `&'static str` for callers that cannot render late (dialog snapshots,
    /// native window titles); uses the process-level language mirror.
    pub fn label(self) -> &'static str {
        self.msg().get(i18n::current())
    }
}

/// An in-progress scrollbar-thumb drag on a menu panel.
///
/// The scroll offset is a pure function of the pointer's travel from the grab
/// point (`per_px` rows per pixel), captured once at `pointerdown`: recomputing
/// the thumb geometry on every move would make the content chase the pointer
/// as the thumb resizes under it.
#[derive(Clone, Copy, Debug)]
pub struct MenuDrag {
    /// client y of the grab
    pub start_y: f32,
    /// scroll offset at the grab
    pub start_scroll: f32,
    /// scroll px per pointer px (max_scroll / thumb travel)
    pub per_px: f32,
    /// scroll upper bound at the grab
    pub max: f32,
}

/// Live menu-bar state: which dropdown is open, where the keyboard cursor
/// is, whether the Alt access keys are shown.
///
/// The state machine itself lives in [`menu`]; this is only the
/// data it moves around.
#[derive(Default)]
pub struct MenuUi {
    /// The open caption (menu bar dropdown).
    ///
    /// An **identity**, not caption text: text changes with the language, while
    /// "which menu is open" must survive a language switch (see
    /// [`crate::ui::actions::Caption`]).
    pub open: Option<crate::ui::actions::Caption>,
    /// expanded submenu inside the open dropdown
    pub sub: Option<String>,
    /// client-space x of the open caption's left edge, in CSS px.
    ///
    /// The dropdown is rendered at the **root** of the app rather than inside
    /// the caption: the menu bar has a `z-index`, which creates a stacking
    /// context, so a dropdown nested inside it can never paint above the find
    /// bar or the results panel no matter how large its own `z-index` is. Being
    /// the caption's own left edge (measured from the click event, which
    /// reports both client and element coordinates) is what keeps the panel
    /// aligned under its caption.
    pub anchor_x: f32,
    /// row highlighted by the pointer in the top-level panel
    pub hover: Option<usize>,
    /// row highlighted by the pointer in the fly-out
    pub hover_sub: Option<usize>,
    /// row the keyboard cursor is on
    pub cursor: usize,
    /// The menu bar is "active": the access keys are underlined.
    ///
    /// Windows shows the mnemonic underline only while <kbd>Alt</kbd> is held or
    /// a menu is open, and hides it again on Escape or after a caption is picked.
    /// Driven from the key handler (see [`EditorState::menu_alt_key`]) and read
    /// by the title bar's captions.
    pub alt: bool,
    /// Vertical scroll of the open dropdown (px). Non-zero only while the menu
    /// is taller than the space below the caption — see
    /// [`crate::ui::state::menu::max_scroll`].
    pub scroll: f32,
    /// Vertical scroll of the fly-out (px), independent of the parent panel.
    pub sub_scroll: f32,
    /// Thumb drag in progress on the dropdown / fly-out.
    pub scroll_drag: Option<MenuDrag>,
    pub sub_drag: Option<MenuDrag>,
    /// The menu-bar overflow list ("more" button) is open: captions that do
    /// not fit the window are stacked here vertically.
    pub more_open: bool,
    /// Index of the first caption hidden by overflow (0 = all visible).
    /// Written back by the TitleBar layout.
    pub more_from: usize,
}

impl MenuUi {
    /// Drop every scroll offset and drag — called whenever a panel opens or
    /// swaps, so a menu never comes up scrolled halfway down from last time.
    pub fn reset_scroll(&mut self) {
        self.scroll = 0.0;
        self.sub_scroll = 0.0;
        self.scroll_drag = None;
        self.sub_drag = None;
    }

    /// Keep the keyboard cursor inside the visible slice of a scrolling panel.
    ///
    /// `rows` is the panel's row count, `cursor` the highlighted row and
    /// `flyout` selects which panel (and therefore which scroll offset) is
    /// adjusted. The geometry is read from the same helpers the renderer uses,
    /// so the keyboard and the pixels can never disagree about what "visible"
    /// means.
    pub fn ensure_cursor_visible(&mut self, rows: usize, cursor: usize, win_h: f32, flyout: bool) {
        use menu::{max_scroll, panel_height, ROW_H};
        let max_s = max_scroll(rows, win_h);
        let panel_h = panel_height(rows, win_h);
        let y0 = cursor as f32 * ROW_H;
        let y1 = y0 + ROW_H;
        let cur = if flyout { self.sub_scroll } else { self.scroll };
        let mut s = cur.clamp(0.0, max_s);
        if y0 < s {
            s = y0;
        }
        if y1 > s + panel_h {
            s = (y1 - panel_h).max(0.0);
        }
        s = s.min(max_s);
        if flyout {
            self.sub_scroll = s;
        } else {
            self.scroll = s;
        }
    }
}

/// Macro recording/playback state. See [`crate::ui::recorder`]: `last` is the
/// most recent finished macro available for playback, kept separate from the
/// recording buffer because "stop recording" must not discard what was just
/// recorded.
#[derive(Default)]
pub struct MacroState {
    /// Step buffer while a macro is being recorded; `Some` = recording
    pub rec: Option<Vec<crate::ui::recorder::MacroStep>>,
    /// The most recently completed macro (playback source)
    pub last: Vec<crate::ui::recorder::MacroStep>,
    /// Playback in progress — recording hooks short-circuit on this,
    /// otherwise playback would record itself
    pub playing: bool,
}

/// Frameless window state: geometry + maximize/topmost. The OS is the source
/// of truth; these are mirrors.
#[derive(Clone, Copy)]
pub struct WindowUi {
    /// client size of the window in CSS pixels (from winit)
    pub w: f32,
    pub h: f32,
    /// the frameless window is currently maximized (drives the restore glyph and
    /// keeps the custom title bar in sync with the OS)
    pub maximized: bool,
    /// the window is pinned above every other window (topmost)
    ///
    /// Mirrors `WS_EX_TOPMOST`; [`winshell::win::frameless`] is the source of truth and
    /// this is what the title-bar menu renders a check mark from.
    pub topmost: bool,
}

impl Default for WindowUi {
    fn default() -> Self {
        Self {
            w: 1280.0,
            h: 860.0,
            maximized: false,
            topmost: false,
        }
    }
}

/// The panel currently shown in the sidebar ([`EditorState::side_panel`]).
///
/// The sidebar has a single panel slot (`components::sidebar`); the workspace
/// directory tree and the document outline share it and are switched by this
/// enum. Panel width/collapse reuse the same [`EditorState::ws_show`] +
/// [`EditorState::sidebar_w`] layout (the toggle is at the bottom-left of the
/// status bar; collapsed means no sidebar).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum SidePanel {
    /// Workspace directory tree (`state::workspace`).
    #[default]
    Workspace,
    /// Document outline (`state::outline`, a Markdown heading hierarchy tree).
    Outline,
}

pub struct EditorState {
    /// open tabs; `active` indexes into this
    pub tabs: Vec<Tab>,
    /// Index into [`Self::tabs`]; **invariant: `tabs.is_empty() || active <
    /// tabs.len()`**.
    ///
    /// `tab()` / `tab_mut()` dereference it with bare indexing — they sit on the
    /// hot path of every frame and every keypress, so an out-of-range value is
    /// not an error code, it is a panicking window. Only three places may write
    /// this field, and each clamps: `activate_tab_clean` (via
    /// `set_active_tab`, which rejects an out-of-range index),
    /// `remove_tab` (shifts left, then clamps), and the test helper in
    /// `state::hidden`. `actions::tests::active_tab_stays_in_range_after_every_command`
    /// walks the whole head-less command table and touches `tab()` after each
    /// one, so a future write that forgets to clamp fails the build instead of
    /// crashing in front of a user.
    pub active: usize,
    /// Shared font metrics (the `Arc` cached by `metrics_for_family`; the read
    /// path derefs to a bare `Metrics`; changing font size = swapping the whole
    /// Arc, see `zoom`/`reset_zoom`).
    pub metrics: std::sync::Arc<Metrics>,
    /// Vertical scroll position, in **f64** pixels.
    ///
    /// A billion-line document is ~2e10 px tall at a 20px line height; f32
    /// carries only 24 bits of mantissa, so past ~1M lines the pixel grid
    /// degrades to 1000+px steps and the wheel visually stutters/jumps. f64
    /// keeps the mapping exact past any realistic document size. Rendering
    /// coordinates are still f32 — they are computed as differences in f64
    /// first (see `viewport::build`), which cancels the large offset.
    pub scroll_top: f64,
    pub scroll_left: f32,
    /// Widest-line scan backing the horizontal scrollbar (see [`HExtent`]).
    pub h_extent: HExtent,
    /// Soft-wrap index (visual row ↔ screen/document line). Kept in sync by
    /// `state::heal` before rendering.
    pub wrap_index: WrapIndex,
    /// Scrollbar thumb currently being dragged, if any.
    pub sb_drag: Option<crate::ui::scrollbar::SbDrag>,
    /// Which scrollbar part the pointer is over / pressed on — purely visual
    /// (hover + press colors), but it has to survive a re-render, which is why
    /// it lives on the state and not in a `use_signal` inside the bar.
    pub sb_hot: Option<crate::ui::scrollbar::SbPart>,
    pub sb_press: bool,
    /// Minimap (right-side code thumbnail) toggle (View → Minimap)
    pub minimap: bool,
    /// User preference for the JSON Lines preview (`None` = no explicit
    /// choice; fall back to the per-document-type default).
    ///
    /// Persisted in `settings.json` and applied by [`Self::adopt_view_prefs`]
    /// when a JSON Lines document is opened — it must not be applied to the
    /// blank tab present at startup.
    pub jsonl_pref: Option<bool>,
    /// The minimap is being dragged (press jumps there, then follows the pointer).
    ///
    /// The drag is glued to the **indicator**: `grab` is the offset between the
    /// pointer and the indicator top at press, `top` is the strip's own scroll —
    /// frozen while the indicator travels inside the strip, nudged only when the
    /// drag pushes the indicator against an edge. Both are read by the paint
    /// (through `mm_geom`), so the mapping and what is painted can never
    /// disagree — the old model froze the *pre-jump* strip scroll while the
    /// paint re-centred the indicator every frame, and the two disagreed by up
    /// to a full strip height, which is the "drag does not follow the hand" bug.
    pub mm_drag: Option<crate::ui::minimap::MmDrag>,
    /// The minimap strip's scroll, latched by a drag.
    ///
    /// `(document id, scroll it was latched at, strip top)`. The paint honours
    /// the latch while the document is unchanged and the scroll has not been
    /// moved by anything else further than the indicator can travel inside the
    /// strip — that is what keeps the strip standing still *after* the release
    /// instead of snapping back under the centring policy. A bigger step is a
    /// jump (click, goto, find) and re-centres, exactly like the pre-drag
    /// behaviour. `RefCell` for the same reason as `mm_ink`: the paint refreshes
    /// it through `&self`.
    pub mm_top: std::cell::RefCell<Option<(u64, f64, f64)>>,
    /// Per-line ink cache of the minimap (idempotent derivation per
    /// generation; see [`crate::ui::minimap::MmInk`]).
    ///
    /// Affects drawing cost only, not semantics: cache hit or miss yields the
    /// same ink.
    pub mm_ink: std::cell::RefCell<crate::ui::minimap::MmInk>,
    pub view: ViewRect,
    /// true while the primary mouse button drags a selection (text view)
    pub drag: bool,
    /// true while a press is held in the **document view** (a drag selection
    /// may be in progress).
    ///
    /// Separate from `drag` because the two views scroll differently: the text
    /// view owns `scroll_top` and auto-scrolls itself
    /// (`drag_autoscroll`), while the wys content scrolls inside blitz and its
    /// auto-scroll goes through the kernel
    /// (`components::mdws::drag_tick`).
    pub md_drag: bool,
    /// internal clipboard fallback
    pub clip: String,
    pub recent: VecDeque<String>,
    /// status-bar message (errors, save results, ...). `None` shows the file
    /// name; a message replaces it until the next action overwrites it.
    ///
    /// Stores the **key** ([`i18n::Message`]), not a rendered string: the last
    /// message follows a language switch, and no language's text gets frozen
    /// into the state.
    pub status: Option<i18n::Message>,
    /// menu bar dropdown state (see [`MenuUi`])
    pub menu: MenuUi,
    /// In-page context menu (fallback for non-Windows native menus; see
    /// [`menu::CtxMenu`]). On Windows the right click goes through
    /// `TrackPopupMenu` (`ctxmenu::show`) and this field stays empty.
    pub ctx: Option<menu::CtxMenu>,
    pub font_size: f32,
    /// window geometry & chrome state (see [`WindowUi`])
    pub win: WindowUi,
    /// Height by which the soft keyboard (TabTip) covers the window bottom,
    /// in CSS px.
    ///
    /// On touch screens the keyboard is a system top-level window floating
    /// above the app — it does **not** resize the app window and does **not**
    /// change `SPI_GETWORKAREA`. Every redraw frame the editor probes the
    /// keyboard's top edge (see [`winshell::win::softkb`]), converts it into
    /// this inset, and [`Self::set_keyboard_inset`] folds it into the layout:
    /// the viewport bottom yields and the caret line always scrolls above the
    /// keyboard. 0 = no avoidance.
    pub kb_inset: f32,
    /// A candidate "long press": `(press instant, physical screen x, physical
    /// screen y)`.
    ///
    /// Only the primary pointer's hold is recorded (the `primary` flag of the
    /// window-level `PointerButton`). Cancelled when the pointer moves past
    /// `LONG_PRESS_MOVE_PX`, on lift, or as soon as a window gesture starts.
    /// After `LONG_PRESS_MS` of holding, the context menu pops in the next
    /// `RedrawRequested` frame (see `maybe_long_press`). The project has no
    /// async runtime/timer, so the expiry check piggybacks on redraw frames:
    /// while held, a `std::thread` sleeps until the deadline and wakes the
    /// window with `request_redraw`.
    pub touch_press: Option<(std::time::Instant, f64, f64)>,
    /// In-app modal dialog (About / MD5 digest / table view / Markdown preview…).
    /// `None` = nothing shown. Native `rfd` boxes are invisible over the
    /// frameless blitz window, which is why this exists — see
    /// [`crate::ui::components::dialog`].
    pub dialog: Option<Dialog>,
    /// highlight switch (View → Syntax Highlighting)
    pub highlight_on: bool,
    /// force highlighting on very large documents (View → Highlight Large Files)
    pub highlight_all: bool,
    /// color theme
    pub dark: bool,
    /// UI language (see `crates/i18n/README.md`).
    ///
    /// The single source of truth: every message takes its language from here
    /// via [`Self::tr`], so a change to the `Signal<EditorState>` redraws its
    /// readers in the same frame — no restart. The process mirror
    /// (`i18n::current()`) serves libraries without this state and is updated in
    /// one place, [`Self::set_locale`].
    pub locale: i18n::Locale,
    /// paint a terminator glyph at the end of every line — `←↓` CRLF /
    /// `↓` LF / `←` CR (View → Show Line Endings)
    pub show_eol: bool,
    /// paint spaces as `·` and tabs as `→` (View → Show Whitespace)
    pub show_ws: bool,
    /// caret shape (Settings → Caret → Style); default is a thin I-beam
    pub caret_style: CaretStyle,
    /// caret width in CSS px (only used by `CaretStyle::Line`)
    pub caret_w: f32,
    /// blink the caret (Settings → Caret → Blink)
    pub caret_blink: bool,
    /// full blink cycle in ms (visible→hidden→visible)
    pub caret_blink_ms: u32,
    /// Current blink phase (true = visible).
    ///
    /// Written back by `BlinkTick` (a 265ms timer thread) through the idle
    /// frame hook (see the blink arm of `opens.rs::needs_idle_poll`). This was
    /// once a CSS `caret-blink` infinite animation — but blitz re-arms
    /// `request_redraw` every frame for windows with a running animation (the
    /// `is_animating` branch of `View::redraw`), so the whole app redrawed at
    /// 40–60fps forever. Moving the phase into state leaves only one frame per
    /// half-cycle when idle.
    pub caret_phase: bool,
    /// whether the document (not a find box) currently owns the keyboard.
    /// The selection and caret are painted dimmed while this is false.
    pub doc_focused: bool,
    /// soft-wrap long lines — not implemented yet, see FEATURES.md
    pub wrap: bool,
    /// auto-close the matching bracket/quote when typing an opening one
    /// (applies on the typing path; pasting is unaffected)
    pub auto_close: bool,
    /// find / replace / goto-line UI state
    pub find: FindState,
    /// Preferences popup state (a Blitz owned window; see `ui::prefswin`)
    pub prefs: PrefsWinState,
    /// Insert-number popup state (a Blitz owned window; see `ui::numwin`)
    pub numbering: NumWinState,
    /// About popup state (a Blitz owned window; see `ui::aboutwin`)
    pub about: AboutWinState,
    /// anchor set by Edit → Begin/End Selection, `(doc_id, pos)`
    pub sel_anchor: Option<(u64, u64)>,
    /// Startup skeleton bootstrap flag: `false` = the app root draws the
    /// skeleton screen and schedules a bootstrap frame
    /// (`EditorState::skeleton`); the real state is swapped in wholesale by
    /// `EditorState::default` after the skeleton's first frame (see the
    /// RedrawRequested branch in `ui::editor`).
    /// [`Self::minimal`] / [`Self::default`] are always `true` — tests and the
    /// full state never go through the skeleton.
    pub booted: bool,
    /// Vim-style modal editing (see [`crate::ui::vim`])
    pub vim: crate::ui::vim::Vim,
    /// Exit was picked; the window layer turns this into a real close
    pub quit_requested: bool,
    /// Two-phase state machine for closing dirty documents: the tab index
    /// pending while the self-drawn confirm dialog is up (rfd native dialogs
    /// are invisible over the frameless blitz window; see `docs.rs`)
    pub pending_close: Option<u64>,
    /// Document with a pending reload confirmation (doc id, two-phase; see
    /// `docs.rs::reload_active`)
    pub pending_reload: Option<u64>,
    /// Close-all sweep in progress: `finish_close` keeps closing the rest
    /// after the pending tab is handled
    pub close_all_pending: bool,
    /// Macro recording/playback state (see [`MacroState`])
    pub macros: MacroState,
    /// Last click in the editor area: `(instant, x, y, multi-click count)`.
    /// See `editor_click`.
    pub click_at: Option<(std::time::Instant, f32, f32, u32)>,
    /// Per-document line bookmarks (keyed by document id).
    pub bookmarks: std::collections::HashMap<u64, std::collections::BTreeSet<u64>>,
    /// File mtime recorded at open/save time (per document id), used to
    /// detect external modification.
    pub mtimes: std::collections::HashMap<u64, std::time::SystemTime>,
    /// The active document was modified externally (hint shown, until
    /// reload/close)
    pub external_hint: bool,
    /// Instant of the last external-modification check (throttled to ~1s)
    pub last_watch: Option<std::time::Instant>,
    /// Background search/replace job (for huge documents; see
    /// `find.rs`'s `ASYNC_SEARCH_THRESHOLD`)
    pub search_job: Option<SearchJob>,
    /// Text marks (right click → Style ...): doc_id → ordered mark list;
    /// see `marks`
    pub hl_marks: std::collections::HashMap<u64, Vec<Mark>>,
    /// Anchor of the column-mode selection (right click → Begin/End Selection
    /// (Column Mode))
    pub col_anchor: Option<u64>,
    /// Hidden lines: doc_id → ordered line numbers; see `hidden`
    pub hidden: std::collections::HashMap<u64, std::collections::BTreeSet<u64>>,
    /// Invalidation-check cache for [`HiddenCache`] (see the `hidden` module
    /// docs)
    pub hidden_cache: Option<HiddenCache>,
    /// Snapshot of macro files on disk (used for button indices in the
    /// `Macro → Run Macro…` dialog; see [`crate::ui::macros_io`])
    pub macro_files: Vec<PathBuf>,
    /// Crash-recovery salvage: a session pending restoration found at startup
    /// (`Some` while the confirm dialog is up; see [`crate::ui::recovery`])
    pub pending_recovery: Option<crate::ui::recovery::SessionManifest>,
    /// Instant of the last successful recovery-journal write (throttled to
    /// [`crate::ui::recovery::JOURNAL_INTERVAL`])
    pub recovery_last: Option<std::time::Instant>,
    /// Content signature at the last journal write; `None` = retry pending
    pub recovery_sig: Option<u64>,
    /// Per-document gen at the last journal write (doc_id → gen): an unchanged
    /// gen skips rewriting the content snapshot, so moving the caret doesn't
    /// drag big files through a rewrite every 3 seconds
    pub recovery_backup_gens: std::collections::HashMap<u64, u64>,
    /// Recently run external commands (hint text and the "run again" list of
    /// `Run → Run…`)
    pub run_recent: Vec<String>,
    /// Read-only mode: intercepts every path that would modify the document
    /// (session-scoped, not persisted — "read-only" is a temporary stance on
    /// this one file; defaulting back to writable after a restart feels right)
    pub read_only: bool,
    /// files being opened on worker threads
    pub pending_opens: Vec<PendingOpen>,
    /// progressive index scans still filling in a huge document's line index
    pub index_jobs: Vec<IndexJob>,
    /// command-line files not started yet (see [`Self::flush_startup_paths`])
    pub startup_paths: Vec<PathBuf>,
    /// how a worker thread wakes the window; set once from the app root
    pub wake: Option<WakeFn>,
    /// Whether the toolbar currently participates in layout (cached; see
    /// [`Self::sync_toolbar_layout`])
    pub toolbar_shown: bool,
    // ---- workspace (folder sidebar; see [`workspace`]) ---------------------
    /// Workspace root; `None` = not open (sidebar hidden)
    pub ws_root: Option<PathBuf>,
    /// Cache of already-loaded directory children (lazy: one level read per
    /// expansion)
    pub ws_nodes: HashMap<PathBuf, Vec<WsNode>>,
    /// Set of expanded directories
    pub ws_expanded: HashSet<PathBuf>,
    /// Visible-row cache (pre-order flattening of the tree), rebuilt on every
    /// tree change
    pub ws_rows: Rc<Vec<WsRow>>,
    /// Sidebar visibility (user collapsible; only meaningful with a root)
    pub ws_show: bool,
    /// Sidebar width (currently fixed [`SIDEBAR_W`]; field kept for future
    /// drag-resizing)
    pub ws_width: f32,
    /// Sidebar's current panel (workspace tree / document outline; see
    /// [`SidePanel`])
    pub side_panel: SidePanel,
}

/// One background search/replace scan.
///
/// The thread holds a **cloned snapshot** of the `PieceTable` (cloning the
/// piece list is O(piece count), not O(bytes); nearly free in the mmap
/// single-piece case): the user can keep editing while the scan runs, results
/// come back tagged with the snapshot's `gen`, and the existing
/// `FindState::valid_for` mechanism marks them stale — no new state needs to
/// be invented for "the document changed mid-scan".
pub struct SearchJob {
    pub rx: std::sync::mpsc::Receiver<SearchOutcome>,
    /// Bytes scanned so far (Relaxed write by the worker; UI only shows progress)
    pub progress: std::sync::Arc<std::sync::atomic::AtomicU64>,
    pub total: u64,
    pub cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
    pub kind: SearchJobKind,
    pub started: std::time::Instant,
    /// Document identity when the scan started. The user may switch tabs
    /// mid-scan — results belong only to the document that started them and
    /// must be validated on collection, otherwise stale ops would be written
    /// into a different document.
    pub doc_id: u64,
    /// Batches already written by streaming replace (one transaction per
    /// batch; later batches merge into the same undo step).
    pub replace_batches: usize,
    /// Matches already processed by streaming replace (including no-ops where
    /// the replacement equals the original).
    pub replace_matches: usize,
    /// Document `gen` after the last write by this job. The next batch can
    /// only be applied on top of this state; a mismatch means the user edited
    /// in between, and the job must stop instead of blindly writing at old
    /// coordinates.
    pub replace_gen: u64,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SearchJobKind {
    FindAll,
    ReplaceAll,
}

/// Completion result of a background search.
pub enum SearchOutcome {
    FindAll {
        matches: Vec<crate::search::Match>,
        truncated: bool,
        cancelled: bool,
        gen: u64,
    },
    /// One batch of replace ops already mapped to current document
    /// coordinates (streaming, written back while scanning).
    ReplaceBatch {
        ops: Vec<crate::buffer::doc::EditOp>,
        matches: usize,
        cancelled: bool,
        gen: u64,
    },
    /// End of streaming replace (sent even when there was nothing to write).
    ReplaceDone {
        matches: usize,
        cancelled: bool,
        gen: u64,
    },
}

impl Default for EditorState {
    fn default() -> Self {
        // Read settings before building metrics: `minimal()` resolves the font
        // once at 14px, and if the user configured another size, `apply` would
        // resolve it again. fontdue takes ~13ms to parse a 453KB Consolas in a
        // release headless benchmark — paying that twice for nothing.
        // The test entry `minimal()` still skips settings, so unit tests are
        // unaffected by local configuration.
        let (settings, settings_warning) = crate::ui::settings::load_with_status();
        let font_size = if font_size_ok(settings.font_size) {
            settings.font_size
        } else {
            DEFAULT_FONT_SIZE
        };
        let mut st = Self::minimal_with_font(font_size);
        settings.apply(&mut st);
        st.status = settings_warning;
        st.queue_command_line_files();
        // Crash-recovery salvage: the previous session's wreckage is found
        // here and a restore confirm dialog is raised.
        // The test entry `minimal()` does not go through this; under cfg(test)
        // config_dir is also None.
        st.check_recovery();
        st
    }
}

/// Pick a language for a freshly opened document (extension first, then a
/// shebang / modeline in the first line).
///
/// **Byte-zero file always Plain Text**: a newly created empty doc has no
/// content evidence, and every special mode inferred by extension is a guess —
/// an empty .md would raise the Markdown three-view toolbar, a byte-zero CSV
/// shows the toolbar. Default opens as Plain Text (the syntax menu can switch
/// manually); once there is content, reopen and re-detect by extension/first
/// line.
fn detect_spec(doc: &TextDocument) -> &'static LangSpec {
    if doc.total_bytes() == 0 {
        return LangSpec::plain();
    }
    let first = if doc.line_count() > 0 {
        doc.line_text(0)
    } else {
        String::new()
    };
    for_path_and_first_line(doc.meta.path.as_deref(), &first)
}

/// Number of an untitled placeholder name: `Some(0)` = no number, `Some(2)` =
/// the second document.
///
/// Recognizes the placeholder in **every** language, not just the current one: a
/// round trip goes through both (zh→en still holds "未命名", en→zh holds
/// "Untitled 2"), and matching one language would drop the number.
fn untitled_number(name: &str) -> Option<usize> {
    for loc in i18n::Locale::ALL {
        let base = loc.tr(i18n::Msg::DocUntitled);
        if name == base.as_ref() {
            return Some(0);
        }
        if let Some(rest) = name.strip_prefix(base.as_ref()) {
            if let Ok(n) = rest.trim().parse::<usize>() {
                return Some(n);
            }
        }
    }
    None
}

impl EditorState {
    /// One empty document, no argument parsing, no dialogs.
    ///
    /// `Default` adds the files named on the command line on top of this; tests
    /// and head-less smoke checks use `minimal` so they can never be influenced
    /// by (or block on) the process arguments.
    pub fn minimal() -> Self {
        Self::minimal_with_font(DEFAULT_FONT_SIZE)
    }

    /// Startup skeleton state (the app root's first-frame state): only the
    /// dark/light flag is freshly read ([`crate::ui::settings::peek_dark`],
    /// one small file IO), the rest is [`Self::minimal`]. `booted=false` makes
    /// the app root draw the skeleton screen and swap in [`Self::default`]
    /// wholesale after the skeleton's first frame (all the real loading work
    /// happens in that frame).
    pub fn skeleton() -> Self {
        let mut st = Self::minimal();
        st.dark = crate::ui::settings::peek_dark();
        st.booted = false;
        st
    }

    /// [`Self::minimal`] with an explicit font size. `Default` uses this to
    /// build the metrics once for the persisted font size instead of parsing
    /// the font at 14px and immediately throwing that work away.
    fn minimal_with_font(font_size: f32) -> Self {
        let metrics = metrics_for_family("Consolas", font_size);
        let mut doc = TextDocument::empty();
        // The placeholder name is document state (tab, title bar and save-as all
        // read it), so it is rendered at creation. `minimal()` is the test entry
        // point and must not depend on the process: it uses the default language,
        // and `apply` fixes it up through `rerender_derived_names`.
        doc.meta.display_name = i18n::Locale::DEFAULT
            .tr(i18n::Msg::DocUntitled)
            .into_owned();
        Self {
            tabs: vec![Tab::with_spec(doc, LangSpec::plain())],
            active: 0,
            metrics,
            scroll_top: 0.0,
            scroll_left: 0.0,
            h_extent: HExtent::default(),
            wrap_index: WrapIndex::default(),
            sb_drag: None,
            sb_hot: None,
            sb_press: false,
            minimap: true,
            jsonl_pref: None,
            mm_drag: None,
            mm_top: std::cell::RefCell::new(None),
            mm_ink: std::cell::RefCell::new(crate::ui::minimap::MmInk::new()),
            view: ViewRect::default(),
            drag: false,
            md_drag: false,
            clip: String::new(),
            recent: VecDeque::with_capacity(10),
            status: None,
            menu: MenuUi::default(),
            ctx: None,
            hl_marks: std::collections::HashMap::new(),
            col_anchor: None,
            hidden: std::collections::HashMap::new(),
            hidden_cache: None,
            macro_files: Vec::new(),
            pending_recovery: None,
            recovery_last: None,
            recovery_sig: None,
            recovery_backup_gens: std::collections::HashMap::new(),
            run_recent: Vec::new(),
            font_size,
            win: WindowUi::default(),
            kb_inset: 0.0,
            touch_press: None,
            dialog: None,
            highlight_on: true,
            highlight_all: false,
            dark: true,
            locale: i18n::Locale::DEFAULT,
            show_eol: false,
            show_ws: false,
            caret_style: CaretStyle::Line,
            caret_w: 2.0,
            caret_blink: true,
            caret_blink_ms: 530,
            caret_phase: true,
            doc_focused: true,
            wrap: false,
            auto_close: true,
            find: FindState::default(),
            prefs: PrefsWinState::default(),
            numbering: NumWinState::default(),
            about: AboutWinState::default(),
            sel_anchor: None,
            booted: true,
            vim: crate::ui::vim::Vim::default(),
            quit_requested: false,
            pending_close: None,
            pending_reload: None,
            close_all_pending: false,
            macros: MacroState::default(),
            click_at: None,
            bookmarks: std::collections::HashMap::new(),
            mtimes: std::collections::HashMap::new(),
            external_hint: false,
            last_watch: None,
            search_job: None,
            read_only: false,
            pending_opens: Vec::new(),
            index_jobs: Vec::new(),
            startup_paths: Vec::new(),
            wake: None,
            toolbar_shown: false,
            ws_root: None,
            ws_nodes: HashMap::new(),
            ws_expanded: HashSet::new(),
            ws_rows: Rc::new(Vec::new()),
            ws_show: false,
            ws_width: SIDEBAR_W,
            side_panel: SidePanel::Workspace,
        }
    }

    /// Open the files passed on the command line as tabs.
    ///
    /// The paths are only *queued* here. Loading them synchronously would hold
    /// up the very first frame — double-clicking a 2 GB log in Explorer would
    /// show a blank window until the scan finished — so they are started by
    /// [`Self::flush_startup_paths`] once the window can drive its own redraws.
    ///
    /// Parsing lives in [`crate::ui::cli_open_files`]: the same argv is what a
    /// file dropped onto `infeditor.exe` produces, and the second-launch
    /// hand-off (`ui::single`) must read it identically.
    fn queue_command_line_files(&mut self) {
        for path in crate::ui::cli_open_files() {
            if self.startup_paths.contains(&path) {
                continue;
            }
            self.startup_paths.push(path);
        }
    }

    /// Start every queued startup open.
    ///
    /// Idempotent: called once from [`Self::set_waker`] (which is when worker
    /// threads become able to wake the window) and again as a fallback from the
    /// first [`Self::poll_opens`]. With no waker installed the loads run inline,
    /// which is what the unit tests rely on.
    pub fn flush_startup_paths(&mut self) {
        if self.startup_paths.is_empty() {
            return;
        }
        for p in std::mem::take(&mut self.startup_paths) {
            self.open_path_async(p);
        }
    }
}

impl EditorState {
    /// The active tab (document + caret + language + highlighter).
    pub fn tab(&self) -> &Tab {
        &self.tabs[self.active]
    }

    pub fn tab_mut(&mut self) -> &mut Tab {
        &mut self.tabs[self.active]
    }

    pub fn active_doc(&self) -> &TextDocument {
        &self.tab().doc
    }

    pub fn active_doc_mut(&mut self) -> &mut TextDocument {
        &mut self.tab_mut().doc
    }

    // ---- theme / highlighting --------------------------------------------

    /// Active color theme.
    pub fn theme(&self) -> &'static Theme {
        Theme::of(self.dark)
    }

    /// Language of a document (falls back to plain text).
    pub fn spec_of(&self, i: usize) -> &'static LangSpec {
        self.tabs
            .get(i)
            .map(|t| t.spec)
            .unwrap_or_else(LangSpec::plain)
    }

    /// Language of the active document.
    pub fn spec(&self) -> &'static LangSpec {
        self.tab().spec
    }

    // -----------------------------------------------------------------------
    // UI text (i18n)
    // -----------------------------------------------------------------------

    /// Renders a message: `st.tr(Msg::MenuFile)` / `st.tr(Fmt::LineCol { .. })`.
    ///
    /// UI code takes text only through here. `i18n::current()` reads the process
    /// mirror, which does not subscribe to the `Signal` and would not redraw.
    pub fn tr(&self, text: impl i18n::Text) -> std::borrow::Cow<'static, str> {
        text.render(self.locale)
    }

    /// Switches the UI language: state → menu → process mirror. Persisting is
    /// done by the command dispatcher ([`crate::ui::actions::Cmd::SetLocale`] is
    /// a settings command).
    ///
    /// Open menus **must** close here: the open state is language-neutral, but
    /// the fly-out is located by row position, and a stale position would fly out
    /// a different submenu.
    pub fn set_locale(&mut self, locale: i18n::Locale) {
        if self.locale == locale {
            return;
        }
        self.locale = locale;
        i18n::set_current(locale);
        self.menu_close();
        self.rerender_derived_names();
        self.status = Some(
            Fmt::StatusLanguage {
                name: locale.native_name(),
            }
            .into(),
        );
    }

    /// Recomputes **derived** display names (the untitled placeholders).
    ///
    /// Its own function because the leak in i18n is rarely a missing translation
    /// — it is a rendered string stored in state, which freezes into that
    /// language and stops being user data. Whether a name can be recomputed is
    /// decided by semantics (no path, placeholder name), never by looking for CJK
    /// characters.
    pub(crate) fn rerender_derived_names(&mut self) {
        for tab in &mut self.tabs {
            if tab.doc.meta.path.is_some() {
                continue;
            }
            let Some(n) = untitled_number(&tab.doc.meta.display_name) else {
                continue;
            };
            tab.doc.meta.display_name = if n == 0 {
                self.locale.tr(i18n::Msg::DocUntitled).into_owned()
            } else {
                self.locale.tr(i18n::Fmt::DocUntitledN { n }).into_owned()
            };
        }
    }

    /// The UI language.
    pub fn locale(&self) -> i18n::Locale {
        self.locale
    }

    /// Switch the active document's language (and re-detect highlighting).
    pub fn set_language(&mut self, spec: &'static LangSpec) {
        let i = self.active;
        self.set_language_at(i, spec);
    }

    /// Tab-indexed [`Self::set_language`].
    ///
    /// The save-as path needs it for a tab it just wrote (which is the active
    /// one today, but the write tail must not depend on that), so the body
    /// lives here and both entry points share it — a second copy is how the
    /// two would drift.
    pub(super) fn set_language_at(&mut self, i: usize, spec: &'static LangSpec) {
        if i >= self.tabs.len() {
            return;
        }
        let t = &mut self.tabs[i];
        t.spec = spec;
        t.hl.set_spec(spec);
        t.doc.meta.file_type = spec.name.to_string();
        // Keep the toolbar and table view in sync when switching to/from CSV
        // (preserving the user-adjusted delimiter)
        let (delim, table) = (t.csv.delim, t.csv.table);
        let was_enabled = t.csv.enabled;
        t.csv = csv::CsvView::for_doc(spec.name, t.doc.meta.csv_mode, t.doc.meta.path.as_deref());
        t.csv.delim = delim;
        if was_enabled {
            // Documents whose toolbar was enabled manually: a language switch
            // does not turn it off for the user
            t.csv.enabled = true;
        } else {
            t.csv.table = table && t.csv.enabled;
        }
        self.relayout();
    }

    /// Re-run language detection for the active document.
    pub fn redetect_language(&mut self) {
        let spec = detect_spec(self.active_doc());
        self.set_language(spec);
        self.status = Some(
            Fmt::StatusLang {
                name: crate::ui::lang_label(self.locale, spec.name).into_owned(),
            }
            .into(),
        );
    }

    /// Convenience for menus: toggle language by display name.
    pub fn set_language_by_name(&mut self, name: &str) {
        let spec = crate::highlight::by_name(name);
        self.set_language(spec);
        self.status = Some(
            Fmt::StatusLang {
                name: crate::ui::lang_label(self.locale, spec.name).into_owned(),
            }
            .into(),
        );
    }

    pub fn toggle_theme(&mut self) {
        self.dark = !self.dark;
        self.status = Some(
            if self.dark {
                Msg::StatusThemeDark
            } else {
                Msg::StatusThemeLight
            }
            .into(),
        );
    }

    /// Maximize the window, or restore it when it is already maximized.
    ///
    /// The OS is the source of truth, but the flag is updated here too rather
    /// than waiting for the `SurfaceResized` that follows: the resize strips and
    /// the restore glyph read it, and a frame of disagreement shows a resize
    /// cursor on a maximized window.
    pub fn toggle_window_maximized(&mut self) {
        let next = !self.win.maximized;
        winshell::win::frameless::toggle_maximize(self.win.maximized);
        self.win.maximized = next;
    }
}

impl EditorState {
    /// Test helper: render the status-bar message into text in the current
    /// language (for test assertions).
    pub fn status_text(&self) -> String {
        self.status
            .as_ref()
            .map(|m| self.tr(m.clone()).into_owned())
            .unwrap_or_default()
    }

    /// Test helper: render the find hint into text in the current language
    /// (for test assertions).
    pub fn find_message_text(&self) -> String {
        self.find
            .message
            .as_ref()
            .map(|m| self.tr(m.clone()).into_owned())
            .unwrap_or_default()
    }
}

impl EditorState {
    /// Toggle Vim-style modal editing (View → Vim Mode).
    pub fn toggle_vim(&mut self) {
        self.vim.toggle();
        self.status = if self.vim.enabled {
            Some(
                Fmt::StatusVimOn {
                    mode: self.tr(self.vim.mode.msg()).into_owned(),
                }
                .into(),
            )
        } else {
            Some(Msg::StatusVimOff.into())
        };
    }

    pub fn toggle_highlight(&mut self) {
        self.highlight_on = !self.highlight_on;
        self.status = Some(
            if self.highlight_on {
                Msg::StatusHighlightOn
            } else {
                Msg::StatusHighlightOff
            }
            .into(),
        );
    }
}

impl EditorState {
    // ---- window geometry -------------------------------------------------

    /// Update the client area size (CSS px) and re-derive the viewport rect.
    /// The layout uses explicit pixels because Blitz does not resolve
    /// `bottom:` / `height:100%` inside an auto-height container.
    pub fn set_window_size(&mut self, w: f32, h: f32) {
        let w = w.max(320.0);
        let h = h.max(200.0);
        if (w - self.win.w).abs() < 1.0 && (h - self.win.h).abs() < 1.0 {
            return;
        }
        self.win.w = w;
        self.win.h = h;
        self.relayout();
    }
}

impl EditorState {
    /// Recompute the viewport rectangle from the window size and the docked
    /// find bar / results panel / tool bar. Explicit pixels again — Blitz
    /// cannot derive a height from `top:` + `bottom:` insets.
    ///
    /// `kb_inset` (the on-screen keyboard, see [`Self::set_keyboard_inset`]) is
    /// subtracted like every other docked chrome: the caret must stay above the
    /// keyboard, and the only lever the layout has is a shorter viewport.
    ///
    /// The viewport **no longer insets for the resize band** (the old scheme
    /// inset all four edges by `resize_grab_edge`, which looked ugly and
    /// wasted area for nothing): edge resizing is now driven by the system
    /// `WM_NCHITTEST` non-client hit test (`winshell::win::snap` +
    /// `winshell::win::frameless::hit_test_arbitrated`). The hit band is a
    /// uniform narrow strip of the native border width
    /// (`SM_CXSIZEFRAME + SM_CXPADDEDBORDER`) and always wins; the only thing
    /// that yields is the title bar's window-button cluster (see
    /// [`Self::hit_exempt_rects`]).
    pub fn relayout(&mut self) {
        let bar = self.find.bar_height();
        let panel = self.find.panel_height();
        let tb = self.toolbar_h();
        // The workspace sidebar takes a strip on the viewport's left (see the
        // `workspace` module); the viewport shifts right accordingly
        let sw = self.sidebar_w();
        self.view.x = sw;
        self.view.y = CHROME_TOP + bar + tb;
        self.view.w = (self.win.w - sw).max(40.0);
        self.view.h =
            (self.win.h - CHROME_TOP - bar - panel - tb - STATUS_H - self.kb_inset).max(40.0);
        self.clamp_scroll();
        // Layout geometry changed → the interactive component rect set changes
        // with it (hit testing runs inside WndProc, which cannot touch the
        // Signal, so the layout layer must push it).
        winshell::win::frameless::set_hit_exempt(self.hit_exempt_rects());
    }

    /// The current "interactive component rect set" (client-area CSS px
    /// `[x, y, w, h]`) that `WM_NCHITTEST` hit testing yields to. After
    /// revision two (the mainstream approach) it contains **only the title
    /// bar's window-button cluster**: the resize band is a uniform narrow
    /// strip of the native border width
    /// (`SM_CXSIZEFRAME + SM_CXPADDEDBORDER`) and always wins — scrollbars,
    /// sidebar and status bar no longer yield (content runs to the window
    /// edge; only the outermost border-width strip overlaps the resize band,
    /// isomorphic to scrollbars hugging the non-client border inside native
    /// windows). The button cluster is the sole exception (the close button's
    /// top pixels must still be the close button; native HTCLOSE likewise
    /// beats HTTOP). Rules and trade-offs in winshell's
    /// `frameless::hit_test_arbitrated`; for the resize path's layout
    /// timing see [`Self::relayout`]'s comment in this file.
    ///
    /// Pure function (reads own geometry), so it can be unit-tested headless
    /// — it touches no global slots and is parallel-test safe.
    pub fn hit_exempt_rects(&self) -> Vec<[f32; 4]> {
        vec![[self.win.w - 3.0 * WINBTN_W, 0.0, 3.0 * WINBTN_W, TITLEBAR_H]]
    }

    /// Set the soft keyboard inset (CSS px) and relayout. `0` = keyboard
    /// hidden, no avoidance.
    ///
    /// The probe actually queries the system only every ~120ms (see
    /// `winshell::win::softkb`), far below frame rate; changes below 0.5px
    /// are ignored outright, avoiding the "same state relayout + scroll every
    /// frame" idle churn.
    pub fn set_keyboard_inset(&mut self, px: f32) {
        let px = px.max(0.0);
        if (px - self.kb_inset).abs() < 0.5 {
            return;
        }
        self.kb_inset = px;
        self.relayout();
        // The keyboard popup chops off part of the viewport: the line being
        // edited may end up behind the keyboard, so scroll it back first.
        self.scroll_caret_into_view();
    }

    /// Height of the docked tool bar (CSV / JSON / JSONL / Markdown) for the
    /// active tab: [`TOOLBAR_H`] when it is visible, else 0.
    ///
    /// Markdown / JSON keep a toolbar permanently per language (regardless of
    /// the preview switches); CSV and JSONL are decided by their own view
    /// toggles (`csv.enabled` / `jsonl.enabled`) — the latter lets users
    /// manually enable the JSONL preview toolbar on any document.
    pub fn toolbar_h(&self) -> f32 {
        let Some(tab) = self.tabs.get(self.active) else {
            return 0.0;
        };
        if tab.csv.enabled
            || tab.spec.name == "Markdown"
            || tab.spec.name == "JSON"
            || tab.jsonl.enabled
            || tab.spec.name == "JSON Lines"
        {
            TOOLBAR_H
        } else {
            0.0
        }
    }

    /// Fallback sync for toolbar visibility changes: layout recomputes only
    /// when this cached value flips, so per-frame calls stay cheap. The
    /// open/load/tab-switch paths already call [`Self::relayout`] themselves;
    /// this just guards against a path that forgot (toolbar hidden under the
    /// viewport).
    pub fn sync_toolbar_layout(&mut self) {
        let want = self.toolbar_h() > 0.0;
        if want != self.toolbar_shown {
            self.toolbar_shown = want;
            self.relayout();
        }
    }

    pub fn caret(&self) -> &Caret {
        &self.tab().caret
    }

    pub fn caret_mut(&mut self) -> &mut Caret {
        &mut self.tab_mut().caret
    }
}

impl EditorState {
    /// Zoom the editor font (Ctrl+wheel / menu).
    pub fn zoom(&mut self, dir: f32) {
        let mut size = self.font_size + dir * 1.0;
        size = size.clamp(MIN_FONT_SIZE, MAX_FONT_SIZE);
        if (size - self.font_size).abs() < 0.01 {
            return;
        }
        self.font_size = size;
        self.metrics = metrics_for_family("Consolas", size);
        self.scroll_caret_into_view();
    }

    /// View → Reset Zoom.
    pub fn reset_zoom(&mut self) {
        if (self.font_size - DEFAULT_FONT_SIZE).abs() < 0.01 {
            return;
        }
        self.font_size = DEFAULT_FONT_SIZE;
        self.metrics = metrics_for_family("Consolas", DEFAULT_FONT_SIZE);
        self.scroll_caret_into_view();
    }
}

/// Default editor font size (px) — the value View → Reset Zoom goes back to.
///
/// 15px is one `ZoomIn` step above the historical 14px: the 14px default read a
/// notch small next to every other editor on a 1.5x display, and one step is
/// the smallest change that still lands on the zoom grid (there is no 14.5 step
/// — `zoom` moves the size by whole pixels).
pub const DEFAULT_FONT_SIZE: f32 = 15.0;
/// Zoom range, also the range a persisted `font_size` has to fall in.
pub const MIN_FONT_SIZE: f32 = 8.0;
pub const MAX_FONT_SIZE: f32 = 48.0;

/// A persisted font size is only usable when it is a real number inside the
/// zoom range. `serde` turns an overflowing literal into `inf`, and a
/// non-finite value reaching `metrics_for_family` breaks the layout and is
/// written back as `null` — which then fails the whole-settings parse on the
/// next start (a self-poisoning config).
pub fn font_size_ok(size: f32) -> bool {
    size.is_finite() && (MIN_FONT_SIZE..=MAX_FONT_SIZE).contains(&size)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Visual-first defaults at open: a Markdown document opens in the WYSIWYG
    /// document view, an oversized one stays in the source view. (The JSON
    /// tree default's own cases live in `state::json`'s tests.)
    #[test]
    fn tab_visual_defaults_markdown_and_oversize_fallback() {
        let doc = TextDocument::from("# hello\n\n- a\n- b\n".to_string());
        let t = Tab::with_spec(doc, crate::highlight::by_name("Markdown"));
        assert!(t.md_wys, "markdown should open in the document view (wys)");
        assert!(
            !t.json.enabled,
            "a markdown tab must not get the JSON tree default"
        );

        // Past the visual-default cap the document opens as source
        let big = format!("{}\n", "paragraph ".repeat(1_000_000)); // > 8 MiB
        let doc = TextDocument::from(big);
        let t = Tab::with_spec(doc, crate::highlight::by_name("Markdown"));
        assert!(!t.md_wys, "oversized markdown stays in the source view");
    }

    /// The visual-first default is language-scoped: a plain-text tab (the
    /// startup blank tab, new files) must stay in the text view.
    #[test]
    fn tab_visual_defaults_leave_plain_text_alone() {
        let doc = TextDocument::from("# not really markdown\n".to_string());
        let t = Tab::with_spec(doc, crate::highlight::by_name("Plain Text"));
        assert!(!t.md_wys);
        assert!(!t.json.enabled);
        assert!(!t.jsonl.enabled);
    }

    /// The "interactive component rect set" computed by the layout layer (the
    /// `WM_NCHITTEST` yield basis): after revision two it contains **only the
    /// title bar's window-button cluster** — the resize band is a uniform
    /// narrow strip of the native border width and always wins; scrollbars,
    /// sidebar and status bar no longer yield.
    #[test]
    fn relayout_publishes_interactive_component_rects() {
        let mut st = EditorState::minimal();
        st.set_window_size(1000.0, 800.0);

        // Viewport hugs the window's right/bottom edges (no resize-band inset)
        assert_eq!(st.view.x, 0.0, "viewport left edge hugs the window edge");
        assert_eq!(
            st.view.w, 1000.0,
            "viewport right edge hugs the window edge"
        );
        assert!(
            (st.view.y + st.view.h - (800.0 - STATUS_H)).abs() < 0.01,
            "viewport bottom edge hugs the status bar (no gap for the resize band)"
        );

        let rects = st.hit_exempt_rects();
        assert_eq!(
            rects,
            vec![[1000.0 - 3.0 * WINBTN_W, 0.0, 3.0 * WINBTN_W, TITLEBAR_H]],
            "only the window-button cluster yields: {rects:?}"
        );

        // No extra rects when scrollbars appear / the sidebar opens (the
        // resize band always wins)
        st.win.h = 200.0;
        st.relayout();
        assert_eq!(
            st.hit_exempt_rects().len(),
            1,
            "rect set unchanged after scrollbars appear: {:?}",
            st.hit_exempt_rects()
        );
    }

    /// Line numbers show verbatim up to 10 digits; from line 10^10 on they
    /// switch to scientific notation, and the width formula (`gutter_w`)
    /// shares the same function as the display format.
    #[test]
    fn line_numbers_switch_to_scientific_past_ten_digits() {
        assert_eq!(format_line_no(1), "1");
        assert_eq!(format_line_no(9_999_999_999), "9999999999");
        let big = format_line_no(10_000_000_000);
        assert_eq!(big, "1.00e10");
        assert_eq!(format_line_no(12_345_678_901_234), "1.23e13");
        // Scientific notation keeps the gutter width around 7 glyphs instead
        // of growing without bound with the line count
        assert!(
            format_line_no(u64::MAX).chars().count() <= 8,
            "{}",
            format_line_no(u64::MAX)
        );
    }

    /// `line_no_digits` (pure-arithmetic width) must agree with
    /// `format_line_no` (actual display) value for value — the high-frequency
    /// path's two heap allocations were replaced with a division loop; this
    /// cross-check guards the contract.
    #[test]
    fn line_no_digits_matches_format_line_no() {
        for n in [
            1u64,
            9,
            10,
            99,
            100,
            1234,
            99_999,
            9_999_999_999,
            10_000_000_000,
            10_000_000_001,
            12_345_678_901_234,
            u64::MAX,
        ] {
            assert_eq!(
                line_no_digits(n),
                format_line_no(n).chars().count(),
                "n = {n}"
            );
        }
    }

    /// The in-page find bar docks under the tab strip and shrinks the editor.
    #[test]
    fn the_find_bar_docks_and_takes_layout_space() {
        let mut st = EditorState::minimal();
        st.set_window_size(1200.0, 800.0);
        let base = st.view.h;
        st.open_find_inline(false, false);
        assert!(st.view.h < base, "the in-page bar does take space");
        assert_eq!(st.find.bar_height(), crate::ui::state::FIND_ROW_H);
        st.close_find();
        assert_eq!(st.view.h, base);
    }

    /// Byte-zero file always Plain Text: special mode inferred by extension (Markdown/CSV)
    /// is meaningless for a freshly created, contentless document. Once there
    /// is content, detection by extension applies as usual.
    #[test]
    fn empty_file_detects_as_plain_text() {
        let mut doc = TextDocument::empty();
        doc.meta.path = Some(PathBuf::from("C:/tmp/notes.md"));
        assert_eq!(detect_spec(&doc).name, "Plain Text", "Empty .md ≠ Markdown");

        doc.meta.path = Some(PathBuf::from("C:/tmp/data.csv"));
        assert_eq!(detect_spec(&doc).name, "Plain Text", "Empty .csv ≠ CSV");

        // With content: extension detection as usual
        let mut doc = TextDocument::empty();
        doc.insert_text(0, "# 标题\n");
        doc.meta.path = Some(PathBuf::from("C:/tmp/notes.md"));
        assert_eq!(detect_spec(&doc).name, "Markdown");

        // File with no extension is also Plain Text (already plain, locked behavior)
        let mut doc = TextDocument::empty();
        doc.meta.path = Some(PathBuf::from("C:/tmp/README"));
        assert_eq!(detect_spec(&doc).name, "Plain Text");
    }
}
