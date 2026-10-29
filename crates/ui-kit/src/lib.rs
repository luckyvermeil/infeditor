//! `ui-kit`: a general-purpose UI component library (Blitz / Dioxus-Native).
//!
//! The goal is to converge UI primitives that are independent of any specific
//! application into one reusable component library — like Ant Design, so other
//! Blitz applications can also directly `use ui_kit::…` to assemble their UI.
//!
//! # Design baseline: standard HTML first
//!
//! Components land on **standard HTML elements** wherever possible, and
//! interactive states land on **standard CSS** wherever possible:
//!
//! - [`Button`] / [`IconButton`] are real `<button>`s; hover/press/disabled go
//!   through `:hover` / `:active` / `:disabled` (blitz's stylo parses these
//!   pseudo-classes natively);
//! - The **semantics** of [`Checkbox`] / [`Radio`] are real
//!   `<input type="checkbox|radio">`s (controlled `checked` attribute /
//!   same-name mutual exclusion / accesskit roles), but the **look is
//!   self-drawn**: blitz's native widgets hard-code the fill and the ring gap
//!   to pure white, which under a dark theme is a glaring white box / white
//!   ring and cannot be recolored with CSS (see the `checkbox` / `radio`
//!   module docs);
//! - [`Separator`] is a real `<hr>`;
//! - Hover/press transitions are ordinary CSS `transition`s (blitz supports
//!   real transitions: they self-sustain redraws while running and stop when
//!   done, with no idle leak).
//!
//! Only things with **no standard counterpart** are self-drawn: the menu panel
//! (geometry / scrolling / keyboard navigation), the modal shell (blitz has no
//! `<dialog>`), and display-only text boxes (the host of the composer mode
//! keeps keyboard routing in its own hands). The three-state feedback inside
//! these components still uses the [`motion::Press`] signal machine (self-drawn
//! elements have no pseudo-classes available… strictly speaking
//! `:hover`/`:active` do apply to divs, but a menu row's highlight is jointly
//! determined by host state (the keyboard cursor) and hover, and
//! attribute-driven beats pseudo-class-driven for that).
//!
//! # Boundary with the application (the decoupling contract)
//!
//! This crate **only depends on the renderer (dioxus-native/blitz) and std**:
//!
//! - it knows no business concept (the editor's `EditorState`, command enums,
//!   document models…);
//! - the theme is injected via the pure-color [`Theme`] palette — the
//!   application converts with `impl From<&AppTheme> for ui_kit::Theme` at the
//!   render point (value copy, zero allocation); the component library never
//!   references the application's theme type internally;
//! - components are **controlled pure display widgets**: state arrives via
//!   props, interaction is reported upward via
//!   [`EventHandler`](dioxus_native::prelude::EventHandler); no business state
//!   is held, and no global signal is read or written.
//!
//! # Style injection (hosts must read this)
//!
//! A component's colors come from the runtime theme, so the stylesheet is
//! **generated** per theme: the host mounts one [`KitStyle`] at the root of
//! every window that uses this library (rendering a `<style>`). On theme
//! switch `KitStyle` rebuilds the node keyed by the light/dark flag — blitz
//! only parses the text when a `<style>` node is created; editing the text
//! in place does not re-parse.
//!
//! # The boolean-attribute trap
//!
//! dioxus-native writes `Bool(false)` as a literal `"false"` attribute, while
//! blitz judges `disabled` by **attribute presence** — `disabled: false`
//! disables the button. You must write
//! `disabled: disabled.then_some(true)` (None → clears the attribute).
//! `checked` has no such problem (falsy is specially handled to clear). See
//! the [`style`] module docs.
//!
//! # Modules
//!
//! | Module | Contents |
//! |---|---|
//! | [`tokens`] | Design tokens: font stack, type scale, radii, widget sizes |
//! | [`motion`] | Motion tokens and the three-state tap surface ([`motion::Press`]): for self-drawn components |
//! | [`style`] | Class-based stylesheet (generated per theme) + the [`KitStyle`] injection component |
//! | [`Theme`] | General theme palette (flat colors + light/dark flag) |
//! | `button` | [`Button`]: a real `<button>`, four kinds: primary/secondary/ghost/danger |
//! | `checkbox` | [`Checkbox`] / [`CheckMark`]: a controlled checkbox + a purely visual check mark (reused by the menu check column) |
//! | `radio` | [`Radio`]: a controlled radio, native semantics (same-name exclusion) / self-drawn look (ring + dot) |
//! | `text_input` | [`TextInput`]: a single-line display widget (the host routes the keyboard) |
//! | `icon_button` | [`IconButton`]: a real `<button>`, a square icon button |
//! | `menu` | [`MenuPanel`] / [`MenuRowView`]: dropdown menu panel (geometry + scrolling + three-state rows) |
//! | `modal` | [`Modal`]: a modal dialog shell (scrim + centered panel + title/body/action row) |
//!
//! # Two iron rules of blitz (observed inside the components; hosts must know)
//!
//! 1. blitz's default `click` action calls `clear_focus()` for clicks that hit
//!    no focusable element — the host's only keyboard focus anchor (a
//!    transparent textarea) loses focus (vendor patch five only exempts this
//!    when the focus is a text input). Clickable elements must call
//!    `prevent_default()` in `onpointerdown` / `onclick`. This library's
//!    buttons / checkboxes / icon buttons already do.
//! 2. Popup child windows forbid CSS **animations** (blitz self-sustains
//!    per-frame redraws for windows with running animations, starving the main
//!    window's message pump) — this library only uses `transition` and the
//!    modal entrance keyframes ([`MODAL_CSS`]), neither infinite; the keyframes
//!    should only be injected in the main window.

mod button;
mod checkbox;
mod icon_button;
mod menu;
mod modal;
pub mod motion;
mod radio;
mod separator;
pub mod style;
mod text_input;
mod theme;
pub mod tokens;

pub use button::{Button, ButtonKind};
pub use checkbox::{CheckMark, Checkbox};
pub use icon_button::IconButton;
pub use menu::{
    max_scroll, panel_avail_h, panel_height, panel_style, row_colors, row_indent, thumb_geom,
    visible_rows, wheel_step, MenuPanel, MenuRow, MenuRowView, PanelGeom, CHECK_W, MENU_PAD_L,
    PANEL_W, PANEL_Z, ROW_H, SCROLLBAR_W,
};
pub use modal::{Modal, MODAL_CSS};
pub use radio::Radio;
pub use separator::Separator;
pub use style::{stylesheet, KitStyle};
pub use text_input::TextInput;
pub use theme::{Theme, DARK, LIGHT};

/// Re-export of the component library's dioxus dependency: hosts can write
/// `rsx!` and use types like Signal / EventHandler without declaring
/// `dioxus-native` themselves.
pub use dioxus_native::prelude;
