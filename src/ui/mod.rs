//! UI layer (Blitz + Dioxus) for InfEditor

pub mod aboutwin;
pub mod actions;
pub mod assoc;
pub mod commands;
pub mod components;
pub mod design;
pub mod editor;
pub mod find;
pub mod findwin;
pub mod guard;
pub mod macros_io;
pub mod mdnet;
pub mod metrics;
pub mod minimap;
pub mod motion;
pub mod numwin;
pub mod prefswin;
pub mod recorder;
pub mod recovery;
pub mod scroll;
pub mod scrollbar;
pub mod settings;
pub mod shell;
pub mod shortcuts;
pub mod single;
pub mod smoke;
pub mod state;
pub mod touch;
pub mod vim;
pub mod vim_bridge;

#[cfg(test)]
pub(crate) mod testutil;

pub use state::EditorState;

// UI text: everything under `ui` takes it from these two types (see
// crates/i18n/README.md). Re-exported so UI code has one spelling:
// `st.tr(Msg::MenuFile)` / `st.tr(Fmt::LineCol { .. })`.
pub use i18n::{Fmt, Locale, Msg};

/// Display label for a syntax-highlighting language name.
///
/// The highlighter's names are canonical English identifiers ("Rust",
/// "JSON Lines", …) and double as persistence/command keys (`Cmd::Lang`,
/// macros' `Lang:<name>`), so they are deliberately **not** translated
/// wholesale. `Log` is the one exception: its identifier used to be the
/// Chinese "日志", which leaked raw into every UI language; it is now `Log`
/// with its label coming from the catalog ([`Msg::LangLog`]).
pub fn lang_label(loc: Locale, name: &str) -> std::borrow::Cow<'_, str> {
    if name == "Log" {
        loc.tr(Msg::LangLog)
    } else {
        std::borrow::Cow::Borrowed(name)
    }
}

// ---------------------------------------------------------------------------
// Theme adaptation: `highlight::Theme` → `winshell::PopTheme`
// ---------------------------------------------------------------------------
//
// Why this `From` impl lives in the UI layer instead of `highlight::theme`:
//
// * `highlight` is the engine layer (lexing + colors), `winshell` is the
//   window/popup shell layer. Putting the conversion there would make the
//   **engine** import the **shell layer** — the engine would then depend on the
//   UI crate, and `cargo test` for the engine's unit tests would have to
//   compile the whole window stack.
// * Semantically it is not part of "theme definition" either; it is the
//   adaptation applied at the render point that feeds themes to self-drawn
//   controls (`PopTheme` only understands flat palettes, see the module
//   comment in `winshell::shell::controls`).
// * `Theme` is this crate's own type, so `impl From<&Theme> for
//   winshell::PopTheme` is legal in any module of this crate (orphan rule);
//   the UI layer is the most fitting place.
//
// Value copy, zero allocation: all of `Theme`'s colors are `&'static str`.
impl From<&'static crate::highlight::Theme> for winshell::PopTheme {
    fn from(t: &'static crate::highlight::Theme) -> Self {
        winshell::PopTheme {
            accent: t.accent,
            input_bg: t.input_bg,
            input_border: t.input_border,
            menu_fg: t.menu_fg,
            menu_bg: t.menu_bg,
            menu_border: t.menu_border,
            menu_open: t.menu_open,
            btn_hover: t.btn_hover,
            btn_active: t.btn_active,
            panel_bg: t.panel_bg,
            gutter_fg: t.gutter_fg,
            caret: t.caret,
        }
    }
}

// ---------------------------------------------------------------------------
// Theme adaptation: `highlight::Theme` → `ui_kit::Theme`
// ---------------------------------------------------------------------------
//
// Same reasoning and placement as the `PopTheme` conversion above: the
// component library (`ui-kit`) only understands its own flat palette, and the
// app performs a **value-copy conversion** at the render point (all
// `&'static str`, zero allocation). Keeping the conversion in the UI layer
// means `highlight` (the engine layer) never has to import the component
// library.
impl From<&'static crate::highlight::Theme> for ui_kit::Theme {
    fn from(t: &'static crate::highlight::Theme) -> Self {
        ui_kit::Theme {
            name: t.name,
            dark: t.dark,
            bg: t.bg,
            fg: t.fg,
            panel_bg: t.panel_bg,
            menu_bg: t.menu_bg,
            menu_fg: t.menu_fg,
            menu_border: t.menu_border,
            menu_open: t.menu_open,
            hover_bg: t.hover_bg,
            hover_fg: t.hover_fg,
            active_bg: t.active_bg,
            active_fg: t.active_fg,
            btn_hover: t.btn_hover,
            btn_active: t.btn_active,
            input_bg: t.input_bg,
            input_border: t.input_border,
            focus_ring: t.focus_ring,
            accent: t.accent,
            gutter_bg: t.gutter_bg,
            gutter_fg: t.gutter_fg,
            shadow: t.shadow,
            dim_backdrop: t.dim_backdrop,
            close_hot: t.close_hot,
            close_down: t.close_down,
            caret: t.caret,
            sb_thumb: t.sb_thumb,
            selection: t.selection,
            selection_fg: t.selection_fg,
        }
    }
}

/// File paths given on the command line (`infeditor.exe a.txt b.log`).
///
/// When launched via Explorer's "Open with" / file association the system puts
/// the file paths in argv, and GUI automation tests open documents through the
/// same path — both share this single entry point. Directories and
/// non-existent paths are passed through unchanged for
/// [`crate::ui::state::EditorState::open_dropped`] to judge (it shows a
/// status-bar hint); this only strips the few noise arguments the window
/// subsystem adds.
///
/// This is also the entry point for **dropping a file onto `infeditor.exe`** in
/// Explorer: the shell starts the exe with the dropped item as `argv[1]`, so the
/// two cases are literally the same input. What happens next depends on whether
/// this process got to be the instance: the primary opens the paths itself, a
/// second launch hands them to the running one (`ui::single`).
///
/// Robustness over cleverness: a leading `-` marks a switch (macOS app-bundle
/// launches pass `-psn_0_123`, the window subsystem adds flags), surrounding
/// quotes are stripped (a `.lnk` or a hand-edited registry command can carry
/// them literally), and duplicates are dropped — dropping two files, then
/// dropping them again, must not queue the same tab twice.
///
/// A leading `/` is deliberately **not** treated as a switch: on POSIX that is
/// every absolute path.
pub fn cli_open_files() -> Vec<std::path::PathBuf> {
    parse_cli_args(std::env::args().skip(1))
}

/// The pure part of [`cli_open_files`] (argv is process-global and cannot be
/// injected in a test).
fn parse_cli_args(args: impl IntoIterator<Item = String>) -> Vec<std::path::PathBuf> {
    let mut out: Vec<std::path::PathBuf> = Vec::new();
    for arg in args {
        let arg = arg.trim();
        // Quotes around a path are noise, not part of the name. Both kinds:
        // Explorer is not the only source of a command line.
        let arg = arg.trim_matches('"').trim_matches('\'');
        if arg.is_empty() || arg.starts_with('-') {
            continue;
        }
        let path = std::path::PathBuf::from(arg);
        // `platform::paths_equal` (not plain `==`): `C:\a.txt` and `c:/a.txt`
        // are the same file, and a double drop would open two tabs on one file.
        if out
            .iter()
            .any(|p| platform::paths_equal(p.as_path(), path.as_path()))
        {
            continue;
        }
        out.push(path);
    }
    out
}

#[cfg(test)]
mod cli_tests {
    use super::parse_cli_args;

    /// The parser is the only thing standing between "the user dropped a file on
    /// the exe" and "a tab opened": noise must be dropped, real paths kept
    /// exactly once.
    #[test]
    fn cli_parsing_keeps_paths_and_drops_noise() {
        let args = ["-psn_0_123", "", "  ", "C:\\a\\b.md", "\"C:\\c d.txt\""];
        let got = parse_cli_args(args.iter().map(|s| s.to_string()));
        assert_eq!(
            got,
            vec![
                std::path::PathBuf::from("C:\\a\\b.md"),
                std::path::PathBuf::from("C:\\c d.txt"),
            ]
        );
    }

    /// The same argument twice is one tab: dropping two files, then dropping
    /// them again, must not queue the same tab twice.
    #[test]
    fn cli_parsing_drops_duplicates() {
        let args = ["C:/a/b.md", "C:/a/b.md"];
        assert_eq!(parse_cli_args(args.iter().map(|s| s.to_string())).len(), 1);
    }

    /// Windows path identity: case and separator spelling do not make a second
    /// file, so `C:/a/b.md` and `c:\a\b.md` are one tab. Gated because the
    /// premise is Windows-only — on POSIX those are two distinct files (`\` is
    /// a legal filename character and the comparison is exact), so asserting one
    /// tab there would encode the wrong behaviour for the platform.
    #[cfg(windows)]
    #[test]
    fn cli_parsing_ignores_case_and_separator_on_windows() {
        let args = ["C:/a/b.md", "c:\\a\\b.md"];
        assert_eq!(parse_cli_args(args.iter().map(|s| s.to_string())).len(), 1);
    }

    /// A POSIX absolute path is a path, not a switch.
    #[test]
    fn posix_absolute_path_is_kept() {
        let args = ["/home/me/notes.md"];
        assert_eq!(
            parse_cli_args(args.iter().map(|s| s.to_string())),
            vec![std::path::PathBuf::from("/home/me/notes.md")]
        );
    }
}

#[cfg(test)]
mod tests {
    use crate::highlight::theme;

    /// The `highlight::Theme` → `ui_kit::Theme` adaptation must carry over
    /// every color value the app uses (a missed one leaves a component-library
    /// control with an empty/wrong color — the value-copy conversion has no
    /// default fallback).
    #[test]
    fn theme_converts_to_uikit_palette() {
        for t in [&theme::DARK, &theme::LIGHT] {
            let u: ui_kit::Theme = t.into();
            assert_eq!(u.name, t.name);
            assert_eq!(u.dark, t.dark);
            assert_eq!(u.accent, t.accent);
            assert_eq!(u.menu_bg, t.menu_bg);
            assert_eq!(u.menu_fg, t.menu_fg);
            assert_eq!(u.menu_border, t.menu_border);
            assert_eq!(u.btn_hover, t.btn_hover);
            assert_eq!(u.btn_active, t.btn_active);
            assert_eq!(u.input_bg, t.input_bg);
            assert_eq!(u.input_border, t.input_border);
            assert_eq!(u.hover_bg, t.hover_bg);
            assert_eq!(u.active_bg, t.active_bg);
            assert_eq!(u.shadow, t.shadow);
            assert_eq!(u.dim_backdrop, t.dim_backdrop);
            assert_eq!(u.close_hot, t.close_hot);
            assert_eq!(u.sb_thumb, t.sb_thumb);
            assert_eq!(u.caret, t.caret);
        }
    }
}
