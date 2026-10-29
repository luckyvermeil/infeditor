//! Menu-bar state machine: open/close, Alt access keys, keyboard
//! navigation and activation. The rows themselves come from
//! [`crate::ui::actions`].
//!
//! The panel geometry constants and pure functions also live here: keyboard
//! navigation (the state layer) and rendering (components::menu, re-exported)
//! share the same numbers, so the state layer no longer depends back on
//! components.

use super::EditorState;

/// Height of one dropdown row (px). Fixed so the fly-out's y can be computed
/// from a row index without measuring.
pub const ROW_H: f32 = 26.0;

/// Tallest a dropdown may be: the space between the caption and the window
/// bottom, and never fewer than three rows (a two-row panel is unusable).
pub fn panel_avail_h(win_h: f32) -> f32 {
    (win_h - crate::ui::state::TITLEBAR_H - 6.0).max(ROW_H * 3.0)
}

/// How many whole rows fit in a panel — the distance <kbd>PageUp</kbd> /
/// <kbd>PageDown</kbd> move the keyboard cursor by.
pub fn visible_rows(win_h: f32) -> usize {
    (panel_avail_h(win_h) / ROW_H).floor().max(1.0) as usize
}

/// Height the panel is actually drawn at: its natural height, clamped.
pub fn panel_height(rows: usize, win_h: f32) -> f32 {
    (rows as f32 * ROW_H).min(panel_avail_h(win_h))
}

/// Largest valid scroll offset for a panel with `rows` rows (0 when it fits).
pub fn max_scroll(rows: usize, win_h: f32) -> f32 {
    ((rows as f32 * ROW_H) - panel_height(rows, win_h)).max(0.0)
}

impl EditorState {
    /// Close every menu surface and forget the keyboard cursor.
    ///
    /// Also drops the access-key underline: picking a caption is how a
    /// <kbd>Alt</kbd>-opened menu ends, and Windows hides the mnemonics again at
    /// that point.
    pub fn menu_close(&mut self) {
        self.menu.open = None;
        self.menu.sub = None;
        self.menu.hover = None;
        self.menu.hover_sub = None;
        self.menu.cursor = 0;
        self.menu.alt = false;
        self.menu.more_open = false;
        self.menu.reset_scroll();
        // The in-page context menu lives and dies with the dropdown: every
        // path that closes the dropdown closes it too (Escape, clicking
        // elsewhere, opening another menu...). The context menu's own close
        // paths (backdrop click / row click) go through close_ctx_menu only,
        // never here.
        self.close_ctx_menu();
    }

    /// Overflow "more" button: toggles the vertical list of hidden captions.
    pub fn menu_toggle_more(&mut self) {
        self.menu.more_open = !self.menu.more_open;
        if self.menu.more_open {
            // Mutually exclusive with the normal dropdown: opening the more list collapses the real menu
            self.menu.open = None;
            self.menu.sub = None;
            self.menu.hover = None;
            self.menu.hover_sub = None;
        }
    }

    /// Open `label` from <kbd>Alt</kbd>+letter (rather than a click).
    ///
    /// Unlike [`Self::menu_toggle`] this never *closes* a menu: <kbd>Alt</kbd>+F
    /// while 文件 is already open re-opens it on the first row instead of
    /// dismissing it — the mnemonic always opens.
    pub fn menu_open_from_alt(&mut self, cap: super::super::actions::Caption, anchor_x: f32) {
        self.menu.open = Some(cap);
        self.menu.anchor_x = anchor_x;
        self.menu.sub = None;
        self.menu.hover = None;
        self.menu.hover_sub = None;
        self.menu.cursor = 0;
        self.menu.alt = true;
        self.menu.reset_scroll();
        self.doc_focused = false;
    }

    /// Handle a key while the menu bar owns the keyboard.
    ///
    /// Returns `true` when the key was consumed. Two distinct things happen
    /// here, matching Windows:
    ///
    /// * a bare <kbd>Alt</kbd> only *reveals* the access keys (`menu_alt`), it
    ///   does not open anything;
    /// * <kbd>Alt</kbd>+letter opens the matching caption, or — when a menu is
    ///   already open — runs the item in it whose label carries that letter.
    pub fn menu_alt_key(&mut self, ch: Option<char>, escape: bool) -> bool {
        if escape {
            // Escape backs out one level: close the fly-out, else the dropdown,
            // else just drop the access keys.
            if self.menu.sub.take().is_some() {
                self.menu.hover_sub = None;
                self.menu.sub_scroll = 0.0;
            } else if self.menu.open.take().is_some() {
                self.menu.hover = None;
                self.menu.cursor = 0;
                self.menu.reset_scroll();
            } else {
                self.menu.alt = false;
            }
            self.doc_focused = self.menu.open.is_none();
            return true;
        }
        let Some(ch) = ch else {
            // A bare Alt *only* reveals the access keys. It must not open
            // anything, or alt-tabbing into the editor would leave a dropdown
            // behind.
            self.menu.alt = true;
            return true;
        };
        self.menu.alt = true;
        // A menu is already open: the letter picks an item inside it.
        if self.menu.open.is_some() {
            if self.menu.sub.is_some() {
                return self.menu_pick_in_sub(ch);
            }
            return self.menu_pick_in_main(ch);
        }
        crate::ui::actions::open_caption_by_accel(self, ch)
    }

    /// Does any row of `rows` start with / contain `ch`?
    ///
    /// Windows matches the *first* item whose label contains the character, and
    /// separators never match. The check is ASCII-only because the access keys
    /// are ASCII even when the labels are Chinese (`另存为(&S)...`).
    fn row_matches(row: &crate::ui::actions::MenuRow, ch: char) -> bool {
        if row.sep {
            return false;
        }
        row.label.chars().any(|c| c.eq_ignore_ascii_case(&ch))
    }

    fn menu_pick_in_main(&mut self, ch: char) -> bool {
        let rows = crate::ui::actions::menu_rows(self);
        let Some(idx) = rows.iter().position(|r| Self::row_matches(r, ch)) else {
            return false;
        };
        self.menu.cursor = idx;
        self.scroll_cursor_into_panel(rows.len(), false);
        if rows[idx].header {
            // a submenu caption: fly it out, exactly like pressing the arrow key
            self.menu.sub = Some(rows[idx].label.clone());
            self.menu.hover_sub = None;
            self.menu.sub_scroll = 0.0;
            return true;
        }
        self.menu_activate()
    }

    fn menu_pick_in_sub(&mut self, ch: char) -> bool {
        let Some(top) = self.menu.open else {
            return false;
        };
        let rows = crate::ui::actions::submenu_rows(self, top);
        let Some(idx) = rows.iter().position(|r| Self::row_matches(r, ch)) else {
            return false;
        };
        self.menu_run_sub_row(idx);
        true
    }

    /// Open (or toggle) a caption. `anchor_x` is the caption's left edge in
    /// client pixels, measured from the click's element offset.
    ///
    /// Picking a caption ends the <kbd>Alt</kbd> session the way Windows does:
    /// the access keys stop being underlined.
    pub fn menu_toggle(&mut self, cap: super::super::actions::Caption, anchor_x: f32) {
        crate::ui::shell::debug_log(&format!("menu_toggle {cap:?} @ {anchor_x}"));
        self.menu.more_open = false;
        if self.menu.open == Some(cap) {
            self.menu_close();
            return;
        }
        self.menu.open = Some(cap);
        self.menu.anchor_x = anchor_x;
        self.menu.sub = None;
        self.menu.hover = None;
        self.menu.hover_sub = None;
        self.menu.cursor = 0;
        self.menu.alt = false;
        self.menu.reset_scroll();
    }

    /// Switch to another caption while one is open (Windows hover-swap).
    pub fn menu_focus_caption(&mut self, cap: super::super::actions::Caption, anchor_x: f32) {
        if self.menu.open.is_some() && self.menu.open != Some(cap) {
            self.menu.open = Some(cap);
            self.menu.anchor_x = anchor_x;
            self.menu.sub = None;
            self.menu.hover = None;
            self.menu.hover_sub = None;
            self.menu.cursor = 0;
            self.menu.reset_scroll();
        }
    }

    /// Move the keyboard cursor by `delta` rows, skipping separators and
    /// wrapping around — the behaviour every native menu has.
    pub fn menu_move(&mut self, delta: i32) {
        let rows = crate::ui::actions::menu_rows(self);
        if rows.is_empty() {
            return;
        }
        let n = rows.len() as i32;
        let mut i = (self.menu.cursor as i32).clamp(0, n - 1);
        for _ in 0..n {
            i = (i + delta).rem_euclid(n);
            if !rows[i as usize].sep {
                break;
            }
        }
        self.menu.cursor = i as usize;
        self.menu.hover = None;
        self.scroll_cursor_into_panel(rows.len(), false);
    }

    /// Clamp the panel's scroll so `cursor` is inside the visible slice.
    ///
    /// Factored out because every cursor move (arrows, mnemonic picks, the
    /// pointer) has to do it, and forgetting it in one path is how a keyboard
    /// user ends up scrolling blind.
    fn scroll_cursor_into_panel(&mut self, rows: usize, flyout: bool) {
        let cursor = if flyout {
            self.menu.hover_sub.unwrap_or(0)
        } else {
            self.menu.cursor
        };
        let win_h = self.win.h;
        self.menu.ensure_cursor_visible(rows, cursor, win_h, flyout);
    }

    /// Jump the keyboard cursor to the first row (`Home`).
    pub fn menu_cursor_first(&mut self) {
        let rows = crate::ui::actions::menu_rows(self);
        if rows.is_empty() {
            return;
        }
        self.menu.cursor = 0;
        if rows[0].sep {
            self.menu_move(1);
        } else {
            self.scroll_cursor_into_panel(rows.len(), false);
        }
    }

    /// Jump the keyboard cursor to the last row (`End`).
    pub fn menu_cursor_last(&mut self) {
        let rows = crate::ui::actions::menu_rows(self);
        if rows.is_empty() {
            return;
        }
        self.menu.cursor = rows.len() - 1;
        if rows[self.menu.cursor].sep {
            self.menu_move(-1);
        } else {
            self.scroll_cursor_into_panel(rows.len(), false);
        }
    }

    /// Activate the highlighted row. Returns `true` when something ran.
    pub fn menu_activate(&mut self) -> bool {
        let rows = crate::ui::actions::menu_rows(self);
        let Some(row) = rows.get(self.menu.cursor).cloned() else {
            return false;
        };
        if row.sep {
            return false;
        }
        if row.header {
            // activating a caption flies its submenu out / folds it back
            let same = self.menu.sub.as_deref() == Some(row.label.as_str());
            self.menu.sub = if same { None } else { Some(row.label.clone()) };
            self.menu.hover_sub = None;
            self.menu.sub_scroll = 0.0;
            self.menu.cursor = 0;
            return true;
        }
        if let Some(cmd) = row.cmd {
            self.menu_close();
            crate::ui::actions::run(self, cmd);
            return true;
        }
        false
    }

    /// Run a row by index (pointer activation).
    pub fn menu_run_row(&mut self, idx: usize) {
        self.menu.cursor = idx;
        self.menu_activate();
    }

    /// Run a row of the fly-out by index.
    pub fn menu_run_sub_row(&mut self, idx: usize) {
        let Some(top) = self.menu.open else {
            return;
        };
        let rows = crate::ui::actions::submenu_rows(self, top);
        let Some(row) = rows.get(idx) else { return };
        if row.sep || row.header {
            return;
        }
        if let Some(cmd) = row.cmd {
            self.menu_close();
            crate::ui::actions::run(self, cmd);
        }
    }

    /// Move the keyboard cursor inside the fly-out.
    pub fn menu_move_sub(&mut self, delta: i32) {
        let Some(top) = self.menu.open else {
            return;
        };
        let rows = crate::ui::actions::submenu_rows(self, top);
        if rows.is_empty() {
            return;
        }
        let n = rows.len() as i32;
        let cur = self.menu.hover_sub.unwrap_or(0) as i32;
        let mut i = cur.clamp(0, n - 1);
        for _ in 0..n {
            i = (i + delta).rem_euclid(n);
            if !rows[i as usize].sep {
                break;
            }
        }
        self.menu.hover_sub = Some(i as usize);
        self.scroll_cursor_into_panel(rows.len(), true);
    }

    // ---- In-page context menu (non-Windows TrackPopupMenu fallback) ----------------

    /// Open the in-page context menu at `(x, y)` (CSS px, window-relative).
    ///
    /// `items` is flattened **at open time**: submenus become a non-clickable
    /// section header followed by their (indented) children — an overlay
    /// panel has no room for a second fly-out hierarchy, and every command
    /// stays reachable. Check marks are snapshotted from the current state,
    /// exactly like the native menu's `MF_CHECKED` at track time.
    pub fn open_ctx_menu(&mut self, x: f32, y: f32, items: &[crate::ui::actions::Item<'_>]) {
        // Mutually exclusive with the menu-bar dropdown: collapse the dropdown
        // first (it also collapses any old context menu in passing).
        self.menu_close();
        let mut rows: Vec<CtxRow> = Vec::with_capacity(items.len() * 2);
        self.flatten_ctx(items, &mut rows, 0);
        // The keyboard cursor starts at the first clickable row (separators/section headers are not clickable)
        let cursor = rows.iter().position(|r| r.cmd.is_some()).unwrap_or(0);
        self.ctx = Some(CtxMenu {
            x,
            y,
            rows,
            scroll: 0.0,
            cursor,
        });
        self.doc_focused = false;
    }

    /// Move the keyboard cursor inside the in-page context menu, skipping
    /// separators and section headers (same conventions as the menu-bar
    /// dropdown's `menu_move`: wrap around + scroll the cursor row into
    /// view).
    pub fn ctx_menu_move(&mut self, delta: i32) {
        let Some(m) = self.ctx.as_mut() else {
            return;
        };
        let n = m.rows.len() as i32;
        if n == 0 {
            return;
        }
        let mut i = (m.cursor as i32).clamp(0, n - 1);
        for _ in 0..n {
            i = (i + delta).rem_euclid(n);
            if m.rows[i as usize].cmd.is_some() {
                break;
            }
        }
        m.cursor = i as usize;
        self.ctx_scroll_cursor_into_view();
    }

    /// Home / End: the first / last clickable row.
    pub fn ctx_menu_home(&mut self) {
        if let Some(m) = self.ctx.as_mut() {
            if let Some(i) = m.rows.iter().position(|r| r.cmd.is_some()) {
                m.cursor = i;
                self.ctx_scroll_cursor_into_view();
            }
        }
    }

    pub fn ctx_menu_end(&mut self) {
        if let Some(m) = self.ctx.as_mut() {
            if let Some(i) = m.rows.iter().rposition(|r| r.cmd.is_some()) {
                m.cursor = i;
                self.ctx_scroll_cursor_into_view();
            }
        }
    }

    /// Scroll the cursor row into the panel's visible area after keyboard
    /// movement (panel height = the clamped [`panel_height`], visible rows =
    /// [`visible_rows`]).
    fn ctx_scroll_cursor_into_view(&mut self) {
        let Some(m) = self.ctx.as_mut() else {
            return;
        };
        let max_s = max_scroll(m.rows.len(), self.win.h);
        let visible = (visible_rows(self.win.h).max(1)) as f32;
        let top = m.cursor as f32 * ROW_H;
        let bottom = top + ROW_H;
        if top < m.scroll {
            m.scroll = top;
        } else if bottom > m.scroll + visible * ROW_H {
            m.scroll = bottom - visible * ROW_H;
        }
        m.scroll = m.scroll.clamp(0.0, max_s);
    }

    /// Close the in-page context menu (no-op when it is not open).
    pub fn close_ctx_menu(&mut self) {
        if self.ctx.take().is_some() {
            self.doc_focused = true;
        }
    }

    /// Activate row `i` of the in-page context menu: run its command and
    /// close. Separators and section headers have no command and are ignored.
    ///
    /// Returns whether a command ran (the overlay tests use this to keep the
    /// "click outside = close, click row = run + close" split honest).
    pub fn ctx_menu_activate(&mut self, i: usize) -> bool {
        let Some(m) = &self.ctx else {
            return false;
        };
        let Some(cmd) = m.rows.get(i).and_then(|r| r.cmd) else {
            return false;
        };
        self.close_ctx_menu();
        crate::ui::actions::run(self, cmd);
        true
    }

    /// Flatten an [`Item`](crate::ui::actions::Item) tree into overlay rows.
    fn flatten_ctx(
        &self,
        items: &[crate::ui::actions::Item<'_>],
        out: &mut Vec<CtxRow>,
        depth: u8,
    ) {
        for it in items {
            match it {
                crate::ui::actions::Item::Sep => out.push(CtxRow {
                    label: String::new(),
                    cmd: None,
                    header: false,
                    sep: true,
                    checkable: false,
                    checked: false,
                    depth,
                }),
                crate::ui::actions::Item::Cmd { label, cmd, .. } => out.push(CtxRow {
                    label: label.to_string(),
                    cmd: Some(*cmd),
                    header: false,
                    sep: false,
                    checkable: crate::ui::actions::is_toggle(*cmd),
                    checked: crate::ui::actions::is_checked(self, *cmd),
                    depth,
                }),
                crate::ui::actions::Item::Sub { label, items } => {
                    out.push(CtxRow {
                        label: label.to_string(),
                        cmd: None,
                        header: true,
                        sep: false,
                        checkable: false,
                        checked: false,
                        depth,
                    });
                    self.flatten_ctx(items, out, depth + 1);
                }
            }
        }
    }
}

/// One flattened row of the in-page context menu (see [`CtxMenu`]).
#[derive(Clone, Debug, PartialEq)]
pub struct CtxRow {
    /// Row label.
    pub label: String,
    /// Command to run on click; `None` = inert (section header).
    pub cmd: Option<crate::ui::actions::Cmd>,
    /// Section header (a flattened submenu title): muted, never clickable.
    pub header: bool,
    /// Separator line (`label` / `cmd` are meaningless).
    pub sep: bool,
    /// This row's command is a toggle (decides the check column, same rule as
    /// the menu bar: by "has a checkbox", never by "is checked right now").
    pub checkable: bool,
    /// Check state snapshot taken when the menu opened.
    pub checked: bool,
    /// Indent level: 0 = top level, 1 = child of a flattened submenu.
    pub depth: u8,
}

/// Open state of the in-page context menu (non-Windows fallback for the
/// native `TrackPopupMenu`, rendered by
/// [`crate::ui::components::menu::CtxMenuOverlay`]).
#[derive(Clone, Debug, PartialEq)]
pub struct CtxMenu {
    /// Anchor (CSS px, window-relative): where the right click / long press
    /// landed. The overlay clamps the panel back into the window.
    pub x: f32,
    pub y: f32,
    /// Flattened rows.
    pub rows: Vec<CtxRow>,
    /// Wheel scroll offset (the panel clamps its height to the window; the
    /// editor context menu is taller than short windows).
    pub scroll: f32,
    /// Keyboard cursor (clickable rows; separators/section headers are skipped).
    pub cursor: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A bare <kbd>Alt</kbd> only *reveals* the access keys — it must not open a
    /// menu. Opening one on a bare Alt would make every alt-tab into the app
    /// leave a dropdown behind.
    #[test]
    fn a_bare_alt_reveals_the_keys_without_opening_a_menu() {
        let mut st = EditorState::minimal();
        assert!(st.menu_alt_key(None, false));
        assert!(st.menu.alt, "the access keys must be revealed");
        assert!(st.menu.open.is_none(), "no menu may open on a bare Alt");
    }
    /// <kbd>Alt</kbd>+letter opens the caption and keeps the keys visible while
    /// it is open — Windows underlines them for as long as the menu is up.
    #[test]
    fn alt_letter_opens_a_caption_and_keeps_the_keys_visible() {
        let mut st = EditorState::minimal();
        assert!(st.menu_alt_key(Some('f'), false));
        assert_eq!(st.menu.open, Some(crate::ui::actions::Caption::File));
        assert!(st.menu.alt);
        // Escape backs out one level: the dropdown closes, the keys stay.
        assert!(st.menu_alt_key(None, true));
        assert!(st.menu.open.is_none());
        assert!(st.menu.alt, "one Escape must not also drop the mnemonics");
        // A second Escape drops them.
        assert!(st.menu_alt_key(None, true));
        assert!(!st.menu.alt);
    }
    /// Picking a caption ends the <kbd>Alt</kbd> session, so the mnemonics stop
    /// being underlined even though a menu is now open.
    #[test]
    fn clicking_a_caption_ends_the_alt_session() {
        let mut st = EditorState::minimal();
        st.menu_alt_key(Some('v'), false);
        assert!(st.menu.alt);
        st.menu_close();
        assert!(!st.menu.alt, "closing a menu must hide the mnemonics");
    }
    /// An unknown <kbd>Alt</kbd> letter consumes nothing and changes nothing, so
    /// it can still fall through to whatever it meant before.
    #[test]
    fn an_unknown_alt_letter_is_not_consumed() {
        let mut st = EditorState::minimal();
        assert!(!st.menu_alt_key(Some('z'), false));
        assert!(st.menu.open.is_none());
    }

    /// A keyboard cursor that walks off the bottom of a scrolled panel drags
    /// the scroll with it; back at the top the scroll returns to zero.
    #[test]
    fn keyboard_navigation_scrolls_a_tall_menu_into_view() {
        use super::max_scroll;
        let mut st = EditorState::minimal();
        st.win.h = 300.0;
        let rows = 200usize;

        st.menu.cursor = rows - 1;
        st.menu
            .ensure_cursor_visible(rows, st.menu.cursor, st.win.h, false);
        assert_eq!(st.menu.scroll, max_scroll(rows, st.win.h));

        st.menu.cursor = 0;
        st.menu.ensure_cursor_visible(rows, 0, st.win.h, false);
        assert_eq!(st.menu.scroll, 0.0);
    }

    /// A menu that fits on screen never scrolls, whatever garbage a window
    /// resize left in the offset.
    #[test]
    fn a_menu_that_fits_never_scrolls() {
        let mut st = EditorState::minimal();
        st.win.h = 300.0;
        st.menu.scroll = 999.0;
        st.menu.ensure_cursor_visible(3, 1, st.win.h, false);
        assert_eq!(st.menu.scroll, 0.0);
    }

    /// Opening a menu always starts at the top, even if the same caption was
    /// scrolled down last time it was open.
    #[test]
    fn opening_a_menu_resets_the_scroll() {
        let mut st = EditorState::minimal();
        st.menu.scroll = 120.0;
        st.menu.sub_scroll = 40.0;
        st.menu_toggle(crate::ui::actions::Caption::File, 4.0);
        assert_eq!(st.menu.scroll, 0.0);
        assert_eq!(st.menu.sub_scroll, 0.0);
    }
}
