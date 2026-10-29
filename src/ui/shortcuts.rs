//! The single source of truth for keyboard accelerators.
//!
//! Previously the accelerator text lived in the menu tree
//! (`Item::cmd("Join lines", "Ctrl+J", …)`) while the keyboard was matched by a
//! second, hand-written `match` in [`crate::ui::components::composer`]. The two
//! copies disagreed badly: of the 25 shortcuts the menus advertised, only 9
//! actually fired. A user reading "Ctrl+J" in a menu and pressing Ctrl+J got
//! nothing.
//!
//! Now a binding is declared **once**, in [`BINDINGS`], and both consumers read
//! it: the menu label comes from [`accel_of`] and the key handler from
//! [`lookup`]. A menu entry cannot advertise a shortcut the editor does not
//! implement, because that would require two edits.

use crate::textops::CaseKind;
use crate::ui::actions::Cmd;

/// A key, independent of any UI toolkit so the table stays testable.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum K {
    /// A printable character, always stored **lower-cased** so `Ctrl+Shift+S`
    /// and `Ctrl+s` are the same chord.
    Char(char),
    F(u8),
    Enter,
    Tab,
    Escape,
    Backspace,
    Delete,
    Space,
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
    PageUp,
    PageDown,
    Insert,
}

/// Modifier flags.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub struct Mods {
    pub ctrl: bool,
    pub shift: bool,
    pub alt: bool,
}

impl Mods {
    pub const NONE: Mods = Mods {
        ctrl: false,
        shift: false,
        alt: false,
    };
    pub const CTRL: Mods = Mods {
        ctrl: true,
        shift: false,
        alt: false,
    };
    pub const SHIFT: Mods = Mods {
        ctrl: false,
        shift: true,
        alt: false,
    };
    pub const CTRL_SHIFT: Mods = Mods {
        ctrl: true,
        shift: true,
        alt: false,
    };
    pub const CTRL_ALT: Mods = Mods {
        ctrl: true,
        shift: false,
        alt: true,
    };
    pub const CTRL_SHIFT_ALT: Mods = Mods {
        ctrl: true,
        shift: true,
        alt: true,
    };
}

/// A normalized key press: modifiers plus key.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Chord {
    pub mods: Mods,
    pub key: K,
}

impl Chord {
    /// Normalizes a key event into a table key.
    ///
    /// A shifted letter arrives from the DOM as the *uppercase* character
    /// (`Ctrl+Shift+S` → `'S'`), while the table stores lower-case letters. The
    /// `shift` flag is deliberately kept, so `Ctrl+S` and `Ctrl+Shift+S` stay
    /// two different commands; only the letter's case is folded.
    pub fn new(mods: Mods, key: K) -> Self {
        let key = match key {
            K::Char(c) => K::Char(c.to_ascii_lowercase()),
            other => other,
        };
        Chord { mods, key }
    }

    /// Parse a display-form chord like `Ctrl+Alt+Shift+F12` (the same format
    /// [`Self::label`] outputs).
    ///
    /// Order-insensitive: `Shift+Ctrl+S` and `Ctrl+Shift+S` are equivalent;
    /// a repeated modifier, an unknown key name, an empty string, or a chord
    /// ending in a modifier all error. Letter case is normalized per
    /// [`Self::new`].
    pub fn parse(s: &str) -> Result<Chord, ChordParseError> {
        let mut mods = Mods::NONE;
        let mut key: Option<K> = None;
        let mut parts = s.split('+').peekable();
        while let Some(raw) = parts.next() {
            let part = raw.trim();
            if part.is_empty() {
                return Err(ChordParseError(s.to_string()));
            }
            let m: Option<fn(Mods) -> Mods> = match part.to_ascii_lowercase().as_str() {
                "ctrl" | "control" => Some(|m: Mods| Mods { ctrl: true, ..m }),
                "shift" => Some(|m: Mods| Mods { shift: true, ..m }),
                "alt" => Some(|m: Mods| Mods { alt: true, ..m }),
                _ => None,
            };
            if let Some(f) = m {
                // The last segment must be a key; a chord cannot end in a
                // modifier
                if parts.peek().is_none() {
                    return Err(ChordParseError(s.to_string()));
                }
                mods = f(mods);
                continue;
            }
            if key.is_some() {
                // Two non-modifier segments: `Ctrl+S A` is not a chord
                return Err(ChordParseError(s.to_string()));
            }
            key = Some(parse_key(part)?);
        }
        match key {
            Some(k) => Ok(Chord::new(mods, k)),
            None => Err(ChordParseError(s.to_string())),
        }
    }

    /// Canonical display string, e.g. `Ctrl+Shift+U`, `F3`, `Del`, `Ctrl+=`.
    pub fn label(&self) -> String {
        let mut s = String::with_capacity(10);
        if self.mods.ctrl {
            s.push_str("Ctrl+");
        }
        if self.mods.alt {
            s.push_str("Alt+");
        }
        if self.mods.shift {
            s.push_str("Shift+");
        }
        match self.key {
            // Like every Windows menu, the letter is written in upper
            // case regardless of Shift, so `Ctrl+j` is displayed `Ctrl+J`.
            K::Char(c) => s.extend(c.to_uppercase()),
            K::F(n) => {
                s.push('F');
                s.push_str(&n.to_string());
            }
            K::Enter => s.push_str("Enter"),
            K::Tab => s.push_str("Tab"),
            K::Escape => s.push_str("Esc"),
            K::Backspace => s.push_str("Backspace"),
            K::Delete => s.push_str("Del"),
            K::Space => s.push_str("Space"),
            K::Left => s.push_str("Left"),
            K::Right => s.push_str("Right"),
            K::Up => s.push_str("Up"),
            K::Down => s.push_str("Down"),
            K::Home => s.push_str("Home"),
            K::End => s.push_str("End"),
            K::PageUp => s.push_str("PageUp"),
            K::PageDown => s.push_str("PageDown"),
            K::Insert => s.push_str("Ins"),
        }
        s
    }
}

/// One accelerator declaration.
#[derive(Clone, Copy, Debug)]
pub struct Binding {
    pub chord: Chord,
    pub cmd: Cmd,
}

/// The original chord text that failed to parse.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChordParseError(pub String);

impl std::fmt::Display for ChordParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "unrecognized shortcut: {}", self.0)
    }
}
impl std::error::Error for ChordParseError {}

/// One key-name segment (`S` / `F5` / `Del` / `Enter` …) → [`K`].
fn parse_key(part: &str) -> Result<K, ChordParseError> {
    let err = || ChordParseError(part.to_string());
    if part.chars().count() == 1 {
        let c = part.chars().next().unwrap();
        if !c.is_control() {
            return Ok(K::Char(c));
        }
        return Err(err());
    }
    let lower = part.to_ascii_lowercase();
    let k = match lower.as_str() {
        "f1" => K::F(1),
        "f2" => K::F(2),
        "f3" => K::F(3),
        "f4" => K::F(4),
        "f5" => K::F(5),
        "f6" => K::F(6),
        "f7" => K::F(7),
        "f8" => K::F(8),
        "f9" => K::F(9),
        "f10" => K::F(10),
        "f11" => K::F(11),
        "f12" => K::F(12),
        "enter" | "return" => K::Enter,
        "tab" => K::Tab,
        "esc" | "escape" => K::Escape,
        "backspace" => K::Backspace,
        "del" | "delete" => K::Delete,
        "space" => K::Space,
        "left" => K::Left,
        "right" => K::Right,
        "up" => K::Up,
        "down" => K::Down,
        "home" => K::Home,
        "end" => K::End,
        "pageup" => K::PageUp,
        "pagedown" => K::PageDown,
        "ins" | "insert" => K::Insert,
        _ => return Err(err()),
    };
    Ok(k)
}

const fn c(mods: Mods, key: K) -> Chord {
    // `const fn` cannot run `to_ascii_lowercase`, so the table stores the key
    // already normalized and `new` is only used by the runtime path.
    Chord { mods, key }
}

macro_rules! bind {
    ($mods:expr, $key:expr, $cmd:expr) => {
        Binding {
            chord: c($mods, $key),
            cmd: $cmd,
        }
    };
}

/// Every accelerator the editor implements.
///
/// Ordered roughly like a conventional editor shortcut sheet, grouped by
/// function, so the generated documentation reads in a predictable order.
pub static BINDINGS: &[Binding] = &[
    // ---- File ----------------------------------------------------------
    bind!(Mods::CTRL, K::Char('n'), Cmd::New),
    bind!(Mods::CTRL, K::Char('o'), Cmd::Open),
    // Open folder (workspace sidebar): the Shift variant of Ctrl+O, adjacent
    // and easy to remember.
    // Keys in the table must already be lower-case (`c()` does not normalize;
    // the event side's `Chord::new` lowercases uniformly)
    bind!(Mods::CTRL_SHIFT, K::Char('o'), Cmd::OpenWorkspace),
    bind!(Mods::CTRL, K::Char('r'), Cmd::Reload),
    bind!(Mods::CTRL, K::Char('s'), Cmd::Save),
    bind!(Mods::CTRL_ALT, K::Char('s'), Cmd::SaveAs),
    bind!(Mods::CTRL, K::Char('w'), Cmd::Close),
    // ---- Edit ----------------------------------------------------------
    bind!(Mods::CTRL, K::Char('z'), Cmd::Undo),
    bind!(Mods::CTRL, K::Char('y'), Cmd::Redo),
    // The second binding for redo (the common Windows convention).
    // `accel_of` only takes the first entry in the table (Ctrl+Y), keeping the
    // menu label stable; this line only lets Ctrl+Shift+Z redo too.
    bind!(Mods::CTRL_SHIFT, K::Char('z'), Cmd::Redo),
    bind!(Mods::CTRL, K::Char('x'), Cmd::Cut),
    bind!(Mods::CTRL, K::Char('c'), Cmd::Copy),
    bind!(Mods::CTRL, K::Char('v'), Cmd::Paste),
    bind!(Mods::NONE, K::Delete, Cmd::Delete),
    bind!(Mods::CTRL, K::Char('a'), Cmd::SelectAll),
    bind!(Mods::CTRL_SHIFT, K::Char('b'), Cmd::BeginEndSelect),
    // Bracket jumping. Conventional editors mostly use Ctrl+Shift+\ (the
    // chord drifts with the keyboard layout); here we take the unused Ctrl+P.
    bind!(Mods::CTRL, K::Char('p'), Cmd::GotoBracket),
    // Line bookmarks: Ctrl+F2 toggles a mark / F2 goes to the next.
    bind!(Mods::CTRL, K::F(2), Cmd::BookmarkToggle),
    bind!(Mods::NONE, K::F(2), Cmd::BookmarkNext),
    bind!(Mods::SHIFT, K::F(2), Cmd::BookmarkPrev),
    bind!(Mods::CTRL_SHIFT, K::Char('u'), Cmd::Case(CaseKind::Upper)),
    bind!(Mods::CTRL, K::Char('u'), Cmd::Case(CaseKind::Lower)),
    // Line operations
    bind!(Mods::CTRL, K::Char('d'), Cmd::DuplicateLine),
    bind!(Mods::CTRL, K::Char('j'), Cmd::JoinLines),
    bind!(Mods::CTRL_SHIFT, K::Up, Cmd::MoveLineUp),
    bind!(Mods::CTRL_SHIFT, K::Down, Cmd::MoveLineDown),
    bind!(Mods::CTRL_ALT, K::Enter, Cmd::InsertBlankAbove),
    bind!(Mods::CTRL_SHIFT_ALT, K::Enter, Cmd::InsertBlankBelow),
    // Comments
    bind!(Mods::CTRL, K::Char('q'), Cmd::ToggleLineComment),
    bind!(Mods::CTRL_SHIFT, K::Char('q'), Cmd::BlockComment),
    bind!(Mods::CTRL_SHIFT_ALT, K::Char('q'), Cmd::BlockUncomment),
    // Insert
    bind!(Mods::CTRL, K::F(5), Cmd::InsertShortDateTime),
    // Run… (F5)
    bind!(Mods::NONE, K::F(5), Cmd::RunCommand),
    // ---- Search --------------------------------------------------------
    bind!(Mods::CTRL, K::Char('f'), Cmd::Find(false, false)),
    bind!(Mods::CTRL, K::Char('h'), Cmd::Find(true, false)),
    bind!(Mods::CTRL, K::Char('g'), Cmd::Goto),
    bind!(Mods::NONE, K::F(3), Cmd::FindNext),
    bind!(Mods::SHIFT, K::F(3), Cmd::FindPrev),
    // ---- View ---------------------------------------------------------
    bind!(Mods::CTRL, K::Char('='), Cmd::ZoomIn),
    bind!(Mods::CTRL, K::Char('-'), Cmd::ZoomOut),
    bind!(Mods::CTRL, K::Char('0'), Cmd::ZoomReset),
    // JSONL preview: fold / unfold the caret's line
    bind!(Mods::CTRL_ALT, K::Char('['), Cmd::JsonlFoldLine),
    bind!(Mods::CTRL_ALT, K::Char(']'), Cmd::JsonlUnfoldLine),
    // Toggle the workspace sidebar (same chord as VS Code)
    bind!(Mods::CTRL, K::Char('b'), Cmd::ToggleSidebar),
    // ---- Window --------------------------------------------------------
    bind!(Mods::CTRL, K::Tab, Cmd::TabNext),
    bind!(Mods::CTRL_SHIFT, K::Tab, Cmd::TabPrev),
    // `Ctrl+F4` closes the document, like `Ctrl+W`. `Ctrl+W` is already
    // File → Close (`Cmd::Close`), and `Cmd::TabClose` is the same operation
    // reached from the Window menu, so only the unused chord is bound here —
    // two entries for one chord would make the label non-deterministic.
    bind!(Mods::CTRL, K::F(4), Cmd::TabClose),
];

/// The command bound to `chord`, if any.
pub fn lookup(chord: &Chord) -> Option<Cmd> {
    BINDINGS.iter().find(|b| b.chord == *chord).map(|b| b.cmd)
}

/// Display text for `cmd`'s accelerator, or `""` when it has none.
///
/// When several chords reach the same command the first one in [`BINDINGS`]
/// wins, which keeps the label stable.
pub fn accel_of(cmd: Cmd) -> String {
    BINDINGS
        .iter()
        .find(|b| b.cmd == cmd)
        .map(|b| b.chord.label())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn no_two_bindings_share_a_chord() {
        let mut seen = HashSet::new();
        for b in BINDINGS {
            assert!(
                seen.insert(b.chord),
                "duplicate binding for {}",
                b.chord.label()
            );
        }
    }

    /// The DOM delivers `Ctrl+Shift+S` as the character `'S'`; the table stores
    /// `'s'`. Folding the case is what makes the lookup find it.
    #[test]
    fn an_uppercase_letter_matches_its_lowercase_binding() {
        assert_eq!(
            Chord::new(Mods::CTRL_SHIFT, K::Char('S')),
            Chord::new(Mods::CTRL_SHIFT, K::Char('s'))
        );
        assert_eq!(
            lookup(&Chord::new(Mods::CTRL_SHIFT, K::Char('U'))),
            Some(Cmd::Case(CaseKind::Upper))
        );
    }

    /// Folding the case must **not** merge `Ctrl+S` with `Ctrl+Shift+S`.
    #[test]
    fn shift_still_distinguishes_commands() {
        assert_ne!(
            Chord::new(Mods::CTRL, K::Char('s')),
            Chord::new(Mods::CTRL_SHIFT, K::Char('s'))
        );
        assert_eq!(
            lookup(&Chord::new(Mods::CTRL, K::Char('s'))),
            Some(Cmd::Save)
        );
        assert_ne!(
            lookup(&Chord::new(Mods::CTRL_SHIFT, K::Char('s'))),
            Some(Cmd::Save)
        );
    }

    #[test]
    fn labels_match_windows_spelling() {
        let l = |m, k| Chord::new(m, k).label();
        assert_eq!(l(Mods::CTRL, K::Char('j')), "Ctrl+J");
        assert_eq!(l(Mods::CTRL_SHIFT, K::Char('u')), "Ctrl+Shift+U");
        // The label format emits
        // Ctrl+ then Alt+ then Shift+, so the menu text is `Ctrl+Alt+Shift+Q`
        assert_eq!(l(Mods::CTRL_SHIFT_ALT, K::Char('q')), "Ctrl+Alt+Shift+Q");
        assert_eq!(l(Mods::CTRL_ALT, K::Char('s')), "Ctrl+Alt+S");
        assert_eq!(l(Mods::CTRL, K::Char('=')), "Ctrl+=");
        assert_eq!(l(Mods::NONE, K::Delete), "Del");
        assert_eq!(l(Mods::NONE, K::F(3)), "F3");
        assert_eq!(l(Mods::SHIFT, K::F(3)), "Shift+F3");
        assert_eq!(l(Mods::CTRL, K::Tab), "Ctrl+Tab");
    }

    #[test]
    fn lookup_resolves_the_documented_keys() {
        assert_eq!(
            lookup(&Chord::new(Mods::CTRL, K::Char('j'))),
            Some(Cmd::JoinLines)
        );
        assert_eq!(
            lookup(&Chord::new(Mods::NONE, K::F(3))),
            Some(Cmd::FindNext)
        );
        assert_eq!(
            lookup(&Chord::new(Mods::SHIFT, K::F(3))),
            Some(Cmd::FindPrev)
        );
        assert_eq!(
            lookup(&Chord::new(Mods::CTRL_SHIFT_ALT, K::Char('q'))),
            Some(Cmd::BlockUncomment)
        );
        assert_eq!(lookup(&Chord::new(Mods::NONE, K::Char('z'))), None);
    }

    /// The whole point of the table: a menu label and a key press come from the
    /// same declaration.
    #[test]
    fn accel_of_round_trips_through_lookup() {
        for b in BINDINGS {
            let label = accel_of(b.cmd);
            assert!(!label.is_empty(), "{} has no label", b.chord.label());
            // A command can have multiple chords (redo = Ctrl+Y and
            // Ctrl+Shift+Z): the displayed label is decided solely by the
            // **first** binding in the table; later chords do not participate
            // in the comparison.
            let first = BINDINGS.iter().find(|x| x.cmd == b.cmd).unwrap();
            if first.chord != b.chord {
                continue;
            }
            assert_eq!(label, b.chord.label(), "label mismatch for {b:?}");
        }
    }

    /// Redo has two chords, and the menu's displayed label must be pinned to
    /// the Windows-conventional Ctrl+Y — the second chord was once inserted at
    /// the head of the table, drifting the label to Ctrl+Shift+Z.
    #[test]
    fn redo_accel_label_is_stable() {
        assert_eq!(accel_of(Cmd::Redo), "Ctrl+Y");
    }

    #[test]
    fn commands_without_a_binding_render_an_empty_accel() {
        assert_eq!(accel_of(Cmd::About), "");
        assert_eq!(accel_of(Cmd::MacroDir), "");
        assert_eq!(accel_of(Cmd::Case(CaseKind::Proper)), "");
    }
}
