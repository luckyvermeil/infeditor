//! The generic toolbar component: the toolbars of all four document types — CSV / JSON / JSONL /
//! Markdown — share one **shell**, **buttons**, **grouping** and **layout**.
//!
//! The old implementation crammed each format's hand-rolled `x += w + 4.0` arithmetic, fixed
//! widths and button rendering into one giant function — adding a group meant editing three
//! places at once, and JSONL had no toolbar at all. After the refactor there are three layers:
//!
//! 1. **Semantic layer** (`csv_items` / `md_items` / `json_items` / `jsonl_items`):
//!    pure functions reading `&Tab` and producing `Vec<Group>` — each group has a name (rendered
//!    as a group caption) and a button sequence; no dioxus involved, unit-testable;
//! 2. **Layout layer** (`layout`): lines the groups up left to right as `(x, ToolBtn)`, inserting
//!    the group caption at each group's head, a **vertical separator** between groups, and the
//!    uniform `GAP` inside each group;
//! 3. **Render layer** (`Toolbar` / `ToolButton`): the one shell and the one-styled button.
//!
//! Button availability model (`ToolBtn::disabled` / `view_btn`):
//! - a feature the current view mode does **not support** → the whole group is not rendered
//!   (e.g. Markdown edit commands in preview view, CSV parse options in text view);
//! - unavailable for a non-view reason → `disabled`, greyed and unclickable (e.g. the preview
//!   of an oversized JSON);
//! - for paired view toggles (table view / text view), clicking the currently selected half is a
//!   no-op.
//!
//! Every button goes through the same command path, [`crate::ui::actions::run`] — the toolbar,
//! menus and macro recording can never disagree about a command.

use std::borrow::Cow;

use dioxus_native::prelude::*;

use crate::ui::actions::{self, Cmd, WysTool};
use crate::ui::commands::markdown::MdAct;
use crate::ui::components::{mdicons, mdws};
use crate::ui::state::csv::CsvView;
use crate::ui::state::json::JsonView;
use crate::ui::state::jsonl::JsonlView;
use crate::ui::state::{EditorState, CHROME_TOP, TOOLBAR_H};
use crate::ui::{Fmt, Locale, Msg};

// ---- uniform style constants ------------------------------------------------
/// Button height (px).
const BTN_H: f32 = 22.0;
/// The button's vertical offset inside the band: `(TOOLBAR_H - BTN_H) / 2`, centering it.
const BTN_Y: f32 = (TOOLBAR_H - BTN_H) / 2.0;
/// The band's left padding (px).
const PAD_X: f32 = 8.0;
/// Gap between buttons inside a group (px).
const GAP: f32 = 4.0;
/// Horizontal space reserved for a separator (px): the line centers with `(SEP_W-1)/2` blank on
/// each side.
const SEP_W: f32 = 13.0;
/// Button horizontal padding (one side, px); the width formula pads the label by it.
const BTN_PAD_X: f32 = 7.0;
/// Button font size (px), shared with the width formula to decide button width.
const FONT_SIZE: f32 = 12.0;
/// Approximate width of a fullwidth glyph in a line (px, ~1em at 12px).
const WIDE: f32 = FONT_SIZE;
/// Approximate width of a halfwidth glyph in a line (px).
const NARROW: f32 = FONT_SIZE * 0.58;
/// Icon button width (px): square.
const ICON_W: f32 = 24.0;

/// The kinds of dropdown panel on the WYSIWYG toolbar (emoji grid / heading levels / table grid /
/// edit mode / more▾ menu).
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum MenuKind {
    Emoji,
    Headings,
    Table,
    EditMode,
    More,
}

impl MenuKind {
    /// The hover tooltip (an icon button conveys no meaning without one).
    pub fn tip(self) -> Msg {
        match self {
            MenuKind::Emoji => Msg::TbMenuEmoji,
            MenuKind::Headings => Msg::TbMenuHeadings,
            MenuKind::Table => Msg::TbMenuInsertTable,
            MenuKind::EditMode => Msg::TbMenuEditMode,
            MenuKind::More => Msg::TbMenuMore,
        }
    }

    /// The triggering button's icon name.
    fn icon(self) -> &'static str {
        match self {
            MenuKind::Emoji => "emoji",
            MenuKind::Headings => "headings",
            MenuKind::Table => "table",
            MenuKind::EditMode => "edit",
            MenuKind::More => "more",
        }
    }
}

/// Icon name → hover tooltip. The icons are pure SVG with no text; without the tip the button
/// can only be guessed. Stores a key, not a string; text is fetched per current language at draw
/// time; undo/redo reuse the menu's existing wording.
fn icon_tip(icon: &str) -> Option<Msg> {
    Some(match icon {
        "bold" => Msg::TbTipBold,
        "italic" => Msg::TbTipItalic,
        "strike" => Msg::TbTipStrike,
        "link" => Msg::TbMdLink,
        "image" => Msg::TbMdImage,
        "list" => Msg::TbTipBulletList,
        "ordered-list" => Msg::TbTipNumberedList,
        "check" => Msg::TbTipTaskList,
        "outdent" => Msg::TbTipOutdent,
        "indent" => Msg::TbTipIndent,
        "quote" => Msg::TbMdQuote,
        "line" => Msg::TbTipHr,
        "code" => Msg::TbTipCodeBlock,
        "before" => Msg::TbTipInsertBefore,
        "after" => Msg::TbTipInsertAfter,
        "undo" => Msg::MenuUndo,
        "redo" => Msg::MenuRedo,
        _ => return None,
    })
}

/// Button labels: either a key from the static table (translated per language) or a
/// language-independent literal.
#[derive(Clone, PartialEq, Debug)]
pub enum ToolLabel {
    /// Translatable text: stores a [`Msg`], fetched per current language at draw time.
    Msg(Msg),
    /// Language-independent literals (halfwidth marks like `,`, `Tab`, `H1`) and group captions
    /// assembled on the fly.
    Text(String),
}

impl ToolLabel {
    /// Fetch the text for the current language. `Msg` borrows the static string, `Text` borrows
    /// itself — neither allocates.
    fn text<'a>(&'a self, loc: Locale) -> Cow<'a, str> {
        match self {
            ToolLabel::Msg(m) => Cow::Borrowed(m.get(loc)),
            ToolLabel::Text(s) => Cow::Borrowed(s.as_str()),
        }
    }
}

impl From<Msg> for ToolLabel {
    fn from(m: Msg) -> Self {
        ToolLabel::Msg(m)
    }
}

impl From<&'static str> for ToolLabel {
    fn from(s: &'static str) -> Self {
        ToolLabel::Text(s.to_string())
    }
}

impl PartialEq<Msg> for ToolLabel {
    fn eq(&self, m: &Msg) -> bool {
        matches!(self, ToolLabel::Msg(k) if k == m)
    }
}

impl PartialEq<&str> for ToolLabel {
    fn eq(&self, s: &&str) -> bool {
        matches!(self, ToolLabel::Text(t) if t == s)
    }
}

/// One toolbar button's semantics and layout width.
///
/// The render layer knows only this type and does not care which format the button belongs to —
/// that is exactly where "one uniform style" lands: buttons of all four toolbars draw with the
/// same `ToolButton` and look identical.
///
/// `PartialEq` is a dioxus component-props requirement (for diffing), not a logical need.
#[derive(Clone, PartialEq)]
pub struct ToolBtn {
    pub label: ToolLabel,
    pub w: f32,
    pub cmd: Cmd,
    pub on: bool,
    /// A pure caption (like the "Delimiter" prefix, a group name): non-interactive, no border,
    /// dim colors.
    pub label_only: bool,
    /// A vertical separator: the visual boundary between groups, non-interactive.
    pub sep: bool,
    /// Greyed out and unclickable (a non-view reason: the document itself cannot use it).
    pub disabled: bool,
    /// A paired view toggle (table view / text view …): clicking the `on` half is a no-op, or
    /// "clicking the selected view" would switch away from it, which feels backwards.
    pub view_btn: bool,
    /// An icon (a `mdicons` name): renders as an SVG icon button instead of a text button.
    pub icon: Option<&'static str>,
    /// A dropdown panel: clicking opens the panel instead of running a command (emoji/headings/
    /// table/edit mode/more).
    pub menu: Option<MenuKind>,
    /// Hover tooltip (stores a key; text is fetched at draw time). An icon button has only its
    /// icon and is unreadable without it; text buttons keep `None` and compute "label +
    /// accelerator" at render time (`tooltip::tip_text`).
    pub tip: Option<Msg>,
}

impl ToolBtn {
    /// Build a button from a label and a command. **Width is not computed here**: a `Msg` label's
    /// text varies by language, so `layout` computes widths for the current language (`sep` and
    /// icon buttons keep fixed widths).
    pub fn new(label: impl Into<ToolLabel>, cmd: Cmd, on: bool) -> Self {
        Self {
            label: label.into(),
            w: 0.0,
            cmd,
            on,
            label_only: false,
            sep: false,
            disabled: false,
            view_btn: false,
            icon: None,
            menu: None,
            tip: None,
        }
    }

    /// An icon button (WYSIWYG toolbar) of fixed `ICON_W` width.
    pub fn icon_btn(icon: &'static str, cmd: Cmd, on: bool) -> Self {
        let mut b = Self::new("", cmd, on);
        b.icon = Some(icon);
        b.w = ICON_W;
        b.tip = icon_tip(icon);
        b
    }

    /// A dropdown panel button (clicking opens the panel, not a command).
    pub fn menu_btn(kind: MenuKind) -> Self {
        let mut b = Self::icon_btn(kind.icon(), Cmd::About, false);
        b.menu = Some(kind);
        b.tip = Some(kind.tip());
        b
    }

    /// A non-interactive caption (a button group's semantic prefix telling users what the
    /// buttons after it are for).
    pub fn group_label(label: impl Into<ToolLabel>) -> Self {
        let mut b = Self::new(label, Cmd::About, false);
        b.label_only = true;
        b
    }

    /// A paired view toggle: `on` means this view is showing now and clicking again does nothing.
    pub fn view(label: impl Into<ToolLabel>, cmd: Cmd, on: bool) -> Self {
        let mut b = Self::new(label, cmd, on);
        b.view_btn = true;
        b
    }

    /// Greyed out and unclickable: the document itself (not the current view) cannot use it.
    pub fn disabled(label: impl Into<ToolLabel>, cmd: Cmd) -> Self {
        let mut b = Self::new(label, cmd, false);
        b.disabled = true;
        b
    }

    /// A vertical separator (between-group boundary).
    pub fn sep() -> Self {
        Self {
            label: ToolLabel::Text(String::new()),
            w: SEP_W,
            cmd: Cmd::About,
            on: false,
            label_only: true,
            sep: true,
            disabled: false,
            view_btn: false,
            icon: None,
            menu: None,
            tip: None,
        }
    }
}

/// A group of function buttons: `name` renders as the group caption (View / Edit / Parse …);
/// `None` means no caption (the WYSIWYG icon strip).
pub struct Group {
    pub name: Option<Msg>,
    pub items: Vec<ToolBtn>,
}

impl Group {
    fn new(name: Msg, items: Vec<ToolBtn>) -> Self {
        Group {
            name: Some(name),
            items,
        }
    }

    /// A group with no name (the WYSIWYG icon strip).
    fn bare(items: Vec<ToolBtn>) -> Self {
        Group { name: None, items }
    }
}

/// Button width: ASCII glyphs ≈ `NARROW`px, fullwidth ≈ `WIDE`px, plus `BTN_PAD_X` on each side;
/// never smaller than the button's height (a square floor) so even very short labels (`,`, `|`)
/// stay clickable.
///
/// This is the key to "one visual rhythm": labels differ in length but height, corner radius,
/// padding and font size are identical, and width adapts only to the text — the four toolbars
/// never look like a patchwork of wide and narrow buttons.
pub fn btn_width(label: &str) -> f32 {
    let text: f32 = label
        .chars()
        .map(|c| if c.is_ascii() { NARROW } else { WIDE })
        .sum();
    (text + BTN_PAD_X * 2.0).max(BTN_H)
}

/// The band's total width (the last button's right edge after layout) — used for overflow
/// decisions (pure function, unit-testable).
fn toolbar_total_w(groups: &[Group], loc: Locale) -> f32 {
    layout(groups, loc).last().map_or(0.0, |(x, b)| x + b.w)
}

/// Line the groups up left to right as a sequence with x coordinates.
///
/// Each group first places a group caption ([`ToolBtn::group_label`]; unnamed groups skip it —
/// the WYSIWYG toolbar has no captions), then a vertical separator goes between groups
/// ([`ToolBtn::sep`]): the line centers in [`SEP_W`]px with natural blank on each side.
///
/// Text-button widths are computed here for the current language ([`btn_width`]): the label is a
/// [`Msg`] key whose text varies by language, so construction time cannot know it.
fn layout(groups: &[Group], loc: Locale) -> Vec<(f32, ToolBtn)> {
    let mut out = Vec::new();
    let mut x = PAD_X;
    for (gi, g) in groups.iter().enumerate() {
        if gi > 0 {
            out.push((x, ToolBtn::sep()));
            x += SEP_W;
        }
        if let Some(name) = g.name {
            let text = format!("{}:", name.get(loc));
            let lw = btn_width(&text);
            let mut lbl = ToolBtn::new(ToolLabel::Text(text), Cmd::About, false);
            lbl.label_only = true;
            lbl.w = lw;
            out.push((x, lbl));
            x += lw + GAP;
        }
        for (bi, b) in g.items.iter().enumerate() {
            if bi > 0 {
                x += GAP;
            }
            let mut b = b.clone();
            if !b.sep && b.icon.is_none() {
                b.w = btn_width(&b.label.text(loc));
            }
            out.push((x, b));
            x += out.last().unwrap().1.w;
        }
    }
    out
}

/// The shell: a [`TOOLBAR_H`]px band docked under the tab strip (under the find bar when it is
/// open).
///
/// Recomputing the layout on a visibility flip is the fallback — the open/load/switch-tab paths
/// already call [`EditorState::relayout`] each; this only guards against a path being missed
/// (the toolbar ending up covered by the viewport).
#[component]
pub fn Toolbar() -> Element {
    let mut state: Signal<EditorState> = use_context();
    // The open dropdown panel (button x anchor + kind). A full-screen transparent backdrop sits
    // underneath; clicking outside closes it.
    let menu: Signal<Option<(f32, MenuKind)>> = use_signal(|| None);

    // Conditional write: an unconditional `state.write()` during render mark_dirties when the
    // guard drops, so the component marks itself dirty on every render and re-triggers another
    // render pass. `sync_toolbar_layout` only needs to write when visibility actually flips.
    {
        let want = { state.read().toolbar_h() > 0.0 };
        if want != state.read().toolbar_shown {
            state.write().sync_toolbar_layout();
        }
    }

    // Snapshot: read everything this component needs in one go. A dioxus Signal read guard
    // cannot be held across functions, and reading centrally also keeps the layout code below
    // pure and independent of the render context.
    let groups: Vec<Group> = {
        let st = state.read();
        let Some(tab) = st.tabs.get(st.active) else {
            return rsx! {};
        };
        let vw = st.win.w;
        let mut groups = Vec::new();
        let mut has_wys = false;
        if tab.csv.enabled {
            groups.extend(csv_items(&tab.csv));
        }
        if tab.spec.name == "Markdown" {
            has_wys = true;
            if tab.md_wys {
                groups.extend(md_groups(
                    MdToolbarMode::Document,
                    Some(&mdws::focus_state()),
                    mdws::can_undo(),
                    mdws::can_redo(),
                ));
            } else {
                groups.extend(md_groups(
                    MdToolbarMode::Text,
                    None,
                    tab.doc.can_undo(),
                    tab.doc.can_redo(),
                ));
            }
        }
        if tab.spec.name == "JSON" {
            let too_large = JsonView::is_too_large(&tab.doc);
            groups.extend(json_items(&tab.json, too_large));
        }
        if tab.spec.name == "JSON Lines" || tab.jsonl.enabled {
            groups.extend(jsonl_items(&tab.jsonl));
        }
        // Yield for narrow windows (see fit_wys_groups): the status-bar text
        // takes ~150px at the right edge; reserve it too. The markdown view
        // toggles stay put and the entries around them yield instead.
        if has_wys {
            let loc = st.locale();
            groups = fit_wys_groups(groups, (vw - 160.0).max(320.0), loc);
        }
        groups
    };
    if groups.is_empty() {
        return rsx! {};
    }

    let t = state.read().theme();
    let vw = state.read().win.w;
    let vh = state.read().win.h;
    let y = CHROME_TOP + state.read().find.bar_height();
    let loc = state.read().locale();

    let bar_style = format!(
        "position:absolute; left:0; top:{y}px; width:{vw}px; height:{TOOLBAR_H}px;
         background:{bg}; border-bottom:1px solid {border}; overflow:hidden;
         user-select:none;",
        bg = t.gutter_bg,
        border = t.menu_border
    );

    // ---- status text on the right ---------------------------------------------
    // A pure read snapshot — this once fetched the status text with `state.write()`, and the
    // guard drop unconditionally mark_dirtied, self-marking on every render and perpetuating the
    // re-render (conflicting with the conditional-write iron rule above). The CSV row count goes
    // through the read-only cache (`csv_rows_cached`: no text on the first frame with a cold
    // cache; it appears on the next state change once warm).
    let status_text: Option<String> = {
        let st = state.read();
        // State stores a key, not a rendered string; the text is fetched per current language
        // at draw time
        status_text(&st).map(|m| st.tr(m).into_owned())
    };
    let fg = t.menu_fg.to_string();

    let placed = layout(&groups, loc);
    let btn_els: Vec<Element> = placed
        .into_iter()
        .map(|(x, b)| {
            rsx! { ToolButton { state, menu, x, btn: b, t, loc } }
        })
        .collect();

    // Dropdown panel: lay a full-window transparent backdrop (click outside closes) and float
    // the panel above it.
    let open = menu();
    let panel = open.map(|(mx, kind)| {
        rsx! { DropdownPanel { state, menu, x: mx, y: y + TOOLBAR_H, vw, vh, kind, t, loc } }
    });

    rsx! {
        div {
            role: "toolbar",
            style: "{bar_style}",
            onpointerdown: move |evt| {
                evt.stop_propagation();
                evt.prevent_default();
            },
            { btn_els.into_iter() }
            if let Some(s) = status_text {
                div {
                    style: "position:absolute; right:12px; top:0; height:{TOOLBAR_H}px;
                            line-height:{TOOLBAR_H}px; font-size:11px; color:{fg};
                            opacity:0.75; white-space:nowrap; max-width:50%;
                            overflow:hidden; text-overflow:ellipsis;",
                    "{s}"
                }
            }
        }
        if let Some(p) = panel {
            { p }
        }
    }
}

/// Yield space in narrow windows (pure function, unit-testable). The wys icon strip and the
/// view + table groups overflow the viewport in a narrow window; buttons are absolutely positioned
/// left to right, so anything past the edge is unreachable and items can only be removed.
///
/// **The view toggles never leave the strip.** They are the document's only view switch, they
/// head the strip (so they stay on screen even when the tail clips), and their on-state is what
/// tells the user which view is active. Everything else yields around them, in this order:
///
/// 1. the table group's "Table:" caption (its buttons remain);
/// 2. the trailing "Edit mode" dropdown — its items *are* the two view toggles, which are right
///    there on screen, so this costs nothing;
/// 3. the "More" dropdown (export / word count; word count also lives in Tools);
/// 4. the contextual table group itself (only present while a cell is focused).
///
/// This used to collapse the two toggles into a single "View" (eye) menu and finally drop them.
/// One eye icon in place of 文档视图/文本视图 contradicts the design: the pair is the strip's
/// anchor, the eye hides which view is active, and "the view switch was unfindable" was the
/// original user complaint that put the two labeled buttons there.
fn fit_wys_groups(mut groups: Vec<Group>, limit: f32, loc: Locale) -> Vec<Group> {
    if fits_width(&groups, limit, loc) {
        return groups;
    }
    // Position before the caption is cleared: the group keeps its buttons when
    // only the caption goes, and the last step needs to find it again.
    let table_ix = groups
        .iter()
        .position(|g| g.name == Some(Msg::TbGroupTable));
    if let Some(ix) = table_ix {
        groups[ix].name = None;
    }
    if fits_width(&groups, limit, loc) {
        return groups;
    }
    for icon in ["edit", "more"] {
        drop_icon(&mut groups, icon);
        if fits_width(&groups, limit, loc) {
            return groups;
        }
    }
    if let Some(ix) = table_ix {
        groups.remove(ix);
    }
    groups
}

fn fits_width(groups: &[Group], limit: f32, loc: Locale) -> bool {
    toolbar_total_w(groups, loc) <= limit
}

/// Remove the first button carrying `icon` (the trailing single-button entries are identified by
/// their icon; labels are localized).
fn drop_icon(groups: &mut [Group], icon: &str) {
    for g in groups.iter_mut() {
        if let Some(i) = g.items.iter().position(|b| b.icon == Some(icon)) {
            g.items.remove(i);
            return;
        }
    }
}

/// The band's button: the one style shared by all four toolbars (height / corner / border /
/// selected state).
///
/// The selected state (`on`): accent background + white text, paired one-to-one with toggle
/// states like "Table view / Header row"; the rest are action keys ("Beautify", "Sort") whose
/// `on` is always `false`.
/// Hover / press use [`crate::ui::motion`]'s unified colors, the same source as the find bar and
/// menu buttons.
/// Greying (`disabled`): lower opacity, no hover feedback, clicking does nothing — telling the
/// user "this feature cannot act on the current document" instead of vanishing silently.
#[component]
fn ToolButton(
    mut state: Signal<EditorState>,
    mut menu: Signal<Option<(f32, MenuKind)>>,
    x: f32,
    btn: ToolBtn,
    t: &'static crate::highlight::Theme,
    /// The current language: passed in by the shell (which subscribes to state); this component
    /// reads no state during render — doing so would subscribe every button to the whole
    /// `EditorState` (a full re-render per keystroke).
    loc: Locale,
) -> Element {
    // Vertical separator: the 1px boundary between groups. A real `<hr>` (`.k-sep` carries
    // pointer-events:none and its color from the class); the toolbar positions each part
    // absolutely, so geometry is inline.
    if btn.sep {
        let style = format!(
            "position:absolute; left:{x}px; top:6px; width:1px; height:{h}px;",
            h = TOOLBAR_H - 12.0,
        );
        return rsx! {
            hr { class: "k-sep", style: "{style}" }
        };
    }
    let label_text = btn.label.text(loc).into_owned();
    // A pure caption: unclickable, no border, dim colors — it only tells the user what the run
    // of buttons after it is for
    if btn.label_only {
        let style = format!(
            "position:absolute; left:{x}px; top:{BTN_Y}px; width:{w}px; height:{BTN_H}px;
             line-height:{BTN_H}px; text-align:right; font-size:{FONT_SIZE}px;
             box-sizing:border-box; color:{fg}; opacity:0.7; pointer-events:none;",
            w = btn.w,
            fg = t.menu_fg,
        );
        return rsx! {
            div { style: "{style}", "{label_text}" }
        };
    }
    // Tri-state in the `.tbtn` / `.tbtn.on` / `.tbtn.dis` classes (components::style).
    let style = format!(
        "position:absolute; left:{x}px; top:{BTN_Y}px; width:{w}px; height:{BTN_H}px;
         line-height:{BTN_H}px; text-align:center; font-size:{FONT_SIZE}px;
         border-radius:3px; box-sizing:border-box; cursor:default; touch-action:none;",
        w = btn.w
    );
    // View toggles: the already-selected half is a no-op (clicking must not switch away from
    // itself).
    let inert = btn.disabled || (btn.view_btn && btn.on);
    // Button content: an SVG icon or a text label. **Always through dangerous_inner_html and
    // never empty** — blitz's mutation writer replaces children of an empty inner_html with a
    // zero node and the next template placeholder's load_child panics out of bounds (hit in
    // practice: opening any md crashed). Labels all come from static strings in code (no user
    // content), so nothing is escaped.
    // The icon SVG uses an explicit color: white when `on`, the foreground otherwise (greying is
    // `.tbtn.dis`'s opacity; the color does not change)
    let icon_fg = if btn.on { "#ffffff" } else { t.menu_fg };
    let content = btn
        .icon
        .map(|name| mdicons::svg(name, icon_fg))
        .unwrap_or_else(|| label_text.clone());
    // Iron rule (not a debug_assert): an empty inner_html in blitz = replacement by a zero node →
    // the template placeholder's load_child panics out of bounds. Unknown icon names
    // (`mdicons::svg` returning an empty string) are covered too — a crash in the render loop
    // gets no second chance.
    let content = if content.is_empty() {
        "\u{a0}".to_string() // &nbsp;
    } else {
        content
    };
    // Hover tip: icon buttons use `btn.tip` (the icon-name table), text buttons use "label +
    // accelerator".
    let mut tip: Signal<crate::ui::components::Tip> = use_context();
    let tip_label = match btn.tip {
        Some(m) => m.get(loc).to_string(),
        None => crate::ui::components::tip_text(&label_text, btn.cmd),
    };
    rsx! {
        div {
            class: if btn.disabled {
                "tbtn dis"
            } else if btn.on {
                "tbtn on"
            } else {
                "tbtn"
            },
            role: "button",
            style: "{style}",
            onmouseenter: move |evt| {
                if tip_label.is_empty() {
                    return;
                }
                let c = evt.data.client_coordinates();
                tip.write().show(tip_label.clone(), c.x, c.y);
            },
            onmouseleave: move |_| tip.write().hide(),
            onpointerdown: move |evt| {
                tip.write().hide();
                evt.stop_propagation();
                evt.prevent_default();
            },
            onclick: move |evt| {
                tip.write().hide();
                evt.stop_propagation();
                evt.prevent_default();
                if inert {
                    return;
                }
                if let Some(kind) = btn.menu {
                    // Dropdown panel: clicking the same button again = collapse
                    let next = match menu() {
                        Some((_, k)) if k == kind => None,
                        _ => Some((x, kind)),
                    };
                    menu.set(next);
                    return;
                }
                actions::run(&mut state.write(), btn.cmd);
            },
            dangerous_inner_html: "{content}",
        }
    }
}

// ===========================================================================
// dropdown panels (WYSIWYG toolbar: emoji / headings / table / edit mode / more)
// ===========================================================================

/// The toolbar's dropdown panel: a full-window transparent backdrop underneath (click outside to
/// close), with the panel floating anchored to its button.
#[component]
fn DropdownPanel(
    mut state: Signal<EditorState>,
    mut menu: Signal<Option<(f32, MenuKind)>>,
    x: f32,
    y: f32,
    vw: f32,
    vh: f32,
    kind: MenuKind,
    t: &'static crate::highlight::Theme,
    /// The current language (passed by the shell; see ToolButton's `loc` comment).
    loc: Locale,
) -> Element {
    let pw = match kind {
        MenuKind::Emoji => 276.0,
        MenuKind::Table => 172.0,
        _ => 176.0,
    };
    let ph = match kind {
        MenuKind::Emoji => 8.0 * 30.0 + 10.0,
        MenuKind::Table => 6.0 * 26.0 + 10.0,
        MenuKind::Headings => 7.0 * 26.0 + 10.0,
        MenuKind::EditMode => 3.0 * 26.0 + 10.0,
        MenuKind::More => 4.0 * 26.0 + 10.0,
    };
    // Pull the panel inward when it hugs the right, so it never overflows the window.
    let px = (x.max(8.0)).min((vw - pw - 8.0).max(8.0));
    let overlay = format!(
        "position:absolute; left:0; top:0; width:{vw}px; height:{vh}px; \
         z-index:60; background:transparent;"
    );
    let panel = format!(
        "position:absolute; left:{px}px; top:{y}px; width:{pw}px; height:{ph}px;
         z-index:61; background:{bg}; border:1px solid {bd}; border-radius:6px;
         box-sizing:border-box; padding:4px; overflow:hidden; user-select:none;
         touch-action:none; box-shadow:0 4px 16px {shadow};",
        bg = t.menu_bg,
        bd = t.menu_border,
        shadow = t.shadow,
    );
    // The panels are shared by both markdown views: the commands (and the
    // kernel-derived on-states) follow the active mode — document view drives
    // the wys kernel, text view edits the markdown source.
    let in_wys = {
        let st = state.read();
        st.tabs.get(st.active).is_some_and(|t| t.md_wys)
    };
    let rows: Vec<Element> = match kind {
        MenuKind::Headings => [
            (Msg::TbHead1, 1),
            (Msg::TbHead2, 2),
            (Msg::TbHead3, 3),
            (Msg::TbHead4, 4),
            (Msg::TbHead5, 5),
            (Msg::TbHead6, 6),
        ]
        .into_iter()
        .map(|(label, n)| {
            let on = in_wys && mdws::focus_state().head == Some(n);
            let cmd = if in_wys {
                Cmd::WysTool(WysTool::SetHead(n))
            } else {
                Cmd::MdEdit(MdAct::Heading(n))
            };
            rsx! { PanelRow { state, menu, label, on, cmd, t, loc } }
        })
        .chain(std::iter::once({
            // Body: when the focused block is a heading, clicking the same level again restores
            // a paragraph (set_head's toggle semantics); a non-heading block has no corresponding
            // action — greyed no-op (it once bound Cmd::About and opened the About dialog).
            // Text view: strip the heading marker from the target lines.
            let (on, cmd, disabled) = if in_wys {
                match mdws::focus_head_level() {
                    Some(l) => (true, Cmd::WysTool(WysTool::SetHead(l)), Some(false)),
                    None => (false, Cmd::WysTool(WysTool::SetHead(1)), Some(true)),
                }
            } else {
                (false, Cmd::MdEdit(MdAct::Body), Some(false))
            };
            rsx! { PanelRow { state, menu, label: Msg::TbBody, on, cmd, disabled, t, loc } }
        }))
        .collect(),
        MenuKind::EditMode => {
            let wys = {
                let st = state.read();
                st.tabs.get(st.active).is_some_and(|t| t.md_wys)
            };
            [
                // Two views now: document view (WYSIWYG editing) and text view.
                (Msg::TbViewWys, Cmd::MdWys, wys),
                (Msg::TbViewText, Cmd::MdText, !wys),
            ]
            .into_iter()
            .map(|(label, cmd, on)| {
                rsx! { PanelRow { state, menu, label, on, cmd, t, loc } }
            })
            .collect()
        }
        MenuKind::More => [
            (Msg::TbMoreExportHtml, Cmd::MdExportHtml),
            (Msg::TbMoreExportMd, Cmd::MdExportMd),
            (Msg::TbMoreWordCount, Cmd::WordCount),
        ]
        .into_iter()
        .map(|(label, cmd)| {
            rsx! { PanelRow { state, menu, label, on: false, cmd, t, loc } }
        })
        .collect(),
        MenuKind::Emoji => mdws::EMOJIS
            .iter()
            .enumerate()
            .map(|(i, e)| {
                // 9-column grid; 30px step (28px cell + 2px gap)
                let lx = (i % 9) as f32 * 30.0 + 4.0;
                let ly = (i / 9) as f32 * 30.0 + 4.0;
                rsx! {
                    div {
                        key: "{i}",
                        style: "position:absolute; left:{lx}px; top:{ly}px; width:28px; height:28px;
                                line-height:28px; text-align:center; font-size:16px; cursor:default;
                                border-radius:4px;",
                        onclick: move |evt| {
                            evt.stop_propagation();
                            // Document view: the wys kernel inserts the emoji;
                            // text view: a literal insert into the source.
                            let cmd = if in_wys {
                                Cmd::WysTool(WysTool::Emoji(i as u8))
                            } else {
                                Cmd::MdEdit(MdAct::Emoji(i as u8))
                            };
                            actions::run(&mut state.write(), cmd);
                            menu.set(None);
                        },
                        "{e}"
                    }
                }
            })
            .collect(),
        MenuKind::Table => (0..6usize)
            .flat_map(|r| (0..6usize).map(move |c| (r, c)))
            .map(|(r, c)| {
                let (rr, cc) = (r, c);
                let lx = c as f32 * 26.0 + 4.0;
                let ly = r as f32 * 26.0 + 4.0;
                let bd = t.menu_border;
                rsx! {
                    div {
                        key: "{rr}-{cc}",
                        style: "position:absolute; left:{lx}px; top:{ly}px; width:24px; height:24px;
                                box-sizing:border-box; border:1px solid {bd}; cursor:default;",
                        onclick: move |evt| {
                            evt.stop_propagation();
                            // Document view: the wys kernel builds the grid;
                            // text view: a source template of the same size.
                            let cmd = if in_wys {
                                Cmd::WysTool(WysTool::InsertTable {
                                    rows: rr as u8 + 1,
                                    cols: cc as u8 + 1,
                                })
                            } else {
                                Cmd::MdEdit(MdAct::TableGrid {
                                    rows: rr as u8 + 1,
                                    cols: cc as u8 + 1,
                                })
                            };
                            actions::run(&mut state.write(), cmd);
                            menu.set(None);
                        },
                    }
                }
            })
            .collect(),
    };
    rsx! {
        div {
            style: "{overlay}",
            onpointerdown: move |evt| {
                evt.stop_propagation();
                menu.set(None);
            },
        }
        div {
            style: "{panel}",
            { rows.into_iter() }
        }
    }
}

/// One menu row inside a panel (text + hover highlight + selected check; clicking runs the command
/// and collapses the panel).
/// `disabled`: greyed and clicking does nothing — "Body" has no action on a non-heading block; it
/// once bound `Cmd::About` (opening the About dialog).
#[component]
fn PanelRow(
    mut state: Signal<EditorState>,
    mut menu: Signal<Option<(f32, MenuKind)>>,
    label: Msg,
    on: bool,
    cmd: Cmd,
    disabled: Option<bool>,
    t: &'static crate::highlight::Theme,
    /// The current language (passed by the shell; no state reads during render — see
    /// ToolButton's `loc` comment).
    loc: Locale,
) -> Element {
    let disabled = disabled.unwrap_or(false);
    let text = label.get(loc);
    let mark = if on { "✓ " } else { "" };
    let opacity = if disabled { "0.4" } else { "1" };
    rsx! {
        div {
            class: if on { "prow on" } else { "prow" },
            style: "position:relative; margin:1px 4px; height:24px;
                    line-height:24px; padding-left:8px; font-size:12px; border-radius:3px;
                    box-sizing:border-box; cursor:default; opacity:{opacity};
                    white-space:nowrap; overflow:hidden; text-overflow:ellipsis;",
            onpointerdown: move |evt| {
                evt.stop_propagation();
                evt.prevent_default();
            },
            onclick: move |evt| {
                evt.stop_propagation();
                evt.prevent_default();
                if !disabled {
                    actions::run(&mut state.write(), cmd);
                }
                menu.set(None);
            },
            "{mark}{text}"
        }
    }
}

// ===========================================================================
// semantic layer: several named groups per document type (pure functions, read &Tab, no dioxus,
// unit-testable)
// ===========================================================================

/// CSV groups: **View** (table view / text view) + **Parse** (header row, delimiter) +, in table
/// view, **row/column editing** (insert/delete rows and columns, acting on the selected
/// rows/columns).
/// Sorting lives on the header captions (click cycles three states, the arrow shows next to the
/// column name).
///
/// Parse and edit options only make sense in table view (text view does not parse), so these
/// groups do not render in text view. They are visible by default for csv/tsv documents, and the
/// same features apply fully to TSV (the delimiter is just a parse parameter).
fn csv_items(csv: &CsvView) -> Vec<Group> {
    // No leading caption: the view toggles are self-explanatory, and dropping
    // the "视图:" label removes the toolbar's lead-in blank (the first item).
    let mut v = vec![Group::bare(vec![
        ToolBtn::view(Msg::TbViewTable, Cmd::CsvToggleMode, csv.table),
        ToolBtn::view(Msg::TbViewText, Cmd::CsvToggleMode, !csv.table),
    ])];
    if csv.table {
        v.push(Group::new(
            Msg::TbGroupParse,
            vec![
                ToolBtn::new(Msg::TbCsvHeader, Cmd::CsvToggleHeader, csv.header),
                ToolBtn::group_label(Msg::TbCsvDelimLabel),
                ToolBtn::new(",", Cmd::CsvDelim(b','), csv.delim == b','),
                ToolBtn::new("Tab", Cmd::CsvDelim(b'\t'), csv.delim == b'\t'),
                ToolBtn::new(";", Cmd::CsvDelim(b';'), csv.delim == b';'),
                ToolBtn::new("|", Cmd::CsvDelim(b'|'), csv.delim == b'|'),
            ],
        ));
        v.push(Group::new(
            Msg::MenuEdit,
            vec![
                ToolBtn::new(Msg::TbCsvRowAbove, Cmd::CsvRowAbove, false),
                ToolBtn::new(Msg::TbCsvRowBelow, Cmd::CsvRowBelow, false),
                ToolBtn::new(Msg::TbCsvRowDel, Cmd::CsvRowDel, false),
                ToolBtn::new(Msg::TbCsvColLeft, Cmd::CsvColLeft, false),
                ToolBtn::new(Msg::TbCsvColRight, Cmd::CsvColRight, false),
                ToolBtn::new(Msg::TbCsvColDel, Cmd::CsvColDel, false),
            ],
        ));
    }
    v
}

/// Which markdown view the strip is built for: **one construction, two
/// command sets** — the document view drives the wys kernel, the text view
/// edits the markdown source. Buttons whose action has no text-view
/// equivalent (outdent/indent, insert-before/after) are greyed out in text
/// view rather than silently doing nothing.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum MdToolbarMode {
    Document,
    Text,
}

/// The markdown toolbar — the **same strip in both views**: the view group is
/// always the head, the icon strip follows in a fixed order (emoji/headings
/// menus, inline formats, lists, block tools, undo/redo), and the trailing
/// edit-mode dropdown + more menu stay. Switching between the document and
/// text view never moves a button, only the selected state and the command
/// behind each button travel (the same consistency rule the old three-mode
/// bands followed; user feedback: the view entries must line up across
/// modes).
///
/// `f` is the wys kernel's focus state (inline marks / list / quote / code
/// on-states, focused-cell table group); the text view has no kernel, so it
/// passes `None` and every on-state reads false. `can_undo` / `can_redo` come
/// from the active view's owner (wys model / piece table). `Cmd::Undo` /
/// `Cmd::Redo` need no per-mode command: in document view the wys intercept
/// reroutes them to the wys model.
fn md_groups(
    mode: MdToolbarMode,
    f: Option<&mdws::WysFocus>,
    can_undo: bool,
    can_redo: bool,
) -> Vec<Group> {
    use WysTool as Wt;
    let document = mode == MdToolbarMode::Document;
    let m = f.map(|f| &f.inline);
    // on-state of an inline mark: the document view reads the kernel's focus,
    // the text view has none (all off).
    let mark = |v: bool| document && v;

    // Group 0: the view group (same order and labels in both modes — the
    // buttons never move). No leading caption: the toggles are
    // self-explanatory, and dropping the "视图:" label removes the toolbar's
    // lead-in blank (the first item).
    let view_group = Group::bare(vec![
        ToolBtn::view(Msg::TbViewWys, Cmd::MdWys, document),
        ToolBtn::view(Msg::TbViewText, Cmd::MdText, !document),
    ]);

    // The strip: (icon, wys command, text command, on-state-in-document-view).
    // The wys column drives the kernel; the text column edits the markdown
    // source. The four wys-structure buttons (outdent/indent/before/after)
    // have no honest source-view semantics — they carry a placeholder text
    // command here and are greyed out below.
    let strip: Vec<(&'static str, Cmd, Cmd, bool)> = vec![
        (
            "bold",
            Cmd::WysTool(Wt::Wrap(wysiwyg::WrapKind::Bold)),
            Cmd::MdEdit(MdAct::Bold),
            m.is_some_and(|m| m.bold),
        ),
        (
            "italic",
            Cmd::WysTool(Wt::Wrap(wysiwyg::WrapKind::Italic)),
            Cmd::MdEdit(MdAct::Italic),
            m.is_some_and(|m| m.italic),
        ),
        (
            "strike",
            Cmd::WysTool(Wt::Wrap(wysiwyg::WrapKind::Del)),
            Cmd::MdEdit(MdAct::Strike),
            m.is_some_and(|m| m.strike),
        ),
        (
            "link",
            Cmd::WysTool(Wt::Link),
            Cmd::MdEdit(MdAct::Link),
            m.is_some_and(|m| m.link),
        ),
        // image insert (a URL input → the `![](url)` template)
        (
            "image",
            Cmd::WysTool(Wt::Image),
            Cmd::MdEdit(MdAct::Image),
            false,
        ),
        (
            "list",
            Cmd::WysTool(Wt::ToggleList { ordered: false }),
            Cmd::MdEdit(MdAct::Bullet),
            document && f.is_some_and(|f| f.ordered == Some(false) && !f.task),
        ),
        (
            "ordered-list",
            Cmd::WysTool(Wt::ToggleList { ordered: true }),
            Cmd::MdEdit(MdAct::Numbered),
            document && f.is_some_and(|f| f.ordered == Some(true) && !f.task),
        ),
        (
            "check",
            Cmd::WysTool(Wt::ToggleTask),
            Cmd::MdEdit(MdAct::TaskList),
            document && f.is_some_and(|f| f.task),
        ),
        (
            "outdent",
            Cmd::WysTool(Wt::Outdent),
            Cmd::MdEdit(MdAct::Hr),
            false,
        ),
        (
            "indent",
            Cmd::WysTool(Wt::Indent),
            Cmd::MdEdit(MdAct::Hr),
            false,
        ),
        (
            "quote",
            Cmd::WysTool(Wt::ToggleQuote),
            Cmd::MdEdit(MdAct::Quote),
            document && f.is_some_and(|f| f.quote),
        ),
        (
            "line",
            Cmd::WysTool(Wt::InsertHr),
            Cmd::MdEdit(MdAct::Hr),
            false,
        ),
        (
            "code",
            Cmd::WysTool(Wt::ToggleCodeBlock),
            Cmd::MdEdit(MdAct::CodeBlock),
            document && f.is_some_and(|f| f.code),
        ),
        (
            "inline-code",
            Cmd::WysTool(Wt::Wrap(wysiwyg::WrapKind::Code)),
            Cmd::MdEdit(MdAct::Code),
            m.is_some_and(|m| m.code),
        ),
        (
            "before",
            Cmd::WysTool(Wt::InsertBefore),
            Cmd::MdEdit(MdAct::Hr),
            false,
        ),
        (
            "after",
            Cmd::WysTool(Wt::InsertAfter),
            Cmd::MdEdit(MdAct::Hr),
            false,
        ),
    ];
    let mut items: Vec<ToolBtn> = strip
        .into_iter()
        .map(|(icon, wys_cmd, text_cmd, on)| {
            let cmd = if document { wys_cmd } else { text_cmd };
            ToolBtn::icon_btn(icon, cmd, mark(on))
        })
        .collect();
    // The three menus are shared: the emoji grid / heading list / table grid
    // panels read the mode from state and dispatch per-view commands.
    items.insert(0, ToolBtn::menu_btn(MenuKind::Emoji));
    items.insert(1, ToolBtn::menu_btn(MenuKind::Headings));
    items.push(ToolBtn::menu_btn(MenuKind::Table));
    items.push(ToolBtn::icon_btn("undo", Cmd::Undo, false));
    items.push(ToolBtn::icon_btn("redo", Cmd::Redo, false));
    for b in &mut items {
        // grey out undo/redo when the active view's owner has no more steps
        if (!can_undo && b.icon == Some("undo")) || (!can_redo && b.icon == Some("redo")) {
            b.disabled = true;
        }
        // Text view: the four wys-structure buttons have no source-view
        // semantics (see the strip table) — grey them out.
        if !document
            && matches!(
                b.icon,
                Some("outdent") | Some("indent") | Some("before") | Some("after")
            )
        {
            b.disabled = true;
        }
    }
    items.push(ToolBtn::menu_btn(MenuKind::EditMode));
    items.push(ToolBtn::menu_btn(MenuKind::More));

    let mut groups = vec![view_group, Group::bare(items)];
    // Table-operations group: appears only when the focus is inside a table
    // cell — a document-view concept (the text view edits `|` rows as plain
    // lines and has no cell focus).
    if let Some(f) = f.filter(|f| document && f.td) {
        groups.push(Group::new(
            Msg::TbGroupTable,
            vec![
                ToolBtn::new(
                    Msg::TbTableRowPlus,
                    Cmd::WysTool(Wt::Table(wysiwyg::TableCmd::InsertRowAfter)),
                    false,
                ),
                ToolBtn::new(
                    Msg::TbTableRowMinus,
                    Cmd::WysTool(Wt::Table(wysiwyg::TableCmd::DeleteRow)),
                    false,
                ),
                ToolBtn::new(
                    Msg::TbTableColPlus,
                    Cmd::WysTool(Wt::Table(wysiwyg::TableCmd::InsertColAfter)),
                    false,
                ),
                ToolBtn::new(
                    Msg::TbTableColMinus,
                    Cmd::WysTool(Wt::Table(wysiwyg::TableCmd::DeleteCol)),
                    false,
                ),
                ToolBtn::new(
                    Msg::TbAlignLeft,
                    Cmd::WysTool(Wt::Table(wysiwyg::TableCmd::Align(1))),
                    f.td_align == Some(1),
                ),
                ToolBtn::new(
                    Msg::TbAlignCenter,
                    Cmd::WysTool(Wt::Table(wysiwyg::TableCmd::Align(2))),
                    f.td_align == Some(2),
                ),
                ToolBtn::new(
                    Msg::TbAlignRight,
                    Cmd::WysTool(Wt::Table(wysiwyg::TableCmd::Align(3))),
                    f.td_align == Some(3),
                ),
            ],
        ));
    }
    groups
}

/// JSON groups: **View** (preview view / text view) plus, in text mode, **Format** (beautify) or,
/// in tree view, **Browse** (expand/collapse all). "Beautify" exists only in text mode (it would
/// rewrite the document in tree view, throwing off every row number in the tree, and expand/
/// collapse already suffices inside the tree).
///
/// The tree view being unavailable for oversized documents (> 8 MiB) is a reason of the document
/// itself (outside the view), so "Preview view" is **greyed out**, not hidden — the user can see
/// the feature exists and why it will not respond (the status bar explains), instead of assuming
/// JSON has no preview.
fn json_items(json: &JsonView, too_large: bool) -> Vec<Group> {
    let preview = if too_large && !json.enabled {
        ToolBtn::disabled(Msg::TbViewPreview, Cmd::JsonPreview)
    } else {
        ToolBtn::view(Msg::TbViewPreview, Cmd::JsonPreview, json.enabled)
    };
    let mut v = vec![Group::bare(vec![
        preview,
        ToolBtn::view(Msg::TbViewText, Cmd::JsonPreview, !json.enabled),
    ])];
    if json.enabled {
        v.push(Group::new(
            Msg::TbGroupBrowse,
            vec![
                ToolBtn::new(Msg::TbJsonExpandAll, Cmd::JsonExpandAll, false),
                ToolBtn::new(Msg::TbJsonCollapseAll, Cmd::JsonCollapseAll, false),
            ],
        ));
    } else {
        v.push(Group::new(
            Msg::TbGroupFormat,
            vec![ToolBtn::new(Msg::TbJsonBeautify, Cmd::FormatJson, false)],
        ));
    }
    v
}

/// JSONL groups: **View** (preview view / text view) plus, with the preview on, **Operations**
/// (collapse all; per-line expansion goes through the line-head marker, matching the JSONL
/// preview's interaction model). On by default for "JSON Lines" documents, or when the user
/// manually enables the JSONL preview on any document.
fn jsonl_items(jsonl: &JsonlView) -> Vec<Group> {
    let mut v = vec![Group::bare(vec![
        ToolBtn::view(Msg::TbViewPreview, Cmd::ToggleJsonl, jsonl.enabled),
        ToolBtn::view(Msg::TbViewText, Cmd::ToggleJsonl, !jsonl.enabled),
    ])];
    if jsonl.enabled {
        v.push(Group::new(
            Msg::TbGroupOps,
            vec![ToolBtn::new(
                Msg::TbJsonCollapseAll,
                Cmd::JsonlCollapseAll,
                false,
            )],
        ));
    }
    v
}

/// The status summary on the band's right (CSV rows×cols / JSON node count / JSONL expanded
/// lines).
///
/// When several formats' toolbars coexist (manually enabled), the first of CSV → JSON → JSONL
/// wins; the Markdown toolbar has no status text. **Pure reads**: no signal may be written.
/// Returns a key ([`Message`]), not a rendered string — the text is fetched per current language
/// at draw time.
fn status_text(st: &EditorState) -> Option<i18n::Message> {
    let i = st.active;
    if st.tabs.get(i).is_some_and(|t| t.csv.enabled) {
        // A cold cache returns None: parsing is written by the CSV view's build path, and the
        // text shows here once warm
        let (rows, _, _, truncated) = st.csv_rows_cached()?;
        let header = st.tabs[i].csv.header;
        let data = rows.len().saturating_sub(header as usize);
        let cols = rows.first().map(Vec::len).unwrap_or(0);
        return Some(if truncated {
            Fmt::TbCsvSizeTruncated { rows: data, cols }.into()
        } else {
            Fmt::TbCsvSize { rows: data, cols }.into()
        });
    }
    if st.tabs.get(i).is_some_and(|t| t.spec.name == "JSON") {
        let tab = &st.tabs[i];
        let too_large = tab.doc.total_bytes() as usize > crate::ui::state::json::MAX_DOC_BYTES;
        let enabled = tab.json.enabled;
        let nodes = tab.json.node_count();
        let err = tab.json.error().map(|e| e.to_string());
        return Some(if !enabled && too_large {
            Msg::TbStatusJsonTooLarge.into()
        } else {
            match (nodes, &err) {
                (_, Some(e)) => Fmt::TbStatusJsonError { err: e.clone() }.into(),
                (Some(n), None) => Fmt::TbStatusJsonNodes { n }.into(),
                (None, None) => Msg::TbViewText.into(),
            }
        });
    }
    if st
        .tabs
        .get(i)
        .is_some_and(|t| t.spec.name == "JSON Lines" || t.jsonl.enabled)
    {
        let n = st.tabs[i].jsonl.open.len();
        return Some(if n == 0 {
            Msg::TbStatusJsonlHint.into()
        } else {
            Fmt::StatusJsonlExpanded { n }.into()
        });
    }
    None
}
// ===========================================================================
// Tests: the semantic and layout layers are pure functions independent of dioxus, so unit tests
// cover them.
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::commands::testutil::st_of;

    /// A fixed language for tests: what is asserted are keys (wording may change), not the
    /// runtime language.
    const ZH: Locale = Locale::ZhCn;

    #[test]
    fn btn_width_scales_with_label_width() {
        // A halfwidth label is narrower than a fullwidth one; padding sits on both sides.
        let comma = btn_width(",");
        let wide = btn_width("首行标题");
        assert!(
            comma < wide,
            "the fullwidth label should be wider: {comma} vs {wide}"
        );
        // A very short label is never narrower than the button height (the square floor).
        assert!(comma >= BTN_H - 0.001, "{comma}");
        // 3 halfwidth chars ≈ 3*NARROW + 2*PAD.
        let approx = 3.0 * NARROW + 2.0 * BTN_PAD_X;
        let three = btn_width("H1x");
        assert!((three - approx).abs() < 1.5, "{three} vs {approx}");
    }

    #[test]
    fn layout_names_groups_and_separates_them() {
        let groups = vec![
            Group::new(
                Msg::MenuView,
                vec![
                    ToolBtn::new("a", Cmd::Undo, false),
                    ToolBtn::new("b", Cmd::Undo, false),
                ],
            ),
            Group::new(
                Msg::TbGroupFormat,
                vec![ToolBtn::new("c", Cmd::Undo, false)],
            ),
        ];
        let placed = layout(&groups, ZH);
        // group 1 = caption + 2 buttons; a separator; group 2 = caption + 1 button → 6 items
        assert_eq!(placed.len(), 6);
        // The first element is the group caption against the left padding (the caption = group
        // name + colon, assembled on the fly).
        assert!(placed[0].1.label_only && placed[0].1.label == "视图:");
        assert!((placed[0].0 - PAD_X).abs() < 1e-6);
        // In-group spacing = the previous width + GAP.
        assert!((placed[1].0 - (placed[0].0 + placed[0].1.w + GAP)).abs() < 1e-6);
        // The separator lands after group 1 as a 1px non-interactive line.
        let sep = &placed[3].1;
        assert!(sep.sep && sep.label_only && sep.w == SEP_W);
        // After the separator comes group 2's caption.
        assert!(placed[4].1.label_only && placed[4].1.label == "格式:");
    }

    #[test]
    fn csv_items_reflect_state() {
        let mut st = st_of("a,b\n1,2\n");
        st.tabs[0].csv.enabled = true;
        st.tabs[0].csv.table = true;
        st.tabs[0].csv.delim = b';';
        st.tabs[0].csv.header = true;
        let groups = csv_items(&st.tabs[0].csv);
        assert_eq!(
            groups.len(),
            3,
            "table view should have three groups: View+Parse+Edit"
        );
        assert_eq!(
            groups[0].name, None,
            "the view group has no leading caption"
        );
        // Table mode on → "Table view" checked, "Text view" unchecked, both are view keys.
        let view = &groups[0].items;
        assert!(view[0].on && view[0].label == Msg::TbViewTable && view[0].view_btn);
        assert!(!view[1].on && view[1].label == Msg::TbViewText);
        // Parse group: the delimiter has a "Delimiter:" caption prefix and its selected state
        // reflects the current delim.
        let parse = &groups[1].items;
        assert!(parse[1].label_only && parse[1].label.text(ZH).contains("分隔符"));
        assert!(parse[4].on && parse[4].label == ";");
        // header row on.
        assert!(parse[0].on);
        // Sorting lives on the header: the toolbar has no sort button anymore.
        assert!(!groups[1]
            .items
            .iter()
            .any(|b| b.label.text(ZH).contains("排序")));

        // Text view: the whole parse group is not rendered (unsupported features do not show).
        let mut st2 = st_of("a,b\n1,2\n");
        st2.tabs[0].csv.enabled = true;
        st2.tabs[0].csv.table = false;
        let groups2 = csv_items(&st2.tabs[0].csv);
        assert_eq!(
            groups2.len(),
            1,
            "text view should have only the View group"
        );
    }

    /// The GUI suite clicks these buttons **by geometry**
    /// (`tests_py/test_regressions.py::_toolbar_x`): accesskit reports a 0x0 rect for
    /// every self-drawn control, so a derived coordinate plus the status text the
    /// command writes is the only way to hit one. Renaming a label, reordering the
    /// groups or changing the spacing moves every later button — this test then
    /// fails, and `CSV_GROUPS` over there has to be updated with these numbers.
    #[test]
    fn csv_toolbar_button_positions() {
        let mut st = st_of("a,b\n1,2\n");
        st.tabs[0].csv.table = true;
        let placed = layout(&csv_items(&st.tabs[0].csv), ZH);
        // Left edges of the clickable text buttons (`label_only` covers the group
        // caption and the "分隔符:" placeholder, `sep` the group dividers).
        let got: Vec<(String, f32)> = placed
            .iter()
            .filter(|(_, b)| !b.sep && !b.label_only && b.icon.is_none())
            .map(|(x, b)| (b.label.text(ZH).into_owned(), *x))
            .collect();
        let want: [(&str, f32); 13] = [
            ("表格视图", 8.0),
            ("文本视图", 74.0),
            ("首行标题", 197.96),
            (",", 324.92),
            ("Tab", 350.92),
            (";", 389.8),
            ("|", 415.8),
            ("行上+", 499.76),
            ("行下+", 548.72),
            ("删行", 597.68),
            ("列左+", 639.68),
            ("列右+", 688.64),
            ("删列", 737.6),
        ];
        assert_eq!(got.len(), want.len(), "button flow changed: {got:?}");
        for (i, ((label, x), (want_label, want_x))) in got.iter().zip(want).enumerate() {
            assert_eq!(label.as_str(), want_label, "button #{i} changed: {got:?}");
            assert!(
                (*x - want_x).abs() < 0.05,
                "button {label:?} moved from x={want_x} to x={x}: update CSV_GROUPS in \
                 tests_py/test_regressions.py (its _toolbar_x mirrors this layout)"
            );
        }
    }

    #[test]
    fn json_items_show_tree_controls_only_when_enabled() {
        let mut st = st_of("{\"a\":1}");
        // Text view: View + Format (beautify).
        let off = json_items(&st.tabs[0].json, false);
        assert_eq!(off.len(), 2);
        assert_eq!(off[0].name, None, "no leading caption");
        assert_eq!(off[1].name, Some(Msg::TbGroupFormat));
        assert!(!off[0].items[0].on);
        assert!(off[1].items.iter().any(|b| b.label == Msg::TbJsonBeautify));
        // Tree view on: View + Browse (expand/collapse all), **no** "Beautify" — in tree view
        // it would rewrite the document and invalidate the whole tree, and collapse/expand is
        // already built in.
        st.tabs[0].json.enabled = true;
        let on = json_items(&st.tabs[0].json, false);
        assert_eq!(on.len(), 2);
        assert!(on[0].items[0].on);
        assert!(on[1].items.iter().any(|b| b.label == Msg::TbJsonExpandAll));
        assert!(on[1]
            .items
            .iter()
            .any(|b| b.label == Msg::TbJsonCollapseAll));
        assert!(!on[1].items.iter().any(|b| b.label == Msg::TbJsonBeautify));

        // Oversized document (text mode): preview view is greyed and unclickable instead of
        // disappearing.
        let big_st = st_of("{\"a\":1}");
        let big = json_items(&big_st.tabs[0].json, true);
        let pv = &big[0].items[0];
        assert!(pv.disabled && pv.label == Msg::TbViewPreview);
        assert!(!pv.on);
        // Already in tree view (an oversized document cannot get there, but for defensive
        // consistency): not greyed.
        let in_tree = json_items(&st.tabs[0].json, true);
        assert!(!in_tree[0].items[0].disabled);
    }

    #[test]
    fn jsonl_items_toggle_reflects_enabled() {
        let mut st = st_of("{\"a\":1}\n");
        st.tabs[0].jsonl.enabled = true;
        let items = jsonl_items(&st.tabs[0].jsonl);
        assert_eq!(items.len(), 2, "preview on: View + Operations");
        assert_eq!(items[0].items[0].label, Msg::TbViewPreview);
        assert!(items[0].items[0].on);
        assert!(items[1]
            .items
            .iter()
            .any(|b| b.label == Msg::TbJsonCollapseAll));
        st.tabs[0].jsonl.enabled = false;
        let off = jsonl_items(&st.tabs[0].jsonl);
        assert_eq!(off.len(), 1, "text view has only the view group");
        assert!(!off[0].items[0].on);
    }

    #[test]
    fn md_toolbar_view_group_is_two_buttons_in_both_modes() {
        // Text view: the view group (document/text, two buttons now that the
        // preview view is gone) + the icon strip — the same strip as the
        // document view, with the text-mode commands.
        let groups = md_groups(MdToolbarMode::Text, None, false, false);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].name, None, "no leading caption");
        let views = &groups[0].items;
        assert_eq!(views.len(), 2, "preview view removed: two buttons left");
        assert_eq!(views[0].label, Msg::TbViewWys);
        assert!(!views[0].on && views[0].cmd == Cmd::MdWys);
        assert_eq!(views[1].label, Msg::TbViewText);
        assert!(views[1].on && views[1].cmd == Cmd::MdText);

        // Text view commands: bold edits the source, undo/redo belong to the
        // piece table, and the four wys-structure buttons are greyed out.
        let it = &groups[1].items;
        assert!(it
            .iter()
            .any(|b| b.icon == Some("bold") && b.cmd == Cmd::MdEdit(MdAct::Bold)));
        assert!(it
            .iter()
            .any(|b| b.icon == Some("undo") && b.cmd == Cmd::Undo));
        for icon in ["outdent", "indent", "before", "after"] {
            assert!(
                it.iter()
                    .find(|b| b.icon == Some(icon))
                    .is_some_and(|b| b.disabled),
                "{icon} must be greyed out in text view"
            );
        }
        // No focused-cell table group in text view (no cell focus exists).
        assert!(groups.len() == 2);

        // Document view: same view group, wys commands, kernel on-states.
        let doc = md_groups(
            MdToolbarMode::Document,
            Some(&mdws::WysFocus::default()),
            false,
            false,
        );
        assert_eq!(doc[0].items.len(), 2);
        assert!(doc[0].items[0].on && doc[0].items[0].cmd == Cmd::MdWys);
        assert!(!doc[0].items[1].on && doc[0].items[1].cmd == Cmd::MdText);
        assert!(doc[1].items.iter().any(|b| b.icon == Some("bold")
            && b.cmd == Cmd::WysTool(WysTool::Wrap(wysiwyg::WrapKind::Bold))));
    }

    /// Narrow-window yielding keeps the two **view toggles** on the strip: the
    /// contextual/secondary entries around them yield instead (table caption →
    /// redundant edit dropdown → more → table group), and even a
    /// 300px budget never replaces them with the old single "eye" menu.
    ///
    /// Regression: the strip used to collapse 文档视图 / 文本视图 into one eye
    /// icon as soon as a focused table cell pushed the strip past the budget —
    /// which used to happen at the *default* window size, so the view switch was
    /// gone exactly while editing a table (and the eye also hid the active view).
    /// Dropping the fullscreen button made the strip ~29px narrower, so it now
    /// fits the default width; the squeeze is exercised with budgets measured
    /// off the real strip instead of hardcoding the window size.
    #[test]
    fn narrow_wys_toolbar_keeps_view_toggles_and_sheds_around_them() {
        let f = mdws::WysFocus {
            head: None,
            ordered: Some(false),
            task: false,
            quote: false,
            code: false,
            td: true,
            td_align: Some(1),
            inline: mdws::InlineMarks::default(),
        };
        // md_groups appends the real 7-button table group when a cell is focused.
        let build = || md_groups(MdToolbarMode::Document, Some(&f), true, true);
        let total = toolbar_total_w(&build(), ZH);
        // The default window is 1280 logical px, but its *client* area is 1265
        // CSS px (the frameless window still has an invisible frame), so the
        // strip gets 1265 - 160 = 1105. The doc+table strip needs ~1085 and
        // fits; the collapse this test guards against is now a narrow-window
        // story, not a default-size one.
        assert!(
            total <= 1105.0,
            "the doc+table strip should fit the default budget, otherwise the \
             toolbar sheds entries at the default window size: {total}"
        );

        // Width once the "Table:" caption yields — the strip's first shedding
        // step. A budget between the two widths forces exactly that one step.
        let no_caption = {
            let mut g = build();
            g.last_mut().expect("the table group is last").name = None;
            toolbar_total_w(&g, ZH)
        };
        let mid = (total + no_caption) / 2.0;
        let fitted = fit_wys_groups(build(), mid, ZH);
        assert!(
            toolbar_total_w(&fitted, ZH) <= mid,
            "the fitted strip must respect the budget: {}",
            toolbar_total_w(&fitted, ZH)
        );
        // The toggles are intact: both labels, still first, still showing state
        assert_eq!(fitted[0].items.len(), 2, "both view toggles stay");
        assert_eq!(fitted[0].items[0].label, Msg::TbViewWys);
        assert!(fitted[0].items[0].on);
        assert_eq!(fitted[0].items[1].label, Msg::TbViewText);
        assert!(!fitted[0].items[1].on);
        assert!(
            !fitted
                .iter()
                .any(|g| g.items.iter().any(|b| b.icon == Some("preview"))),
            "no eye-menu replacement may appear"
        );
        // At the default budget only the table caption yields: the table's own
        // buttons stay, and so does the (now redundant) edit dropdown.
        assert!(!fitted.iter().any(|g| g.name == Some(Msg::TbGroupTable)));
        assert!(fitted.iter().any(|g| g
            .items
            .iter()
            .any(|b| b.cmd == Cmd::WysTool(WysTool::Table(wysiwyg::TableCmd::InsertRowAfter)))));

        // A little narrower and the redundant edit dropdown goes too — its
        // items *are* the two toggles that remain on screen. 10px under the
        // caption-less width is enough: shedding the 24px icon (+gap) covers it.
        let tighter = fit_wys_groups(build(), no_caption - 10.0, ZH);
        assert!(
            !tighter
                .iter()
                .any(|g| g.items.iter().any(|b| b.icon == Some("edit"))),
            "the duplicate view dropdown yields before any unique control"
        );
        assert_eq!(tighter[0].items.len(), 2);

        // Tiny window: still never the view toggles (the tail may clip, but the
        // head of the strip is what must remain reachable).
        let tiny = fit_wys_groups(build(), 300.0, ZH);
        assert_eq!(tiny[0].items.len(), 2);
        assert!(tiny[0].items.iter().all(|b| b.view_btn));
        assert!(
            !tiny.iter().any(|g| g.name == Some(Msg::TbGroupTable)),
            "the table group is the last thing to go"
        );
    }

    /// WYSIWYG toolbar: the head group holds the three view-mode buttons (user feedback: the view
    /// switch was unfindable in wys), followed by an empty group name (no caption), icon buttons,
    /// fixed-order key anchors, and dropdown panel buttons.
    #[test]
    fn md_toolbar_document_view_follows_fixed_order() {
        let f = mdws::WysFocus {
            head: None,
            ordered: Some(false),
            task: false,
            quote: false,
            code: false,
            td: false,
            td_align: None,
            inline: mdws::InlineMarks::default(),
        };
        let groups = md_groups(MdToolbarMode::Document, Some(&f), true, true);
        assert_eq!(groups.len(), 2);
        // Group 0: the view group (two view modes directly visible, same order
        // and labels as text mode; in document view only "Document view" is
        // on) — no leading caption.
        assert_eq!(groups[0].name, None);
        let views = &groups[0].items;
        assert_eq!(views.len(), 2);
        assert_eq!(views[0].label, Msg::TbViewWys);
        assert!(views[0].on && views[0].cmd == Cmd::MdWys);
        assert_eq!(views[1].label, Msg::TbViewText);
        assert!(!views[1].on && views[1].cmd == Cmd::MdText);
        // Group 1: the icon strip
        assert!(groups[1].name.is_none(), "the toolbar has no group caption");
        let it = &groups[1].items;
        // order anchors
        let names: Vec<&str> = it.iter().filter_map(|b| b.icon).collect();
        for want in [
            "emoji",
            "headings",
            "bold",
            "italic",
            "strike",
            "link",
            "image",
            "list",
            "ordered-list",
            "check",
            "outdent",
            "indent",
            "quote",
            "line",
            "code",
            "inline-code",
            "before",
            "after",
            "table",
            "undo",
            "redo",
            "edit",
            "more",
        ] {
            assert!(names.contains(&want), "missing icon {want}: {names:?}");
        }
        // emoji first, headings second (fixed order)
        assert_eq!(names[0], "emoji");
        assert_eq!(names[1], "headings");
        // dropdown panel buttons
        assert_eq!(it[0].menu, Some(MenuKind::Emoji));
        assert_eq!(it[1].menu, Some(MenuKind::Headings));
        // command anchors: bold → wys Wrap(Bold); undo → the shared Cmd::Undo
        // (the wys intercept reroutes it to the wys model in document view)
        assert!(it.iter().any(|b| b.icon == Some("bold")
            && b.cmd == Cmd::WysTool(WysTool::Wrap(wysiwyg::WrapKind::Bold))));
        assert!(it
            .iter()
            .any(|b| b.icon == Some("undo") && b.cmd == Cmd::Undo));
        // icon button width = ICON_W; none has a text label
        assert!(it.iter().all(|b| b.icon.is_some() && b.w == ICON_W));
        assert!(it
            .iter()
            .all(|b| matches!(&b.label, ToolLabel::Text(s) if s.is_empty())));
        // focus on a bullet list item → list lights up
        assert!(it
            .iter()
            .find(|b| b.icon == Some("list"))
            .is_some_and(|b| b.on));
        // inline format on states wire to WysFocus.inline
        let f_bold = mdws::WysFocus {
            inline: mdws::InlineMarks {
                bold: true,
                italic: true,
                ..Default::default()
            },
            ..Default::default()
        };
        let gb = md_groups(MdToolbarMode::Document, Some(&f_bold), true, true);
        assert!(gb[1]
            .items
            .iter()
            .find(|b| b.icon == Some("bold"))
            .is_some_and(|b| b.on));
        assert!(gb[1]
            .items
            .iter()
            .find(|b| b.icon == Some("italic"))
            .is_some_and(|b| b.on));
        assert!(gb[1]
            .items
            .iter()
            .find(|b| b.icon == Some("strike"))
            .is_none_or(|b| !b.on));
        // grey out undo/redo (redo wired too)
        let g2 = md_groups(MdToolbarMode::Document, Some(&f), false, false);
        assert!(g2[1]
            .items
            .iter()
            .find(|b| b.icon == Some("undo"))
            .is_some_and(|b| b.disabled));
        assert!(g2[1]
            .items
            .iter()
            .find(|b| b.icon == Some("redo"))
            .is_some_and(|b| b.disabled));
        // table-operations group: appears only with focus inside a Td
        let f_td = mdws::WysFocus {
            td: true,
            ..Default::default()
        };
        let gt = md_groups(MdToolbarMode::Document, Some(&f_td), true, true);
        assert_eq!(gt.len(), 3, "focus in a Td appends the Table group");
        assert_eq!(gt[2].name, Some(Msg::TbGroupTable));
        let labels: Vec<String> = gt[2]
            .items
            .iter()
            .map(|b| b.label.text(ZH).into_owned())
            .collect();
        assert_eq!(labels, ["行+", "行-", "列+", "列-", "左", "中", "右"]);
        assert!(gt[2]
            .items
            .iter()
            .any(|b| b.cmd == Cmd::WysTool(WysTool::Table(wysiwyg::TableCmd::DeleteRow))));
        // align keys emit the kernel's `TableCmd::Align` (onto the undo stack) instead of the
        // host editing directly
        assert!(gt[2]
            .items
            .iter()
            .any(|b| b.cmd == Cmd::WysTool(WysTool::Table(wysiwyg::TableCmd::Align(2)))));
        assert!(
            gt[2].items.iter().all(|b| !b.on),
            "default alignment has no on key"
        );
        // the align on state: the focused column's alignment lights the matching key
        let f_center = mdws::WysFocus {
            td: true,
            td_align: Some(2),
            ..Default::default()
        };
        let ga = md_groups(MdToolbarMode::Document, Some(&f_center), true, true);
        let on: Vec<String> = ga[2]
            .items
            .iter()
            .filter(|b| b.on)
            .map(|b| b.label.text(ZH).into_owned())
            .collect();
        assert_eq!(
            on,
            ["中"],
            "centered focus column → only the Center key is on"
        );
    }

    /// Text view: every strip button carries the text-mode command (no wys
    /// commands leak through), and the on-states are all off (no kernel).
    #[test]
    fn md_toolbar_text_view_has_no_wys_commands() {
        let f = mdws::WysFocus {
            ordered: Some(false),
            quote: true,
            ..Default::default()
        };
        // A stale kernel focus must not leak on-states into the text view.
        let groups = md_groups(MdToolbarMode::Text, Some(&f), true, true);
        let it = &groups[1].items;
        assert!(it.iter().all(|b| !b.on), "no on-state in text view");
        assert!(
            it.iter().all(|b| !matches!(b.cmd, Cmd::WysTool(_))),
            "no wys kernel command in text view: {:?}",
            it.iter().map(|b| &b.cmd).collect::<Vec<_>>()
        );
        // Every non-menu strip button carries a MdEdit or a host command.
        assert!(it
            .iter()
            .find(|b| b.icon == Some("quote"))
            .is_some_and(|b| b.cmd == Cmd::MdEdit(MdAct::Quote)));
        assert!(it
            .iter()
            .find(|b| b.icon == Some("list"))
            .is_some_and(|b| b.cmd == Cmd::MdEdit(MdAct::Bullet)));
    }

    /// A group with an empty name renders no caption.
    #[test]
    fn layout_skips_empty_group_names() {
        let groups = vec![Group::bare(vec![
            ToolBtn::icon_btn("bold", Cmd::Undo, false),
            ToolBtn::icon_btn("italic", Cmd::Undo, false),
        ])];
        let placed = layout(&groups, ZH);
        assert_eq!(placed.len(), 2, "only two buttons, no group caption");
        assert!(!placed[0].1.label_only);
        assert!((placed[0].0 - PAD_X).abs() < 1e-6);
        assert!((placed[1].0 - (placed[0].0 + ICON_W + GAP)).abs() < 1e-6);
    }

    /// The band's total width = the last button's right edge after layout (the basis for
    /// overflow decisions).
    #[test]
    fn toolbar_total_w_measures_last_button_edge() {
        let groups = vec![Group::new(
            Msg::MenuView,
            vec![
                ToolBtn::view(Msg::TbViewTable, Cmd::CsvToggleMode, true),
                ToolBtn::view(Msg::TbViewText, Cmd::CsvToggleMode, false),
            ],
        )];
        let placed = layout(&groups, ZH);
        let want = placed.last().map_or(0.0, |(x, b)| x + b.w);
        assert!((toolbar_total_w(&groups, ZH) - want).abs() < 1e-6);
        // an empty band → 0
        assert!(toolbar_total_w(&[], ZH).abs() < 1e-6);
    }

    #[test]
    fn toolbar_picks_csv_status_when_csv_enabled() {
        let mut st = st_of("a,b,c\n1,2,3\n4,5,6\n");
        st.tabs[0].csv.enabled = true;
        st.tabs[0].csv.table = true;
        st.tabs[0].csv.header = true;
        // status_text is pure reads and only trusts a warm cache — the CSV view's build path
        // parses first (warmed up manually here); a cold cache yields no text
        let _ = st.csv_rows_ensure();
        let s = status_text(&st).unwrap();
        // 3 rows with the first as header → 2 data rows × 3 columns.
        let want: i18n::Message = Fmt::TbCsvSize { rows: 2, cols: 3 }.into();
        assert_eq!(s, want);
    }

    #[test]
    fn toolbar_jsonl_status_counts_expanded_lines() {
        let mut st = st_of("{\"a\":1}\n{\"b\":2}\n");
        // A document with the JSONL preview manually enabled (not a JSON Lines spec) should be
        // covered by the status text too.
        st.tabs[0].jsonl.enabled = true;
        let want: i18n::Message = Msg::TbStatusJsonlHint.into();
        assert_eq!(status_text(&st).unwrap(), want);
        // Expanding line 0 updates the summary (through the real command path, verifying the
        // text branch).
        st.jsonl_expand_line(0);
        let want: i18n::Message = Fmt::StatusJsonlExpanded { n: 1 }.into();
        assert_eq!(status_text(&st).unwrap(), want);
    }

    /// Audit: every **interactive** toolbar button either binds a real command (≠ the
    /// placeholder `Cmd::About`) or is a dropdown entry (`menu.is_some()` — handled by its panel).
    /// Pure captions / separators are non-interactive and skipped. Icon buttons must also have an
    /// icon name with a matching symbol in `mdicons` (otherwise the button renders blank and there
    /// is nothing to click).
    fn assert_wired(groups: &[Group], ctx: &str) {
        for g in groups {
            for b in &g.items {
                if b.label_only || b.sep {
                    continue;
                }
                if b.menu.is_some() {
                    continue;
                }
                assert_ne!(
                    b.cmd,
                    Cmd::About,
                    "{ctx}: interactive button {:?} binds the placeholder command",
                    b.label
                );
                if let Some(name) = b.icon {
                    assert!(
                        !mdicons::svg(name, "#fff").is_empty(),
                        "{ctx}: icon button {name:?} has no matching symbol"
                    );
                }
            }
        }
    }

    #[test]
    fn every_toolbar_button_is_wired() {
        // WYS view (including the table-operations group when focus is in a table)
        let f = mdws::WysFocus {
            td: true,
            ..Default::default()
        };
        assert_wired(
            &md_groups(MdToolbarMode::Document, Some(&f), true, true),
            "document view",
        );
        // text view
        assert_wired(&md_groups(MdToolbarMode::Text, None, true, true), "md text");
        // CSV (table / text views)
        let mut st = st_of("a,b\n1,2\n");
        st.tabs[0].csv.enabled = true;
        st.tabs[0].csv.table = true;
        assert_wired(&csv_items(&st.tabs[0].csv), "CSV table");
        st.tabs[0].csv.table = false;
        assert_wired(&csv_items(&st.tabs[0].csv), "CSV text");
        // JSON / JSONL
        assert_wired(&json_items(&st.tabs[0].json, false), "JSON");
        assert_wired(&json_items(&st.tabs[0].json, true), "JSON oversized");
        st.tabs[0].jsonl.enabled = true;
        assert_wired(&jsonl_items(&st.tabs[0].jsonl), "JSONL");
        st.tabs[0].jsonl.enabled = false;
        assert_wired(&jsonl_items(&st.tabs[0].jsonl), "JSONL text");
    }
}
