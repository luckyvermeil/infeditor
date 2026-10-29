//! Macro recording and playback (one of AGENTS.md's core requirements).
//!
//! # What gets recorded
//!
//! A macro is a sequence of [`MacroStep`]s: document commands ([`Cmd`]) and
//! **directly typed text**. Hooks sit in two places, covering "anything that
//! can change the document":
//!
//! * [`crate::ui::actions::run`] — commands triggered by menu/right-click/shortcut;
//! * [`crate::ui::commands`]'s `insert_text` / `delete_back` / `delete_fwd` /
//!   `insert_newline` / `insert_tab` — the composer's direct typing paths
//!   (backspace, enter, and Tab bypass `Cmd`; without them a macro could not
//!   even record a basic "foo↵bar" sequence).
//!
//! Both call [`EditorState::record_step`](crate::ui::state::EditorState::record_step),
//! a zero-cost no-op when not recording.
//!
//! # Which commands are not recorded
//!
//! A macro is recorded in order to be **replayed**, so "human-interaction"
//! commands like open-file dialogs, popups, and quit are explicitly excluded
//! (the [`MacroStep::Cmd`] filter lives in
//! [`EditorState::record_cmd`](crate::ui::state::EditorState::record_cmd)) —
//! otherwise a macro that accidentally hits quit would take the user's
//! working document down with it. During replay these steps are filtered
//! again even if they slipped in (recorded by older versions) — double
//! insurance.
//!
//! # Playback
//!
//! During playback
//! [`record_step`](crate::ui::state::EditorState::record_step) short-circuits
//! (the `macro_playing` latch); otherwise playback would record itself and
//! the macro would double in size on every run — the most classic bug of this
//! kind of feature.

use crate::ui::actions::Cmd;
use crate::ui::state::EditorState;
use crate::ui::{Fmt, Msg};

/// One macro step: a command, a stretch of typed text, or an editing key that
/// bypasses `Cmd`.
#[derive(Clone, Debug, PartialEq)]
pub enum MacroStep {
    /// Menu / shortcut command
    Cmd(Cmd),
    /// Typed text (an IME commit can be multi-character; replayed verbatim)
    Text(String),
    /// Backspace
    Backspace,
    /// Delete
    Delete,
    /// Enter
    Enter,
    /// Tab
    Tab,
}

/// Maximum number of steps in a replay.
///
/// Defensive cap: no reasonable macro exceeds it, and a tampered macro (say,
/// rewritten externally) must not be able to hang the editor.
pub const MAX_STEPS: usize = 10_000;

impl EditorState {
    /// Start / stop recording (one command; toggles on press).
    ///
    /// Stopping does **not** discard what was just recorded: it moves to
    /// `macro_last` for playback and stays there until the next recording
    /// overwrites it — "record → play repeatedly" is the basic usage. Returns
    /// the status-bar hint text; the caller displays it directly.
    pub fn macro_toggle_record(&mut self) -> Option<i18n::Message> {
        if let Some(steps) = self.macros.rec.take() {
            self.macros.last = steps;
            return Some(Msg::StatusMacroRecordStopped.into());
        }
        self.macros.rec = Some(Vec::new());
        Some(Msg::StatusMacroRecordStarted.into())
    }

    /// Whether recording is in progress.
    pub fn is_recording(&self) -> bool {
        self.macros.rec.is_some()
    }

    /// Number of steps available for playback (while recording = steps
    /// recorded so far; after stopping = the most recent macro).
    pub fn macro_len(&self) -> usize {
        match &self.macros.rec {
            Some(v) => v.len(),
            None => self.macros.last.len(),
        }
    }

    /// Record one step. A no-op when not recording (or while playing back).
    pub fn record_step(&mut self, step: MacroStep) {
        match &mut self.macros.rec {
            // Latch: do not record during playback, or playback would
            // replicate itself.
            Some(steps) if !self.macros.playing && steps.len() < MAX_STEPS => {
                steps.push(step);
            }
            _ => {}
        }
    }

    /// Record a command. Non-replayable commands are dropped here.
    pub fn record_cmd(&mut self, cmd: Cmd) {
        if cmd.is_recordable() {
            self.record_step(MacroStep::Cmd(cmd));
        }
    }

    /// Play the macro back.
    ///
    /// While recording, plays back the **current** buffer ("try as you
    /// record"); otherwise plays the last completed macro. Returns the
    /// status-bar message. Playback clears neither buffer.
    pub fn macro_play(&mut self) -> i18n::Message {
        let steps = match &self.macros.rec {
            Some(v) => v.clone(),
            None => self.macros.last.clone(),
        };
        if steps.is_empty() {
            return Msg::StatusMacroEmpty.into();
        }
        self.macro_play_steps(steps)
    }

    /// Play back an arbitrary set of steps (shared by [`Self::macro_play`]
    /// and macros loaded from disk).
    ///
    /// Two gates: `record_step` short-circuits during playback (no
    /// self-replication); and every command step passes
    /// [`Cmd::is_recordable`] — a file on disk may have been hand-edited, and
    /// a macro containing "quit" has no business taking the user's document
    /// down.
    pub fn macro_play_steps(&mut self, steps: Vec<MacroStep>) -> i18n::Message {
        if steps.is_empty() {
            return Msg::StatusMacroEmpty.into();
        }
        // Drop the latch first: the record hooks inside run / insert_text
        // short-circuit on it. The reset must also happen when playback panics;
        // a scoped guard cannot hold `&mut self` while the steps call
        // `actions::run(self, …)`, so catch the unwind here, clear the flag, and
        // resume the panic unchanged.
        self.macros.playing = true;
        let mut n = 0usize;
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            for step in steps.into_iter().take(MAX_STEPS) {
                match step {
                    MacroStep::Cmd(cmd) if cmd.is_recordable() => {
                        crate::ui::actions::run(self, cmd);
                        n += 1;
                    }
                    MacroStep::Cmd(_) => {}
                    MacroStep::Text(text) => {
                        self.insert_text(&text);
                        n += 1;
                    }
                    MacroStep::Backspace => {
                        self.delete_back();
                        n += 1;
                    }
                    MacroStep::Delete => {
                        self.delete_fwd();
                        n += 1;
                    }
                    MacroStep::Enter => {
                        self.insert_newline();
                        n += 1;
                    }
                    MacroStep::Tab => {
                        self.insert_tab();
                        n += 1;
                    }
                }
            }
        }));
        self.macros.playing = false;
        if let Err(payload) = outcome {
            std::panic::resume_unwind(payload);
        }
        Fmt::StatusMacroPlayed { steps: n }.into()
    }
}

impl Cmd {
    /// Whether this command may be recorded into — and replayed from — a macro.
    ///
    /// **The rule.** A command is replayable iff its effect on the document is
    /// a deterministic function of the document and the editor state, and it
    /// does not
    ///
    /// 1. require or produce human interaction (native file/directory pickers,
    ///    confirmation flows, modal result dialogs, an input box),
    /// 2. hand work to another process or the browser, or
    /// 3. flip a *session* preference, so a replay would silently change the
    ///    mode the user is working in (theme, interface language, vim mode).
    ///
    /// **Why the table is written out in full.** It used to be a deny list with
    /// a `_ => true` catch-all, while the doc comment claimed "deny by
    /// default" — so a new interactive command became silently recordable
    /// until someone remembered to add it. The match below has no wildcard:
    /// adding a variant to [`Cmd`] does not compile until it is classified
    /// here, so the answer is a one-word decision at the point the command is
    /// born instead of a list surgery later.
    ///
    /// View toggles (theme aside — that one is a session preference) *are*
    /// replayable: they only switch which surface renders the same document,
    /// and a macro recorded while flipping them has to replay the flip.
    pub fn is_recordable(self) -> bool {
        use Cmd::*;
        match self {
            // ---- Native pickers and confirmation flows ----------------------
            // A replay would either block on a modal system dialog or act on a
            // file the user never chose.
            Open | OpenFolder | OpenWorkspace | OpenRecent(_) | SaveAs | Reload => false,
            Close | CloseAll | CloseSave | CloseDiscard | CloseCancel | Quit | ForceQuit => false,
            ReloadConfirm | ReloadCancel => false,
            RecoveryRestore | RecoveryDiscard | RecoveryDiscardConfirm | RecoveryDiscardCancel => {
                false
            }
            // WYS "export HTML / Markdown": `rfd::FileDialog` asks for a path
            // every single time, so a replay would block on a system dialog.
            // (`Save` stays recordable: it only prompts for an *untitled*
            // document, and a macro recorded around that prompt is what the
            // user actually did.)
            MdExportHtml | MdExportMd => false,

            // ---- Commands that take over the next keystrokes ----------------
            // The find bar / goto box / numbering popup / settings / shortcut
            // list all grab input, and the digest and word-count commands
            // answer through a modal dialog: replaying them would stack
            // dialogs or leave a window waiting for input on every run.
            Find(_, _) | Goto => false,
            InsertNumbering => false,
            Preferences | PreferencesClosed | ShortcutHelp | About => false,
            Md5 | Sha1 | WordCount | CharCode => false,

            // ---- Window and tab lifecycle -----------------------------------
            // The window manager's own verbs (move/resize/maximize each start a
            // system modal loop) and tab closing, which would act on whatever
            // tab happens to be active when the macro is replayed.
            TabClose | TabCloseAll => false,
            AlwaysOnTop | WindowToggleMaximize | WindowMinimize | WindowMove | WindowResize => {
                false
            }

            // ---- Session preferences ----------------------------------------
            // These would silently change the input mode or the interface
            // language on every replay.
            ToggleVim | ToggleTheme | SetLocale(_) => false,

            // ---- The recording machinery itself -----------------------------
            // Reflexive, and each one acts on "this session" rather than on the
            // document.
            MacroRecord | MacroPlayback | MacroSave | MacroSaveNamed | MacroRun
            | MacroPlaySaved(_) | MacroDir => false,

            // ---- Other processes --------------------------------------------
            // Human interaction in a terminal or a browser; a replay would fire
            // them again with whatever state the machine is in by then.
            RunCommand | RunExec | RunAgain(_) | WebSearch => false,

            // ---- Replayable: file -------------------------------------------
            New | Save | SaveAll => true,

            // ---- Replayable: edit / selection -------------------------------
            Undo | Redo | Cut | Copy | Paste | Delete | SelectAll | CopyAll => true,
            CopyFileName | CopyFilePath | BeginEndSelect | BeginEndSelectColumn => true,
            GotoBracket | ToggleAutoClose => true,
            BookmarkToggle | BookmarkNext | BookmarkPrev => true,
            ToggleReadOnly | ToggleSidebar | ToggleOutline => true,
            MarkStyle(_, _) | MarkClear(_) => true,
            HideLines | ShowHiddenLines => true,

            // ---- Replayable: line / whitespace / case / comment -------------
            Case(_) | SortLines(_, _) => true,
            DuplicateLine | DeleteDupLine | DeleteConsecutiveDupLine => true,
            SplitLines | JoinLines | MoveLineUp | MoveLineDown => true,
            InsertBlankAbove | InsertBlankBelow | ReverseLines | RandomizeLines => true,
            TrimBoth | TrimLeading | TrimTrailing | EolToSpace => true,
            TabToSpace | SpaceToTab | SpaceToTabLeading => true,
            RemoveEmptyLines | RemoveEmptyLinesWithBlank => true,
            ToggleLineComment | BlockComment | BlockUncomment => true,
            UnicodeNfc | UnicodeNfd | InsertShortDateTime | InsertLongDateTime => true,

            // ---- Replayable: find (the bar itself is not) -------------------
            FindNext | FindPrev => true,

            // ---- Replayable: view toggles -----------------------------------
            ZoomIn | ZoomOut | ZoomReset => true,
            ToggleHighlight | HighlightAll | WordWrap | ShowEol | ShowWs => true,
            ToggleMinimap | ToggleJsonl | JsonlCollapseAll | JsonlFoldLine | JsonlUnfoldLine => {
                true
            }
            JsonPreview | JsonCollapseAll | JsonExpandAll => true,
            ToggleCaretBlink | CycleCaretStyle => true,

            // ---- Replayable: encoding / language ----------------------------
            // Both are document properties, not session preferences: they change
            // how the same bytes are interpreted and travel with the file.
            SetEncoding(_) | SetNewline(_) | Lang(_) => true,

            // ---- Replayable: document views (wys / csv / jsonl) -------------
            // Same reason as the view toggles above: a surface switch, not a
            // session preference.
            MdWys | MdText | CsvTable | CsvToggleMode | CsvDelim(_) | CsvToggleHeader => true,
            MdEdit(_) | WysTool(_) | WysUndo | WysRedo => true,
            // The *submits* of the in-app link/image boxes are replayable (they
            // read whatever the box holds); the exports are not (see above).
            WysLinkSubmit | WysImageSubmit => true,

            // ---- Replayable: csv table edits --------------------------------
            CsvSort(_) | CsvRowAbove | CsvRowBelow | CsvRowDel => true,
            CsvColLeft | CsvColRight | CsvColDel => true,

            // ---- Replayable: tools that rewrite the document ----------------
            FormatJson | FormatXml => true,

            // ---- Replayable: caret navigation -------------------------------
            TabNext | TabPrev => true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::textops::CaseKind;
    use crate::ui::testutil::st_of;

    /// Newline normalization: an empty document's detected_newline is CRLF on
    /// Windows; the assertions care about content, not line-ending shape.
    fn norm(s: &str) -> String {
        s.replace("\r\n", "\n").replace('\r', "\n")
    }

    #[test]
    fn record_and_replay_types_text_and_newlines() {
        let mut st = st_of("");
        st.macro_toggle_record();
        assert!(st.is_recording());

        st.insert_text("foo");
        st.insert_newline();
        st.insert_text("bar");
        st.delete_back();
        st.insert_text("!");
        st.macro_toggle_record();
        assert!(!st.is_recording());
        assert_eq!(
            st.macro_len(),
            5,
            "macro survives in macro_last after stop, ready for playback"
        );

        // Recording itself already wrote the text into the document; playback
        // repeats **the same sequence of operations**, so it appends rather
        // than overwrites.
        let after_record = norm(&st.active_doc().read_all());
        assert_eq!(
            after_record, "foo\nba!",
            "document content when recording stops"
        );
        let out = st.macro_play();
        assert_eq!(out.key(), "With:StatusMacroPlayed", "{out:?}");
        assert_eq!(
            norm(&st.active_doc().read_all()),
            format!("{after_record}{after_record}"),
            "playback = the same sequence of operations again"
        );
        // A second playback keeps appending.
        st.macro_play();
        assert_eq!(
            norm(&st.active_doc().read_all()),
            format!("{after_record}{after_record}{after_record}")
        );
    }

    #[test]
    fn commands_run_while_recording_are_captured() {
        let mut st = st_of("hello\nworld\n");
        st.set_caret(0, false);
        st.macro_toggle_record();
        // Record through the unified run entry (exactly the menu/shortcut
        // path).
        crate::ui::actions::run(&mut st, Cmd::Case(CaseKind::Upper));
        crate::ui::actions::run(&mut st, Cmd::MoveLineDown);
        st.macro_toggle_record();

        // Replay the same macro on another document (select all first;
        // commands act on the selection).
        let mut st2 = st_of("alpha\nbeta\n");
        st2.select_all();
        st2.macros.last = vec![MacroStep::Cmd(Cmd::Case(CaseKind::Upper))];
        st2.macro_play();
        assert_eq!(norm(&st2.active_doc().read_all()), "ALPHA\nBETA\n");
    }

    /// Human-interaction commands must never enter a macro: a macro that
    /// accidentally hits "quit" must not take the user's document down.
    #[test]
    fn interactive_commands_are_never_recorded() {
        use Cmd::*;
        let deny = [
            Quit,
            ForceQuit,
            Open,
            SaveAs,
            Find(false, false),
            Goto,
            Preferences,
            About,
            ToggleVim,
            WebSearch,
            RunExec,
        ];
        for c in deny {
            assert!(
                !c.is_recordable(),
                "{c:?} is not replayable and must not be recorded"
            );
        }
        let allow = [
            Undo,
            Redo,
            Copy,
            Paste,
            SelectAll,
            FindNext,
            FindPrev,
            Save,
            ZoomIn,
            DuplicateLine,
            TabNext,
        ];
        for c in allow {
            assert!(c.is_recordable(), "{c:?} is a repeatable document command");
        }
    }

    /// Where the classification table draws its line: switching *which surface
    /// renders the document* is replayable, switching *the session's mode* is
    /// not.
    ///
    /// Three commands changed sides when the deny list became an exhaustive
    /// table, all because the list contradicted itself or missed a picker:
    ///
    /// * `CsvTable` was denied while its in-table twin `CsvToggleMode` was
    ///   allowed — `csv_toggle_table` / `csv_toggle_mode` are the same kind of
    ///   view toggle, so denying one was an accident of how the list grew.
    /// * `WordCount` / `CharCode` were allowed while `Md5` / `Sha1` were
    ///   denied, although all four answer through the same modal dialog
    ///   (`show_dialog`) — a replayed macro would have stacked one dialog per
    ///   run.
    /// * `MdExportHtml` / `MdExportMd` and `InsertNumbering` were allowed, but
    ///   each opens a **native save dialog** / an owned numbering window: a
    ///   replay would block on a path prompt or leave a window waiting for
    ///   input. They now sit with `SaveAs` / `Find`, denied for the same
    ///   reason.
    #[test]
    fn surface_toggles_replay_but_session_modes_do_not() {
        use Cmd::*;
        for c in [
            CsvTable,
            CsvToggleMode,
            MdWys,
            MdText,
            WordWrap,
            ToggleMinimap,
        ] {
            assert!(c.is_recordable(), "{c:?} only switches the surface");
        }
        for c in [ToggleVim, ToggleTheme, SetLocale(i18n::Locale::EnUs)] {
            assert!(
                !c.is_recordable(),
                "{c:?} changes the session's mode, not the document"
            );
        }
        for c in [
            WordCount,
            CharCode,
            Md5,
            Sha1,
            Preferences,
            ShortcutHelp,
            // Each of these opens a native dialog or an owned popup window.
            MdExportHtml,
            MdExportMd,
            InsertNumbering,
        ] {
            assert!(
                !c.is_recordable(),
                "{c:?} opens a dialog and must not replay"
            );
        }
    }

    /// Playback must never record itself — otherwise the macro doubles on
    /// every run.
    #[test]
    fn replaying_does_not_grow_the_macro() {
        let mut st = st_of("");
        st.macro_toggle_record();
        st.insert_text("x");
        crate::ui::actions::run(&mut st, Cmd::DuplicateLine);
        st.macro_toggle_record();
        let len_before = st.macros.last.len();

        // Playback commands can leave a selection (e.g. duplicate line
        // selects the whole line); clear it before playing so "insert" does
        // not become "replace selection" — legal but hard to assert.
        st.clear_selection();
        let doc0 = st.active_doc().read_all();
        st.macro_play();
        let doc1 = st.active_doc().read_all();
        st.clear_selection();
        st.macro_play();
        assert_eq!(
            st.macros.last.len(),
            len_before,
            "playback must not change the macro itself"
        );
        assert_ne!(doc1, doc0, "the first playback changed the document");
        assert_ne!(
            st.active_doc().read_all(),
            doc1,
            "the second playback changed the document again"
        );
    }

    /// Non-replayable commands are skipped during playback even if they slip
    /// into the buffer.
    #[test]
    fn replay_skips_interactive_steps() {
        let mut st = st_of("abc");
        st.macros.rec = Some(vec![MacroStep::Cmd(Cmd::Quit), MacroStep::Text("z".into())]);
        st.macro_play();
        assert_eq!(
            st.active_doc().read_all(),
            "zabc",
            "Quit skipped, text replayed as usual"
        );
    }

    /// Playing with nothing recorded must report a hint, not panic or
    /// silently succeed.
    #[test]
    fn play_without_a_recording_reports_instead_of_panicking() {
        let mut st = st_of("");
        let out = st.macro_play();
        assert_eq!(out.key(), "Plain:StatusMacroEmpty", "{out:?}");
    }
}
