//! Vim-style modal editing.
//!
//! Design constraints that shaped this:
//!
//! * **Pure.** The engine turns a key into a [`Plan`] — a list of intent
//!   operations against the buffer — and never touches a [`TextDocument`](crate::buffer::TextDocument)
//!   itself. Everything here is unit-testable without a window, which is the
//!   only way to test a modal editor honestly.
//! * **Non-modal parts stay non-modal.** Typing into the find box, and the
//!   handful of chords that must always work (`Ctrl+S`, `Ctrl+F`), keep their
//!   ordinary meaning while Vim is on. A user who enables Vim for editing
//!   should not lose the ability to save.
//! * **No hidden state.** The pending operator (`d`, `g`, `c`, …), the register
//!   and the command line are all fields on [`Vim`], so `Vim::handle` is a
//!   total function of its inputs.
//!
//! Supported surface (deliberately the 90 % people actually use):
//!
//! | mode | keys |
//! |---|---|
//! | Normal | `h j k l`, `w b e`, `0 ^ $`, `gg G`, `{n}G`, `dd`, `dw`, `d{motion}`, `x`, `D`, `C`, `cc`, `yy`, `p P`, `u`, `Ctrl+R`, `i I a A o O`, `v V`, `J`, `.`, `>> <<`, `n N`, `/ : ?` |
//! | Insert | `Esc`, `Backspace`, arrows, `Ctrl+[` |
//! | Visual | `h j k l w b 0 $`, `d y c`, `Esc` |
//! | Command line | `w`, `q`, `wq`, `x`, `n<line>`, `Esc` |

use crate::textops::is_word_char;
use crate::ui::actions::Cmd;
use crate::ui::shortcuts::K;

// The engine does not hold an EditorState, but messages must be keys rather
// than rendered strings (the status bar must follow along after a language
// switch), so this only depends on the i18n framework.
use i18n::{Fmt, Msg};

/// A key event reduced to what Vim cares about.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Ev {
    /// A printable character; carries no modifiers (a chord like `Ctrl+X` is
    /// never a Vim key).
    Char(char),
    /// A non-character key.
    Key(K),
}

/// Which modal key map is active.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Mode {
    #[default]
    Normal,
    Insert,
    Visual,
    CommandLine,
}

impl Mode {
    /// The mode name as a **key**, not a rendered string: callers render it in
    /// their own locale (`st.tr(mode.msg())`), the engine never freezes wording.
    pub fn msg(self) -> Msg {
        match self {
            Mode::Normal => Msg::VimModeNormal,
            Mode::Insert => Msg::VimModeInsert,
            Mode::Visual => Msg::VimModeVisual,
            Mode::CommandLine => Msg::VimModeCmdline,
        }
    }
}

/// What the engine wants the editor to do.
///
/// The editor owns the buffer; the engine only describes intent, so an operator
/// can be validated (and its bounds computed) before anything mutates.
#[derive(Clone, PartialEq, Debug)]
pub enum Op {
    /// Move the caret to an absolute byte offset.
    Goto(u64),
    /// Move the caret to line `n` (1-based; clamped by the bridge).
    GotoLine(u64),
    /// Set the find pattern and jump to the next match.
    Search(String),
    /// Move by a signed number of lines.
    Lines(i64),
    /// Move by a signed number of characters (word motions resolve to this
    /// after the engine has read the line).
    Chars(i64),
    /// Move by a signed number of *words* (`w` forward, `b` backward); the
    /// bridge does the real word-boundary scan over the document. The old
    /// "≈6 characters per word" approximation made `dw` on `foo.bar` stop
    /// mid-word and `999999w` loop millions of times.
    WordMove(i64),
    /// Vim `e`: to the *last character* of the current / next word; the count
    /// repeats it and `de` deletes through that character inclusively.
    WordEnd(i64),
    /// Extend/shrink the selection to an absolute byte offset.
    SelectTo(u64),
    /// Delete `[from, to)`.
    Delete {
        from: u64,
        to: u64,
    },
    /// Delete the selection, or the character under the caret.
    DeleteChar(Option<u64>),
    /// Delete whole lines containing the caret (or the selection); the count
    /// is how many lines from the caret's line (`3dd` → 3, `dj` → 2).
    DeleteLines(u64),
    /// Replace the selection (or the character under the caret) and enter Insert.
    Change(Option<u64>),
    /// Clear the contents of `count` lines from the caret's line (Vim `cc`)
    /// and enter Insert; one empty line remains.
    ChangeLines(u64),
    /// Yank `[from, to)` into the unnamed register.
    Yank {
        from: u64,
        to: u64,
    },
    /// Yank whole lines containing the caret (`3yy` → 3).
    YankLines(u64),
    /// Paste the register after (or before) the caret; `linewise` pastes as
    /// whole lines.
    Paste {
        after: bool,
        linewise: bool,
    },
    /// Insert text and stay in Insert mode.
    Insert(String),
    /// Open a new line below (or above) and enter Insert.
    OpenLine {
        below: bool,
    },
    /// Join the caret line with the one below.
    JoinLine,
    /// Indent or dedent the caret line.
    ShiftIndent {
        outdent: bool,
    },
    /// Run a menu command.
    Run(Cmd),
    /// Find next / previous.
    FindNext,
    FindPrev,
    /// Show a message in the status bar. The **key**, not a rendered string:
    /// the bridge stores it in `EditorState::status` and the status bar renders
    /// it in the current language.
    Status(i18n::Message),
}

/// A key's result.
#[derive(Clone, PartialEq, Debug, Default)]
pub struct Plan {
    /// Apply in order. Rejected operators produce an empty plan.
    pub ops: Vec<Op>,
}

impl Plan {
    pub fn one(op: Op) -> Self {
        Plan { ops: vec![op] }
    }
    pub fn many(ops: Vec<Op>) -> Self {
        Plan { ops }
    }
}

/// Longest pending text for the command line, so it cannot grow without bound.
const CMD_MAX: usize = 200;

/// Persistent Vim state for the whole application.
#[derive(Clone, Debug)]
pub struct Vim {
    pub enabled: bool,
    pub mode: Mode,
    /// keys typed after an operator that expects a motion (`d`, `c`, `y`, `>`,
    /// `<`, `g`) plus a repeat count
    pending: String,
    /// count typed before the operator, e.g. `3dd`
    count: String,
    /// the `"` register (single unnamed register, which is all we expose)
    pub register: String,
    /// the register holds whole lines, so `p` pastes a line
    register_linewise: bool,
    /// text of the in-progress `:` command
    cmdline: String,
    /// the last executed command, for `.`
    last: Option<String>,
    /// motion in progress for `f`/`F`/`t`/`T`
    find_char: Option<(char, bool)>,
    /// true while the pending `f`-family key was `t`/`T` (stop *before* the hit)
    find_till: bool,
}

impl Default for Vim {
    fn default() -> Self {
        Vim {
            enabled: false,
            mode: Mode::Normal,
            pending: String::new(),
            count: String::new(),
            register: String::new(),
            register_linewise: false,
            cmdline: String::new(),
            last: None,
            find_char: None,
            find_till: false,
        }
    }
}

/// An enabled Vim for test suites outside this module (the bridge tests type
/// real key sequences through [`Vim::handle_key`]).
#[cfg(test)]
pub(crate) fn test_vim() -> Vim {
    Vim {
        enabled: true,
        ..Default::default()
    }
}

/// Read-only facts about the buffer that the engine needs but must not own.
///
/// Passed in per keystroke so the engine stays free of buffer lifetimes; the
/// editor fills it from the active document.
#[derive(Clone, Debug, Default)]
pub struct Buffer {
    /// byte offset where the current line starts
    pub line_start: u64,
    /// byte offset just past the last character of the current line,
    /// *excluding* the terminator
    pub line_end: u64,
    /// byte offset just past the last character of the line, including the
    /// terminator
    pub line_end_with_term: u64,
    /// byte offset of the start of the next line
    pub next_line_start: u64,
    /// byte offset of the start of the previous line
    pub prev_line_start: u64,
    /// total bytes
    pub len: u64,
    /// current selection, if any
    pub sel: Option<(u64, u64)>,
    /// text of the current line (for word motions and `$`)
    pub line: String,
    /// is the current line the last one?
    pub at_eof: bool,
    /// byte offset of the first non-blank on the current line
    pub first_nonblank: u64,
}

impl Vim {
    /// Toggle Vim mode on/off, returning the editor to Normal.
    pub fn toggle(&mut self) {
        self.enabled = !self.enabled;
        self.reset();
    }

    pub fn set_enabled(&mut self, on: bool) {
        if self.enabled != on {
            self.enabled = on;
            self.reset();
        }
    }

    fn reset(&mut self) {
        self.mode = Mode::Normal;
        self.pending.clear();
        self.count.clear();
        self.cmdline.clear();
        self.find_char = None;
    }

    /// True when the key map should swallow the key event entirely.
    pub fn should_prevent_default(mode: Mode) -> bool {
        mode != Mode::Normal
    }

    /// Handle one key. `buf` describes the document *before* the key.
    ///
    /// Returns `None` when the key is not Vim's business, in which case the
    /// caller falls through to the normal bindings.
    pub fn handle_key(&mut self, ev: Ev, caret: u64, buf: &Buffer) -> Option<Plan> {
        if !self.enabled {
            return None;
        }
        // accept either spelling of a character key; the rest of the engine
        // only ever matches `Ev::Char`
        let ev = match ev {
            Ev::Key(K::Char(c)) => Ev::Char(c),
            other => other,
        };
        match self.mode {
            Mode::Insert => self.insert_mode(ev, buf),
            Mode::CommandLine => self.cmdline_mode(ev),
            Mode::Normal => self.normal_mode(ev, caret, buf),
            Mode::Visual => self.visual_mode(ev, caret, buf),
        }
    }

    // ---- insert ---------------------------------------------------------

    fn insert_mode(&mut self, ev: Ev, _buf: &Buffer) -> Option<Plan> {
        match ev {
            Ev::Key(K::Escape) => {
                self.mode = Mode::Normal;
                Some(Plan::one(Op::Status(Msg::VimModeNormal.into())))
            }
            // printable characters insert; arrows and chords are left to the
            // editor's own key path so they keep their normal meaning
            Ev::Char(c) => Some(Plan::one(Op::Insert(c.to_string()))),
            _ => None,
        }
    }

    // ---- command line ---------------------------------------------------

    fn cmdline_mode(&mut self, ev: Ev) -> Option<Plan> {
        match ev {
            Ev::Key(K::Escape) => {
                self.cmdline.clear();
                self.mode = Mode::Normal;
                Some(Plan::one(Op::Status(Msg::VimStCancelled.into())))
            }
            Ev::Key(K::Backspace) => {
                // the prompt character is part of `cmdline`, so it must not be
                // deletable — otherwise the user is left staring at an empty
                // prompt with no hint what mode they are in
                if self.cmdline.len() > 1 {
                    self.cmdline.pop();
                }
                None
            }
            Ev::Key(K::Enter) => {
                let line = std::mem::take(&mut self.cmdline);
                self.mode = Mode::Normal;
                Some(self.run_command(&line))
            }
            Ev::Char(c) if !c.is_control() && self.cmdline.chars().count() < CMD_MAX => {
                // non-control characters only: ASCII-only would reject `/mode`
                // searches in Chinese (and any other non-ASCII input)
                self.cmdline.push(c);
                None
            }
            _ => None,
        }
    }

    /// A deliberately tiny `:` set: write, quit, write+quit, close, goto line.
    fn run_command(&mut self, line: &str) -> Plan {
        // the prompt character is part of `cmdline` so the user sees it while
        // typing, but it is not part of the command
        let cmd = line.trim().trim_start_matches([':', '/']).trim();
        // `/pattern` is a search, not a `:` command
        if line.trim().starts_with('/') && !cmd.is_empty() {
            return Plan::many(vec![Op::Search(cmd.to_string()), Op::FindNext]);
        }
        let ops = match cmd {
            "w" => vec![Op::Run(Cmd::Save)],
            "q" => vec![Op::Run(Cmd::TabClose)],
            "wq" | "x" => vec![Op::Run(Cmd::Save), Op::Run(Cmd::TabClose)],
            "" => vec![],
            other => {
                // `:42` jumps to a line, the one numeric command worth having
                match other.parse::<u64>() {
                    Ok(n) if n >= 1 => {
                        vec![Op::GotoLine(n)]
                    }
                    _ => vec![Op::Status(
                        Fmt::VimStUnknownCmd {
                            cmd: other.to_string(),
                        }
                        .into(),
                    )],
                }
            }
        };
        Plan::many(ops)
    }

    // ---- normal ---------------------------------------------------------

    fn normal_mode(&mut self, ev: Ev, caret: u64, buf: &Buffer) -> Option<Plan> {
        // a pending `g` waits for its second key
        if self.pending == "g" {
            self.pending.clear();
            return Some(self.handle_g(ev, caret, buf));
        }
        // `f`/`F`/`t`/`T` wait for the target character
        if let Some((_, backward)) = self.find_char.take() {
            if let Ev::Char(c) = ev {
                return Some(self.find_char_plan(c, caret, buf, backward));
            }
            // a non-character key cancels the search and falls through
        }

        // Escape clears any pending operator / count, as in Vim
        if ev == Ev::Key(K::Escape) {
            self.pending.clear();
            self.count.clear();
            self.find_char = None;
            return Some(Plan::one(Op::Status(Msg::VimModeNormal.into())));
        }

        // a pending operator (`d c y > <`) is completed by the next motion
        if matches!(self.pending.as_str(), "d" | "c" | "y" | ">" | "<") {
            if let Some(mut motion) = motion_op(ev, buf) {
                let n: i64 = self.count.parse().unwrap_or(1);
                self.count.clear();
                motion = scale_motion(motion, n);
                // NOTE: do *not* clear `pending` here — resolve_operator
                // consumes it itself (mem::take); clearing first made every
                // operator+motion pair (`dw`, `dj`, `cw`, …) resolve to an
                // empty plan.
                let plan = self.resolve_operator(caret, motion, buf);
                return Some(plan);
            }
            // digits keep building the repeat count (`d3w`), exactly like the
            // pre-operator digit branch below
            if let Ev::Char(c) = ev {
                if c.is_ascii_digit() && !(c == '0' && self.count.is_empty()) {
                    if self.count.len() < 6 {
                        self.count.push(c);
                    }
                    return Some(Plan::default());
                }
                // `dd` / `yy` / `cc`: the same operator twice means "whole
                // line" (`3dd` counts as 3); any other key cancels the
                // operator without typing
                if let Some(op) = operator_of(ev) {
                    let same = self.pending == op;
                    let n: usize = self.count.parse().unwrap_or(1);
                    self.count.clear();
                    self.pending.clear();
                    if same {
                        return Some(self.line_operator(op, n));
                    }
                }
                self.count.clear();
                self.pending.clear();
                return Some(Plan::default());
            }
            // a non-motion, non-character key (Enter, F3, PageDown…) while an
            // operator is pending: abandon the operator. Letting the key
            // through with `pending`/`count` intact made the *next* key
            // complete a surprise edit anchored to a stale state.
            self.pending.clear();
            self.count.clear();
        }

        // digits build a count, except the `0` that means "column zero"
        if let Ev::Char(c) = ev {
            if c.is_ascii_digit() && !(c == '0' && self.count.is_empty()) {
                if self.count.len() < 6 {
                    self.count.push(c);
                }
                return Some(Plan::default());
            }
        }

        // operators: d c y > < g
        if let Some(op) = operator_of(ev) {
            // `dd` / `yy` / `cc`: the same operator twice means "whole line",
            // which is why the pending operator has to be consumed here rather
            // than simply overwritten. (Reachable only when the first key of
            // the pair arrived through a path that did not route through the
            // pending-operator branch above.)
            if self.pending == op {
                let n: usize = self.count.parse().unwrap_or(1);
                self.count.clear();
                self.pending.clear();
                return Some(self.line_operator(op, n));
            }
            self.pending = op.to_string();
            self.find_char = None;
            return Some(Plan::default());
        }

        let has_count = !self.count.is_empty();
        let count: i64 = if self.count.is_empty() {
            1
        } else {
            self.count.parse().unwrap_or(1)
        };
        if has_count {
            self.count.clear();
        }

        let plan = self.normal_action(ev, caret, buf, count, has_count);
        if let Some(p) = &plan {
            if !p.ops.is_empty() {
                self.last = Some(ev_text(ev));
            }
        }
        plan
    }

    /// The line-wise form of an operator: `dd`, `yy`, `cc`.
    ///
    /// `n` is the repeat count typed before the operator (`3dd` deletes 3
    /// lines); it is always at least 1.
    fn line_operator(&mut self, op: &str, n: usize) -> Plan {
        let n = (n.max(1)) as u64;
        match op {
            "d" => Plan::one(Op::DeleteLines(n)),
            "y" => Plan::one(Op::YankLines(n)),
            "c" => {
                self.mode = Mode::Insert;
                Plan::one(Op::ChangeLines(n))
            }
            _ => Plan::default(),
        }
    }

    /// The plan for a completed `f`/`F`/`t`/`T`: jump to the target character
    /// on the current line (for `t`/`T`, up to the character *before* it). A
    /// miss consumes the key without moving.
    fn find_char_plan(&self, target: char, caret: u64, buf: &Buffer, backward: bool) -> Plan {
        let base = buf.line_start;
        let text = &buf.line;
        let mut off = (caret.min(buf.line_end).saturating_sub(base)) as usize;
        while off > 0 && !text.is_char_boundary(off) {
            off -= 1;
        }
        let hits: Vec<usize> = text
            .char_indices()
            .filter(|(i, c)| {
                *c == target
                    && match backward {
                        // `F`/`T` search strictly left of the caret, `f`/`t`
                        // strictly right
                        true => *i < off,
                        false => *i > off,
                    }
            })
            .map(|(i, _)| i)
            .collect();
        let found = match backward {
            true => hits.last().copied(),
            false => hits.first().copied(),
        };
        let Some(mut i) = found else {
            return Plan::one(Op::Status(Msg::VimStCharNotFound.into()));
        };
        if self.find_till {
            if backward {
                // `T`: land one cell **after** the hit character (between it and
                // the origin) — the two branches used to be written backwards:
                // T retreated before the hit, t advanced after it, each walking
                // one cell too many or too few
                if let Some(c) = text[i..].chars().next() {
                    i += c.len_utf8();
                }
            } else {
                // `t`: land one cell **before** the hit character
                let prev = text[..i].char_indices().next_back().map(|(j, _)| j);
                if let Some(p) = prev {
                    i = p;
                }
            }
        }
        Plan::one(Op::Goto(base + i as u64))
    }

    fn handle_g(&mut self, ev: Ev, _caret: u64, buf: &Buffer) -> Plan {
        match ev {
            // `gg` goes to the first non-blank of the line, which is Vim's
            // "first non-blank of the file" for a single-line view and the
            // expected behaviour once the editor resolves it
            Ev::Char('g') => Plan::one(Op::Goto(buf.first_nonblank)),
            // any other key cancels the pending `g` without typing into the doc
            _ => Plan::default(),
        }
    }

    fn normal_action(
        &mut self,
        ev: Ev,
        caret: u64,
        buf: &Buffer,
        n: i64,
        has_count: bool,
    ) -> Option<Plan> {
        match ev {
            // ---- motions ----
            Ev::Key(K::Left) => Some(Plan::one(Op::Chars(-n))),
            Ev::Key(K::Right) => Some(Plan::one(Op::Chars(n))),
            Ev::Key(K::Up) => Some(Plan::one(Op::Lines(-n))),
            Ev::Key(K::Down) => Some(Plan::one(Op::Lines(n))),

            Ev::Char('h') => Some(Plan::one(Op::Chars(-n))),
            Ev::Char('l') => Some(Plan::one(Op::Chars(n))),
            Ev::Char('k') => Some(Plan::one(Op::Lines(-n))),
            Ev::Char('j') => Some(Plan::one(Op::Lines(n))),

            Ev::Char('0') => Some(Plan::one(Op::Goto(buf.line_start))),
            Ev::Char('^') => Some(Plan::one(Op::Goto(buf.first_nonblank))),
            Ev::Char('$') => Some(Plan::one(Op::Goto(end_col(buf, n)))),

            Ev::Char('G') => {
                // `G` with no count means the last line; an explicit `1G`
                // means line 1 (folding "no count" and "count = 1" together
                // used to send `1G` to the end of the file)
                if has_count {
                    Some(Plan::one(Op::GotoLine(n.max(0) as u64)))
                } else {
                    Some(Plan::one(Op::GotoLine(u64::MAX)))
                }
            }
            Ev::Char('w') => Some(Plan::one(Op::WordMove(n.max(1)))),
            Ev::Char('b') => Some(Plan::one(Op::WordMove(-n.max(1)))),
            Ev::Char('e') => Some(Plan::one(Op::WordEnd(n.max(1)))),

            Ev::Char('f') | Ev::Char('F') | Ev::Char('t') | Ev::Char('T') => {
                let backward = matches!(ev, Ev::Char('F') | Ev::Char('T'));
                self.find_till = matches!(ev, Ev::Char('t') | Ev::Char('T'));
                self.find_char = Some((' ', backward));
                None
            }

            // ---- operators that are complete on their own ----
            Ev::Char('x') => Some(Plan::one(Op::DeleteChar(buf.sel.map(|_| caret)))),
            Ev::Char('D') => {
                let from = sel_start(buf).unwrap_or(caret);
                Some(Plan::one(Op::Delete {
                    from,
                    to: end_col(buf, 1),
                }))
            }
            Ev::Char('C') => {
                let from = sel_start(buf).unwrap_or(caret);
                self.mode = Mode::Insert;
                Some(Plan::one(Op::Change(Some(from))))
            }
            Ev::Char('J') => Some(Plan::one(Op::JoinLine)),
            Ev::Char('p') => Some(Plan::one(Op::Paste {
                after: true,
                linewise: self.register_linewise,
            })),
            Ev::Char('P') => Some(Plan::one(Op::Paste {
                after: false,
                linewise: self.register_linewise,
            })),
            Ev::Char('u') => Some(Plan::one(Op::Run(Cmd::Undo))),
            Ev::Key(K::F(3)) => Some(Plan::one(Op::FindNext)),
            Ev::Char('n') => Some(Plan::one(Op::FindNext)),
            Ev::Char('N') => Some(Plan::one(Op::FindPrev)),
            Ev::Char('/') => {
                self.mode = Mode::CommandLine;
                self.cmdline = "/".into();
                Some(Plan::default())
            }
            Ev::Char(':') => {
                self.mode = Mode::CommandLine;
                self.cmdline = ":".into();
                Some(Plan::default())
            }
            Ev::Char('.') => match &self.last {
                Some(t) => Some(Plan::one(Op::Status(
                    Fmt::VimStRepeatUnsupported {
                        count: t.to_string(),
                    }
                    .into(),
                ))),
                None => Some(Plan::one(Op::Status(Msg::VimStNothingToRepeat.into()))),
            },

            // ---- mode switches ----
            Ev::Char('i') => {
                self.mode = Mode::Insert;
                Some(Plan::default())
            }
            Ev::Char('I') => {
                self.mode = Mode::Insert;
                Some(Plan::one(Op::Goto(buf.first_nonblank)))
            }
            Ev::Char('a') => {
                self.mode = Mode::Insert;
                Some(Plan::one(Op::Chars(1)))
            }
            Ev::Char('A') => {
                self.mode = Mode::Insert;
                Some(Plan::one(Op::Goto(end_col(buf, 1))))
            }
            Ev::Char('o') => {
                self.mode = Mode::Insert;
                Some(Plan::one(Op::OpenLine { below: true }))
            }
            Ev::Char('O') => {
                self.mode = Mode::Insert;
                Some(Plan::one(Op::OpenLine { below: false }))
            }
            Ev::Char('v') => {
                self.mode = Mode::Visual;
                Some(Plan::one(Op::Status(Msg::VimModeVisual.into())))
            }
            Ev::Char('V') => {
                self.mode = Mode::Visual;
                Some(Plan::one(Op::Status(Msg::VimModeVisualLine.into())))
            }

            // Unimplemented character keys are swallowed outright: in real Vim
            // Normal-mode letters either mean something or have no side effect,
            // and letting them through would type characters like `q` (the
            // habitual macro key) straight into the document. Key events
            // (Home/End/PageUp…) stay transparent, keeping the application's
            // semantics.
            Ev::Char(_) => Some(Plan::default()),
            _ => None,
        }
    }

    /// Resolve a pending operator plus the motion that follows it.
    fn resolve_operator(&mut self, target_from: u64, motion: Op, buf: &Buffer) -> Plan {
        let op = std::mem::take(&mut self.pending);
        let plan = match op.as_str() {
            "d" => self.delete_with(motion, target_from, buf),
            "y" => self.yank_with(motion, target_from, buf),
            "c" => {
                self.mode = Mode::Insert;
                self.change_with(motion, target_from, buf)
            }
            ">" => Plan::one(Op::ShiftIndent { outdent: false }),
            "<" => Plan::one(Op::ShiftIndent { outdent: true }),
            _ => Plan::default(),
        };
        plan
    }

    fn motion_end(&self, motion: &Op, from: u64, buf: &Buffer) -> Option<(u64, u64)> {
        match motion {
            Op::Goto(p) => Some((*p, *p)),
            Op::Chars(n) => {
                // Same semantics as vim_bridge::vim_line_hi: an in-line move
                // may not pass the end of the line's content (line_end),
                // otherwise `dl` at the end of a line would also delete the
                // line terminator (an accidental J)
                let to = (from as i64 + n)
                    .max(buf.line_start as i64)
                    .min(buf.line_end.max(buf.line_start) as i64) as u64;
                Some((from, to))
            }
            Op::Lines(_) => Some((from, from)),
            Op::WordMove(_) => Some((from, word_motion_target(motion, from, buf))),
            Op::WordEnd(_) => {
                // `de` deletes up to and **including** the word-end character:
                // exclusive endpoint, then advance one more character
                let end = word_motion_target(motion, from, buf);
                let rel = end
                    .saturating_sub(buf.line_start)
                    .min(buf.line.len() as u64) as usize;
                let extra = buf.line[rel..]
                    .chars()
                    .next()
                    .map(|c| c.len_utf8() as u64)
                    .unwrap_or(0);
                Some((from, end + extra))
            }
            _ => None,
        }
    }

    fn delete_with(&mut self, motion: Op, from: u64, buf: &Buffer) -> Plan {
        match motion {
            // `dj` deletes the caret line *and* the one the motion spans,
            // i.e. |k| + 1 lines in total
            Op::Lines(k) => Plan::one(Op::DeleteLines(k.unsigned_abs() + 1)),
            other => match self.motion_end(&other, from, buf) {
                Some((a, b)) => {
                    let (lo, hi) = (a.min(b), a.max(b));
                    Plan::many(vec![
                        Op::Yank { from: lo, to: hi },
                        Op::Delete { from: lo, to: hi },
                    ])
                }
                None => Plan::default(),
            },
        }
    }

    fn yank_with(&mut self, motion: Op, from: u64, buf: &Buffer) -> Plan {
        match motion {
            Op::Lines(k) => Plan::one(Op::YankLines(k.unsigned_abs() + 1)),
            other => match self.motion_end(&other, from, buf) {
                Some((a, b)) => Plan::one(Op::Yank {
                    from: a.min(b),
                    to: a.max(b),
                }),
                None => Plan::default(),
            },
        }
    }

    fn change_with(&mut self, motion: Op, from: u64, buf: &Buffer) -> Plan {
        match motion {
            Op::Lines(k) => Plan::one(Op::ChangeLines(k.unsigned_abs() + 1)),
            other => match self.motion_end(&other, from, buf) {
                Some((a, b)) => Plan::one(Op::Change(Some(a.min(b)))),
                None => Plan::default(),
            },
        }
    }

    // ---- visual ---------------------------------------------------------

    fn visual_mode(&mut self, ev: Ev, caret: u64, buf: &Buffer) -> Option<Plan> {
        match ev {
            Ev::Key(K::Escape) => {
                self.mode = Mode::Normal;
                Some(Plan::one(Op::Status(Msg::VimModeNormal.into())))
            }
            Ev::Key(K::Left) | Ev::Char('h') => {
                Some(Plan::one(Op::SelectTo(caret.saturating_sub(1))))
            }
            Ev::Key(K::Right) | Ev::Char('l') => Some(Plan::one(Op::SelectTo(caret + 1))),
            Ev::Key(K::Up) | Ev::Char('k') => Some(Plan::one(Op::Lines(-1))),
            Ev::Key(K::Down) | Ev::Char('j') => Some(Plan::one(Op::Lines(1))),
            Ev::Char('0') => Some(Plan::one(Op::SelectTo(buf.line_start))),
            Ev::Char('$') => Some(Plan::one(Op::SelectTo(end_col(buf, 1)))),
            Ev::Char('d') | Ev::Char('y') | Ev::Char('c') => {
                // operate directly on the selection, as visual mode means
                let (a, b) = match buf.sel {
                    Some((a, b)) => (a.min(b), a.max(b)),
                    None => (caret, caret),
                };
                match ev {
                    Ev::Char('d') => Some(Plan::many(vec![
                        Op::Yank { from: a, to: b },
                        Op::Delete { from: a, to: b },
                    ])),
                    Ev::Char('y') => Some(Plan::one(Op::Yank { from: a, to: b })),
                    _ => {
                        self.mode = Mode::Insert;
                        Some(Plan::one(Op::Change(Some(a))))
                    }
                }
            }
            Ev::Char('v') | Ev::Char('V') => {
                self.mode = Mode::Normal;
                Some(Plan::one(Op::Status(Msg::VimModeNormal.into())))
            }
            // Unimplemented character keys are swallowed: in Visual mode
            // letting them through would write the pressed character into the
            // document with "type over the selection" semantics (pressing `3`
            // would replace the selection with 3).
            Ev::Char(_) => Some(Plan::default()),
            _ => None,
        }
    }

    /// The register, for the status bar / tests.
    pub fn register_text(&self) -> &str {
        &self.register
    }

    /// True when the register holds whole lines (so `p` pastes a line).
    pub fn register_is_linewise(&self) -> bool {
        self.register_linewise
    }

    /// Put `text` in the unnamed register.
    pub fn set_register(&mut self, text: String, linewise: bool) {
        self.register = text;
        self.register_linewise = linewise;
    }

    pub fn cmdline_text(&self) -> &str {
        &self.cmdline
    }

    pub fn pending_text(&self) -> &str {
        &self.pending
    }

    /// True when a motion is waiting for its second key.
    pub fn awaiting_motion(&self) -> bool {
        !self.pending.is_empty()
    }
}

fn sel_start(buf: &Buffer) -> Option<u64> {
    buf.sel.map(|(a, _)| a)
}

/// In-line word-motion targets (the engine only receives the current line;
/// the cross-line part is decided by the editor side's line attribution).
///
/// - `WordMove(k>0)`: the next word start (the exclusive endpoint of `dw` is
///   exactly it); `k<0`: the previous word start.
/// - `WordEnd`: the position of the **last character** of the current / next
///   word (`de` then advances one character).
///
/// A word = a run of [`is_word_char`] characters, or a run of non-whitespace
/// punctuation; whitespace separates. When none is found in the line take the
/// line boundary — far more accurate than the old "≈6 chars per word"
/// approximation.
fn word_motion_target(motion: &Op, from: u64, buf: &Buffer) -> u64 {
    let line = &buf.line;
    let base = buf.line_start;
    let line_end = buf.line_end.max(base);
    let mut off = from.saturating_sub(base).min(line.len() as u64) as usize;
    while off > 0 && !line.is_char_boundary(off) {
        off -= 1;
    }
    let chars: Vec<(usize, char)> = line.char_indices().collect();
    match motion {
        Op::WordMove(k) if *k > 0 => {
            let mut idx = chars.partition_point(|(o, _)| *o < off);
            if idx < chars.len() && !chars[idx].1.is_whitespace() {
                let cls = is_word_char(chars[idx].1);
                while idx < chars.len()
                    && !chars[idx].1.is_whitespace()
                    && is_word_char(chars[idx].1) == cls
                {
                    idx += 1;
                }
            }
            while idx < chars.len() && chars[idx].1.is_whitespace() {
                idx += 1;
            }
            if idx < chars.len() {
                base + chars[idx].0 as u64
            } else {
                line_end
            }
        }
        Op::WordMove(_) => {
            let mut idx = chars.partition_point(|(o, _)| *o < off);
            while idx > 0 && chars[idx - 1].1.is_whitespace() {
                idx -= 1;
            }
            if idx > 0 {
                let cls = is_word_char(chars[idx - 1].1);
                while idx > 0
                    && !chars[idx - 1].1.is_whitespace()
                    && is_word_char(chars[idx - 1].1) == cls
                {
                    idx -= 1;
                }
            }
            if idx > 0 {
                base + chars[idx].0 as u64
            } else {
                base
            }
        }
        Op::WordEnd(_) => {
            // Cursor on a non-whitespace char → the last character of the
            // current run (`de` deletes up to and including the word end);
            // cursor on whitespace → the last character of the next run after
            // skipping the whitespace.
            let mut idx = chars.partition_point(|(o, _)| *o < off);
            if idx < chars.len() && !chars[idx].1.is_whitespace() {
                let cls = is_word_char(chars[idx].1);
                while idx + 1 < chars.len()
                    && !chars[idx + 1].1.is_whitespace()
                    && is_word_char(chars[idx + 1].1) == cls
                {
                    idx += 1;
                }
                base + chars[idx].0 as u64
            } else {
                while idx < chars.len() && chars[idx].1.is_whitespace() {
                    idx += 1;
                }
                if idx < chars.len() {
                    let cls = is_word_char(chars[idx].1);
                    while idx + 1 < chars.len()
                        && !chars[idx + 1].1.is_whitespace()
                        && is_word_char(chars[idx + 1].1) == cls
                    {
                        idx += 1;
                    }
                    base + chars[idx].0 as u64
                } else {
                    line_end
                }
            }
        }
        _ => from,
    }
}

/// The motion form of a key, for completing a pending operator (`dw`, `c$`, …).
fn motion_op(ev: Ev, buf: &Buffer) -> Option<Op> {
    match ev {
        Ev::Key(K::Left) | Ev::Char('h') => Some(Op::Chars(-1)),
        Ev::Key(K::Right) | Ev::Char('l') => Some(Op::Chars(1)),
        Ev::Key(K::Up) | Ev::Char('k') => Some(Op::Lines(-1)),
        Ev::Key(K::Down) | Ev::Char('j') => Some(Op::Lines(1)),
        Ev::Char('0') => Some(Op::Goto(buf.line_start)),
        Ev::Char('^') => Some(Op::Goto(buf.first_nonblank)),
        Ev::Char('$') => Some(Op::Goto(end_col(buf, 1))),
        Ev::Char('w') => Some(Op::WordMove(1)),
        Ev::Char('b') => Some(Op::WordMove(-1)),
        Ev::Char('e') => Some(Op::WordEnd(1)),
        _ => None,
    }
}

/// Scale a character/line motion by a repeat count (`d3w`).
fn scale_motion(m: Op, n: i64) -> Op {
    if n <= 1 {
        return m;
    }
    match m {
        Op::Chars(k) => Op::Chars(k.saturating_mul(n)),
        Op::Lines(k) => Op::Lines(k.saturating_mul(n)),
        Op::WordMove(k) => Op::WordMove(k.saturating_mul(n)),
        Op::WordEnd(k) => Op::WordEnd(k.saturating_mul(n)),
        other => other,
    }
}

fn end_col(buf: &Buffer, n: i64) -> u64 {
    // `$` goes to the last character; `2$` to the last of the next line, which
    // the editor resolves, so only the first line is modelled here.
    let back = (n - 1).max(0) as u64;
    let mut p = buf.line_end.saturating_sub(back).max(buf.line_start);
    // never land mid-UTF-8 on a multi-byte line
    while p > buf.line_start && !buf.line.is_char_boundary((p - buf.line_start) as usize) {
        p -= 1;
    }
    p
}

fn operator_of(ev: Ev) -> Option<&'static str> {
    match ev {
        Ev::Char('d') => Some("d"),
        Ev::Char('c') => Some("c"),
        Ev::Char('y') => Some("y"),
        Ev::Char('>') => Some(">"),
        Ev::Char('<') => Some("<"),
        Ev::Char('g') => Some("g"),
        _ => None,
    }
}

/// Textual form of a key, used as the `.` repeat label and in status text.
fn ev_text(ev: Ev) -> String {
    match ev {
        Ev::Char(c) => c.to_string(),
        Ev::Key(k) => format!("{k:?}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn buf_of(line: &str) -> Buffer {
        let len = line.len() as u64;
        Buffer {
            line_start: 0,
            line_end: len,
            line_end_with_term: len,
            next_line_start: len,
            prev_line_start: 0,
            len,
            sel: None,
            line: line.to_string(),
            at_eof: true,
            first_nonblank: line.len() as u64 - line.trim_start().len() as u64,
        }
    }

    fn press(v: &mut Vim, ev: Ev, caret: u64, buf: &Buffer) -> Plan {
        v.handle_key(ev, caret, buf).unwrap_or_default()
    }

    #[test]
    fn normal_motions_move_the_caret() {
        let mut v = Vim {
            enabled: true,
            ..Default::default()
        };
        let buf = buf_of("hello world");
        assert_eq!(
            press(&mut v, Ev::Char('l'), 0, &buf).ops,
            vec![Op::Chars(1)]
        );
        assert_eq!(
            press(&mut v, Ev::Key(K::Right), 0, &buf).ops,
            vec![Op::Chars(1)]
        );
        assert_eq!(
            press(&mut v, Ev::Char('h'), 5, &buf).ops,
            vec![Op::Chars(-1)]
        );
        assert_eq!(
            press(&mut v, Ev::Char('j'), 0, &buf).ops,
            vec![Op::Lines(1)]
        );
        assert_eq!(
            press(&mut v, Ev::Char('k'), 0, &buf).ops,
            vec![Op::Lines(-1)]
        );
    }

    #[test]
    fn counts_multiply_motions() {
        let mut v = Vim {
            enabled: true,
            ..Default::default()
        };
        let buf = buf_of("abcdef");
        press(&mut v, Ev::Char('3'), 0, &buf);
        assert_eq!(
            press(&mut v, Ev::Char('l'), 0, &buf).ops,
            vec![Op::Chars(3)]
        );
        // the count is consumed, so the next `l` is a plain step
        assert_eq!(
            press(&mut v, Ev::Char('l'), 0, &buf).ops,
            vec![Op::Chars(1)]
        );
    }

    /// `0` is "column zero" only when no count is being typed; otherwise it is
    /// a digit.
    #[test]
    fn zero_after_a_count_is_a_digit() {
        let mut v = Vim {
            enabled: true,
            ..Default::default()
        };
        let buf = buf_of("abcdef");
        press(&mut v, Ev::Char('1'), 0, &buf);
        press(&mut v, Ev::Char('0'), 0, &buf);
        assert_eq!(
            press(&mut v, Ev::Char('l'), 0, &buf).ops,
            vec![Op::Chars(10)]
        );
        // and on its own `0` jumps to the line start
        assert_eq!(press(&mut v, Ev::Char('0'), 3, &buf).ops, vec![Op::Goto(0)]);
    }

    #[test]
    fn insert_mode_types_until_escape() {
        let mut v = Vim {
            enabled: true,
            ..Default::default()
        };
        let buf = buf_of("");
        press(&mut v, Ev::Char('i'), 0, &buf);
        assert_eq!(v.mode, Mode::Insert);
        assert_eq!(
            press(&mut v, Ev::Char('a'), 0, &buf).ops,
            vec![Op::Insert("a".into())]
        );
        press(&mut v, Ev::Key(K::Escape), 0, &buf);
        assert_eq!(v.mode, Mode::Normal);
    }

    #[test]
    fn capital_i_enters_insert_at_the_first_nonblank() {
        let mut v = Vim {
            enabled: true,
            ..Default::default()
        };
        let buf = buf_of("    indented");
        press(&mut v, Ev::Char('I'), 0, &buf);
        assert_eq!(v.mode, Mode::Insert);
        // re-derive: pressing I with the caret anywhere jumps to col 4
        assert_eq!(buf.first_nonblank, 4);
    }

    #[test]
    fn o_and_o_open_a_line() {
        let mut v = Vim {
            enabled: true,
            ..Default::default()
        };
        let buf = buf_of("text");
        assert_eq!(
            press(&mut v, Ev::Char('o'), 0, &buf).ops,
            vec![Op::OpenLine { below: true }]
        );
        assert_eq!(v.mode, Mode::Insert);
        // back to Normal before trying the other one
        press(&mut v, Ev::Key(K::Escape), 0, &buf);
        assert_eq!(
            press(&mut v, Ev::Char('O'), 0, &buf).ops,
            vec![Op::OpenLine { below: false }]
        );
        assert_eq!(v.mode, Mode::Insert);
    }

    #[test]
    fn dd_deletes_the_line_and_yanks_it() {
        let mut v = Vim {
            enabled: true,
            ..Default::default()
        };
        let buf = buf_of("one\ntwo\nthree");
        press(&mut v, Ev::Char('d'), 4, &buf);
        assert!(v.awaiting_motion());
        assert_eq!(
            press(&mut v, Ev::Char('d'), 4, &buf).ops,
            vec![Op::DeleteLines(1)]
        );
    }

    #[test]
    fn counted_dd_deletes_that_many_lines() {
        let mut v = Vim {
            enabled: true,
            ..Default::default()
        };
        let buf = buf_of("one\ntwo\nthree\nfour");
        press(&mut v, Ev::Char('3'), 0, &buf);
        press(&mut v, Ev::Char('d'), 0, &buf);
        assert!(v.awaiting_motion());
        assert_eq!(
            press(&mut v, Ev::Char('d'), 0, &buf).ops,
            vec![Op::DeleteLines(3)]
        );
    }

    #[test]
    fn dj_deletes_the_caret_line_and_the_next() {
        let mut v = Vim {
            enabled: true,
            ..Default::default()
        };
        let buf = buf_of("one\ntwo\nthree");
        press(&mut v, Ev::Char('d'), 0, &buf);
        assert_eq!(
            press(&mut v, Ev::Char('j'), 0, &buf).ops,
            vec![Op::DeleteLines(2)]
        );
        // and `d3j` spans four lines in total
        let mut v = Vim {
            enabled: true,
            ..Default::default()
        };
        press(&mut v, Ev::Char('d'), 0, &buf);
        press(&mut v, Ev::Char('3'), 0, &buf);
        assert_eq!(
            press(&mut v, Ev::Char('j'), 0, &buf).ops,
            vec![Op::DeleteLines(4)]
        );
    }

    #[test]
    fn counted_yy_yanks_that_many_lines() {
        let mut v = Vim {
            enabled: true,
            ..Default::default()
        };
        let buf = buf_of("one\ntwo\nthree\nfour");
        press(&mut v, Ev::Char('3'), 0, &buf);
        press(&mut v, Ev::Char('y'), 0, &buf);
        assert_eq!(
            press(&mut v, Ev::Char('y'), 0, &buf).ops,
            vec![Op::YankLines(3)]
        );
    }

    #[test]
    fn cc_clears_the_line_and_enters_insert() {
        let mut v = Vim {
            enabled: true,
            ..Default::default()
        };
        let buf = buf_of("one\ntwo\nthree");
        press(&mut v, Ev::Char('c'), 0, &buf);
        assert_eq!(
            press(&mut v, Ev::Char('c'), 0, &buf).ops,
            vec![Op::ChangeLines(1)]
        );
        assert_eq!(v.mode, Mode::Insert);
        // a count groups lines: `3cc` clears three
        let mut v = Vim {
            enabled: true,
            ..Default::default()
        };
        press(&mut v, Ev::Char('3'), 0, &buf);
        press(&mut v, Ev::Char('c'), 0, &buf);
        assert_eq!(
            press(&mut v, Ev::Char('c'), 0, &buf).ops,
            vec![Op::ChangeLines(3)]
        );
    }

    #[test]
    fn operator_then_motion_forms_a_range() {
        let mut v = Vim {
            enabled: true,
            ..Default::default()
        };
        let mut buf = buf_of("hello world");
        buf.line_start = 0;
        press(&mut v, Ev::Char('d'), 0, &buf);
        // `dw` deletes the word
        let plan = v.resolve_operator(0, Op::Chars(1), &buf);
        assert_eq!(
            plan.ops,
            vec![Op::Yank { from: 0, to: 1 }, Op::Delete { from: 0, to: 1 }]
        );
    }

    /// Regression: `dl` at the end of a line once also deleted the newline
    /// (Op::Chars had no line_end upper clamp, equivalent to accidentally
    /// running J). A plain `l` goes through vim_line_hi's clamp; the operator
    /// path (motion_end) must have the same semantics.
    #[test]
    fn dl_at_line_end_does_not_swallow_newline() {
        let mut v = Vim {
            enabled: true,
            ..Default::default()
        };
        // Line content "abc" (line_end=3); the terminator sits at line_end
        let mut buf = buf_of("abc");
        buf.line_end = 3;
        buf.line_end_with_term = 4;
        buf.next_line_start = 4;
        press(&mut v, Ev::Char('d'), 2, &buf);
        let plan = v.resolve_operator(2, Op::Chars(1), &buf);
        assert_eq!(
            plan.ops,
            vec![Op::Yank { from: 2, to: 3 }, Op::Delete { from: 2, to: 3 }],
            "dl at line end only deletes the last character, not the newline"
        );
    }

    #[test]
    fn change_enters_insert_mode_and_keeps_the_range() {
        let mut v = Vim {
            enabled: true,
            ..Default::default()
        };
        let buf = buf_of("hello");
        press(&mut v, Ev::Char('c'), 0, &buf);
        let plan = v.resolve_operator(0, Op::Chars(3), &buf);
        assert_eq!(plan.ops, vec![Op::Change(Some(0))]);
        assert_eq!(v.mode, Mode::Insert);
    }

    #[test]
    fn indent_operators_do_not_touch_the_buffer_range() {
        let mut v = Vim {
            enabled: true,
            ..Default::default()
        };
        let buf = buf_of("  x");
        press(&mut v, Ev::Char('>'), 0, &buf);
        assert_eq!(
            v.resolve_operator(0, Op::Chars(1), &buf).ops,
            vec![Op::ShiftIndent { outdent: false }]
        );
    }

    #[test]
    fn visual_mode_extends_then_leaves() {
        let mut v = Vim {
            enabled: true,
            ..Default::default()
        };
        let buf = buf_of("abcdef");
        press(&mut v, Ev::Char('v'), 0, &buf);
        assert_eq!(v.mode, Mode::Visual);
        assert_eq!(
            press(&mut v, Ev::Char('l'), 2, &buf).ops,
            vec![Op::SelectTo(3)]
        );
        press(&mut v, Ev::Key(K::Escape), 2, &buf);
        assert_eq!(v.mode, Mode::Normal);
    }

    #[test]
    fn command_line_runs_w_and_wq() {
        let mut v = Vim {
            enabled: true,
            ..Default::default()
        };
        let buf = buf_of("");
        press(&mut v, Ev::Char(':'), 0, &buf);
        assert_eq!(v.mode, Mode::CommandLine);
        press(&mut v, Ev::Char('w'), 0, &buf);
        assert_eq!(
            press(&mut v, Ev::Key(K::Enter), 0, &buf).ops,
            vec![Op::Run(Cmd::Save)]
        );
        assert_eq!(v.mode, Mode::Normal);

        press(&mut v, Ev::Char(':'), 0, &buf);
        for c in "wq".chars() {
            press(&mut v, Ev::Char(c), 0, &buf);
        }
        assert_eq!(
            press(&mut v, Ev::Key(K::Enter), 0, &buf).ops,
            vec![Op::Run(Cmd::Save), Op::Run(Cmd::TabClose)]
        );
    }

    #[test]
    fn command_line_backspace_escapes_cancel() {
        let mut v = Vim {
            enabled: true,
            ..Default::default()
        };
        let buf = buf_of("");
        // the `:` prompt stays in the buffer so the user can see it while
        // typing; only the command itself is stripped when it runs
        press(&mut v, Ev::Char(':'), 0, &buf);
        press(&mut v, Ev::Char('x'), 0, &buf);
        assert_eq!(v.cmdline_text(), ":x");
        press(&mut v, Ev::Key(K::Backspace), 0, &buf);
        press(&mut v, Ev::Key(K::Backspace), 0, &buf);
        assert_eq!(v.cmdline_text(), ":");
        press(&mut v, Ev::Char('w'), 0, &buf);
        assert_eq!(v.cmdline_text(), ":w");
        press(&mut v, Ev::Key(K::Escape), 0, &buf);
        assert_eq!(v.mode, Mode::Normal);
        assert!(v.cmdline_text().is_empty());
    }

    #[test]
    fn an_unknown_command_reports_instead_of_panicking() {
        let mut v = Vim {
            enabled: true,
            ..Default::default()
        };
        let buf = buf_of("");
        press(&mut v, Ev::Char(':'), 0, &buf);
        press(&mut v, Ev::Char('z'), 0, &buf);
        press(&mut v, Ev::Char('z'), 0, &buf);
        let plan = press(&mut v, Ev::Key(K::Enter), 0, &buf);
        assert!(matches!(&plan.ops[0], Op::Status(s) if s.key() == "With:VimStUnknownCmd"));
    }

    #[test]
    fn disabled_vim_never_consumes_a_key() {
        let mut v = Vim::default();
        let buf = buf_of("x");
        assert!(v.handle_key(Ev::Char('i'), 0, &buf).is_none());
    }

    #[test]
    fn toggling_clears_pending_state() {
        let mut v = Vim {
            enabled: true,
            ..Default::default()
        };
        let buf = buf_of("x");
        press(&mut v, Ev::Char('d'), 0, &buf);
        assert!(v.awaiting_motion());
        v.toggle();
        assert!(!v.enabled);
        assert!(!v.awaiting_motion());
        assert_eq!(v.mode, Mode::Normal);
    }

    #[test]
    fn escape_is_only_swallowed_outside_normal_mode() {
        // in Normal mode Escape must reach the editor so it can close menus
        assert!(!Vim::should_prevent_default(Mode::Normal));
        assert!(Vim::should_prevent_default(Mode::Insert));
    }

    /// Regression: operator+motion `w`/`b` once returned `Op::Chars(±6)` (the
    /// "≈6 chars per word" approximation), so `dw` on `foo.bar` deleted
    /// `foo.ba` instead of one word.
    #[test]
    fn operator_with_w_uses_real_word_boundary() {
        let mut v = Vim {
            enabled: true,
            ..Default::default()
        };
        let buf = buf_of("foo.bar baz");
        press(&mut v, Ev::Char('d'), 0, &buf);
        let plan = press(&mut v, Ev::Char('w'), 0, &buf);
        // dw = from 0 to the next word start (the '.' position 3) —
        // exclusive endpoint
        assert_eq!(
            plan.ops,
            vec![Op::Yank { from: 0, to: 3 }, Op::Delete { from: 0, to: 3 },]
        );
    }

    /// Regression: `e` once produced `Op::Chars(6)` (an unbounded loop), and
    /// `de`'s range was a ±6 approximation; now `e` uses WordEnd and `de`
    /// deletes through the word end **including** the last character.
    #[test]
    fn operator_with_e_deletes_through_the_word_end() {
        let mut v = Vim {
            enabled: true,
            ..Default::default()
        };
        let buf = buf_of("hello world");
        press(&mut v, Ev::Char('d'), 0, &buf);
        let plan = press(&mut v, Ev::Char('e'), 0, &buf);
        // "hello" is 0..5 (e included); de's exclusive endpoint is 5
        assert_eq!(
            plan.ops,
            vec![Op::Yank { from: 0, to: 5 }, Op::Delete { from: 0, to: 5 },]
        );
        // Pressing e alone: the cursor should move to the word-end character
        // (index 4)
        let mut v2 = Vim {
            enabled: true,
            ..Default::default()
        };
        let plan = press(&mut v2, Ev::Char('e'), 0, &buf);
        assert_eq!(plan.ops, vec![Op::WordEnd(1)]);
    }

    /// Regression: `999999e` once expanded into ~6 million per-character
    /// moves; now `e` is WordEnd and the count is handed to the bridge's scan
    /// loop (bounded, advancing one step at a time).
    #[test]
    fn counted_e_is_word_end_not_character_loop() {
        let mut v = Vim {
            enabled: true,
            ..Default::default()
        };
        let buf = buf_of("a b c d e");
        for c in "999999".chars() {
            press(&mut v, Ev::Char(c), 0, &buf);
        }
        let plan = press(&mut v, Ev::Char('e'), 0, &buf);
        assert_eq!(plan.ops, vec![Op::WordEnd(999999)]);
    }
}
