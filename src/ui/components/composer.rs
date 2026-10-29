//! The single real DOM focus target of the editor.
//!
//! A transparent `textarea` parked either over the caret (document editing) or
//! over the focused find-bar box. Everything else is *virtual* focus: blitz
//! panics when `MountedData::set_focus` runs from a dioxus task, and it never
//! moves focus away once set, so one permanent composer + state routing is the
//! only workable input model here.
//!
//! The textarea is a pure **focus/IME anchor** — it never holds text. Character
//! input is consumed in `onkeydown` (`Key::Character` comes straight from
//! `WM_CHAR` in winit-win32, so it is byte-identical to what blitz's default
//! action would have inserted) and written directly into the document; IME
//! commits arrive as vendor-patch-7 synthetic `keydown` events (blitz rewrites
//! `WindowEvent::Ime(Commit)` in the DOM layer) and are routed by the same
//! `onkeydown` path. The earlier model — let the default action type into the
//! textarea, mirror it out via `oninput`, then reset the value — had a
//! structural race: dioxus clears the mirror signal in an effect, but the
//! matching `value=""` mutation only reaches blitz's internal editor at the
//! next async flush. A keystroke landing in between saw an already-cleared
//! snapshot and re-inserted (or dropped) everything the DOM still held —
//! visible exactly when typing fast.

use dioxus_elements::input_data::MouseButton;
use dioxus_native::prelude::*;
use dioxus_native::winit;
use std::sync::Arc;
use winit::window::Window;

use crate::ui::actions::{self, Cmd};
use crate::ui::components::findbar::field_composer_rect;
use crate::ui::shortcuts::{self, Chord, Mods, K};
use crate::ui::state::csv::SelKind;
use crate::ui::state::{EditorState, FindFocus};
use crate::ui::vim;
use crate::ui::{Fmt, Msg};
use i18n::Message;

/// Keys handled while a find-bar box owns the keyboard.
fn find_key(mut state: Signal<EditorState>, data: &KeyboardData) -> bool {
    let ctrl = data.modifiers().ctrl();
    match data.key() {
        Key::Enter => {
            let mut st = state.write();
            match st.find.focus {
                FindFocus::Pattern => st.find_next(),
                FindFocus::Replacement => st.replace_one(),
                FindFocus::Goto => {
                    st.commit_goto();
                }
                FindFocus::Doc => {}
            }
            true
        }
        Key::Escape => {
            state.write().close_find();
            true
        }
        Key::Tab => {
            let mut st = state.write();
            st.find.focus = match st.find.focus {
                FindFocus::Pattern => {
                    if st.find.replace {
                        FindFocus::Replacement
                    } else {
                        FindFocus::Pattern
                    }
                }
                FindFocus::Replacement => FindFocus::Pattern,
                other => other,
            };
            true
        }
        Key::Backspace => state.write().find_backspace(),
        Key::Delete => true,
        // Up/Down walk the match list
        Key::ArrowUp => {
            state.write().find_prev();
            true
        }
        Key::ArrowDown => {
            state.write().find_next();
            true
        }
        // single-line boxes: swallow the caret keys
        Key::ArrowLeft | Key::ArrowRight | Key::Home | Key::End => true,
        Key::Character(s) if ctrl => match s.to_ascii_lowercase().as_str() {
            "v" => {
                state.write().find_paste_clip();
                true
            }
            "c" => {
                state.write().find_copy_clip();
                true
            }
            "a" => true,
            _ => false,
        },
        // Literal input into the find box: same source as document typing
        // (WM_CHAR in onkeydown), no longer going through the textarea's
        // oninput (reason: see the Character branch of edit_key).
        Key::Character(s) => {
            state.write().find_insert(&s);
            true
        }
        _ => false,
    }
}

/// Translate a DOM key event into the toolkit-independent [`Chord`] the
/// binding table is keyed by. Returns `None` for keys that only ever reach the
/// document as navigation.
pub fn chord_of(data: &KeyboardData) -> Option<Chord> {
    let m = data.modifiers();
    let mods = Mods {
        ctrl: m.ctrl(),
        shift: m.shift(),
        alt: m.alt(),
    };
    let key = match data.key() {
        Key::Character(s) => {
            // Ctrl+Shift+P arrives as an uppercase 'P'; the table is
            // lower-case, and `Chord::new` would fix it anyway, but doing it
            // here keeps `Chord`'s invariant local.
            let mut it = s.chars();
            let c = it.next()?;
            if it.next().is_some() {
                return None; // IME commit: not a chord
            }
            K::Char(c)
        }
        Key::Enter => K::Enter,
        Key::Tab => K::Tab,
        Key::Escape => K::Escape,
        Key::Backspace => K::Backspace,
        Key::Delete => K::Delete,
        Key::ArrowLeft => K::Left,
        Key::ArrowRight => K::Right,
        Key::ArrowUp => K::Up,
        Key::ArrowDown => K::Down,
        Key::Home => K::Home,
        Key::End => K::End,
        Key::PageUp => K::PageUp,
        Key::PageDown => K::PageDown,
        _ => return None,
    };
    Some(Chord::new(mods, key))
}

/// Route <kbd>Alt</kbd> (alone or with a letter) to the menu bar.
///
/// Returns `Some(true)` when the key was consumed, `Some(false)` when it was a
/// menu-bar key but did nothing (still consumed by the caller so it cannot leak
/// into the document), and `None` when this was not a menu-bar key at all.
///
/// A letter that is not an access key (`Alt`+<kbd>Z</kbd>, say) returns `None`
/// so it keeps whatever meaning it had before; Windows would swallow it into the
/// menu's "type to filter" behaviour, which this menu bar does not implement.
///
/// The bare-<kbd>Alt</kbd> session also routes *unmodified* letters here: with
/// a native menu bar Windows enters menu mode on Alt release and turns the next
/// letter into a mnemonic (`WM_SYSKEYDOWN` without the Alt state). This menu
/// bar has no native window to hang that off, so [`crate::ui::state::menu`]'s
/// `menu.alt` flag *is* the session — see the routing call site.
fn menu_alt_key(mut state: Signal<EditorState>, data: &KeyboardData) -> Option<bool> {
    match data.key() {
        // A bare Alt press only reveals the access keys.
        Key::Alt => Some(state.write().menu_alt_key(None, false)),
        Key::Escape => Some(state.write().menu_alt_key(None, true)),
        Key::Character(s) if !data.modifiers().ctrl() => {
            // `Alt` + a character may be more than one code unit (an IME commit);
            // only a single character can be an access key.
            let mut it = s.chars();
            let c = it.next()?;
            if it.next().is_some() {
                return None;
            }
            // Either Alt is still held (Alt+letter chord) or the bare-Alt
            // session is active (Alt released, letter follows).
            if !data.modifiers().alt() && !state.read().menu.alt {
                return None;
            }
            Some(state.write().menu_alt_key(Some(c), false))
        }
        _ => None,
    }
}

/// Keys for an in-place CSV cell edit (see [`crate::ui::state::csv::CellEdit`]).
///
/// Excel semantics: Enter commits and moves down, Tab commits and moves right,
/// Escape cancels; characters (and Ctrl+V) go into the cell buffer. Like the
/// dialog field this is a single-line sink — navigation keys are swallowed.
fn csv_edit_key(mut state: Signal<EditorState>, data: &KeyboardData) -> bool {
    match data.key() {
        Key::Enter => {
            state.write().csv_commit_edit(1);
            true
        }
        Key::Tab => {
            state.write().csv_commit_edit(2);
            true
        }
        Key::Escape => {
            state.write().csv_cancel_edit();
            true
        }
        Key::Backspace => {
            state.write().csv_edit_backspace();
            true
        }
        Key::Character(s) => {
            if data.modifiers().ctrl() {
                // Ctrl+C/V are already intercepted by the global shortcut table
                // (Cmd::Copy/Paste → the CSV branches of copy()/paste()), so they
                // never reach here; other Ctrl combos do not enter the cell buffer.
                let _ = s;
                true
            } else {
                state.write().csv_edit_insert(&s);
                true
            }
        }
        // single-line edit: swallow all navigation/edit keys
        _ => true,
    }
}

/// Keys for the CSV table view **while not editing a cell** (Excel semantics):
/// arrow keys move the selection, Enter/F2 begins an in-place edit, Delete
/// deletes the selected row/column or clears the selected cell. Ctrl+C/V are
/// routed by the global shortcut table to `copy()`/`paste()` (CSV branches in
/// `commands::csv::csv_copy_route` / `csv_paste_route`), so they never reach
/// here. Everything else falls through unchanged (the table is a view — other
/// keys keep their document meaning).
fn csv_table_key(mut state: Signal<EditorState>, data: &KeyboardData) -> bool {
    if data.modifiers().ctrl() || data.modifiers().alt() {
        return false;
    }
    match data.key() {
        Key::ArrowUp => state.write().csv_move_sel(-1, 0),
        Key::ArrowDown => state.write().csv_move_sel(1, 0),
        Key::ArrowLeft => state.write().csv_move_sel(0, -1),
        Key::ArrowRight => state.write().csv_move_sel(0, 1),
        Key::Enter | Key::F2 => {
            let Some(s) = state.read().tab().csv.sel else {
                return false;
            };
            state.write().csv_begin_edit(s.row, s.col);
            true
        }
        Key::Delete => {
            let Some(s) = state.read().tab().csv.sel else {
                return false;
            };
            match s.kind {
                SelKind::Row => {
                    state.write().csv_delete_row();
                    true
                }
                SelKind::Col => {
                    state.write().csv_delete_col();
                    true
                }
                SelKind::Cell => {
                    state.write().csv_clear_cell();
                    true
                }
            }
        }
        _ => false,
    }
}

/// Keys handled while a modal dialog owns the keyboard.
///
/// An input dialog (Run…, Save Macro…) collects text into `DialogInput`; every
/// other dialog is a plain sink where only Escape does anything. In both cases
/// *nothing* may fall through to the document — the dialog is modal, and the
/// composer keeps DOM focus for the whole session.
fn dialog_key(mut state: Signal<EditorState>, data: &KeyboardData) -> bool {
    let ctrl = data.modifiers().ctrl();
    let has_input = state
        .read()
        .dialog
        .as_ref()
        .is_some_and(|d| d.input.is_some());
    match data.key() {
        Key::Escape => {
            state.write().close_dialog();
            true
        }
        Key::Enter => {
            // Submit only when the dialog actually has a field: a stray Enter
            // must never confirm an information dialog by accident.
            if has_input {
                state.write().dialog_input_submit();
            }
            true
        }
        _ if !has_input => true,
        Key::Backspace => {
            state.write().dialog_input_backspace();
            true
        }
        Key::Character(s) if ctrl => match s.to_ascii_lowercase().as_str() {
            "v" => {
                let mut text = String::new();
                let _clip = platform::clipboard::lock();
                if let Ok(mut cb) = arboard::Clipboard::new() {
                    if let Ok(t) = cb.get_text() {
                        text = t;
                    }
                }
                if text.is_empty() {
                    text = state.read().clip.clone();
                }
                // single-line field: any newline becomes a space
                let text = text.replace(['\r', '\n'], " ");
                if !text.is_empty() {
                    state.write().dialog_input_insert(&text);
                }
                true
            }
            "c" | "x" => {
                let v = state.read().dialog_input();
                if !v.is_empty() {
                    state.write().clip = v.clone();
                    let _clip = platform::clipboard::lock();
                    if let Ok(mut cb) = arboard::Clipboard::new() {
                        let _ = cb.set_text(v);
                    }
                }
                if s.eq_ignore_ascii_case("x") {
                    state.write().dialog_input_clear();
                }
                true
            }
            // any other chord must not reach the document behind a modal
            _ => true,
        },
        Key::Character(s) => {
            state.write().dialog_input_insert(&s);
            true
        }
        // single-line field: swallow every navigation / edit key
        _ => true,
    }
}

/// Whether an event was triggered by the primary (left) mouse button.
///
/// The editor's context menu is raised on the *right* button, and it acts on
/// whatever is selected at that moment. Treating a right press as "click here to
/// move the caret" would clear that selection before the menu could copy it, so
/// every press handler filters on the button first.
pub(crate) fn is_primary_press<
    D: dioxus_elements::point_interaction::PointerInteraction + ?Sized,
>(
    data: &D,
) -> bool {
    matches!(data.trigger_button(), Some(MouseButton::Primary))
}

/// Returns `true` when the key was consumed (the caller must prevent default).
pub fn handle_key(mut state: Signal<EditorState>, data: &KeyboardData) -> bool {
    let mods = data.modifiers();
    let ctrl = mods.ctrl();
    let shift = mods.shift();
    let alt = mods.alt();

    // ---- A modal dialog swallows every key ----------------------------------
    //
    // Must sit **before** the reserved keys / Vim / shortcut table: when the
    // dialog is only guarded at the single edit_key entry, unbound combos
    // (Ctrl+Shift+U…) still trigger commands, Ctrl+W re-opens the dialog over
    // pending_close, and Vim's dd/x/u edit the document directly — content under
    // a confirmation dialog would be modified, and what gets saved after
    // confirmation is not what the user saw. The dialog's own keys
    // (Esc / button navigation / input) are handled entirely by dialog_key.
    if state.read().dialog.is_some() {
        return dialog_key(state, data);
    }

    // ---- the menu bar owns the keyboard while it is active ----------------
    //
    // <kbd>Alt</kbd> arrives as `WM_SYSKEYDOWN`, which winit turns into a
    // normal key event (see `winit-win32/src/keyboard.rs`: `WM_SYSKEYDOWN` goes
    // through the same builder as `WM_KEYDOWN`, only `WM_SYSCHAR` is special), so
    // it reaches us here as `Key::Alt` / `Key::Character(c)` with the ALT
    // modifier set.
    //
    // This has to be decided *before* everything else: with no native menu bar
    // there is nothing else that would consume <kbd>Alt</kbd>, and letting it
    // fall through would type a stray character into the document.
    //
    // `menu.alt` extends the session past the Alt release: Windows enters menu
    // mode on bare-Alt and turns the next letter into a mnemonic even without
    // the modifier — mirror that here or the letter types into the document.
    if alt || data.key() == Key::Alt || state.read().menu.alt {
        if let Some(consumed) = menu_alt_key(state, data) {
            return consumed;
        }
    }

    // Some chords are decided before the find bar gets a look: `Ctrl+F` must
    // open find even while the find bar already owns the keyboard.
    let chord = chord_of(data);
    let Some(chord) = chord else {
        return edit_key(state, data, ctrl, shift);
    };
    if is_reserved(&chord) {
        if let Some(cmd) = shortcuts::lookup(&chord) {
            actions::run(&mut state.write(), cmd);
            return true;
        }
    }

    // WYSIWYG Markdown mode: all editing chords are rerouted to the wys editor
    // (reserved keys — save/find etc. — were already handled normally above).
    // Placed before the shortcut table: in wys mode Ctrl+B/Z… are commands on
    // the wys model, and must not fall into the piece-table bindings.
    if state.read().tab().md_wys && super::mdws::wys_chord_key(&mut state, data, ctrl, shift) {
        return true;
    }

    // Vim owns the keyboard while it is enabled, apart from the chords that
    // must always work (save, find). `vim_key` returns `false` when Vim has no
    // opinion, and then the normal bindings get their turn.
    if state.read().vim.enabled && !is_reserved(&chord) && vim_key(&mut state, data) {
        return true;
    }

    // the binding table — the same source the menu labels come from
    if let Some(cmd) = shortcuts::lookup(&chord) {
        // `Ctrl+Shift+U` must not also fire as `Ctrl+U`, so a command that
        // consumes text needs the *exact* modifier set. Zoom and friends are
        // unaffected by Shift, so they accept the loose form.
        let loose = matches!(
            cmd,
            Cmd::ZoomIn | Cmd::ZoomOut | Cmd::ZoomReset | Cmd::TabNext | Cmd::TabPrev
        );
        let same_mods = loose
            || chord.mods
                == Mods {
                    ctrl,
                    shift,
                    alt: mods.alt(),
                };
        if same_mods {
            actions::run(&mut state.write(), cmd);
            return true;
        }
    }

    edit_key(state, data, ctrl, shift)
}

/// Plain navigation / editing keys, plus the find bar's own key map.
fn edit_key(mut state: Signal<EditorState>, data: &KeyboardData, ctrl: bool, shift: bool) -> bool {
    // An open menu owns the arrow keys, Enter and Escape: the composer keeps DOM
    // focus for the whole session, so without this a dropdown could not be
    // driven from the keyboard at all.
    if state.read().menu.open.is_some() && menu_key(&mut state, data) {
        return true;
    }
    // In-page context menu (non-Windows fallback) owns the same set of keys.
    if state.read().ctx.is_some() && ctx_menu_key(&mut state, data) {
        return true;
    }
    // A modal dialog swallows every key; Escape closes it. An input dialog
    // (Run…, Save Macro…) additionally *collects* text — see `dialog_key`.
    if state.read().dialog.is_some() {
        return dialog_key(state, data);
    }
    // The CSV table's in-place edit owns the keyboard: Enter commits and moves
    // down, Tab commits and moves right, Esc cancels, characters go into the
    // buffer (Excel semantics). It is more "modal" than the find bar — the cell
    // being edited must not have its keys stolen by find.
    if state.read().tab().csv.editing.is_some() {
        return csv_edit_key(state, data);
    }
    // CSV table view (not editing): arrows move the selection, Enter starts an
    // edit, Delete deletes a row/column/clears a cell. Placed before find: the
    // table mode does not compete with the find bar for keys.
    if state.read().tab().csv.enabled && state.read().tab().csv.table && csv_table_key(state, data)
    {
        return true;
    }
    if state.read().find.captured() {
        return find_key(state, data);
    }

    // WYSIWYG Markdown mode: plain keys (typing/Enter/arrows…) are rerouted to
    // the wys editor. Placed after the find bar (keys belong to it while its
    // box is captured) and before the JSON read-only preview (mutually exclusive).
    if state.read().tab().md_wys && super::mdws::wys_key(&mut state, data, ctrl, shift) {
        return true;
    }

    // Whole-document JSON tree view: read-only preview, replacing the text box
    // outright (modeled on the Firefox viewer). Keyboard handling for menus/
    // dialogs/find boxes already happened above; this only blocks editing keys
    // aimed at the document.
    if state.read().tab().json.enabled {
        return false;
    }

    match data.key() {
        Key::ArrowLeft => {
            let mut st = state.write();
            if ctrl {
                st.move_word_left(shift);
            } else {
                st.move_left(shift);
            }
            true
        }
        Key::ArrowRight => {
            let mut st = state.write();
            if ctrl {
                st.move_word_right(shift);
            } else {
                st.move_right(shift);
            }
            true
        }
        Key::ArrowUp => {
            state.write().move_up(shift);
            true
        }
        Key::ArrowDown => {
            state.write().move_down(shift);
            true
        }
        Key::PageUp => {
            state.write().page(-1, shift);
            true
        }
        Key::PageDown => {
            state.write().page(1, shift);
            true
        }
        Key::Home => {
            state.write().move_home(shift, ctrl);
            true
        }
        Key::End => {
            state.write().move_end(shift, ctrl);
            true
        }
        Key::Backspace => {
            state.write().delete_back();
            true
        }
        Key::Delete => {
            state.write().delete_fwd();
            true
        }
        Key::Enter => {
            state.write().insert_newline();
            true
        }
        Key::Tab => {
            state.write().insert_tab();
            true
        }
        Key::Escape => {
            let mut st = state.write();
            // Column selection exits first: it is a one-shot selection intent,
            // while search/find bar should only take over Esc when not stacked
            // on a selection.
            if st.col_anchor.take().is_some() {
                st.status = Some(Msg::StatusColumnSelEnded.into());
                return true;
            }
            // Background search in progress: Esc is first of all "cancel search".
            if st.cancel_search() {
                return true;
            }
            // Next: cancel background opens / line counting (large-file open can abort).
            if st.cancel_pending_opens() || st.cancel_indexing() {
                return true;
            }
            st.clear_selection();
            st.menu_close();
            st.drag = false;
            // The in-page find bar is a page surface, so Escape has to close it.
            if st.find.open {
                st.close_find();
            }
            st.menu.alt = false;
            true
        }
        // Plain character input: written straight into the document (including
        // Enter/Tab arriving as Character("\r"/"\t") — `type_text`/`insert_text`
        // handle them exactly like the old oninput path did).
        //
        // We used to rely on the textarea's default action typing into its own
        // value, then mirroring it into the document via `oninput` — that path
        // needed a two-phase "clear the textarea" reset, and dioxus clearing the
        // mirror signal in an effect and blitz applying `value=""` to its
        // internal editor are two async steps at different times; on fast
        // repeat typing the characters already accumulated in the DOM were
        // re-inserted wholesale (or lost wholesale) against the already-cleared
        // snapshot. winit's `Key::Character` comes straight from WM_CHAR
        // (winit-win32/src/keyboard.rs), byte-identical to what the default
        // action would insert, so consuming it in keydown is equivalent and
        // race-free. IME commits do not pass through here (the WM_CHARs during
        // composition are explicitly ignored by winit); see the
        // `WindowEvent::Ime` branch in editor.rs.
        // `!ctrl` guard: unbound Ctrl+characters (no-ops of the default action
        // in the old path) must not be written into the document as typing;
        // they fall through to `_ => false`.
        Key::Character(s) if !ctrl => {
            state.write().type_text(&s);
            true
        }
        // `Ctrl` + a letter that is not bound must not be swallowed: letting it
        // through would let the browser/OS handle it, which is how a stray
        // Ctrl+H would otherwise close the window.
        _ => false,
    }
}

/// Keyboard navigation for the in-page context menu (same set of keys as
/// [`menu_key`]: arrows move, Enter activates, Esc closes, Home/End jump to the
/// ends, PageUp/PageDown page). Returns `true` when consumed.
fn ctx_menu_key(state: &mut Signal<EditorState>, data: &KeyboardData) -> bool {
    match data.key() {
        Key::Escape => {
            state.write().close_ctx_menu();
            true
        }
        Key::ArrowDown => {
            state.write().ctx_menu_move(1);
            true
        }
        Key::ArrowUp => {
            state.write().ctx_menu_move(-1);
            true
        }
        Key::Enter => {
            let mut st = state.write();
            let cursor = st.ctx.as_ref().map(|m| m.cursor).unwrap_or(0);
            st.ctx_menu_activate(cursor);
            true
        }
        Key::Home => {
            state.write().ctx_menu_home();
            true
        }
        Key::End => {
            state.write().ctx_menu_end();
            true
        }
        Key::PageDown | Key::PageUp => {
            let mut st = state.write();
            let step = crate::ui::components::menu::visible_rows(st.win.h).max(2) as i32 - 1;
            st.ctx_menu_move(if data.key() == Key::PageDown {
                step
            } else {
                -step
            });
            true
        }
        _ => false,
    }
}

/// Keyboard navigation for an open dropdown. Returns `true` when consumed.
fn menu_key(state: &mut Signal<EditorState>, data: &KeyboardData) -> bool {
    let sub_open = state.read().menu.sub.is_some();
    match data.key() {
        Key::Escape => {
            state.write().menu_close();
            true
        }
        Key::ArrowDown => {
            let mut st = state.write();
            if sub_open {
                st.menu_move_sub(1);
            } else {
                st.menu_move(1);
            }
            true
        }
        Key::ArrowUp => {
            let mut st = state.write();
            if sub_open {
                st.menu_move_sub(-1);
            } else {
                st.menu_move(-1);
            }
            true
        }
        Key::ArrowRight => {
            // open the submenu under the cursor, like every native menu
            let mut st = state.write();
            let rows = shortcuts_rows(&st);
            if rows.get(st.menu.cursor).is_some_and(|r| r.header) {
                st.menu_activate();
                true
            } else {
                false
            }
        }
        Key::ArrowLeft => {
            let mut st = state.write();
            if st.menu.sub.take().is_some() {
                st.menu.hover_sub = None;
                true
            } else {
                false
            }
        }
        Key::Enter => state.write().menu_activate(),
        // A tall menu (Language) is scrolled by the keyboard too: PageUp/PageDown
        // jump a panel-full, Home/End run to the ends — the same keys every
        // native menu honours.
        Key::PageDown | Key::PageUp => {
            let mut st = state.write();
            let step = crate::ui::components::menu::visible_rows(st.win.h).max(2) as i32 - 1;
            let step = if data.key() == Key::PageDown {
                step
            } else {
                -step
            };
            if sub_open {
                st.menu_move_sub(step);
            } else {
                st.menu_move(step);
            }
            true
        }
        Key::Home => {
            let mut st = state.write();
            if sub_open {
                st.menu.hover_sub = None;
                st.menu_move_sub(1);
            } else {
                st.menu_cursor_first();
            }
            true
        }
        Key::End => {
            let mut st = state.write();
            if sub_open {
                st.menu.hover_sub = None;
                st.menu_move_sub(-1);
            } else {
                st.menu_cursor_last();
            }
            true
        }
        _ => false,
    }
}

/// Rows of the open dropdown (used by the keyboard path).
fn shortcuts_rows(st: &EditorState) -> Vec<crate::ui::actions::MenuRow> {
    crate::ui::actions::menu_rows(st)
}

/// Chords Vim must never swallow, because they are the user's way out of it
/// and into the parts of the editor Vim does not model (saving, find bar).
fn is_reserved(chord: &Chord) -> bool {
    matches!(
        chord.key,
        K::Char('s') | K::Char('f') | K::Char('h') | K::Char('g') | K::Char('o') | K::Char('n')
    ) && chord.mods.ctrl
}

/// Feed one key to the Vim engine and apply whatever it decided.
///
/// Returns `true` when Vim consumed the key.
fn vim_key(state: &mut Signal<EditorState>, data: &KeyboardData) -> bool {
    let mods = data.modifiers();
    // a Vim key never carries Ctrl/Alt (those are reserved above)
    let ev = match data.key() {
        Key::Character(s) => match s.chars().next() {
            Some(_) if mods.ctrl() || mods.alt() => return false,
            _ => s.chars().next().map(vim::Ev::Char),
        },
        Key::Escape => Some(vim::Ev::Key(K::Escape)),
        Key::Enter => Some(vim::Ev::Key(K::Enter)),
        Key::Backspace => Some(vim::Ev::Key(K::Backspace)),
        Key::Delete => Some(vim::Ev::Key(K::Delete)),
        Key::Tab => Some(vim::Ev::Key(K::Tab)),
        Key::ArrowLeft => Some(vim::Ev::Key(K::Left)),
        Key::ArrowRight => Some(vim::Ev::Key(K::Right)),
        Key::ArrowUp => Some(vim::Ev::Key(K::Up)),
        Key::ArrowDown => Some(vim::Ev::Key(K::Down)),
        Key::Home => Some(vim::Ev::Key(K::Home)),
        Key::End => Some(vim::Ev::Key(K::End)),
        Key::PageUp => Some(vim::Ev::Key(K::PageUp)),
        Key::PageDown => Some(vim::Ev::Key(K::PageDown)),
        _ => None,
    };
    let Some(ev) = ev else { return false };

    // Multi-character commits (IME, some layouts) must not drop all but the
    // first char: in Insert mode insert the whole string, in the other modes
    // just consume it.
    if let Key::Character(s) = data.key() {
        if s.chars().count() > 1 {
            let mut st = state.write();
            if st.vim.enabled && st.vim.mode == vim::Mode::Insert {
                st.apply_vim_plan(&vim::Plan::one(vim::Op::Insert(s.clone())));
            }
            return st.vim.enabled;
        }
    }

    // the engine mutates its own pending state, so it needs a write guard
    let plan = {
        let mut st = state.write();
        let buf = st.vim_buffer();
        let caret = st.caret().pos;
        match st.vim.handle_key(ev, caret, &buf) {
            Some(p) => p,
            None => return false,
        }
    };
    let mut st = state.write();
    // an operator waiting for its motion produces no ops, but the key *is*
    // consumed so the next motion completes it
    st.apply_vim_plan(&plan);
    st.status = Some(vim_status(&st.vim, st.vim.mode));
    true
}

/// Status-bar text describing the modal state, like Vim's own mode indicator.
fn vim_status(vim: &vim::Vim, mode: vim::Mode) -> Message {
    if mode == vim::Mode::CommandLine {
        return Fmt::VimStCmdline {
            text: vim.cmdline_text().to_string(),
        }
        .into();
    }
    if vim.awaiting_motion() {
        return match mode {
            vim::Mode::Visual => Fmt::VimStPendingVisual {
                text: vim.pending_text().to_string(),
            }
            .into(),
            _ => Fmt::VimStPendingNormal {
                text: vim.pending_text().to_string(),
            }
            .into(),
        };
    }
    if vim.register_is_linewise() && !vim.register_text().is_empty() {
        let bytes = vim.register_text().len();
        return match mode {
            vim::Mode::Visual => Fmt::VimStRegisterVisual { bytes }.into(),
            _ => Fmt::VimStRegisterNormal { bytes }.into(),
        };
    }
    Message::Plain(mode.msg())
}

#[component]
pub fn Composer() -> Element {
    let mut state = use_context::<Signal<EditorState>>();
    // Bumped when the textarea loses DOM focus. It is the node's `key`, so
    // bumping it **replaces** the textarea: the fresh node carries `autofocus`
    // and blitz's own autofocus pass (see `blitz-dom/src/mutator.rs`) hands it
    // the focus on the next flush. That is the only way back — `MountedData`
    // `set_focus` panics whenever the document is already borrowed, which it
    // always is inside an event handler or a dioxus task.
    let mut focus_epoch = use_signal(|| 0u32);
    // winit window handle (injected by shell.rs): used to update the IME
    // candidate window anchor.
    let window = use_context::<Arc<dyn Window>>();

    let (x, y, h, w, fs) = {
        let st = state.read();
        match field_composer_rect(&st) {
            Some((x, y, h, w)) => (x, y, h, w, 12.0),
            None => {
                let (x, y, lh) = st.caret_xy();
                (x, y, lh, 320.0, st.font_size)
            }
        }
    };
    let comp_top = y as i32;
    let comp_left = x as i32;
    let comp_h = h as i32;
    let comp_w = w as i32;
    let comp_fs = fs as i32;

    // IME candidate window anchor: pull the candidate window back to the caret
    // whenever the composer rectangle moves (window-level IME is always on in
    // shell.rs). This runs on every render frame; a Cell comparing the last
    // sent rectangle throttles it — winit is only called across FFI when it
    // actually changed. During CSV table editing the anchor belongs to the
    // csvtable edit box (see that component); we skip here but record the
    // state bit: the moment editing ends the tagged tuple necessarily changes,
    // and the anchor automatically returns to the composer.
    let ime_anchor: std::cell::Cell<(i32, i32, i32, i32, bool)> =
        use_hook(|| std::cell::Cell::new((i32::MIN, i32::MIN, i32::MIN, i32::MIN, true)));
    {
        let csv_active = state.read().tab().csv.editing.is_some();
        // wys mode: the candidate window anchor follows the wys caret (parses
        // the shared caret CSS; view.y adds the wys-root top offset). Known
        // residual: the anchor is briefly off during scrolling inside wys-root
        // (see the mdws::ime_anchor comment); it self-heals as soon as the
        // caret moves. In all other modes anchor to the composer textarea's
        // own rectangle.
        let wys_anchored = state.read().tab().md_wys;
        let (ax, ay, aw, ah) = if wys_anchored {
            match super::mdws::ime_anchor() {
                Some((cx, cy, ch)) => (
                    cx as i32,
                    (state.read().view.y as f32 + cy) as i32,
                    2,
                    ch.max(1.0) as i32,
                ),
                // Caret not ready yet: fall back to the composer rectangle so
                // the candidate window is at least visible.
                None => (comp_left, comp_top, comp_w, comp_h),
            }
        } else {
            (comp_left, comp_top, comp_w, comp_h)
        };
        let tag = (ax, ay, aw, ah, csv_active);
        if ime_anchor.get() != tag {
            ime_anchor.set(tag);
            if !csv_active {
                // Update rather than the deprecated set_ime_cursor_area; the
                // cursor_area capability was granted when the window was
                // enabled (shell.rs), and only the win32 backend accepts this
                // anchor.
                let _ = window.request_ime_update(winit::window::ImeRequest::Update(
                    winit::window::ImeRequestData::default().with_cursor_area(
                        winit::dpi::LogicalPosition::new(ax as f64, ay as f64).into(),
                        winit::dpi::LogicalSize::new(aw as f64, ah as f64).into(),
                    ),
                ));
            }
        }
    }

    // Note: `MountedData::set_focus` must never be called from a dioxus task or
    // event handler — dioxus wakes tasks inside `render_immediate`, where blitz
    // holds a mutable borrow of the document (`RefCell already borrowed` panic
    // that aborts the whole render). Focus is acquired once through blitz's
    // `autofocus` pass on this node and never moved afterwards.
    //
    // Whenever the viewport is **replaced by another view** (markdown preview,
    // CSV table), the textarea must be **mouse-transparent**
    // (pointer-events:none): it floats on top at z-index:200, and its rectangle
    // follows the caret in **editor** coordinates — the replacement view's
    // geometry does not coincide, and the textarea would sit exactly over its
    // content, eating the "click the rendered content" hit (in practice the
    // second click always missed). DOM focus and the IME anchor do not depend on
    // hit testing: keyboard/candidate window keep working; only the mouse is
    // ceded to the replacement view.
    //
    // The CSV table was missing here for a long time: the 320px-wide anchor
    // covered the top-left of the table (the caret sits on line 1), so the
    // header row — and with it the whole sort binding — never received a
    // pointerdown, and cells under the anchor could not be clicked either.
    let mouse_passthrough = {
        let st = state.read();
        let t = st.tab();
        t.md_wys || (t.csv.enabled && t.csv.table)
    };
    let pe_style = if mouse_passthrough {
        "pointer-events:none; "
    } else {
        ""
    };
    let style = format!(
        "position:absolute; left:{comp_left}px; top:{comp_top}px; width:{comp_w}px; \
         height:{comp_h}px; background:transparent; color:transparent; caret-color:transparent;
         border:none; outline:none; padding:0; margin:0; resize:none; {pe_style}
         font-family:Consolas,'Cascadia Mono',monospace; font-size:{comp_fs}px;
         line-height:{comp_h}px; white-space:pre; overflow:hidden; z-index:200; touch-action:none;"
    );
    // subscribing here is what makes a focus loss replace the node
    let epoch = focus_epoch();

    rsx! {
        textarea {
            key: "{epoch}",
            style: "{style}",
            // The value stays empty forever — this node only anchors DOM focus
            // and the IME candidate window. All text goes through `onkeydown`
            // (see `edit_key`) or `WindowEvent::Ime` (see `editor.rs`). IME
            // commits are also inserted into this node's internal editor by
            // blitz itself (unavoidable, invisible, and no longer read back);
            // the accumulation is bounded by IME usage and harmless.
            value: "",
            autocorrect: "off",
            autocomplete: "off",
            spellcheck: "false",
            rows: "1",
            autofocus: "true",
            onfocus: move |_| {},
            onblur: move |_| {
                let next = *focus_epoch.peek() + 1;
                focus_epoch.set(next);
            },
            onkeydown: move |evt| {
                if handle_key(state, &evt.data) {
                    evt.prevent_default();
                }
            },
            onpointerdown: move |evt| {
                // only the document owns mouse hit testing; a click inside a
                // find box is handled by the box itself
                if state.read().find.captured() {
                    return;
                }
                // a right press belongs to the context menu, not to the caret
                if !is_primary_press(&*evt.data) {
                    return;
                }
                // A finger press only **records** (`ui::touch`): it may turn
                // out to be a pan-scroll of the document; the caret is placed
                // on release only if nothing slid (window-level
                // PointerButton). Pen and mouse take the original path —
                // precise input places the caret on press.
                let finger = evt.data.pointer_type() == "touch";
                let pid = evt.data.pointer_id();
                let cp = evt.data.client_coordinates();
                if finger {
                    crate::ui::touch::begin(crate::ui::touch::PanTarget::Doc, pid, cp.x, cp.y);
                    return;
                }
                let shift = evt.data.modifiers().shift();
                let hit = state.read().hit_test(cp.x, cp.y);
                // Ctrl+click on a markdown link opens it outside the app and
                // consumes the click; off a link (or off the whitelist) the
                // click keeps its caret-placement semantics.
                if evt.data.modifiers().ctrl() {
                    if let Some(p) = hit {
                        if crate::ui::commands::markdown::open_link_at(&mut state.write(), p) {
                            return;
                        }
                    }
                }
                if let Some(p) = hit {
                    let mut st = state.write();
                    st.drag = true;
                    st.menu.open = None;
                    // Clicking the document also ends the Alt session (minimal
                    // equivalent of menu_close), otherwise bare Alt → click →
                    // typing would let the first letter trigger a mnemonic.
                    st.menu.alt = false;
                    // Same path as viewport: single/double/triple click semantics agree.
                    st.doc_focused = true;
                    st.editor_click(p, cp.x as f32, cp.y as f32, shift);
                }
            },
            onpointerup: move |_evt| {
                // Conditional write: releasing without a drag does not mark
                // dirty (write() marks dirty unconditionally).
                crate::ui::state::update_if(&mut state, |st| st.drag, |st| st.drag = false);
            },
            onwheel: move |evt| {
                // This textarea sits over the caret (320×20, z-index 200):
                // when the mouse happens to rest on the caret's line, the
                // wheel's target is this box. Without forwarding to the
                // document here you get "click into text, scroll, nothing
                // happens". Same entry point as the viewport (including Ctrl
                // zoom).
                let mods = evt.data.modifiers();
                super::viewport::wheel_scroll(state, evt.data.delta(), mods.ctrl(), mods.shift());
            },
        }
    }
}
