//! Background file opening: worker threads, progress reporting and the
//! native-dialog message pump the app drains once per frame.

use std::path::PathBuf;
use std::sync::Arc;

use super::docs::same_path;
use super::{EditorState, Tab};
use crate::ui::{Fmt, Msg};

/// Poke the window so the main loop runs another frame.
///
/// `Window::request_redraw` is one of the very few winit calls that is safe
/// from an arbitrary thread (the `Window` trait is `Send + Sync`), which is
/// exactly what lets a worker thread announce "your document is ready".
pub type WakeFn = std::sync::Arc<dyn Fn() + Send + Sync>;

/// A background open's failure.
///
/// The io variant carries the engine error **whole** rather than flattened
/// into a string at the thread boundary: the status bar renders it at draw
/// time, and `source()` still reaches the OS error behind it.
#[derive(Debug)]
pub enum OpenError {
    /// an io/decode failure from the worker
    Io(crate::io::file::OpenError),
    /// the worker's channel closed before sending a result
    ThreadDied,
}

/// What to do with the result of a background open.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum OpenIntent {
    /// append a new tab
    NewTab,
    /// replace the tab that was showing `doc_id` at `index`
    Replace { index: usize, doc_id: u64 },
}

/// Live byte-level progress of an in-flight background open.
///
/// The worker writes both counters with `Relaxed` stores; the UI only ever
/// needs an approximate reading for the status bar, so no synchronization
/// beyond atomicity is warranted.
#[derive(Debug)]
pub struct OpenProgress {
    pub done: std::sync::atomic::AtomicU64,
    pub total: std::sync::atomic::AtomicU64,
}

impl OpenProgress {
    fn new() -> Self {
        Self {
            done: std::sync::atomic::AtomicU64::new(0),
            total: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// Completion in `0.0..=1.0`; `None` when the size is not known yet.
    pub fn fraction(&self) -> Option<f32> {
        use std::sync::atomic::Ordering::Relaxed;
        let total = self.total.load(Relaxed);
        if total == 0 {
            return None;
        }
        let done = self.done.load(Relaxed).min(total);
        Some(done as f32 / total as f32)
    }
}

/// A file being read, decoded and indexed on a worker thread.
///
/// Reading a multi-hundred-megabyte file and building its line index takes long
/// enough to be a visible freeze, so it happens off the UI thread; the result
/// arrives over `rx` and is collected by [`EditorState::poll_opens`], while
/// `progress` drives the status bar in the meantime.
pub struct PendingOpen {
    pub path: PathBuf,
    pub intent: OpenIntent,
    pub started: std::time::Instant,
    /// size of the source file, read once when the open was queued
    pub size: u64,
    pub progress: Arc<OpenProgress>,
    pub rx: std::sync::mpsc::Receiver<Result<crate::io::Opened, OpenError>>,
}

/// A background scan that is finishing a progressive document's line index.
///
/// The worker holds an immutable snapshot of the piece table and streams
/// [`IndexPatch`](crate::buffer::line_index::IndexPatch)es back every ~4 MB; [`EditorState::poll_index_jobs`] folds
/// them into the live document as long as the document has not been edited
/// since the snapshot (a `gen` mismatch restarts the scan from the new
/// watermark — cheap, because the watermark keeps all completed work).
pub struct IndexJob {
    pub doc_id: u64,
    /// document generation the snapshot was taken at
    pub gen: u64,
    pub rx: std::sync::mpsc::Receiver<crate::buffer::line_index::IndexPatch>,
    pub progress: Arc<OpenProgress>,
    pub cancel: Arc<std::sync::atomic::AtomicBool>,
    pub started: std::time::Instant,
}

impl EditorState {
    // ---- background opening ----------------------------------------------

    /// Open `path` on a worker thread so a huge file cannot freeze the editor.
    pub fn open_path_async(&mut self, path: PathBuf) {
        self.open_async(path, OpenIntent::NewTab);
    }

    /// True while at least one background open is still running.
    pub fn is_loading(&self) -> bool {
        !self.pending_opens.is_empty()
    }

    /// Take over an "open this file" hand-off from a second launch
    /// (`ui::single`), and bring the window forward.
    ///
    /// Called from the idle-frame hook; the frame only reaches here when the
    /// probe ([`Self::needs_idle_poll`]) saw a queued request, so the disk read
    /// and the window activation stay off the idle path.
    ///
    /// An **empty** request is the "just focus me" case (double-clicking the
    /// executable while it already runs): the window comes forward, nothing
    /// opens.
    pub fn poll_remote_open_requests(&mut self) {
        let Some(paths) = crate::ui::single::take_pending() else {
            return;
        };
        // Before opening: the user's click landed in Explorer, so the window
        // that is about to show the file may well be behind it.
        winshell::win::frameless::activate();
        if paths.is_empty() {
            return;
        }
        let msg = self.open_dropped(&paths);
        self.status = Some(msg);
    }

    /// Queue every path from a file drop and describe the outcome.
    ///
    /// Each file is read on its own worker thread, so dropping a 2 GB file does
    /// not freeze the editor. Directories are ignored rather than reported as
    /// errors — dropping a folder is a common mistake, not a failure.
    ///
    /// Returns the status-bar text. Keeping this out of the winit handler is
    /// what makes the drop path testable without synthesising an OLE drag.
    ///
    /// The same function serves three doors that all mean "open these": a drag
    /// onto the window, a drop onto `infeditor.exe` relayed by a second launch
    /// (`ui::single`), and the command line of the primary instance.
    pub fn open_dropped(&mut self, paths: &[PathBuf]) -> i18n::Message {
        if paths.is_empty() {
            return Msg::StatusDropNoFiles.into();
        }
        let before = self.tabs.len() + self.pending_opens.len();
        let dirs = paths.iter().filter(|p| p.is_dir()).count();
        for p in paths.iter().filter(|p| !p.is_dir()) {
            self.open_path_async(p.clone());
        }
        // `open_path_async` is a no-op for a path that is already open or
        // already loading, so this counts what the drop actually did
        let queued = self.tabs.len() + self.pending_opens.len() - before;
        if queued == 0 {
            return match dirs {
                0 => Msg::StatusDropAlreadyOpen.into(),
                n if n == paths.len() => Msg::StatusDropDirsOnly.into(),
                n => Fmt::StatusDropAlreadyOpenDirs { dirs: n }.into(),
            };
        }
        if dirs > 0 {
            Fmt::StatusDropOpeningIgnored { queued, dirs }.into()
        } else {
            Fmt::StatusDropOpening { queued }.into()
        }
    }

    pub(super) fn open_async(&mut self, path: PathBuf, intent: OpenIntent) {
        if intent == OpenIntent::NewTab {
            // same_path (case/separator normalization, Windows): raw path
            // equality would miss `C:\a.txt` already open when Explorer
            // double-clicks `c:/a.txt` → two tabs each saving over the other.
            // The earlier fix only covered the synchronous command-line path;
            // every interactive path goes through here.
            if let Some(i) = self
                .tabs
                .iter()
                .position(|t| same_path(t.doc.meta.path.as_deref(), &path))
            {
                self.set_active_tab(i);
                self.scroll_top = 0.0;
                self.scroll_left = 0.0;
                self.sel_anchor = None;
                return;
            }
        }
        // a second request for the same file while the first is still running
        if self
            .pending_opens
            .iter()
            .any(|p| same_path(Some(&p.path), &path))
        {
            return;
        }

        let (tx, rx) = std::sync::mpsc::channel();
        let progress = Arc::new(OpenProgress::new());
        let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        let thread_path = path.clone();
        let worker_progress = progress.clone();
        // The worker nudges the window on every progress tick, which is what
        // makes the status bar advance instead of sitting on one frozen label.
        let ticker = self.wake.clone();
        let huge = size >= crate::io::HUGE_THRESHOLD * 2;
        let job = move || {
            let sink = &worker_progress;
            let res = if huge {
                crate::io::open_file_progressive(&thread_path, |done, total| {
                    use std::sync::atomic::Ordering::Relaxed;
                    sink.total.store(total, Relaxed);
                    sink.done.store(done, Relaxed);
                    if let Some(w) = ticker.as_ref() {
                        w();
                    }
                })
            } else {
                crate::io::open_file(&thread_path, |done, total| {
                    use std::sync::atomic::Ordering::Relaxed;
                    sink.total.store(total, Relaxed);
                    sink.done.store(done, Relaxed);
                    if let Some(w) = ticker.as_ref() {
                        w();
                    }
                })
            };
            // Opening phases are short and the read/index work has no abort
            // hook; abandoning the open simply drops the pending entry and its
            // result below (see `cancel_pending_opens`).
            let _ = tx.send(res.map_err(OpenError::Io));
        };
        match self.wake.clone() {
            Some(wake) => {
                std::thread::spawn(move || {
                    // Opening/indexing a user file: a panic must fail this open, not
                    // terminate the editor.
                    crate::ui::guard::worker("ui::opens", move || {
                        job();
                    });
                    wake();
                });
            }
            None => job(),
        }

        self.status = Some(opening_status(&path, size, None, self.locale));
        self.pending_opens.push(PendingOpen {
            path,
            intent,
            started: std::time::Instant::now(),
            size,
            progress,
            rx,
        });
    }

    /// **Read-only probe** for the [`Self::poll_opens`] family: the
    /// idle-frame hook (`RedrawRequested`) uses it to decide whether taking
    /// a `Signal::write()` once is worth it.
    ///
    /// Background: the guard of `Signal::write()` **unconditionally
    /// mark_dirty**s on drop. The frame hook taking the write lock every
    /// frame pinned the whole app to a ~40Hz self-perpetuating render loop
    /// (render → dirty → poll → render again), burning a core at idle. Every
    /// condition here is verbatim identical to the corresponding poller's
    /// early-return branch (the same iron rule as `state::heal`):
    ///
    /// * [`Self::poll_opens`]: background opens not yet collected / startup
    ///   paths not flushed / any Blitz-owned dialog open / GUI smoke due;
    /// * [`crate::ui::single::pending`]: a second launch queued an "open this"
    ///   request (set by the hand-off watcher thread, cleared by
    ///   [`Self::poll_remote_open_requests`]);
    /// * [`Self::poll_index_jobs`]: index job queue non-empty;
    /// * `poll_search`: a background search job is running;
    /// * `poll_external_change`: the 1s external-change poll throttle is due
    ///   (documents with paths);
    /// * `recovery_tick`: the 3s rescue-journal throttle is due and the
    ///   signature changed;
    /// * `mdws::flush_if_due`: a debounced wys change is pending and due.
    ///
    /// All false in steady state ⇒ idle frames write nothing and dirty
    /// nothing, and the render loop dies down naturally.
    pub fn needs_idle_poll(&self, now: std::time::Instant) -> bool {
        // -- poll_opens ----------------------------------------------------
        if !self.pending_opens.is_empty() || !self.startup_paths.is_empty() {
            return true;
        }
        // -- single-instance hand-off (ui::single) --------------------------
        // The watcher thread publishes "a request file appeared" as an atomic
        // and wakes the window; this is a single relaxed load, false in steady
        // state.
        if crate::ui::single::pending() {
            return true;
        }
        // poll_dialogs: work exists only while any of the four Blitz-owned
        // dialogs is open (open flags set only while the dialog is truly
        // open — always false on non-Windows; the block is lazy as a whole)
        if self.find.child_win || self.prefs.open || self.numbering.open || self.about.open {
            return true;
        }
        // The GUI smoke arm of poll_opens (env-gated, true only in test
        // processes). The probe uses the non-destructive smoke_pending —
        // smoke_due resets itself (one-shot) when it returns true, so if the
        // probe consumed it first, smoke_due in poll_opens' fix branch below
        // would be false forever.
        if crate::ui::find::smoke_pending()
            || crate::ui::prefswin::smoke_pending()
            || crate::ui::numwin::smoke_pending()
        {
            return true;
        }
        // -- poll_index_jobs / poll_search ---------------------------------
        if !self.index_jobs.is_empty() || self.search_job.is_some() {
            return true;
        }
        // -- poll_external_change: 1s throttle (pathless documents have nothing to do, skip) -----
        let watch_due = match self.last_watch {
            None => true,
            Some(t) => now.duration_since(t).as_millis() >= 1000,
        };
        if watch_due && self.active_doc().meta.path.is_some() {
            return true;
        }
        // -- recovery_tick: 3s throttle + signature change (a pristine fresh session is not written) ---------
        if self.pending_recovery.is_none() && crate::ui::recovery::session_dir().is_some() {
            let pristine = self.tabs.len() == 1
                && !self.tabs[0].doc.is_dirty()
                && self.tabs[0].doc.meta.path.is_none()
                && self.tabs[0].doc.total_bytes() == 0;
            if !pristine {
                let interval_elapsed = self
                    .recovery_last
                    .is_none_or(|t| now.duration_since(t) >= crate::ui::recovery::JOURNAL_INTERVAL);
                if interval_elapsed {
                    let sig_changed = self.recovery_last.is_none()
                        || self.recovery_sig != Some(self.recovery_signature());
                    if sig_changed {
                        return true;
                    }
                }
            }
        }
        // -- mdws::flush_if_due --------------------------------------------
        if crate::ui::components::mdws::flush_pending_due() {
            return true;
        }
        // -- caret blink phase (BlinkTick timer thread → written back by the frame hook) ----------------
        // This used to be a CSS `caret-blink` infinite animation: blitz
        // self-perpetuates request_redraw every frame for windows with a
        // running animation (the `is_animating` branch of `View::redraw`),
        // keeping the whole app at 40–60fps forever. With the 265ms phase
        // flip + frame-hook write-back, blinking costs one frame per half
        // period. When MAIN_BLINK is off (window unfocused and find bar not
        // focused) the phase stops flipping and this condition is naturally
        // false.
        self.caret_blink && self.caret_phase != winshell::shell::bridge::blink_on()
    }

    /// Collect idle-poll work: background opens / search / external changes /
    /// the rescue journal. Returns `true` when state changed (the caller
    /// redraws accordingly).
    ///
    /// While an open is still running this also refreshes the status bar from
    /// its byte counters — the worker calls `request_redraw` on every progress
    /// tick, so this runs often enough for the percentage to actually move.
    pub fn poll_opens(&mut self) -> bool {
        // GUI smoke: auto-open the find dialog at startup (screenshot verification).
        if crate::ui::find::smoke_due() {
            self.open_find(false, false);
        }
        // GUI smoke: auto-open the preferences dialog at startup (screenshot verification).
        if crate::ui::prefswin::smoke_due() {
            self.open_preferences();
        }
        // GUI smoke: auto-open the insert-numbering dialog at startup (screenshot verification).
        if crate::ui::numwin::smoke_due() {
            let _ = self.open_numbering_window();
        }
        // A dialog may have queued a click since the last frame; that is the
        // editor's own work, so it is collected in the same pass rather than
        // needing a second wake-up.
        let changed = self.poll_dialogs();
        // fallback for a window that never installed a waker, and for a startup
        // open queued before the app root ran
        self.flush_startup_paths();
        if self.pending_opens.is_empty() {
            return changed;
        }
        let mut done: Vec<(usize, Result<crate::io::Opened, OpenError>)> = Vec::new();
        for (i, p) in self.pending_opens.iter().enumerate() {
            match p.rx.try_recv() {
                Ok(res) => done.push((i, res)),
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    done.push((i, Err(OpenError::ThreadDied)));
                }
            }
        }
        if done.is_empty() {
            // still working: surface the newest progress reading
            if let Some(p) = self.pending_opens.last() {
                let text = opening_status(&p.path, p.size, p.progress.fraction(), self.locale);
                if self.status.as_ref() != Some(&text) {
                    self.status = Some(text);
                    return true;
                }
            }
            return changed;
        }
        // Remove back-to-front so the earlier indices stay valid.
        for (i, res) in done.into_iter().rev() {
            let p = self.pending_opens.remove(i);
            self.apply_open(&p, res);
        }
        true
    }

    /// Apply everything the native Find/Replace and Preferences windows have
    /// reported.
    ///
    /// The window procedures cannot touch this state directly (a dioxus `Signal`
    /// may be borrowed on the frame they land in), so they queue and the app
    /// drains here — which is also the only point at which running a command is
    /// safe. Returns whether anything changed.
    ///
    /// The search is *incremental*: the document jumps to the first
    /// match as the pattern is typed, so a pattern arriving from a real `EDIT`
    /// gets the same treatment as one typed into the in-page bar.
    pub fn poll_dialogs(&mut self) -> bool {
        // Find dialog: apply its commands (which mutate find state) and
        // mirror the latest state back (flush dedupes internally; no change
        // costs nothing). The dialog is an owned window; its position is
        // decided by the owner mechanism and user dragging, so the main
        // window no longer places it. Each open flag is set only while the
        // dialog is truly open (always false on non-Windows); the block is
        // lazy as a whole.
        let mut changed = false;
        if self.find.child_win {
            crate::ui::findwin::apply_cmds(self);
            crate::ui::findwin::sync_shared(self);
            crate::ui::findwin::flush_snap();
        }
        // Preferences / insert numbering / about dialogs (Blitz-owned windows, see prefswin / numwin)
        if self.prefs.open {
            changed |= crate::ui::prefswin::apply_cmds(self);
            crate::ui::prefswin::sync_shared(self);
            crate::ui::prefswin::flush_snap();
        }
        if self.numbering.open {
            changed |= crate::ui::numwin::apply_cmds(self);
            crate::ui::numwin::sync_shared(self);
            crate::ui::numwin::flush_snap();
        }
        if self.about.open {
            changed |= crate::ui::aboutwin::apply_cmds(self);
            crate::ui::aboutwin::sync_shared(self);
            crate::ui::aboutwin::flush_snap();
        }
        changed
    }

    /// Project the current settings into the Blitz dialogs (preferences).
    ///
    /// Called once per frame from the app root; `Bridge::flush` dedupes
    /// internally, so unchanged frames cost nothing.
    pub fn sync_dialogs(&self) {
        if self.prefs.open {
            crate::ui::prefswin::sync_shared(self);
            crate::ui::prefswin::flush_snap();
        }
        if self.numbering.open {
            crate::ui::numwin::sync_shared(self);
            crate::ui::numwin::flush_snap();
        }
        if self.about.open {
            crate::ui::aboutwin::sync_shared(self);
            crate::ui::aboutwin::flush_snap();
        }
    }

    fn apply_open(&mut self, p: &PendingOpen, res: Result<crate::io::Opened, OpenError>) {
        let opened = match res {
            Ok(o) => o,
            Err(e) => {
                self.status = Some(match e {
                    OpenError::Io(err) => Fmt::StatusOpenFailed {
                        path: p.path.display().to_string(),
                        err: err.to_string(),
                    }
                    .into(),
                    // Message keys render per frame: after a language switch they display in the current language
                    OpenError::ThreadDied => Msg::StatusOpenThreadDied.into(),
                });
                return;
            }
        };
        match p.intent {
            OpenIntent::NewTab => {
                self.insert_opened(opened, &p.path, Some(p.started.elapsed()));
                let i = self
                    .tabs
                    .iter()
                    .position(|t| t.doc.meta.path.as_deref() == Some(p.path.as_path()))
                    .unwrap_or(self.active);
                self.note_mtime_of(i);
            }
            OpenIntent::Replace { index, doc_id } => {
                let still_there = index < self.tabs.len() && self.tabs[index].doc.id == doc_id;
                if !still_there {
                    // the tab was closed or moved while the file was loading
                    self.insert_opened(opened, &p.path, Some(p.started.elapsed()));
                    return;
                }
                let new_id = opened.doc.id;
                let old_id = self.tabs[index].doc.id;
                let progressive = !opened.index_complete;
                // The whole tab is replaced: fresh document, fresh language
                // detection, caret back to the start — exactly the old
                // four-array dance, in one assignment.
                self.tabs[index] = Tab::new(opened.doc);
                self.forget_doc(old_id);
                self.adopt_view_prefs(index);
                self.note_mtime_of(index);
                // Only the reloaded tab's view state resets: the reload runs in
                // the background, and the user may have switched to another tab
                // while it loaded — resetting the *active* tab's scroll and
                // selection there was a visible jump in an unrelated document.
                if index == self.active {
                    self.scroll_top = 0.0;
                    self.scroll_left = 0.0;
                    self.sel_anchor = None;
                    // The global wys singleton still holds the pre-reload text;
                    // drop the pending write-back and re-seed from the reloaded
                    // document (otherwise the next debounce tick would write
                    // the old text straight back over the file we just read).
                    crate::ui::components::mdws::discard_pending();
                    crate::ui::components::mdws::guard_seed(self);
                }
                // A new tab may bring in the CSV toolbar (csv/tsv documents); the viewport must yield again
                self.relayout();
                self.status = Some(
                    Fmt::StatusReloaded {
                        path: p.path.display().to_string(),
                    }
                    .into(),
                );
                if progressive {
                    self.start_index_job(new_id);
                }
            }
        }
    }

    /// Insert a freshly opened document as the active tab.
    ///
    /// `elapsed` is the wall time the background worker spent reading and
    /// indexing the file; it is surfaced in the status bar because it is the
    /// number that matters when opening a multi-gigabyte file.
    pub(super) fn insert_opened(
        &mut self,
        opened: crate::io::Opened,
        path: &std::path::Path,
        elapsed: Option<std::time::Duration>,
    ) {
        let name = opened.doc.meta.display_name.clone();
        let doc_id = opened.doc.id;
        let progressive = !opened.index_complete;
        self.tabs.push(Tab::new(opened.doc));
        self.adopt_view_prefs(self.tabs.len() - 1);
        self.set_active_tab(self.tabs.len() - 1);
        self.scroll_top = 0.0;
        self.scroll_left = 0.0;
        self.sel_anchor = None;
        // A new tab may bring in the CSV toolbar (csv/tsv documents); the viewport must yield again
        self.relayout();
        self.push_recent(path.to_string_lossy().into_owned());
        let lines = self.tab().doc.line_count() as usize;
        let size = human_bytes(opened.file_size, self.locale);
        let secs = elapsed.map(|d| format!("{:.2}", d.as_secs_f32()));
        let mk = |mapped: bool, lossy: bool, secs: Option<&str>| -> i18n::Message {
            match (mapped, lossy, secs) {
                (false, false, None) => Fmt::StatusOpened {
                    name: name.clone(),
                    lines,
                    size: size.clone(),
                }
                .into(),
                (true, false, None) => Fmt::StatusOpenedMapped {
                    name: name.clone(),
                    lines,
                    size: size.clone(),
                }
                .into(),
                (false, true, None) => Fmt::StatusOpenedLossy {
                    name: name.clone(),
                    lines,
                    size: size.clone(),
                }
                .into(),
                (true, true, None) => Fmt::StatusOpenedMappedLossy {
                    name: name.clone(),
                    lines,
                    size: size.clone(),
                }
                .into(),
                (false, false, Some(s)) => Fmt::StatusOpenedSecs {
                    name: name.clone(),
                    lines,
                    size: size.clone(),
                    secs: s.to_string(),
                }
                .into(),
                (true, false, Some(s)) => Fmt::StatusOpenedMappedSecs {
                    name: name.clone(),
                    lines,
                    size: size.clone(),
                    secs: s.to_string(),
                }
                .into(),
                (false, true, Some(s)) => Fmt::StatusOpenedLossySecs {
                    name: name.clone(),
                    lines,
                    size: size.clone(),
                    secs: s.to_string(),
                }
                .into(),
                (true, true, Some(s)) => Fmt::StatusOpenedMappedLossySecs {
                    name,
                    lines,
                    size,
                    secs: s.to_string(),
                }
                .into(),
            }
        };
        self.status = Some(mk(opened.memory_mapped, opened.utf8_lossy, secs.as_deref()));
        // a real file has arrived, so the scratch buffer `minimal()` starts with
        // is no longer wanted
        self.drop_scratch_tab();
        if progressive {
            self.start_index_job(doc_id);
        }
    }

    // ---- progressive index jobs ---------------------------------------------

    /// Spawn the Phase-2 worker for a progressively opened document: it scans
    /// the rest of the file from the current watermark and streams index
    /// patches back. No-op when the index is already complete.
    pub(super) fn start_index_job(&mut self, doc_id: u64) {
        let Some(tab) = self.tabs.iter().find(|t| t.doc.id == doc_id) else {
            return;
        };
        if tab.doc.index_complete() {
            return;
        }
        let gen = tab.doc.gen;
        let from = tab.doc.index_scanned_to();
        let base_line = tab.doc.index_base_line();
        // A prefix that ended on an unresolved `\r`: the continuation must
        // resolve it or one line break (and every line number after it) is lost.
        let pending_cr = tab.doc.index.pending_cr;
        let snap = tab.doc.snapshot();
        let (tx, rx) = std::sync::mpsc::channel();
        let progress = Arc::new(OpenProgress::new());
        let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let wake = self.wake.clone();
        let job_progress = progress.clone();
        let job_cancel = cancel.clone();
        std::thread::Builder::new()
            .name("index-scan".into())
            .spawn(move || {
                crate::ui::guard::worker("buffer::line_index::scan", move || {
                    let table = snap.table();
                    crate::buffer::line_index::scan_continue(
                        &table,
                        from,
                        base_line,
                        pending_cr,
                        |patch| {
                            // the channel dropping just means the tab was closed
                            let _ = tx.send(patch);
                        },
                        &job_cancel,
                        |done, total| {
                            use std::sync::atomic::Ordering::Relaxed;
                            job_progress.total.store(total, Relaxed);
                            job_progress.done.store(done, Relaxed);
                            if let Some(w) = wake.as_ref() {
                                w();
                            }
                        },
                    );
                    if let Some(w) = wake.as_ref() {
                        w();
                    }
                });
            })
            .ok();
        self.index_jobs.push(IndexJob {
            doc_id,
            gen,
            rx,
            progress,
            cancel,
            started: std::time::Instant::now(),
        });
    }

    /// Fold finished index patches into their documents.
    ///
    /// Runs once per frame next to [`Self::poll_opens`]. A patch whose
    /// document was edited since the snapshot is discarded and the scan
    /// restarts from the current watermark (edits keep checkpoints and the
    /// watermark coherent, so nothing is lost). A job whose tab is gone is
    /// cancelled — closing a huge tab stops its scan within ~4 MB.
    pub fn poll_index_jobs(&mut self) -> bool {
        if self.index_jobs.is_empty() {
            return false;
        }
        use std::sync::atomic::Ordering::Relaxed;
        let jobs = std::mem::take(&mut self.index_jobs);
        let mut changed = false;
        let mut restarts: Vec<u64> = Vec::new();
        let mut alive: Vec<IndexJob> = Vec::new();
        for job in jobs {
            let tab_alive = self.tabs.iter().any(|t| t.doc.id == job.doc_id);
            if !tab_alive {
                job.cancel.store(true, Relaxed);
                continue; // drop the job; the worker exits at the next tick
            }
            let mut worker_gone = false;
            let mut gen_mismatch = false;
            loop {
                match job.rx.try_recv() {
                    Ok(patch) => {
                        let Some(tab) = self.tabs.iter_mut().find(|t| t.doc.id == job.doc_id)
                        else {
                            break;
                        };
                        if tab.doc.gen != job.gen {
                            gen_mismatch = true;
                            break;
                        }
                        if tab.doc.index_complete() {
                            continue; // stale patch for an already-finished index
                        }
                        tab.doc.merge_index_patch(patch);
                        // The patch corrects the line→byte mapping: line text
                        // and highlight checkpoints are caches derived from
                        // the old mapping and must be invalidated with it.
                        tab.hl.invalidate_checkpoints();
                        changed = true;
                    }
                    Err(std::sync::mpsc::TryRecvError::Empty) => break,
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        worker_gone = true;
                        break;
                    }
                }
            }
            if gen_mismatch {
                job.cancel.store(true, Relaxed);
                restarts.push(job.doc_id);
                changed = true;
                continue;
            }
            if worker_gone {
                if let Some(tab) = self.tabs.iter().find(|t| t.doc.id == job.doc_id) {
                    let secs = job.started.elapsed().as_secs_f32();
                    if tab.doc.index_complete() {
                        let lines = tab.doc.line_count() as usize;
                        self.status = Some(
                            Fmt::StatusIndexDone {
                                lines,
                                secs: format!("{secs:.2}"),
                            }
                            .into(),
                        );
                    } else {
                        self.status = Some(Msg::StatusIndexStopped.into());
                    }
                }
                changed = true;
                continue;
            }
            // still running: surface progress in the status bar
            if let (Some(_p), Some(tab)) = (
                job.progress.fraction(),
                self.tabs.iter().find(|t| t.doc.id == job.doc_id),
            ) {
                if !tab.doc.index_complete() {
                    let done = job.progress.done.load(Relaxed);
                    let rate = (done as f64
                        / job.started.elapsed().as_secs_f64().max(0.05)
                        / 1024.0
                        / 1024.0) as u64;
                    self.status = Some(Fmt::StatusIndexRunning { rate }.into());
                    changed = true;
                }
            }
            alive.push(job);
        }
        self.index_jobs = alive;
        for doc_id in restarts {
            self.start_index_job(doc_id);
        }
        changed
    }

    /// True while at least one progressive index scan is still running.
    pub fn is_indexing(&self) -> bool {
        !self.index_jobs.is_empty()
    }

    /// Cancel every running index scan (status-bar "indexing" click / Esc).
    /// Returns `true` when something was actually cancelled.
    pub fn cancel_indexing(&mut self) -> bool {
        if self.index_jobs.is_empty() {
            return false;
        }
        for job in &self.index_jobs {
            job.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        // the worker notices at its next progress tick; drop the jobs now so
        // the UI stops reporting on them
        self.index_jobs.clear();
        self.status = Some(Msg::StatusIndexCancelled.into());
        true
    }

    /// Cancel every pending background open (Esc while a file is loading).
    /// The worker thread finishes on its own but its result is discarded.
    pub fn cancel_pending_opens(&mut self) -> bool {
        if self.pending_opens.is_empty() {
            return false;
        }
        self.pending_opens.clear();
        self.status = Some(Msg::StatusOpenCancelled.into());
        true
    }

    /// Remove the untitled, unmodified document that `minimal()` creates, once
    /// at least one real file is open, so the tab bar does not show a stray
    /// empty tab next to it.
    ///
    /// Safe to call after every open: it does nothing when tab 0 is a real file
    /// or has been typed in.
    fn drop_scratch_tab(&mut self) {
        if self.tabs.len() <= 1 {
            return;
        }
        if self.tabs[0].doc.meta.path.is_some() || self.tabs[0].doc.is_dirty() {
            return;
        }
        // Flush wys pending changes into the active tab first, then let indices drift (prevents cross-talk)
        crate::ui::components::mdws::flush_pending(self);
        self.tabs.remove(0);
        self.set_active_tab(self.active.saturating_sub(1));
    }
}

/// `1.5 MB`-style size for status messages.
///
/// The locale is an argument, not `i18n::current()`: a pure function has no
/// business reading the process mirror (README §3), the size must follow the
/// language of the message it is embedded in, and a test that read the mirror
/// would depend on whatever language another test happened to leave behind.
/// KB/MB/GB are proper unit names, identical in both languages.
pub fn human_bytes(n: u64, loc: i18n::Locale) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * KB;
    const GB: u64 = 1024 * MB;
    if n >= GB {
        format!("{:.2} GB", n as f64 / GB as f64)
    } else if n >= MB {
        format!("{:.1} MB", n as f64 / MB as f64)
    } else if n >= KB {
        format!("{:.1} KB", n as f64 / KB as f64)
    } else {
        format!("{n} {}", loc.tr(Msg::CommonBytes))
    }
}

/// Status-bar message for an in-flight background open.
///
/// `fraction` is `None` until the file size is known and carries a `0.0..=1.0`
/// completion ratio afterwards, so the bar reads
/// `正在打开 big.log (256.0 MB) — 42%` while it fills up.
///
/// `loc` is the language the size is rendered in ([`human_bytes`]); the
/// returned [`i18n::Message`] itself stays language-neutral.
pub fn opening_status(
    path: &std::path::Path,
    size: u64,
    fraction: Option<f32>,
    loc: i18n::Locale,
) -> i18n::Message {
    let name = path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string());
    let size = human_bytes(size, loc);
    match fraction {
        Some(f) => {
            let pct = (f * 100.0).clamp(0.0, 100.0).round() as usize;
            Fmt::StatusOpeningFilePct { name, size, pct }.into()
        }
        None => Fmt::StatusOpeningFile { name, size }.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::testutil::st_of;

    /// **Idle-quiet guard**: on a fresh empty document (no background jobs,
    /// no dialogs, phases aligned) `needs_idle_poll` must be false — the
    /// frame hook skips `Signal::write()` based on it. If the probe ever
    /// loosens (an arm also true in steady state), the whole app falls back
    /// to the 40Hz self-perpetuating render loop (measured in the 2026-10
    /// round 7: idle CPU pinned at one core).
    #[test]
    fn idle_poll_is_quiet_in_steady_state() {
        let st = st_of("");
        assert!(
            !st.needs_idle_poll(std::time::Instant::now()),
            "the idle probe must be false in steady state (which arm loosened?)"
        );
    }

    /// Must be detected when the blink phase mismatches (one of the few
    /// "legitimate" idle wake sources).
    #[test]
    fn idle_poll_detects_blink_phase_change() {
        let mut st = st_of("");
        st.caret_phase = !st.caret_phase;
        assert!(st.needs_idle_poll(std::time::Instant::now()));
    }

    fn tmp(name: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("infeditor-state-{}-{}", std::process::id(), name));
        p
    }

    /// `minimal()` has no waker, so `open_path_async` runs inline and the result
    /// is already in the channel by the time `poll_opens` is called — which is
    /// exactly the contract the window layer relies on (it polls after the
    /// worker has asked for a redraw).
    #[test]
    fn background_open_is_collected_by_poll() {
        let path = tmp("async.txt");
        std::fs::write(&path, b"alpha\nbeta\n").unwrap();

        let mut st = EditorState::minimal();
        let scratch = st.tabs[0].doc.id;
        st.open_path_async(path.clone());
        assert!(st.is_loading(), "an open should be queued");
        assert!(st.poll_opens(), "the finished open should be collected");
        assert!(!st.is_loading());
        assert_eq!(
            st.tabs.len(),
            1,
            "the scratch tab gives way to the loaded file"
        );
        assert_ne!(st.tabs[st.active].doc.id, scratch);
        assert_eq!(st.tabs[st.active].doc.line_count(), 2);
        assert_eq!(st.tabs[st.active].doc.line_text(1), "beta");
        // the caret travels inside its tab, so a stale one cannot survive
        assert!(
            st.tabs.iter().all(|t| t.doc.id != scratch),
            "stale scratch tab left behind"
        );
        // a second poll is a no-op
        assert!(!st.poll_opens());

        // opening the same path again just focuses the existing tab
        let n = st.tabs.len();
        st.open_path_async(path.clone());
        assert_eq!(st.tabs.len(), n);
        assert!(!st.is_loading());

        let _ = std::fs::remove_file(&path);
    }
    /// The scratch tab is only discarded when it is untouched: losing typed
    /// text because a background open finished would be a data-loss bug.
    #[test]
    fn dirty_scratch_tab_survives_a_background_open() {
        let path = tmp("keep-scratch.txt");
        std::fs::write(&path, b"file\n").unwrap();

        let mut st = EditorState::minimal();
        st.active_doc_mut().insert_text(0, "unsaved work");
        assert!(
            st.tabs[0].doc.is_dirty(),
            "insert_text should dirty the document"
        );
        let scratch = st.tabs[0].doc.id;

        st.open_path_async(path.clone());
        assert!(st.poll_opens());
        assert_eq!(st.tabs.len(), 2, "the dirty scratch tab must be kept");
        assert!(st.tabs.iter().any(|t| t.doc.id == scratch));
        assert_eq!(st.tabs[st.active].doc.line_text(0), "file");

        let _ = std::fs::remove_file(&path);
    }
    /// Command-line files are queued, not read, during `Default::default`, so
    /// the window can paint its first frame before a huge file is scanned.
    #[test]
    fn startup_paths_are_queued_then_flushed() {
        let mut st = EditorState::minimal();
        // no waker and nothing queued: flushing is a no-op
        st.flush_startup_paths();
        assert!(!st.is_loading());

        // simulate a queued command-line file
        let path = tmp("startup.txt");
        std::fs::write(&path, b"queued\n").unwrap();
        st.startup_paths.push(path.clone());
        assert!(!st.is_loading(), "queuing must not read anything yet");

        // `poll_opens` is the fallback trigger when no waker was installed
        assert!(st.poll_opens(), "the flush should have produced a result");
        assert!(st.startup_paths.is_empty(), "the queue must be drained");
        assert_eq!(st.tabs[st.active].doc.line_text(0), "queued");

        let _ = std::fs::remove_file(&path);
    }
    #[test]
    fn missing_file_reports_instead_of_panicking() {
        let mut st = EditorState::minimal();
        let n = st.tabs.len();
        st.open_path_async(tmp("definitely-missing-infeditor.txt"));
        assert!(st.poll_opens());
        assert_eq!(st.tabs.len(), n, "a failed open must not add a tab");
        assert!(
            st.status
                .as_ref()
                .is_some_and(|m| m.key().contains("StatusOpenFailed")),
            "status was {:?}",
            st.status
        );
    }
    /// Reload must land in the *same* tab, and rebind the caret to the new
    /// document id.
    #[test]
    fn reload_replaces_the_same_tab() {
        let path = tmp("reload.txt");
        std::fs::write(&path, b"one\ntwo\n").unwrap();

        let mut st = EditorState::minimal();
        st.open_path(path.clone());
        let tab = st.active;
        let old_id = st.tabs[tab].doc.id;
        let tabs = st.tabs.len();

        // change the file on disk, then reload
        std::fs::write(&path, b"one\ntwo\nthree\n").unwrap();
        st.reload_active();
        assert!(st.poll_opens());

        assert_eq!(st.tabs.len(), tabs, "reload must not add a tab");
        assert_eq!(st.active, tab);
        assert_eq!(st.tabs[tab].doc.line_count(), 3);
        let new_id = st.tabs[tab].doc.id;
        assert_ne!(old_id, new_id, "a reloaded document gets a fresh id");
        // the caret lives in the tab: reloading swaps the whole value
        assert_eq!(st.tabs[tab].doc.id, new_id);

        let _ = std::fs::remove_file(&path);
    }
    /// Reload must discard a debounced wys edit instead of letting the stale
    /// singleton write it back over the freshly loaded file.
    #[test]
    fn reload_with_pending_wys_edits_ends_with_the_disk_content() {
        use dioxus_native::prelude::dioxus_core::{RuntimeGuard, VirtualDom};
        use dioxus_native::prelude::WritableExt;
        fn empty_app() -> dioxus_native::prelude::Element {
            use dioxus_native::prelude::*;
            rsx! {}
        }
        let vdom = VirtualDom::new(empty_app);
        let _guard = RuntimeGuard::new(vdom.runtime());

        let path = tmp("reload-wys.md");
        std::fs::write(&path, b"# one\n").unwrap();
        let mut st = EditorState::minimal();
        st.open_path(path.clone());
        assert!(st.tab().md_wys, "markdown opens in the document view");
        crate::ui::components::mdws::seed_from_doc(&st);

        // Type into the document view; the edit lives only in the singleton.
        let _ = crate::ui::components::mdws::WYS
            .signal()
            .write()
            .apply(wysiwyg::edit::Cmd::InsertText("pending".into()));
        crate::ui::components::mdws::note_edit(&st);

        std::fs::write(&path, b"# two\n").unwrap();
        st.reload_active();
        assert!(
            st.pending_reload.is_some(),
            "a debounced wys edit must count as dirty before reload"
        );
        st.finish_reload(true);
        assert!(st.poll_opens());
        assert_eq!(st.tab().doc.read_all(), "# two\n");

        // A late debounce flush must not resurrect the discarded edit.
        crate::ui::components::mdws::flush_pending(&mut st);
        assert_eq!(st.tab().doc.read_all(), "# two\n");
        assert!(!st.tab().doc.is_dirty(), "the reloaded document is clean");

        let _ = std::fs::remove_file(&path);
    }

    /// Dropping files must queue them without reading anything inline, ignore
    /// directories instead of failing, and report what it did.
    #[test]
    fn dropped_paths_are_queued_and_described() {
        let a = tmp("drop-a.txt");
        let b = tmp("drop-b.txt");
        std::fs::write(&a, b"a\n").unwrap();
        std::fs::write(&b, b"b\n").unwrap();
        let dir = std::env::temp_dir();

        let mut st = EditorState::minimal();

        // nothing at all
        assert_eq!(st.open_dropped(&[]).key(), "Plain:StatusDropNoFiles");
        assert!(!st.is_loading());

        // a single file
        let msg = st.open_dropped(std::slice::from_ref(&a));
        assert!(msg.key().contains("StatusDropOpening"), "{msg:?}");
        assert!(
            st.is_loading(),
            "the file should be queued, not read inline"
        );
        assert!(st.poll_opens());
        assert_eq!(st.tabs[st.active].doc.line_text(0), "a");

        // the same file again is already open
        assert_eq!(
            st.open_dropped(std::slice::from_ref(&a)).key(),
            "Plain:StatusDropAlreadyOpen"
        );

        // a folder on its own is ignored, not an error
        assert_eq!(
            st.open_dropped(std::slice::from_ref(&dir)).key(),
            "Plain:StatusDropDirsOnly"
        );

        // a mix still opens the real files and mentions the ignored folder
        let msg = st.open_dropped(&[dir.clone(), b.clone()]);
        assert!(msg.key().contains("StatusDropOpening"), "{msg:?}");
        assert!(msg.key().contains("StatusDropOpeningIgnored"), "{msg:?}");
        assert!(st.poll_opens());
        assert_eq!(st.tabs[st.active].doc.line_text(0), "b");

        let _ = std::fs::remove_file(&a);
        let _ = std::fs::remove_file(&b);
    }
    /// `fraction()` is the only thing reading the worker's counters, so it has
    /// to be safe before the worker has written anything (total still 0) and
    /// must never exceed 1.0.
    #[test]
    fn open_progress_fraction_is_safe() {
        use std::sync::atomic::Ordering::Relaxed;
        let p = OpenProgress::new();
        assert_eq!(p.fraction(), None, "unknown total must not divide by zero");

        p.total.store(100, Relaxed);
        p.done.store(0, Relaxed);
        assert_eq!(p.fraction(), Some(0.0));

        p.done.store(40, Relaxed);
        assert_eq!(p.fraction(), Some(0.4));

        // a counter overshooting the total is clamped
        p.done.store(250, Relaxed);
        assert_eq!(p.fraction(), Some(1.0));

        p.total.store(0, Relaxed);
        assert_eq!(p.fraction(), None);
    }
    /// A background open must publish real byte progress, i.e. the counters the
    /// worker writes and the ones `poll_opens` reads are the same object, and
    /// they end at 100 %. `minimal()` has no waker, so this runs inline.
    #[test]
    fn background_open_publishes_progress() {
        let path = tmp("progress.txt");
        let body: String = (0..2000).map(|i| format!("row {i}\n")).collect();
        std::fs::write(&path, &body).unwrap();

        let mut st = EditorState::minimal();
        st.open_path_async(path.clone());
        // the job ran inline, so the reading is final
        let p = st.pending_opens.last().expect("one queued open");
        assert_eq!(p.size, body.len() as u64);
        assert_eq!(
            p.progress.fraction(),
            Some(1.0),
            "a finished open must read 100%"
        );

        assert!(st.poll_opens());
        assert!(!st.is_loading());
        let _ = std::fs::remove_file(&path);
    }
    /// Sizes render in the language they are **asked for**: the unit word is
    /// the only translated part, and taking it from the process mirror would
    /// make this test depend on the order a parallel test switched languages.
    #[test]
    fn human_bytes_reads_naturally() {
        use i18n::Locale;
        assert_eq!(human_bytes(512, Locale::ZhCn), "512 字节");
        assert_eq!(human_bytes(512, Locale::EnUs), "512 bytes");
        // KB/MB/GB are proper unit names, identical in both languages
        assert_eq!(human_bytes(2048, Locale::ZhCn), "2.0 KB");
        assert_eq!(human_bytes(2048, Locale::EnUs), "2.0 KB");
        assert_eq!(human_bytes(3 * 1024 * 1024, Locale::ZhCn), "3.0 MB");
        assert_eq!(human_bytes(2 * 1024 * 1024 * 1024, Locale::ZhCn), "2.00 GB");
    }
    /// The status bar must be able to render both the "size not known yet" and
    /// the "42 % done" phases without a division by zero.
    #[test]
    fn opening_status_formats_both_phases() {
        use i18n::Locale;
        use i18n::Text as _;
        let zh = |m: i18n::Message| m.render(Locale::ZhCn).into_owned();
        let p = PathBuf::from("C:/tmp/big.log");
        let pending = zh(opening_status(&p, 256 * 1024 * 1024, None, Locale::ZhCn));
        assert!(pending.contains("big.log"), "{pending}");
        assert!(pending.contains("256.0 MB"), "{pending}");
        assert!(!pending.contains('%'), "{pending}");

        let quarter = zh(opening_status(
            &p,
            256 * 1024 * 1024,
            Some(0.25),
            Locale::ZhCn,
        ));
        assert!(quarter.contains("25%"), "{quarter}");

        // out-of-range ratios are clamped rather than printed raw
        assert!(zh(opening_status(&p, 1, Some(4.0), Locale::ZhCn)).contains("100%"));
        assert!(zh(opening_status(&p, 1, Some(-1.0), Locale::ZhCn)).contains("0%"));
    }
}
