//! In-app modal dialog model (About / Summary / Table view / Markdown preview
//! / Preferences) and the [`EditorState`](super::EditorState) hooks that show
//! them.

use super::EditorState;
use i18n::Message;

/// Body of an in-app modal dialog (see [`crate::ui::components::dialog`]).
#[derive(Clone, Debug)]
pub enum DialogBody {
    /// Plain text (About, MD5/SHA-1 summary, error notes…). Holds a message
    /// **key**, not a rendered string, so an open dialog follows a language
    /// switch.
    Text(Message),
    /// A data table (CSV table view): header row + data rows.
    Table {
        header: Vec<String>,
        rows: Vec<Vec<String>>,
    },
    /// Pre-rendered HTML (Markdown preview). Content is escaped by
    /// [`wysiwyg::preview_html`] before it gets here.
    Html(String),
    /// Settings → Preferences: a **live** settings panel.
    ///
    /// Unlike the others this is not a snapshot — it renders straight from
    /// [`EditorState`], so flipping a switch inside the dialog updates the
    /// editor and the panel in the same frame.
    Prefs,
}

/// A button in the dialog's action row.
///
/// `cmd` is `None` for a pure "dismiss" button; anything else runs the command
/// and then closes the dialog. `primary` marks the **recommended** action:
/// it renders with the theme accent (Fluent primary button); the other
/// buttons use non-accent colors.
#[derive(Clone, Debug)]
pub struct DialogButton {
    pub label: Message,
    pub cmd: Option<crate::ui::actions::Cmd>,
    pub primary: bool,
}

/// A single-line text field inside a dialog (Run…, Save macro…).
///
/// The buffer lives in the *state*, not in a DOM node, because the editor has
/// exactly one real focus target (the composer's transparent textarea — see
/// `components::composer`). Keys are routed here by the composer while the
/// dialog is up, and the submit command reads the value back through
/// [`EditorState::dialog_input`].
#[derive(Clone, Debug)]
pub struct DialogInput {
    /// current text
    pub value: String,
    /// greyed prompt shown while `value` is empty (message key, not a
    /// rendered string — see [`Dialog::title`])
    pub hint: Message,
    /// Enter / OK runs this; it reads the buffer via `dialog_input()`
    pub submit: crate::ui::actions::Cmd,
}

/// One modal dialog: a title, a body and an optional action row.
#[derive(Clone, Debug)]
pub struct Dialog {
    /// Generation id (incremented on each `Dialog::new`). After a button
    /// click, distinguish "this command put up a **new** dialog" from "the
    /// dialog was not swapped": only the latter leaves closing to the button —
    /// the discard-snapshot confirmation and the recovery prompt returned to
    /// after cancel both rely on it to stay on screen.
    pub id: u64,
    /// Title as a message **key** (`Msg` / `Fmt`): rendering happens at draw
    /// time in [`crate::ui::components::dialog`], so an open dialog follows a
    /// language switch instead of freezing into the language it opened with.
    pub title: Message,
    pub body: DialogBody,
    pub buttons: Vec<DialogButton>,
    /// Ask-to-confirm dialogs must not be dismissed by a stray click on the
    /// dimmed backdrop, or the "are you sure" question answers itself.
    pub dismissable: bool,
    /// A text field above the body (see [`DialogInput`]); `None` for the
    /// information-only dialogs that came first.
    pub input: Option<DialogInput>,
}

impl Dialog {
    fn new(title: impl Into<Message>, body: DialogBody) -> Self {
        static NEXT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        Self {
            id: NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            title: title.into(),
            body,
            buttons: Vec::new(),
            dismissable: true,
            input: None,
        }
    }

    /// Add a single-line text field. `submit` runs on Enter / OK and reads
    /// the typed text through [`EditorState::dialog_input`].
    pub fn with_input(mut self, hint: impl Into<Message>, submit: crate::ui::actions::Cmd) -> Self {
        self.input = Some(DialogInput {
            value: String::new(),
            hint: hint.into(),
            submit,
        });
        self
    }

    pub fn text(title: impl Into<Message>, body: impl Into<Message>) -> Self {
        Self::new(title, DialogBody::Text(body.into()))
    }

    pub fn table(title: impl Into<Message>, header: Vec<String>, rows: Vec<Vec<String>>) -> Self {
        Self::new(title, DialogBody::Table { header, rows })
    }

    pub fn html(title: impl Into<Message>, html: impl Into<String>) -> Self {
        Self::new(title, DialogBody::Html(html.into()))
    }

    /// Settings → Preferences.
    pub fn prefs() -> Self {
        Self::new(crate::ui::Msg::PrefsTitle, DialogBody::Prefs)
    }

    /// Append one action button.
    ///
    /// `cmd` of `None` makes it a plain close button (Cancel), so a
    /// confirmation
    /// dialog can be written as `.button(Msg::DlgQuitConfirm, Some(Cmd::ForceQuit))`.
    pub fn button(
        mut self,
        label: impl Into<Message>,
        cmd: Option<crate::ui::actions::Cmd>,
    ) -> Self {
        self.buttons.push(DialogButton {
            label: label.into(),
            cmd,
            primary: false,
        });
        self
    }

    /// Append the **primary** (recommended) action button — renders with the
    /// theme accent. A dialog should have at most one.
    pub fn primary(
        mut self,
        label: impl Into<Message>,
        cmd: Option<crate::ui::actions::Cmd>,
    ) -> Self {
        self.buttons.push(DialogButton {
            label: label.into(),
            cmd,
            primary: true,
        });
        self
    }

    /// A confirmation: clicking the backdrop does nothing, so the user has to
    /// pick a button.
    pub fn confirm(mut self) -> Self {
        self.dismissable = false;
        self
    }
}

impl EditorState {
    // ---- in-app modal dialogs ----------------------------------------------

    /// Show a modal dialog (About / MD5 / table view / Markdown preview…).
    pub fn show_dialog(&mut self, dialog: Dialog) {
        self.dialog = Some(dialog);
    }

    /// Close the modal dialog, if any.
    ///
    /// Closing = **abandoning** this request: the dialog's answers go through
    /// `Cmd::CloseSave` / `CloseDiscard` / `CloseCancel` (plus the two for
    /// Reload); those take over the pending marker in
    /// [`EditorState::finish_close`] / [`EditorState::finish_reload`] before
    /// closing the frame. The only paths reaching here are "dismissed without
    /// answering" — Esc and the title-bar ×. Both must reset the two-phase
    /// state machine, or a ghost request is left behind:
    ///
    /// * a stale `pending_close` → the next close confirmation uses the
    ///   previous request's doc id;
    /// * worse, a stale `close_all_pending` (Esc-cancel during Close All):
    ///   afterwards, closing **any** dirty document and clicking "Save" makes
    ///   `finish_close` sweep away all the remaining tabs — a single-tab
    ///   close becomes a close-all, with no per-file prompt.
    ///
    /// While a recovery session is still in the foreground (the user
    /// dismissed the recovery prompt with × / Esc, or the "discard snapshot"
    /// confirmation), bring the recovery prompt back: while
    /// `pending_recovery` is pending the recovery log stops writing (see
    /// [`crate::ui::recovery`]'s `recovery_tick_in`), and the "recover or
    /// discard" choice must not dangle unhandled. The two real
    /// recover/discard paths **clear `pending_recovery` before closing the
    /// frame** and never reach here.
    pub fn close_dialog(&mut self) {
        self.dialog = None;
        self.pending_close = None;
        self.pending_reload = None;
        self.close_all_pending = false;
        if self.pending_recovery.is_some() {
            self.show_recovery_prompt();
        }
    }

    /// The open dialog's text field, if it has one.
    pub fn dialog_input(&self) -> String {
        self.dialog
            .as_ref()
            .and_then(|d| d.input.as_ref())
            .map(|i| i.value.clone())
            .unwrap_or_default()
    }

    /// Append to the dialog's text field (keyboard and IME paths both end
    /// here). Returns `false` when no input dialog is open.
    pub fn dialog_input_insert(&mut self, s: &str) -> bool {
        match self.dialog.as_mut().and_then(|d| d.input.as_mut()) {
            Some(inp) => {
                inp.value.push_str(s);
                true
            }
            None => false,
        }
    }

    /// Backspace in the dialog's text field. Returns `false` when there is
    /// no field (a caller that already checked may ignore the result).
    pub fn dialog_input_backspace(&mut self) -> bool {
        match self.dialog.as_mut().and_then(|d| d.input.as_mut()) {
            Some(inp) => {
                inp.value.pop();
                true
            }
            None => false,
        }
    }

    /// Clear the dialog's text field (Ctrl+X).
    pub fn dialog_input_clear(&mut self) {
        if let Some(inp) = self.dialog.as_mut().and_then(|d| d.input.as_mut()) {
            inp.value.clear();
        }
    }

    /// Run the open input dialog's submit command and close it (Enter / OK).
    pub fn dialog_input_submit(&mut self) -> bool {
        let Some(submit) = self
            .dialog
            .as_ref()
            .and_then(|d| d.input.as_ref())
            .map(|i| i.submit)
        else {
            return false;
        };
        // Run first, close second: the submit command (RunExec /
        // MacroSaveNamed) reads the user's typing back through
        // `dialog_input()`; closing first would read an empty string.
        crate::ui::actions::run(self, submit);
        self.close_dialog();
        true
    }
}
