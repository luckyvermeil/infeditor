//! The app shell's class stylesheet: VS Code-style sidebar / status-bar icons / fold markers.
//!
//! # Why `<style>` + pseudo-classes
//!
//! These controls' tri-states (hover / selected / pressed) used to need one `Press` machine per
//! row plus an inline background — with a tree of a few hundred rows, moving the mouse wrote a
//! few hundred signals and re-rendered the whole tree.
//! blitz's stylo supports `:hover` / `:active` natively (see the `ui_kit::style` module docs),
//! so those states moved into the stylesheet: a hover triggers a single style recalc, no
//! component re-renders a single row, and the row components do not even read signals.
//!
//! Colors come from the runtime theme, so the stylesheet is **generated** per theme; `dark` goes
//! into the `<style>` key — blitz parses the text only when the `<style>` node is created, so a
//! theme switch has to rebuild the node.
//!
//! # The division of labor with ui-kit
//!
//! `ui_kit::stylesheet` takes only the primitives **any** app needs (buttons/checkboxes/inputs);
//! this sheet takes **this app's** chrome (the explorer tree, status-bar toggles, code fold
//! markers). Both hang on the main window's root node and never overlap (class prefixes `k-` /
//! `sb-`, `fold`).

use dioxus_native::prelude::*;

use crate::highlight::Theme;
use crate::ui::motion;

/// Generate the shell's class stylesheet (colored by theme).
pub fn stylesheet(t: &'static Theme) -> String {
    let trans = motion::fast(motion::SURFACE);
    format!(
        r#"

/* ---- native scrollbars (css-scrollbars-1) ------------------------------ */
/* blitz's overlay thumb is a **native capability** (the `scrollbars` feature, see the root
   Cargo.toml): `overflow:auto|scroll` containers come with a draggable thumb, hover/press
   feedback and an automatic idle fade-out — the real scroll containers (sidebar/outline/dialog
   bodies) need no hand-drawn bar of their own. Colors follow the theme, sharing the viewport's
   self-drawn scrollbar's sb_* palette; the viewport and the CSV panel are `overflow:hidden`
   virtualized renders that never grow a native bar and still use their own
   `components::scrollbar`. */
* {{ scrollbar-color: {sb_thumb} {sb_track}; }}

/* ---- tri-state on clickable surfaces (the Press machine retired) ------- */
/* These classes stand in for `motion::Press`: blitz's stylo parses `:hover`/`:active` natively
   (see the `ui_kit::style` module docs), so the tri-states go straight into the stylesheet and
   components no longer keep hover/down Signals — a hover triggers one style recalc instead of
   re-rendering the component. Two iron rules:
   **the rest background must live in the class too** — an inline `background` out-specifies any
   selector and `:hover` dies outright (see editor.rs's historical comment);
   **geometry belongs in the class too** — moving only the colors leaves
   `display:flex`/`height`/`padding` in the component, and one miss degrades into "labels wrap
   and menus bunch up".
   `user-select:none`: blitz's selection walks up only **one** parent level, so every chrome
   surface must declare it itself — declaring it on an outer container cannot save the text
   inside. */
.mi {{
    padding:0 9px; height:100%; display:flex; align-items:center;
    white-space:nowrap; cursor:default; user-select:none;
    background:transparent; color:{menu_fg}; {trans}
}}
.mi:hover {{ background:{btn_hover}; }}
.mi:active {{ background:{btn_active}; }}
.mi.open {{ background:{menu_open}; }}
.mi.open:hover {{ background:{menu_open}; }}
.mi.open:active {{ background:{btn_active}; }}

.tab {{
    display:flex; align-items:center; height:24px;
    padding-left:9px; padding-right:3px; max-width:240px;
    font-size:12px; cursor:default; user-select:none;
    background:{tab_bg}; color:{tab_fg}; {trans}
}}
.tab:hover {{ background:{tab_hover_bg}; color:{menu_fg}; }}
.tab:active {{ background:{tab_press_bg}; color:{menu_fg}; }}
.tab.on {{ background:{tab_active_bg}; color:{menu_fg}; }}
.tab.on:hover {{ background:{tab_hover_bg}; color:{menu_fg}; }}
.tab.on:active {{ background:{tab_press_bg}; color:{menu_fg}; }}

/* the tab strip's × / + (square hit area, side length in tabs.rs's TAB_BTN) */
.tab-btn {{
    width:22px; height:22px; flex:0 0 auto; display:flex; align-items:center;
    justify-content:center; border-radius:3px; cursor:default; user-select:none;
    background:transparent; color:{tab_close_fg}; {trans}
}}
.tab-btn:hover {{ background:{btn_hover}; color:{menu_fg}; }}
.tab-btn:active {{ background:{btn_active}; color:{menu_fg}; }}

/* title-bar window buttons: min/max use the button family's tri-state, close gets its own red */
.wb {{
    height:100%; flex:0 0 auto; display:flex; align-items:center; justify-content:center;
    font-size:15px; cursor:default; overflow:hidden; user-select:none;
    background:{menu_bg}; color:{menu_fg}; {trans}
}}
.wb:hover {{ background:{btn_hover}; }}
.wb:active {{ background:{btn_active}; }}
.wb.close:hover {{ background:{close_hot}; color:#ffffff; }}
.wb.close:active {{ background:{close_down}; color:#ffffff; }}

/* the close ×: hover turns close-red (same semantics in dialogs / find bar / title bar) */
.dlg-x {{
    height:100%; display:flex; align-items:center; justify-content:center;
    cursor:default; user-select:none;
    background:transparent; color:{accent}; {trans}
}}
.dlg-x:hover {{ background:{close_hot}; color:#ffffff; }}
.dlg-x:active {{ background:{close_down}; color:#ffffff; }}
.f-x {{ cursor:default; user-select:none; background:transparent; color:{menu_fg}; {trans} }}
.f-x:hover {{ background:{close_hot}; color:#ffffff; }}
.f-x:active {{ background:{close_down}; color:#ffffff; }}

/* find-bar command buttons: `on` = the bar's primary action, parked on the accent background */
.fbtn {{
    cursor:default; user-select:none; box-sizing:border-box;
    background:{input_bg}; color:{menu_fg}; border:1px solid {menu_border}; {trans}
}}
.fbtn:hover {{ background:{btn_hover}; border-color:{btn_hover}; }}
.fbtn:active {{ background:{btn_active}; border-color:{btn_active}; }}
.fbtn.on {{ background:{accent}; color:#ffffff; border-color:{accent}; }}
.fbtn.on:hover {{ background:{btn_hover}; color:{menu_fg}; border-color:{btn_hover}; }}
.fbtn.on:active {{ background:{btn_active}; color:{menu_fg}; border-color:{btn_active}; }}

/* toolbar buttons: `on` stays accent (it does not follow the pointer), `dis` is greyed out with no feedback */
.tbtn {{
    cursor:default; user-select:none; box-sizing:border-box;
    background:transparent; color:{menu_fg}; border:1px solid {menu_border};
    opacity:1; {trans}
}}
.tbtn:hover {{ background:{btn_hover}; border-color:{btn_hover}; }}
.tbtn:active {{ background:{btn_active}; border-color:{btn_active}; }}
.tbtn.on {{ background:{accent}; color:#ffffff; border-color:{accent}; }}
.tbtn.on:hover, .tbtn.on:active {{ background:{accent}; color:#ffffff; border-color:{accent}; }}
.tbtn.dis {{ opacity:0.4; }}
.tbtn.dis:hover, .tbtn.dis:active {{ background:transparent; border-color:{menu_border}; }}

/* dropdown panel rows: `cur` = the keyboard cursor's row, beating hover (the mouse must not override the keyboard) */
.mrow {{ background:{menu_bg}; color:{menu_fg}; cursor:default; user-select:none; {trans} }}
.mrow:hover {{ background:{hover_bg}; color:{hover_fg}; }}
.mrow:active {{ background:{active_bg}; color:{active_fg}; }}
.mrow.cur {{ background:{active_bg}; color:{active_fg}; }}
.mrow.cur:hover {{ background:{active_bg}; color:{active_fg}; }}

/* find-result rows: `on` = the row of the current hit */
.rrow {{ background:transparent; color:{menu_fg}; cursor:default; user-select:none; {trans} }}
.rrow:hover {{ background:{menu_hover}; color:#ffffff; }}
.rrow:active {{ background:{active_bg}; color:#ffffff; }}
.rrow.on {{ background:{menu_hover}; color:{menu_fg}; }}
.rrow.on:hover {{ background:{menu_hover}; color:#ffffff; }}
.rrow.on:active {{ background:{active_bg}; color:#ffffff; }}

/* rows in the toolbar's dropdown panels */
.prow {{ background:transparent; color:{menu_fg}; cursor:default; user-select:none; {trans} }}
.prow:hover, .prow:active {{ background:{menu_hover}; }}
.prow.on {{ background:{menu_open}; }}
.prow.on:hover, .prow.on:active {{ background:{menu_open}; }}

/* hover tooltip (components::tooltip): drawn on the app root, outside every overflow.
   pointer-events:none — it only displays and must never block the pointer. */
.tip {{
    position:absolute; z-index:4000; pointer-events:none;
    padding:3px 8px; white-space:nowrap; font-size:11px; line-height:16px;
    border-radius:3px; max-width:320px; overflow:hidden; text-overflow:ellipsis;
    background:{menu_bg}; color:{menu_fg}; border:1px solid {menu_border};
    box-shadow:0 2px 10px {shadow};
}}

/* ---- panel head -------------------------------------------------------- */
.ws-title {{
    flex:1 1 auto; overflow:hidden; white-space:nowrap;
    font-size:11px; letter-spacing:0.6px; color:{menu_fg};
}}
.ws-btn {{
    height:22px; min-width:22px; padding:0 6px; flex:0 0 auto;
    display:flex; align-items:center; justify-content:center;
    border-radius:3px; font-size:11px; color:{tab_fg};
    cursor:default; touch-action:none; background:transparent; {trans}
}}
.ws-btn:hover {{ background:{btn_hover}; color:{menu_fg}; }}
.ws-btn:active {{ background:{btn_active}; }}

/* ---- explorer tree ---------------------------------------------------- */
/* 22px rows + hover/selected states; the selected state needs two selectors: `.ws-row:hover`
   out-specifies `.ws-row.on`, so without `.ws-row.on:hover` hovering a selected row would "hover
   it back" to a normal row.
   `touch-action:pan-y` (**not** none): the sidebar is a real scroll container (overflow-y:auto on
   `.ws-rows`) but rows cover the whole list — with none on a row, `touch_action_pan_axes` vetoes
   panning at the target node, which disables the parent's native scrolling, showing up as "it
   will not scroll, and a big swipe is instead taken as a click that opens a file".
   pan-y = vertical native panning stays allowed; a vertical displacement within the slop is
   still a tap, and past the slop with no content scrolled (list already at the top/bottom) does
   not swallow the Click either (blitz patch thirteen). */
.ws-row {{
    position:relative; display:flex; align-items:center; height:22px;
    padding-right:8px; white-space:nowrap; font-size:13px; color:{tab_fg};
    cursor:default; user-select:none; touch-action:pan-y; {trans}
}}
.ws-row:hover {{ background:{list_hover}; color:{menu_fg}; }}
.ws-row.on, .ws-row.on:hover {{ background:{list_sel}; color:{list_sel_fg}; }}
/* indent guides: one 1px vertical line per ancestor level; adjacent sibling rows join into one
   continuous column. */
.ws-guide {{ position:absolute; top:0; bottom:0; width:1px; background:{guide}; pointer-events:none; }}
/* the twisty slot. The glyphs are ▸/▾ (U+25B8/25BE) from Consolas — YaHei UI lacks those code
   points and rows inheriting the UI font would draw tofu (verified in practice), so the monospace
   font is pinned here. */
.ws-caret {{
    width:16px; flex:0 0 auto; text-align:center; font-size:9px; color:{tab_fg};
    font-family:Consolas,'Cascadia Mono',monospace;
}}
.ws-name {{ overflow:hidden; }}

/* ---- sidebar toggles at the status bar's lower left -------------------- */
.sb-btn {{
    width:22px; height:18px; flex:0 0 auto; display:flex;
    align-items:center; justify-content:center; border-radius:2px;
    cursor:default; touch-action:none; background:transparent; {trans}
}}
.sb-btn:hover {{ background:{btn_hover}; color:{menu_fg}; }}
.sb-btn:active {{ background:{btn_active}; color:{menu_fg}; }}
/* the current panel's toggle: accent (the same step VS Code uses for "enabled items") */
.sb-btn.on {{ color:{accent}; }}
.sb-btn.on:hover {{ background:{btn_hover}; color:{accent}; }}/* all icons are drawn with boxes; color follows the button's color (currentColor):
   explorer = a folder (tab + body as one silhouette), outline = three shrinking bars. */
.sb-folder {{ position:relative; display:block; width:14px; height:10px; }}
/* The tab is **open at the bottom** and the lid finishes the body's top edge to its
   right, so the outline stays one continuous shape: the old tab was a closed box whose
   bottom border ran *inside* the body, reading as two stacked boxes. `border-box` keeps
   the strokes inside the 14×10 box (content-box made both parts ~3px larger and they
   overlapped even more). */
.sb-folder-tab {{
    position:absolute; left:0; top:0; width:6.5px; height:3.4px; box-sizing:border-box;
    border:1.1px solid currentColor; border-bottom:0;
    border-radius:1.2px 1.2px 0 0;
}}
.sb-folder-lid {{
    position:absolute; left:6.5px; top:3.4px; width:7.5px; height:1.1px;
    background:currentColor;
}}
.sb-folder-body {{
    position:absolute; left:0; top:3.4px; width:14px; height:6.6px; box-sizing:border-box;
    border:1.1px solid currentColor; border-top:0;
    border-radius:0 0 1.2px 1.2px;
}}
.sb-bars {{ position:relative; display:block; width:14px; height:11px; }}
.sb-bar {{ position:absolute; left:0; height:1.5px; border-radius:1px; background:currentColor; }}
.sb-bar1 {{ top:1px; width:14px; }}
.sb-bar2 {{ top:5px; width:10px; }}
.sb-bar3 {{ top:9px; width:6px; }}

/* the minimap's viewport indicator: changes color when it slides across the strip (a child
   follows the parent's hover) */
.mm-strip:hover .mm-slider {{ background:{mm_slider_hover}; }}
.mm-slider {{ background:{mm_slider}; {trans} }}

/* the status bar's VIM badge: permanently accent, one notch darker on hover/open */
.vbadge {{ background:{accent}; color:#ffffff; cursor:default; user-select:none; {trans} }}
.vbadge:hover, .vbadge.open {{ background:{active_bg}; }}

/* ---- fold markers (JSON tree / JSONL expanded blocks) ------------------ */
/* The overlay is `pointer-events:none`; the marker itself restores auto to get `:hover`
   (blitz hit-testing only skips the layer that declared none — child nodes are still hit-tested). */
/* The box height comes from the inline style (= row height, scaling with the font): without a
   pinned height an absolutely positioned `<span>` lets its inline box collapse in the block
   direction — hit-testing requires the point to land inside the box, so hover would never
   fire. */
.fold {{
    width:16px; text-align:center; font-size:10px;
    color:{tab_fg}; font-family:Consolas,'Cascadia Mono',monospace;
    cursor:default; pointer-events:auto; border-radius:2px; {trans}
}}
.fold:hover {{ background:{list_hover}; color:{menu_fg}; }}

/* hover for virtual rows of the JSON tree (VS Code tree row highlight). The row's inline style
   carries only indent and font; the background lives here — inline would out-specify the sheet. */
.jrow:hover {{ background:{list_hover}; }}
"#,
        list_hover = t.list_hover,
        list_sel = t.list_sel,
        list_sel_fg = t.list_sel_fg,
        guide = t.indent_guide,
        accent = t.accent,
        menu_fg = t.menu_fg,
        menu_bg = t.menu_bg,
        menu_hover = t.menu_hover,
        menu_open = t.menu_open,
        menu_border = t.menu_border,
        hover_bg = t.hover_bg,
        hover_fg = t.hover_fg,
        active_bg = t.active_bg,
        active_fg = t.active_fg,
        input_bg = t.input_bg,
        tab_bg = t.tab_bg,
        tab_fg = t.tab_fg,
        tab_active_bg = t.tab_active_bg,
        tab_hover_bg = t.tab_hover_bg,
        tab_press_bg = t.tab_press_bg,
        tab_close_fg = t.tab_close_fg,
        btn_hover = t.btn_hover,
        btn_active = t.btn_active,
        close_hot = t.close_hot,
        close_down = t.close_down,
        sb_thumb = t.sb_thumb,
        sb_track = t.sb_track,
        shadow = t.shadow,
        mm_slider = t.mm_slider,
        mm_slider_hover = t.mm_slider_hover,
        trans = trans,
    )
}

/// Inject the shell stylesheet (mounted once on the main window's root, like `KitStyle`).
#[component]
pub fn AppStyle(t: &'static Theme) -> Element {
    rsx! {
        style { key: "app-{t.dark}", "{stylesheet(t)}" }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::highlight::{DARK, LIGHT};

    /// The stylesheet must carry the theme colors: the two themes generate different strings,
    /// and both contain the selector skeleton.
    #[test]
    fn stylesheet_is_theme_specific_and_complete() {
        let dark = stylesheet(&DARK);
        let light = stylesheet(&LIGHT);
        assert_ne!(dark, light, "the two themes must color differently");
        for css in [&dark, &light] {
            for sel in [
                ".ws-row {",
                ".ws-row.on",
                ".sb-btn {",
                ".sb-btn.on",
                ".sb-folder {",
                ".sb-bars {",
                ".fold {",
                ".jrow:hover",
            ] {
                assert!(css.contains(sel), "missing selector {sel}");
            }
        }
        assert!(
            dark.contains(DARK.list_sel),
            "selected background made it into the stylesheet"
        );
        assert!(
            dark.contains(DARK.accent),
            "toggle highlight color made it into the stylesheet"
        );
        // hovering a selected row must keep the selected state (the specificity trap, see the
        // CSS comment)
        assert!(
            dark.contains(".ws-row.on:hover"),
            "hovering a selected row must not fall back"
        );
    }

    /// Sidebar rows **must** be `pan-y`: they are full-width rows in a scroll container, and
    /// `none` would disable native vertical scrolling altogether ("it will not scroll"), while
    /// `auto` would let horizontal drags into panning too. This is the fix-most-easily-undone
    /// one: "add touch-action:none to every interactive element" brings it straight back.
    #[test]
    fn scrollable_rows_keep_native_panning() {
        let css = stylesheet(&DARK);
        let start = css.find("\n.ws-row {").expect("missing .ws-row rule");
        let end = start + css[start..].find('}').expect("rule never closed");
        let row = &css[start..end];
        assert!(
            row.contains("touch-action:pan-y"),
            "sidebar rows must allow native vertical scrolling: {row}"
        );
        assert!(
            !row.contains("touch-action:none"),
            "sidebar rows must not disable panning: {row}"
        );
    }
}
