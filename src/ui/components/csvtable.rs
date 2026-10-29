//! CSV table view: it **directly replaces** the editor textbox's virtualized table (Excel
//! semantics).
//!
//! Data comes from [`EditorState::csv_rows_ensure`]'s gen-keyed parse cache; rendering only
//! touches the visible row window (the same virtualization idea as the text view). Column widths
//! come from [`auto_col_widths`] (a sample of rows, CJK counted as double width — the same call
//! the scrollbar geometry makes), and user-dragged column/row sizes override them in `CsvView`'s
//! `col_widths` / `row_heights` tables (session memory, never persisted).
//!
//! Interaction mapping (Excel semantics):
//! - click a header → three-state sort cycle (none → ascending → descending), the arrow shown
//!   in the header
//! - double-click a cell → in-place editing (**live write-back**: every keystroke lands on disk;
//!   Enter moves down / Tab moves right / Esc restores the old value; Ctrl+C/V copies the field
//!   text / pastes into the cell)
//! - drag the header's right edge → resize the column; drag the row-number gutter's bottom edge →
//!   resize the row
//! - horizontal and vertical scrollbars; blank rows/columns beyond the data (row numbers/column
//!   letters still shown, selectable)
//!
//! TSV (`\t`-separated) and CSV take exactly the same path — the delimiter is just a parse
//! parameter.

use dioxus_native::prelude::*;
use dioxus_native::winit;
use std::sync::Arc;
use winit::window::Window;

use crate::highlight::Theme;
use crate::ui::design::FONT_MONO;
use crate::ui::state::csv::{auto_col_widths, SelKind};
use crate::ui::state::EditorState;
use crate::ui::Msg;

/// Table metrics live in [`crate::ui::state::csv`] (the command layer measures
/// scrolling without a reverse dependency on this render component) and are
/// re-exported here for the render paths.
pub use crate::ui::state::csv::{CELL_PAD, EMPTY_COL_W, MAX_EMPTY_COLS, ROW_H, SB_W};
/// Height of the column-letter strip (A / B / C …); also the row that hosts the column-resize
/// grips.
const STRIP_H: f32 = 18.0;
/// Width of the row/column resize hot zone (px).
const RESIZE_HOT: f32 = 5.0;
/// Max number of filler blank rows (guards against extreme viewports).
const MAX_EMPTY_ROWS: usize = 200;

use unicode_width::UnicodeWidthChar;

/// The **pixel** width of edit-box text (rendered at 12px). ASCII uses Consolas's advance;
/// fullwidth (CJK/fullwidth punctuation) uses 1em = the font size — Han glyphs in the Chinese
/// fallback fonts (SimSun/YaHei) are always 1em, which tracks real rendering more closely than
/// the old "CJK = 2 cells ≈ 1.1em" approximation (so the edit caret hugs the last character).
/// Only used for caret positioning; column-width estimation still uses
/// [`crate::ui::state::csv::visual_len`] (erring wide is the safe direction, and changing it
/// would alter existing visuals and test coordinates).
fn text_width_px(s: &str, adv_cell: f32, font_px: f32) -> f32 {
    s.chars()
        .map(|c| {
            if c.width().unwrap_or(1) >= 2 {
                font_px
            } else {
                adv_cell
            }
        })
        .sum()
}

/// Column labels (0 → A, 25 → Z, 26 → AA …), same as Excel.
fn col_letter(mut i: usize) -> String {
    let mut s = Vec::new();
    loop {
        s.push(b'A' + (i % 26) as u8);
        if i < 26 {
            break;
        }
        i = i / 26 - 1;
    }
    s.reverse();
    String::from_utf8(s).unwrap_or_default()
}

#[component]
pub fn CsvTableView() -> Element {
    let mut state = use_context::<Signal<EditorState>>();
    // The winit window handle (injected by shell.rs): updates the IME candidate window's anchor
    // when the edit box moves.
    let window = use_context::<Arc<dyn Window>>();
    // The last edit-box window coordinates sent to winit (left, top, width, height) — rendering
    // runs every frame, so the Cell comparison throttles it: the FFI call happens only on a real
    // change (typing widens it, scrolling, moving to another cell).
    let ime_anchor: std::cell::Cell<[f32; 4]> = use_hook(|| std::cell::Cell::new([f32::NAN; 4]));

    // Frame data (rows, column geometry, scroll position): see `frame` for the
    // zero-write contract this relies on.
    let CsvFrame {
        rows_all,
        header_on,
        n_cols,
        n_cols_total,
        widths,
        col_x,
        num_w,
        scroll_left,
        scroll_top,
        vis_rows,
        vw,
        vh,
        vtop,
        vx,
        text_h,
        sel,
        sort_col,
        sort_desc,
        editing,
        truncated,
        lh,
        total_h,
        total_w,
        n_data,
        adv_cell,
    } = frame(state);

    let t: &'static Theme = state.read().theme();
    // The edit-cell caret shares one source with the text editor: style (line/block/underline),
    // width, blink toggle and period all read the same user settings (Settings → Cursor); the
    // phase is flipped by the BlinkTick thread (`caret_phase`). Geometry comes from viewport's
    // same pure function `caret_geometry`.
    let (crt_style, crt_w, cblink, cphase) = {
        let st = state.read();
        (
            st.caret_style,
            st.caret_w.max(1.0),
            st.caret_blink,
            st.caret_phase,
        )
    };
    let head_h = if header_on { lh } else { 0.0 };
    // Round the horizontal scroll before drawing (the stored value keeps full precision): at a
    // fractional pixel the whole table's vertical rules and row-number borders land on fractional
    // coordinates (another source of disappearing rules).
    let num_left = -scroll_left.round();
    let content_left = num_w + num_left;

    // Bounce the anchor cache back to NaN when editing ends: between edits the composer pulls
    // the candidate window back to the caret, so re-entering the same cell must force a resend
    // (NaN != NaN is always true — a natural force).
    if editing.is_none() {
        ime_anchor.set([f32::NAN; 4]);
    }

    let root_style = format!(
        "position:absolute; top:{vtop}px; left:{vx}px; width:{vw}px; height:{vh}px;
         overflow:hidden; background:{bg}; user-select:none;",
        vtop = vtop,
        vx = vx,
        vw = vw,
        vh = vh,
        bg = t.bg
    );
    let num_style = format!(
        "position:absolute; left:{num_left}px; width:{num_w}px; height:{h}px;
         line-height:{h}px; padding-right:8px; box-sizing:border-box; text-align:right;
         font-family:{FONT_MONO}; font-size:12px; color:{fg}; background:{bg2};
         border-right:1px solid {border}; border-bottom:1px solid {border};",
        fg = t.gutter_fg,
        bg2 = t.gutter_bg,
        border = t.gutter_border,
        h = lh,
    );
    let cell_style = format!(
        "position:absolute; height:{h}px; line-height:{h}px;
         padding:0 {pad}px; box-sizing:border-box; white-space:pre; overflow:hidden;
         font-size:12px; color:{fg}; background:{bgx};
         border-right:1px solid {border}; border-bottom:1px solid {border};",
        fg = t.menu_fg,
        border = t.menu_border,
        pad = CELL_PAD,
        h = lh,
        bgx = "transparent",
    );
    let head_style = format!(
        "position:absolute; height:{h}px; line-height:{h}px;
         padding:0 {pad}px; box-sizing:border-box; white-space:pre; overflow:hidden;
         font-weight:600; font-size:12px; background:{bg2}; color:{fg};
         border-right:1px solid {border}; border-bottom:1px solid {border};",
        pad = CELL_PAD,
        bg2 = t.gutter_bg,
        fg = t.menu_fg,
        border = t.gutter_border,
        h = lh,
    );
    let strip_style = format!(
        "position:absolute; top:0; height:{STRIP_H}px; line-height:{STRIP_H}px;
         box-sizing:border-box; text-align:center; font-size:11px; background:{bg2};
         color:{fg}; border-right:1px solid {border}; border-bottom:1px solid {border};",
        bg2 = t.gutter_bg,
        fg = t.gutter_fg,
        border = t.gutter_border,
    );

    // ---- element collection ----------------------------------------------------
    let mut els: Vec<Element> = Vec::new();

    // The column-letter strip (A/B/C…): always shown — with the header off it is the column's
    // "name", and the column-resize grip lives here too (the header cells have one at their right
    // edge as well).
    let k0 = "corner";
    els.push(rsx! {
        div {
            key: "{k0}",
            style: "{strip_style} left:{num_left}px; width:{num_w}px;",
        }
    });
    for i in 0..n_cols_total {
        let (Some(&cx), Some(&w)) = (col_x.get(i), widths.get(i)) else {
            continue;
        };
        let x = content_left + cx;
        let sorted = sort_col == Some(i) && i < n_cols;
        // Whole-column selection: the letter lights up too (Excel semantics)
        let col_sel = sel.is_some_and(|s| s.kind == SelKind::Col && s.col == i && i < n_cols);
        let color = if sorted || col_sel {
            t.accent
        } else {
            t.gutter_fg
        };
        let idx = i;
        els.push(rsx! {
            div {
                key: "s{idx}",
                style: "{strip_style} left:{x}px; width:{w}px; color:{color};",
                onpointerdown: move |evt| {
                    evt.stop_propagation();
                    evt.prevent_default();
                    // Only real data columns are selectable (blank columns have no data to act on)
                    if idx < n_cols {
                        state.write().csv_pick_col(idx);
                    }
                },
                "{col_letter(idx)}"
            }
        });
    }

    // The header row (when the first-row title is on): arrow sort indicator + three-state sort
    // cycle + column-resize grips
    if header_on {
        let y = STRIP_H;
        // The header is not a numbered record, but its row-number slot still belongs to the
        // table: leaving it unpainted punched a white notch into the left end of the band.
        let k_gutter = "nh";
        els.push(rsx! {
            div {
                key: "{k_gutter}",
                style: "{num_style} top:{y}px; height:{lh}px; line-height:{lh}px;",
            }
        });
        // One header cell per **rendered** column, not per header field: blank filler columns
        // (and columns the header row is too short to reach) used to get no cell at all, so
        // the shaded band and its rules stopped mid-table and left a bare white strip where
        // every body row below shows a grid. Only real data columns carry the sort/column
        // behaviors — a filler column has nothing to order by or to resize (its width is
        // fixed at `EMPTY_COL_W`, so a grip there would drag without effect).
        let hdr = rows_all.first();
        for i in 0..n_cols_total {
            let (Some(&cx), Some(&w)) = (col_x.get(i), widths.get(i)) else {
                continue;
            };
            let cell = hdr.and_then(|h| h.get(i)).map(String::as_str).unwrap_or("");
            let x = content_left + cx;
            let is_data = i < n_cols;
            let sorted = is_data && sort_col == Some(i);
            let arrow = if sorted {
                if sort_desc {
                    " ↓"
                } else {
                    " ↑"
                }
            } else {
                ""
            };
            let label = format!("{cell}{arrow}");
            let idx = i;
            let hx = x + w - RESIZE_HOT;
            els.push(rsx! {
                div {
                    key: "h{idx}",
                    style: "{head_style} left:{x}px; top:{y}px; width:{w}px;",
                    onpointerdown: move |evt| {
                        evt.stop_propagation();
                        evt.prevent_default();
                        if is_data {
                            state.write().csv_header_click(idx);
                        }
                    },
                    "{label}"
                }
            });
            // the column-width grip (data columns only)
            if is_data {
                els.push(rsx! {
                    div {
                        key: "hs{idx}",
                        style: "position:absolute; left:{hx}px; top:{y}px; width:{RESIZE_HOT}px;
                                height:{lh}px; cursor:ew-resize; z-index:6;",
                        onpointerdown: move |evt| {
                            evt.stop_propagation();
                            evt.prevent_default();
                            let px = evt.data.client_coordinates().x as f32;
                            state.write().csv_begin_col_resize(idx, px, w);
                        },
                    }
                });
            }
        }
    }

    // data rows + blank rows
    for (abs_row, y, h) in vis_rows {
        let y = y + STRIP_H + head_h;
        let is_empty = abs_row >= n_data;
        // Whole-row selection: the row-number gutter and every cell in the row light up
        let row_sel = sel.is_some_and(|s| s.kind == SelKind::Row && s.row == abs_row && !is_empty);
        let row_num_style = format!(
            "{num_style} top:{y}px; height:{h}px; line-height:{h}px;{bg}",
            bg = if row_sel {
                format!(" background:{};", t.selection)
            } else {
                String::new()
            }
        );
        // The gutter's bottom edge = the row-height grip
        let ry = y + h - RESIZE_HOT / 2.0;
        let absr = abs_row;
        els.push(rsx! {
            div {
                key: "n{absr}",
                style: "{row_num_style}",
                onpointerdown: move |evt| {
                    evt.stop_propagation();
                    evt.prevent_default();
                    if !{ is_empty } {
                        state.write().csv_pick_cell_row(absr);
                    }
                },
                "{crate::ui::state::format_line_no((absr + 1) as u64)}"
            }
        });
        // the row-height grip
        els.push(rsx! {
            div {
                key: "rs{absr}",
                style: "position:absolute; left:{num_left}px; top:{ry}px; width:{num_w}px;
                        height:{RESIZE_HOT}px; cursor:ns-resize; z-index:6;",
                onpointerdown: move |evt| {
                    evt.stop_propagation();
                    evt.prevent_default();
                    let py = evt.data.client_coordinates().y as f32;
                            state.write().csv_begin_row_resize(absr, py);
                },
            }
        });
        for ci in 0..n_cols_total {
            let (Some(&cx), Some(&w)) = (col_x.get(ci), widths.get(ci)) else {
                continue;
            };
            let x = content_left + cx;
            let is_sel = !is_empty
                && sel.is_some_and(|s| match s.kind {
                    SelKind::Cell => s.row == abs_row && s.col == ci,
                    SelKind::Row => s.row == abs_row,
                    SelKind::Col => s.col == ci,
                });
            let bg = if is_sel {
                t.selection
            } else if abs_row % 2 == 1 && !is_empty {
                t.gutter_bg
            } else {
                "transparent"
            };
            let text: String = rows_all
                .get(abs_row + header_on as usize)
                .and_then(|r| r.get(ci))
                .cloned()
                .unwrap_or_default();
            let style = format!(
                "{cell_style} left:{x}px; top:{y}px; width:{w}px; height:{h}px;
                 line-height:{h}px; background:{bg};",
            );
            let ci2 = ci;
            let abs2 = abs_row;
            let editing_here = editing
                .as_ref()
                .is_some_and(|(er, ec, _, _)| *er == abs_row && *ec == ci);
            if editing_here {
                // The in-place edit box: white background + accent border + text + IME preedit +
                // block caret
                let (val, preedit) = editing
                    .as_ref()
                    .map(|(_, _, v, p)| (v.clone(), p.clone()))
                    .unwrap_or_default();
                let estyle = format!(
                    "position:absolute; left:{x}px; top:{y}px; width:{w}px; height:{h}px;
                     box-sizing:border-box; padding:0 {CELL_PAD}px; white-space:pre;
                     overflow:hidden; font-size:12px; color:{fg2};
                     background:{ibg}; border:1px solid {acc};
                     line-height:{h}px;",
                    fg2 = t.menu_fg,
                    ibg = t.input_bg,
                    acc = t.accent,
                );
                let cstyle = format!("color:{};", t.accent);
                // The IME candidate window follows the edit box: window coordinates = the root
                // div's (left:0, top:vtop) inner coordinates plus the root's offset. It compares
                // unconditionally against the Cell's last-sent value and calls winit only on a
                // change (see the ime_anchor comment at the top of the component).
                //
                // With `INFEDITOR_IME=0` (the GUI-automation escape hatch) this whole block must
                // be skipped: entering cell edit **re-Enables** the window IME each time, and with
                // only the window-level guard the "x,y" typed in tests would be composed by the
                // real IME into "新，y".
                if !winshell::shell::ime_disabled() {
                    let rect = [x, y + vtop, w, h];
                    if ime_anchor.get()[..] != rect[..] {
                        let first = ime_anchor.get()[0].is_nan();
                        ime_anchor.set(rect);
                        // At edit start (the anchor was bounced to NaN by the previous
                        // "no-edit" frame) explicitly **Enable**: blitz Disables the window IME on
                        // TextInput blur, and clicking a cell (unfocusable) in the table goes
                        // through a blur/re-mount rhythm of the composer that can swallow the
                        // re-Enable — an Update alone cannot revive a disabled IME (in practice:
                        // typing pinyin in a cell showed no candidate window).
                        // Enable carries the initial anchor; later rectangle changes downgrade to
                        // Update.
                        let req = if first {
                            winit::window::ImeRequest::Enable(
                                winit::window::ImeEnableRequest::new(
                                    winit::window::ImeCapabilities::new().with_cursor_area(),
                                    winit::window::ImeRequestData::default().with_cursor_area(
                                        winit::dpi::LogicalPosition::new(
                                            rect[0] as f64,
                                            rect[1] as f64,
                                        )
                                        .into(),
                                        winit::dpi::LogicalSize::new(
                                            rect[2] as f64,
                                            rect[3] as f64,
                                        )
                                        .into(),
                                    ),
                                )
                                .unwrap(),
                            )
                        } else {
                            // Update rather than the deprecated set_ime_cursor_area; the window
                            // Enable already carries cursor_area capability (shell.rs), so only
                            // then does the win32 backend accept this anchor.
                            winit::window::ImeRequest::Update(
                                winit::window::ImeRequestData::default().with_cursor_area(
                                    winit::dpi::LogicalPosition::new(
                                        rect[0] as f64,
                                        rect[1] as f64,
                                    )
                                    .into(),
                                    winit::dpi::LogicalSize::new(rect[2] as f64, rect[3] as f64)
                                        .into(),
                                ),
                            )
                        };
                        let _ = window.request_ime_update(req);
                    }
                }
                els.push(rsx! {
                    div {
                        key: "e{abs2}_{ci2}",
                        style: "{estyle}",
                        "{val}"
                        span { style: "{cstyle}", "{preedit}" }
                    }
                });
                // The edit indicator caret: the same as the text editor's — geometry computed by
                // viewport's `caret_geometry` pure function (line = thin bar, block = one glyph
                // wide, underline = 2px at the bottom, source-identical to the text view), with
                // the blink on the same phase (`caret_phase`, flipped by the BlinkTick thread).
                // It used to be a █ character glued after the text: no blink, one glyph wide, off
                // baseline (user feedback: "the caret is unusable"). x is estimated from the edit
                // box text's display width (12px monospace), clamped back when it would cross the
                // cell's inner edge.
                let text_end =
                    x + CELL_PAD + text_width_px(&format!("{val}{preedit}"), adv_cell, 12.0);
                let crt_x = text_end.min(x + w - CELL_PAD - 2.0).max(x + CELL_PAD);
                // Leave 2px top and bottom (touching the edge would cover the cell border)
                let lh_in = (h - 4.0).max(4.0);
                let (crt_w_px, crt_top_off, crt_h) =
                    super::viewport::caret_geometry(crt_style, crt_w, adv_cell, lh_in);
                let crt_y = y + 2.0 + crt_top_off;
                // A CSS `caret-blink` infinite animation would make blitz self-renew redraws
                // every frame (the `is_animating` branch of `View::redraw`), so drive the phase
                // instead.
                let crt_op = if cblink && !cphase { 0.0 } else { 1.0 };
                let cc = t.caret;
                let ckey = format!("crt{abs2}_{ci2}");
                els.push(rsx! {
                    div {
                        key: "{ckey}",
                        style: "position:absolute; left:{crt_x}px; top:{crt_y}px; \
                                width:{crt_w_px}px; height:{crt_h}px; background:{cc};
                                opacity:{crt_op}; z-index:7; pointer-events:none;",
                    }
                });
                continue;
            }
            els.push(rsx! {
                div {
                    key: "c{abs2}_{ci2}",
                    style: "{style}",
                    onpointerdown: move |_| {
                        // Note: do **not** stop_propagation / prevent_default here — blitz folds
                        // both into "cancel the default action", and pointerdown's default action
                        // is the **behavior** of focusing / placing the caret / selecting (see
                        // blitz-dom events/driver.rs: the default action is skipped after a
                        // cancel). Gesture accounting (click_count, mousedown_position, resetting
                        // drag_mode) was hoisted to the event entrance by patch fourteen and runs
                        // unconditionally, unaffected by a cancel here — but focus and the caret
                        // would still be lost. No other pointerdown handler exists on the cell's
                        // parent chain, so bubbling is harmless.
                        if !is_empty {
                            state.write().csv_pick_cell(abs2, ci2);
                        }
                    },
                    ondoubleclick: move |evt| {
                        evt.stop_propagation();
                        evt.prevent_default();
                        // Blank cells do not support editing: write-back proceeds against the
                        // existing data rows
                        if !is_empty {
                            state.write().csv_begin_edit(abs2, ci2);
                        }
                    },
                    "{text}"
                }
            });
        }
    }

    // ---- scrollbars -----------------------------------------------------------
    let track_v_len = (vh - SB_W).max(20.0);
    let track_h_len = (vw - num_w - SB_W).max(20.0);
    let (thumb_v_y, thumb_v_h) =
        crate::ui::commands::csv::thumb_geom(scroll_top, total_h, track_v_len);
    let (thumb_h_x, thumb_h_w) =
        crate::ui::commands::csv::thumb_geom(scroll_left, total_w, track_h_len);
    let sb_bg = t.gutter_bg;
    let sb_thumb = t.menu_border;
    let v_track_x = vw - SB_W;
    let h_track_y = vh - SB_W;

    let (k1, k2, k3, k4) = ("trkv", "thmv", "trkh", "thmh");
    // Wheel delta on the horizontal scrollbar (fires only while hovering it — see the element's
    // onwheel). The sign matches the vertical one (`-=`: the raw wheel-up/content-left direction),
    // and a track page-scroll uses the whole track width.
    let h_wheel_dy = move |d: &dioxus_elements::geometry::WheelDelta| -> f64 {
        match d {
            dioxus_elements::geometry::WheelDelta::Pixels(v) => v.x + v.y,
            dioxus_elements::geometry::WheelDelta::Lines(v) => (v.x + v.y) * lh as f64,
            dioxus_elements::geometry::WheelDelta::Pages(v) => v.y * track_h_len as f64,
        }
    };
    els.push(rsx! {
        // the vertical scrollbar
        div {
            key: "{k1}",
            style: "position:absolute; left:{v_track_x}px; top:0; width:{SB_W}px; height:{track_v_len}px;
                    background:{sb_bg}; border-left:1px solid {t.menu_border};",
            onpointerdown: move |evt| {
                evt.stop_propagation();
                evt.prevent_default();
                let y = evt.data.client_coordinates().y as f32 - vtop;
                state.write().csv_sb_track(true, y);
            },
        }
    });
    els.push(rsx! {
        div {
            key: "{k2}",
            style: "position:absolute; left:{v_track_x}px; top:{thumb_v_y}px; width:{SB_W}px;
                    height:{thumb_v_h}px; background:{sb_thumb}; border-radius:2px;
                    cursor:default; z-index:7;",
            onpointerdown: move |evt| {
                evt.stop_propagation();
                evt.prevent_default();
                let y = evt.data.client_coordinates().y as f32 - vtop;
                state.write().csv_sb_drag(true, y);
            },
        }
    });
    els.push(rsx! {
        // The horizontal scrollbar: horizontal wheel scrolling works only while hovering it
        // (stop_propagation blocks the root element's vertical wheel, so wheeling the body never
        // touches horizontal scrolling)
        div {
            key: "{k3}",
            style: "position:absolute; left:0px; top:{h_track_y}px; width:{track_h_len}px; height:{SB_W}px;
                    background:{sb_bg}; border-top:1px solid {t.menu_border};",
            onpointerdown: move |evt| {
                evt.stop_propagation();
                evt.prevent_default();
                // The table's root container sits at view.x: the track conversion needs
                // **viewport-relative** coordinates
                let x = evt.data.client_coordinates().x as f32 - vx;
                state.write().csv_sb_track(false, x);
            },
            onwheel: move |evt| {
                evt.stop_propagation();
                let dy = h_wheel_dy(&evt.data.delta());
                if dy == 0.0 {
                    return;
                }
                let mut st = state.write();
                st.tab_mut().csv.scroll_left -= dy as f32;
                st.clamp_csv_scroll();
            },
        }
    });
    els.push(rsx! {
        div {
            key: "{k4}",
            style: "position:absolute; left:{thumb_h_x}px; top:{h_track_y}px; width:{thumb_h_w}px;
                    height:{SB_W}px; background:{sb_thumb}; border-radius:2px;
                    cursor:default; z-index:7;",
            onpointerdown: move |evt| {
                evt.stop_propagation();
                evt.prevent_default();
                let x = evt.data.client_coordinates().x as f32 - vx;
                state.write().csv_sb_drag(false, x);
            },
            onwheel: move |evt| {
                evt.stop_propagation();
                let dy = h_wheel_dy(&evt.data.delta());
                if dy == 0.0 {
                    return;
                }
                let mut st = state.write();
                st.tab_mut().csv.scroll_left -= dy as f32;
                st.clamp_csv_scroll();
            },
        }
    });

    if truncated {
        let trunc_note = state.read().tr(Msg::CsvTruncatedNote).into_owned();
        let trunc_style = format!(
            "position:absolute; left:0; right:{SB_W}px; bottom:{SB_W}px; height:20px; line-height:20px;
             padding:0 10px; font-size:11px; background:{bg2}; color:{fg};",
            bg2 = t.panel_bg,
            fg = t.menu_fg
        );
        let k5 = "trunc";
        els.push(rsx! {
            div {
                key: "{k5}",
                style: "{trunc_style}",
                "{trunc_note}"
            }
        });
    }

    rsx! {
        div {
            style: "{root_style}",
            onpointerdown: move |evt| {
                // Finger-down accounting: a slide = scroll the table (window-level deltas route
                // into `ui::touch`); a lift with no slide is the tap (the cell/scrollbar's own
                // pointerdown still runs first — they stop_propagation).
                if evt.data.pointer_type() == "touch" {
                    let c = evt.data.client_coordinates();
                    crate::ui::touch::begin(
                        crate::ui::touch::PanTarget::Csv,
                        evt.data.pointer_id(),
                        c.x,
                        c.y,
                    );
                }
            },
            onpointermove: move |evt| {
                // All drag gestures (scrollbar / row & column resize) are consumed here: the
                // start point is recorded on press, and moves update per gesture type
                let dragging = state.read().tab().csv.drag.is_some();
                if dragging {
                    let pt = evt.data.client_coordinates();
                    state.write().csv_drag_move(pt.x as f32, pt.y as f32);
                }
            },
            onpointerup: move |_| {
                // Conditional write: a lift with no active gesture does not mark dirty
                if state.read().tab().csv.drag.is_some() {
                    state.write().csv_end_drag();
                }
            },
            onwheel: move |evt| {
                // The table body's wheel **only scrolls vertically**. This used to also handle
                // Shift+wheel and the touchpad's horizontal component (horizontal scrolling from
                // anywhere), so a vertical notch also nudged the horizontal thumb — horizontal
                // wheeling now belongs solely to the "hovering the horizontal scrollbar" gesture
                // (see the horizontal track/thumb element's own onwheel).
                let d = evt.data.delta();
                // A zero delta (empty events synthesized by some touchpads) returns right away:
                // write() would unconditionally mark dirty while nothing changes before/after
                // clamping
                let dy = match &d {
                    dioxus_elements::geometry::WheelDelta::Pixels(v) => v.y,
                    dioxus_elements::geometry::WheelDelta::Lines(v) => v.y * lh as f64,
                    dioxus_elements::geometry::WheelDelta::Pages(v) => v.y * text_h as f64,
                };
                if dy == 0.0 {
                    return;
                }
                // Enqueue and fuse (see `ui::scroll`); clamping happens in the frame hook
                if crate::ui::scroll::queue(crate::ui::scroll::Target::Csv, 0.0, dy) {
                    state.read().request_frame();
                }
            },
            { els.into_iter() }
        }
    }
}

/// Everything one CSV frame needs: rows, column geometry, scroll position,
/// selection and the caret's cell.
///
/// Computed once per frame by [`frame`]; the render half only reads it. The
/// struct (rather than a tuple) is what keeps a 25-value return type readable.
struct CsvFrame {
    rows_all: std::rc::Rc<Vec<Vec<String>>>,
    header_on: bool,
    n_cols: usize,
    n_cols_total: usize,
    widths: Vec<f32>,
    col_x: Vec<f32>,
    num_w: f32,
    scroll_left: f32,
    scroll_top: f32,
    /// (data row, y, height) for the rows intersecting the viewport
    vis_rows: Vec<(usize, f32, f32)>,
    vw: f32,
    vh: f32,
    vtop: f32,
    vx: f32,
    text_h: f32,
    sel: Option<crate::ui::state::csv::CellSel>,
    sort_col: Option<usize>,
    sort_desc: bool,
    /// (row, col, value, preedit) of the cell being edited
    editing: Option<(usize, usize, String, String)>,
    truncated: bool,
    lh: f32,
    total_h: f32,
    total_w: f32,
    n_data: usize,
    adv_cell: f32,
}

/// Geometry and data for one frame, with the caches warmed up.
///
/// Iron rule: dropping a `state.write()` guard during render unconditionally
/// mark_dirties — an unconditional per-frame write once caused a self-sustaining
/// render loop in CSV table mode. This does zero writes in steady state, the same
/// conditional-write pattern as viewport/toolbar (the probe condition matches the
/// fix).
fn frame(mut state: Signal<EditorState>) -> CsvFrame {
    {
        let st = state.read();
        if st.csv_rows_cached().is_none() {
            // Stale cache: write once to refresh this frame; the next frame enters the
            // pure-read fast lane
            drop(st);
            state.write().csv_rows_ensure();
        }
    }
    let st = state.read();
    let lh = ROW_H.max(st.metrics.lh.max(1.0)).round();
    let vw = st.view.w.max(200.0);
    let vh = st.view.h.max(40.0);
    let vtop = st.view.y;
    let vx = st.view.x;
    let text_h = st.text_h().max(40.0);
    // The cell edit box uses a 12px fixed-pitch font: the advance used to estimate the
    // caret's x scales proportionally from the editor's font size (same font family, advance
    // proportional to size).
    let adv_cell = st.metrics.adv / st.font_size.max(1.0) * 12.0;

    let (rows_all, _, _, truncated) = st.csv_rows_cached().unwrap_or_else(|| {
        (
            std::rc::Rc::new(Vec::new()),
            std::rc::Rc::new(Vec::new()),
            std::rc::Rc::new(Vec::new()),
            false,
        )
    });
    let header_on = st.tab().csv.header;
    let sel = st.tab().csv.sel;
    let sort_col = st.tab().csv.sort_col;
    let sort_desc = st.tab().csv.sort_desc;
    let editing = st.tab().csv.editing.clone().map(|e| {
        let crate::ui::state::csv::CellEdit {
            row,
            col,
            value,
            preedit,
            ..
        } = e;
        (row, col, value, preedit)
    });
    let colw_over = st.tab().csv.col_widths.clone();

    // The column-layout (n_cols + widths) cache: key = row-data Rc identity + overridden
    // widths + header toggle + character advance. If the row data is unchanged (same Rc), do
    // not scan **all** rows for the column count or recompute the sampled width — a 50k-row ×
    // 30-column sheet used to mean 50k row scans + 9000 per-character width measurements
    // every frame (a render-thread-local cache, zero state writes during render).
    let col_layout = {
        type ColKey = (usize, Vec<Option<f32>>, bool, u32);
        type ColLayout = (usize, Vec<f32>);
        thread_local! {
            static LAST: std::cell::RefCell<Option<(ColKey, ColLayout)>> = None.into();
        }
        let key = (
            std::rc::Rc::as_ptr(&rows_all) as usize,
            colw_over.clone(),
            header_on,
            st.metrics.adv.to_bits(),
        );
        // Taking the cache with take() alone would make it one-shot: the hit branch removes
        // the value without putting it back, forcing a recompute next frame (a full 50k×30
        // table scan + a 300-row sample). On a hit, clone the layout (a few f32s, one per
        // column); only a miss computes and writes it back.
        let cached = LAST.with(|l| {
            l.borrow()
                .as_ref()
                .and_then(|(k, v)| (k == &key).then(|| v.clone()))
        });
        match cached {
            Some(layout) => layout,
            None => {
                let computed = auto_col_widths(rows_all.as_ref(), header_on, st.metrics.adv);
                LAST.with(|l| *l.borrow_mut() = Some((key, computed.clone())));
                computed
            }
        }
    };
    let n_cols = col_layout.0;
    let auto: Vec<f32> = col_layout.1;

    // Blank columns: pad up to the viewport width plus a few more (Excel's "more content
    // could go here" look)
    let mut n_cols_total = n_cols;

    let mut widths: Vec<f32> = auto
        .iter()
        .enumerate()
        .map(|(i, w)| colw_over.get(i).copied().flatten().unwrap_or(*w))
        .collect();

    // Pixel alignment (the root cause of vertical lines coming and going): column widths
    // are float estimates from font advances (50.6px, say), and using them directly drops a
    // 1px border-right on a fractional pixel — under HiDPI scaling the rasterizer can swallow
    // the whole line. After rounding, every column boundary sits on a whole CSS px and the
    // rules stay stable.
    for w in widths.iter_mut() {
        *w = w.round();
    }

    // Blank-column fill: compute the total from the data column widths first, then pad
    // with blank columns (EMPTY_COL_W each) if it is still short
    let num_w = ((crate::ui::state::line_no_digits(rows_all.len().max(1) as u64).max(2) as f32)
        * st.metrics.adv
        + 14.0)
        .round();
    let avail_w = vw - num_w - SB_W;
    let mut total_w: f32 = widths.iter().sum();
    while total_w < avail_w && n_cols_total - n_cols < MAX_EMPTY_COLS {
        widths.push(EMPTY_COL_W);
        total_w += EMPTY_COL_W;
        n_cols_total += 1;
    }

    let col_x: Vec<f32> = widths
        .iter()
        .scan(0.0, |acc, w| {
            let x = *acc;
            *acc += *w;
            Some(x)
        })
        .collect();

    // Visible row window: the first row whose cumulative offset passes scroll_top. Row-height
    // overrides are usually rare: the uniform first segment is skipped by division (O(1))
    // and only override rows are walked one by one.
    let overrides = st.tab().csv.row_heights.clone();
    let n_data = rows_all.len().saturating_sub(header_on as usize);
    let total_h = {
        let extra: f32 = overrides.range(..n_data).map(|(_, h)| h - lh).sum();
        n_data as f32 * lh + extra
    };

    // Scroll clamping: compute the legal value first, and only write once when it differs
    // from the stored one (out-of-range appears once after a scroll/document change, then
    // converges)
    let max_v = (total_h - text_h).max(0.0) as f64;
    let max_h = (total_w - (vw - num_w - SB_W)).max(0.0);
    let (st_scroll_top, st_scroll_left) = {
        let csv = &st.tab().csv;
        (csv.scroll_top, csv.scroll_left)
    };
    let scroll_top = st_scroll_top.clamp(0.0, max_v);
    let scroll_left = st_scroll_left.clamp(0.0, max_h);
    if scroll_top != st_scroll_top || scroll_left != st_scroll_left {
        drop(st);
        {
            let mut w = state.write();
            let csv = &mut w.tab_mut().csv;
            csv.scroll_top = scroll_top;
            csv.scroll_left = scroll_left;
        }
    }
    let (scroll_top, scroll_left) = (scroll_top as f32, scroll_left);

    let mut first = 0usize;
    let mut acc = 0.0f32;
    {
        let mut oit = overrides.iter();
        let mut next_ov = oit.next();
        // Uniform segment: jump by division (until the next override row or the target)
        while first < n_data && next_ov.map(|(&r, _)| r) != Some(first) {
            let take = ((scroll_top - acc) / lh).floor();
            if take <= 0.0 {
                break;
            }
            let limit = next_ov.map(|(&r, _)| r).unwrap_or(n_data);
            let step = (take as usize).min(limit - first);
            if step == 0 {
                break;
            }
            first += step;
            acc += step as f32 * lh;
        }
        // Walk row by row around override rows
        while first < n_data {
            let h = if next_ov.map(|(&r, _)| r) == Some(first) {
                let h = overrides.get(&first).copied().unwrap_or(lh);
                next_ov = oit.next();
                h
            } else {
                lh
            };
            if acc + h > scroll_top {
                break;
            }
            acc += h;
            first += 1;
        }
    }
    // Render until the viewport is filled (blank rows included)
    let mut vis_rows: Vec<(usize, f32, f32)> = Vec::new(); // (data row, y, h)
    {
        let rh = |row: usize| overrides.get(&row).copied().unwrap_or(lh);
        let mut y = 0.0f32;
        let mut r = first;
        while y < text_h + lh && r < n_data {
            let h = rh(r);
            vis_rows.push((r, y, h));
            y += h;
            r += 1;
        }
        // Fill the remaining space with blank rows (numbers keep counting up)
        let mut er = 0usize;
        while y < text_h + lh && er < MAX_EMPTY_ROWS {
            vis_rows.push((n_data + er, y, lh));
            y += lh;
            er += 1;
        }
    }
    // Round row positions (same reasoning as column widths): row-height overrides and row
    // height estimates are floats, so the accumulated y carries fractions — a 1px
    // border-bottom on a fractional pixel gets swallowed by the rasterizer. Rounding at the
    // accumulated boundaries makes adjacent rows meet seamlessly.
    for row in vis_rows.iter_mut() {
        let end = row.1 + row.2;
        row.1 = row.1.round();
        row.2 = end.round() - row.1;
    }

    CsvFrame {
        rows_all,
        header_on,
        n_cols,
        n_cols_total,
        widths,
        col_x,
        num_w,
        scroll_left,
        scroll_top,
        vis_rows,
        vw,
        vh,
        vtop,
        vx,
        text_h,
        sel,
        sort_col,
        sort_desc,
        editing,
        truncated,
        lh,
        total_h,
        total_w,
        n_data,
        adv_cell,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_width_px_uses_1em_for_wide_chars() {
        let adv = 6.6_f32; // approximate advance of 12px Consolas
                           // ASCII: per-character advance
        assert!((text_width_px("abc", adv, 12.0) - 3.0 * adv).abs() < 1e-4);
        // fullwidth: 1em (not the old approximation 2×advance = 13.2)
        assert!((text_width_px("汉", adv, 12.0) - 12.0).abs() < 1e-4);
        // mixed: ASCII advance + fullwidth 1em
        assert!((text_width_px("a汉", adv, 12.0) - (adv + 12.0)).abs() < 1e-4);
        // empty string
        assert!(text_width_px("", adv, 12.0).abs() < 1e-4);
        // fullwidth punctuation is also 1em
        assert!((text_width_px("，", adv, 12.0) - 12.0).abs() < 1e-4);
    }
}
