//! Editor themes: token colors plus the surrounding "chrome" colors.
//!
//! Two built-in palettes (dark / light); all colors are static strings so they
//! can be inlined into inline styles without allocation. The dark palette
//! follows VS Code's "Dark 2026" default (1.113, March 2026).

use super::lang::TokenKind;

/// Everything the UI needs to paint itself in one palette.
#[derive(PartialEq)]
pub struct Theme {
    pub name: &'static str,
    pub dark: bool,
    // editor surface
    pub bg: &'static str,
    pub fg: &'static str,
    pub gutter_bg: &'static str,
    pub gutter_fg: &'static str,
    pub gutter_cur_fg: &'static str,
    pub gutter_border: &'static str,
    pub cur_line: &'static str,
    pub selection: &'static str,
    pub selection_fg: &'static str,
    pub caret: &'static str,
    pub border: &'static str,
    // chrome
    pub menu_bg: &'static str,
    pub menu_fg: &'static str,
    pub menu_hover: &'static str,
    pub menu_open: &'static str,
    pub menu_border: &'static str,
    pub tab_bg: &'static str,
    pub tab_active_bg: &'static str,
    pub tab_fg: &'static str,
    pub tab_border: &'static str,
    /// hover of an inactive tab (distinct from `tab_active_bg`, which is the
    /// resting color of the *active* tab — reusing it would make an inactive tab
    /// look selected)
    pub tab_hover_bg: &'static str,
    /// pressed tab
    pub tab_press_bg: &'static str,
    /// the `●` that marks an unsaved document
    pub dirty_dot: &'static str,
    /// resting `×` / `+` glyph on a tab
    pub tab_close_fg: &'static str,
    pub status_bg: &'static str,
    pub status_fg: &'static str,
    pub input_bg: &'static str,
    pub input_border: &'static str,
    pub panel_bg: &'static str,
    pub accent: &'static str,
    pub match_hi: &'static str,
    pub match_hi_dim: &'static str,
    // ---- interaction ----
    //
    // Hover / pressed colors are *state*, not cascade: the menu bar sets its
    // colors inline (a stylesheet `:hover` rule loses to an inline
    // `background`, which is exactly why the light theme used to flash dark
    // blue on hover). Keeping them here also lets the generated stylesheet and
    // the inline styles agree by construction.
    /// hover background of a menu caption or item
    pub hover_bg: &'static str,
    /// text color on `hover_bg`
    pub hover_fg: &'static str,
    /// keyboard-highlighted item (same family as hover, one step stronger)
    pub active_bg: &'static str,
    pub active_fg: &'static str,
    /// ring drawn around the focused text box
    pub focus_ring: &'static str,
    /// hover of a flat button (find bar, panel)
    pub btn_hover: &'static str,
    /// pressed flat button
    pub btn_active: &'static str,
    /// Border color of the bracket-match highlight box
    pub bracket_hi: &'static str,
    /// Line-number color for bookmarked lines
    pub bookmark: &'static str,
    /// Text mark palette (right-click → "Style as..." 1..=5)
    pub mark: [&'static str; 5],
    /// dropdown / panel drop shadow
    pub shadow: &'static str,
    /// Modal backdrop (dimming layer behind dialogs)
    pub dim_backdrop: &'static str,
    /// Close-button hover background (Windows 11 close-red family; unified
    /// across dialogs / find bar / title bar)
    pub close_hot: &'static str,
    /// Close-button pressed background
    pub close_down: &'static str,
    /// Status bar danger color (macro-recording red dot etc.)
    pub status_danger: &'static str,
    /// selection painted while the document does not own the keyboard
    pub sel_inactive: &'static str,
    pub sel_inactive_fg: &'static str,
    /// caret painted while the document does not own the keyboard
    pub caret_inactive: &'static str,
    // ---- scrollbar ----
    /// Track background (also the base for the arrow buttons at both ends)
    pub sb_track: &'static str,
    /// Thumb
    pub sb_thumb: &'static str,
    /// Thumb hover
    pub sb_thumb_hover: &'static str,
    /// Thumb pressed / dragging
    pub sb_thumb_active: &'static str,
    /// Arrow triangles
    pub sb_arrow: &'static str,
    // ---- code minimap ----
    /// Minimap background
    pub mm_bg: &'static str,
    /// Ink bars: color of the horizontal bars a line compresses into
    /// ("has text")
    pub mm_ink: &'static str,
    /// Viewport indicator ("you are here") — semi-transparent so the ink
    /// below stays visible
    pub mm_slider: &'static str,
    /// Indicator box on pointer hover
    pub mm_slider_hover: &'static str,
    /// Full-row background of the caret line
    pub mm_cur: &'static str,
    // ---- sidebar (VS Code explorer) ----
    //
    // The sidebar is a hand-drawn div tree; tri-state (hover / selected) goes
    // through CSS pseudo-classes in `<style>` (see `components::style`), so
    // the colors must be able to enter the stylesheet — they live here rather
    // than in components so both themes can be compared side by side in one
    // place.
    /// Tree/list row hover background (`list.hoverBackground`)
    pub list_hover: &'static str,
    /// Tree/list row selected background (`list.activeSelectionBackground`)
    pub list_sel: &'static str,
    /// Tree/list row selected foreground
    pub list_sel_fg: &'static str,
    /// Tree indent guides
    pub indent_guide: &'static str,
}

/// "Dark 2026" look (VS Code 1.113's new default, per sikora.software's
/// write-up): the editor surface `#121314` is now the *darkest* surface so the
/// canvas reads as the focused workspace, while chrome (`#191A1B`) recedes
/// behind it. Tokens shift to the reduced accent palette — light-blue strings,
/// orange parameters, red structural keywords, purple functions, green tags.
pub const DARK: Theme = Theme {
    name: "Dark",
    dark: true,
    // layered surfaces: editor < gutter/panel < inputs, darkest last
    bg: "#121314",
    fg: "#d4d4d4",
    gutter_bg: "#191a1b",
    // The gutter shares the editor color (VS Code's gutter is not a separate
    // band); only the numbers' brightness differs — the current line number
    // is one step brighter, matching `editorLineNumber.activeForeground`.
    gutter_fg: "#7d8590",
    gutter_cur_fg: "#d4d4d4",
    gutter_border: "#2a2d2e",
    cur_line: "#1e2021",
    selection: "#264f78",
    selection_fg: "#ffffff",
    caret: "#d7d7d7",
    border: "#2a2d2e",
    menu_bg: "#191a1b",
    menu_fg: "#c9d1d9",
    menu_hover: "#1d3e4b",
    menu_open: "#22262a",
    menu_border: "#2f3336",
    tab_bg: "#191a1b",
    tab_active_bg: "#121314",
    tab_fg: "#8b949e",
    tab_border: "#2a2d2e",
    tab_hover_bg: "#22262a",
    tab_press_bg: "#2c3136",
    dirty_dot: "#e2c08d",
    tab_close_fg: "#8b949e",
    status_bg: "#191a1b",
    status_fg: "#c9d1d9",
    input_bg: "#1c2128",
    input_border: "#2f3336",
    panel_bg: "#191a1b",
    accent: "#0078d4",
    // find highlights went from amber to a muted teal-blue family; current
    // match `#284A59` sits one step above the rest at `#1D3E4B`
    match_hi: "#284a59",
    match_hi_dim: "#1d3e4b",
    hover_bg: "#1d3e4b",
    hover_fg: "#e6edf3",
    active_bg: "#284a59",
    active_fg: "#ffffff",
    focus_ring: "#0078d4",
    btn_hover: "#22262a",
    btn_active: "#2c3136",
    bracket_hi: "#7ee787",
    bookmark: "#e2c08d",
    mark: [
        "rgba(255,213,79,0.30)",
        "rgba(105,190,110,0.32)",
        "rgba(90,170,255,0.30)",
        "rgba(255,120,180,0.30)",
        "rgba(255,150,80,0.30)",
    ],
    shadow: "rgba(0,0,0,0.60)",
    dim_backdrop: "rgba(0,0,0,0.45)",
    close_hot: "#c42b1c",
    close_down: "#a3271b",
    status_danger: "#ff6b6b",
    sel_inactive: "#3a3d41",
    sel_inactive_fg: "#c9d1d9",
    caret_inactive: "#6e7681",
    sb_track: "#121314",
    sb_thumb: "#33383d",
    sb_thumb_hover: "#3f454b",
    sb_thumb_active: "#4a5057",
    sb_arrow: "#8b949e",
    mm_bg: "#121314",
    mm_ink: "#6e7681",
    // VS Code's viewport box is a neutral gray overlay
    // (`minimapSlider.background`), not a bright bar: the minimap is already
    // grayscale, and a bright bar would read as content.
    mm_slider: "rgba(121,121,121,0.20)",
    mm_slider_hover: "rgba(121,121,121,0.40)",
    mm_cur: "rgba(255,255,255,0.05)",
    list_hover: "#2a2d2e",
    list_sel: "#04395e",
    list_sel_fg: "#ffffff",
    indent_guide: "#33383d",
};

pub const LIGHT: Theme = Theme {
    name: "Light",
    dark: false,
    bg: "#ffffff",
    fg: "#333333",
    gutter_bg: "#f5f5f5",
    gutter_fg: "#8a8a8a",
    gutter_cur_fg: "#1a1a1a",
    gutter_border: "#d4d4d4",
    cur_line: "#f2f2f2",
    selection: "#add6ff",
    selection_fg: "#000000",
    caret: "#000000",
    border: "#d4d4d4",
    // Default light chrome: #F0F0F0 menu bar with black text and
    // a pale-blue hover, plus a *light* status bar rather than the dark accent
    // bar VS Code uses.
    menu_bg: "#f0f0f0",
    menu_fg: "#333333",
    menu_hover: "#c8e0f4",
    menu_open: "#dcdcdc",
    menu_border: "#c8c8c8",
    tab_bg: "#e8e8e8",
    tab_active_bg: "#ffffff",
    tab_fg: "#555555",
    tab_border: "#c8c8c8",
    tab_hover_bg: "#d8d8d8",
    tab_press_bg: "#c0c0c0",
    dirty_dot: "#c07b00",
    tab_close_fg: "#707070",
    status_bg: "#ededed",
    status_fg: "#333333",
    input_bg: "#ffffff",
    input_border: "#a0a0a0",
    panel_bg: "#f6f6f6",
    accent: "#007acc",
    match_hi: "#ffd75f",
    match_hi_dim: "#f5e6b8",
    hover_bg: "#c8e0f4",
    hover_fg: "#1a1a1a",
    active_bg: "#a8d0ec",
    active_fg: "#000000",
    // Formerly accent=#007acc and focus_ring=#0078d4—same theme two blues conflicting;
    // Focus ring follows accent (consistent with dark theme)
    focus_ring: "#007acc",
    btn_hover: "#e0e0e0",
    btn_active: "#cccccc",
    bracket_hi: "#2e8b2e",
    bookmark: "#c07b00",
    mark: [
        "rgba(255,196,0,0.35)",
        "rgba(46,160,67,0.28)",
        "rgba(56,132,255,0.24)",
        "rgba(255,80,140,0.26)",
        "rgba(255,130,40,0.28)",
    ],
    shadow: "rgba(0,0,0,0.28)",
    dim_backdrop: "rgba(0,0,0,0.30)",
    close_hot: "#c42b1c",
    close_down: "#a3271b",
    status_danger: "#c42b1c",
    sel_inactive: "#e5ebf1",
    sel_inactive_fg: "#333333",
    caret_inactive: "#8f8f8f",
    sb_track: "#f0f0f0",
    sb_thumb: "#cdcdcd",
    sb_thumb_hover: "#a6a6a6",
    sb_thumb_active: "#8f8f8f",
    sb_arrow: "#5a5a5a",
    mm_bg: "#ffffff",
    mm_ink: "#9a9a9a",
    mm_slider: "rgba(100,100,100,0.13)",
    mm_slider_hover: "rgba(100,100,100,0.28)",
    mm_cur: "rgba(0,0,0,0.06)",
    // Light theme sidebar keeps light tone (our app's light chrome is light gray),
    // Selection walks VS Code light theme blue background with white text.
    list_hover: "#e8e8e8",
    list_sel: "#0060c0",
    list_sel_fg: "#ffffff",
    indent_guide: "#d3d3d3",
};

impl Theme {
    pub fn of(dark: bool) -> &'static Theme {
        if dark {
            &DARK
        } else {
            &LIGHT
        }
    }

    /// Foreground color for a token class.
    pub fn color(&self, k: TokenKind) -> &'static str {
        use TokenKind::*;
        if self.dark {
            // Dark 2026 token palette: strings go light blue `#a5d6ff`,
            // structural keywords go red `#ff7b72` while flow keywords keep
            // purple `#c586c0`, functions become purple `#d2a8ff`, tags green
            // `#7ee787`, variables near-white, attributes stay light blue.
            match k {
                Plain | Ident => self.fg,
                Comment => "#8b949e",
                DocComment => "#9aa4ae",
                String | Char => "#a5d6ff",
                Number => "#79c0ff",
                Keyword => "#ff7b72",
                Control => "#c586c0",
                Type => "#79c0ff",
                Function => "#d2a8ff",
                Operator | Punct => "#d4d4d4",
                Preproc => "#ffa657",
                Annotation => "#d2a8ff",
                Tag => "#7ee787",
                AttrName => "#9cdcfe",
                AttrValue => "#a5d6ff",
                Variable => "#e6edf3",
                Heading => "#79c0ff",
                Bold => "#e6edf3",
                Italic => "#d4d4d4",
                Code => "#a5d6ff",
                Link => "#79c0ff",
                Quote => "#8b949e",
                ListMark => "#ff7b72",
                Rule => "#79c0ff",
                // Formulas get their own warm gold so they stand apart from
                // strings (light blue), numbers (blue) and preprocessor orange;
                // the `$` fences drop to a muted gray so the expression, not
                // the delimiters, carries the eye. Inside a formula the ordinary
                // classes keep their colors (Number / Operator / Punct /
                // String / Comment) and commands take the function purple.
                Math => "#e5c07b",
                MathDelim => "#6e7681",
                MathCmd => "#d2a8ff",
                Added => "#7ee787",
                Removed => "#ff7b72",
                Meta => "#ff7b72",
                Col(i) => COL_DARK[(i as usize) % COL_DARK.len()],
            }
        } else {
            match k {
                Plain | Ident => self.fg,
                Comment => "#008000",
                DocComment => "#007000",
                String | Char => "#a31515",
                Number => "#098658",
                Keyword => "#0000ff",
                Control => "#af00db",
                Type => "#267f99",
                Function => "#795e26",
                Operator | Punct => "#333333",
                Preproc => "#af00db",
                Annotation => "#795e26",
                Tag => "#800000",
                AttrName => "#e50000",
                AttrValue => "#0000ff",
                Variable => "#001080",
                Heading => "#000080",
                Bold => "#000000",
                Italic => "#333333",
                Code => "#a31515",
                Link => "#267f99",
                Quote => "#008000",
                ListMark => "#af00db",
                Rule => "#000080",
                // Dark amber, one step below the bookmark's `#c07b00` so a
                // formula and a marked line never read as the same thing.
                // Commands take the control-keyword purple. The fence gray is
                // the gutter's `#8a8a8a`: the old `#9a9a9a` sat at 2.6:1 on
                // white, under the 3:1 floor, and a single thin glyph needs
                // more than that to read at all.
                Math => "#9a6700",
                MathDelim => "#8a8a8a",
                MathCmd => "#af00db",
                Added => "#098658",
                Removed => "#a31515",
                Meta => "#0000ff",
                Col(i) => COL_LIGHT[(i as usize) % COL_LIGHT.len()],
            }
        }
    }

    /// `true` when a token class should be rendered bold (headings, bold md).
    pub fn bold(&self, k: TokenKind) -> bool {
        matches!(k, TokenKind::Heading | TokenKind::Bold)
    }

    pub fn italic(&self, k: TokenKind) -> bool {
        matches!(
            k,
            TokenKind::Italic | TokenKind::Comment | TokenKind::DocComment
        )
    }
}

const COL_DARK: &[&str] = &[
    "#d4d4d4", "#a5d6ff", "#ffa657", "#79c0ff", "#d2a8ff", "#ff7b72", "#7ee787", "#c586c0",
];

const COL_LIGHT: &[&str] = &[
    "#333333", "#001080", "#a31515", "#098658", "#af00db", "#795e26", "#267f99", "#0000ff",
];
