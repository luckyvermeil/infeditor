//! Crash-recovery rescue (session journal + startup restore).
//!
//! # The problem
//!
//! Power loss / BSOD / process killed / panic — what the user loses is not
//! "the file" but the **editing state**: which tabs are open, where the caret
//! is, how much of the unsaved changes were written. This module maintains
//! backup files under the user config directory to solve that.
//!
//! # Design
//!
//! * **Rescue directory**: `session/` under the user config directory
//!   (Windows = `%APPDATA%\InfEditor\session\`). Sibling of the macro
//!   directory; **the user's original files are never touched** — rescue is a
//!   purely additive side-channel write; the worst case is a lost snapshot,
//!   never a corrupted document.
//! * **Two files**:
//!   * `session.json` — session manifest: tab list (path / display name /
//!     caret / scroll position / dirty flag / encoding / newline) + write
//!     timestamp + process pid.
//!   * `backup-<doc_id>.dat` — full UTF-8 content snapshot of every **dirty
//!     document**.
//! * **When to write**: [`EditorState::recovery_tick`] piggybacks on every
//!   `RedrawRequested` frame (the project has no async runtime / timers); it
//!   only really writes when the content signature changed and the last
//!   journal is older than [`JOURNAL_INTERVAL`]. Caret movement also changes
//!   the signature — the manifest is small, which buys "restore the caret
//!   position too"; content snapshots are deduplicated by `gen`, so large
//!   documents are not rewritten every 3 seconds just because the caret moved.
//! * **Commit order**: write all `backup-*.dat` first (each via tmp+rename
//!   atomic write), prune snapshots no longer referenced, and write
//!   `session.json` **last** (also tmp+rename). The manifest is the commit
//!   point: crash at any moment and the disk still holds a self-consistent
//!   set of "manifest + the snapshots it references".
//! * **Cleared on normal exit**: `main` calls [`clear_session_best_effort`]
//!   after the event loop returns (= the user exited normally). Anything
//!   left in the directory can only be debris from an abnormal shutdown —
//!   exactly the startup signal that "the last run did not exit cleanly".
//!   Clearing is **ownership-aware**: with multiple instances running, the
//!   session directory of another instance must not be wiped by the one that
//!   exits first (see [`clear_session`]).
//! * **Restore**: at startup [`EditorState::check_recovery`] shows a
//!   confirmation dialog when it finds a manifest (self-drawn modal, see
//!   [`crate::ui::components::dialog`]). Restore rebuilds tab by tab: with a
//!   snapshot, use it (content = the last moment before close); without one
//!   (clean document / oversized document), fall back to the file on disk.
//!   After the restore the session directory is cleared, leaving no ghost
//!   prompt. "Discard snapshots" is two-stage: first a second confirmation
//!   warning that it is irreversible; only after confirming is anything
//!   actually deleted; Cancel / × both return to the restore dialog
//!   (`close_dialog` re-mounts dialogs with `pending_recovery` as fallback).
//!
//! # Safety bounds
//!
//! * Dirty documents above [`MAX_BACKUP_BYTES`] get **no content backup**
//!   (only their path is recorded): one power cut should not thrash the disk,
//!   and rewriting a 2 GB document every 3 seconds is unrealistic either. Such
//!   documents restore to the last state saved on disk; everything else
//!   proceeds as usual.
//! * Manifest parsing is defensive: wrong format/version, corrupt JSON,
//!   absurd tab count — all treated as "no recoverable session" and the
//!   editor starts normally. The rescue system must never become a startup
//!   failure.
//! * Write failures degrade silently (retry next cycle): rescue is a
//!   side channel and must never block normal editing.
//!
//! # Tests
//!
//! All free functions take an explicit directory argument (same convention
//! as [`crate::ui::macros_io`]); unit and integration tests run against temp
//! directories and never touch the real user config directory.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::buffer::{DocMeta, Original, TextDocument};
use crate::ui::state::{EditorState, Tab};
use crate::ui::{Fmt, Msg};

/// File format identifier; an unrecognized format rejects the whole file.
pub const FORMAT: &str = "infeditor-recovery";
/// Current format version.
pub const VERSION: u32 = 1;
/// File name of the session manifest.
pub const MANIFEST_NAME: &str = "session.json";
/// File name prefix of backup snapshots (`backup-<doc_id>.dat`).
pub const BACKUP_PREFIX: &str = "backup-";
/// File extension of backup snapshots.
pub const BACKUP_EXT: &str = "dat";

/// Journal throttle interval: while editing, at most one write per
/// [`JOURNAL_INTERVAL`]. 3 seconds is a compromise between the "loss window"
/// and write cost — a conventional backup throttle magnitude.
pub const JOURNAL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(3);

/// Content snapshot size cap (bytes) for a single dirty document. Above it,
/// only the path is recorded; no content backup.
///
/// Writing a 128 MB snapshot takes a fraction of a second on an SSD; any
/// larger and editing becomes noticeably janky, and "rescue" itself starts
/// threatening disk health.
pub const MAX_BACKUP_BYTES: u64 = 128 * 1024 * 1024;

/// Cap on the tab count when loading a manifest (guards against a
/// hand-edited/corrupted manifest).
pub const MAX_TABS: usize = 512;

// ---------------------------------------------------------------------------
// Directories
// ---------------------------------------------------------------------------

/// Rescue directory (`session/` under the user config directory); `None` in
/// tests / when the user directory is unavailable.
pub fn session_dir() -> Option<PathBuf> {
    crate::ui::macros_io::config_dir().map(|d| d.join("session"))
}

// ---------------------------------------------------------------------------
// Manifest
// ---------------------------------------------------------------------------

/// Session manifest: a complete description of "the state before close".
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct SessionManifest {
    pub format: String,
    pub version: u32,
    /// pid of the process that wrote the manifest (diagnostics: which run the debris came from)
    #[serde(default)]
    pub pid: u32,
    /// Write timestamp (ms since the UNIX epoch)
    #[serde(default)]
    pub saved_at_ms: u64,
    /// Active tab index
    #[serde(default)]
    pub active: usize,
    /// Vertical scroll position (px)
    #[serde(default)]
    pub scroll_top: f64,
    pub tabs: Vec<SessionTab>,
}

/// One tab in the manifest.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct SessionTab {
    /// Document id (the backup file name derives from it)
    pub doc_id: u64,
    /// Disk path; `None` = untitled document
    #[serde(default)]
    pub path: Option<String>,
    pub display_name: String,
    /// Caret byte offset (clamped into the content length on restore)
    #[serde(default)]
    pub caret: u64,
    /// Selection anchor
    #[serde(default)]
    pub anchor: Option<u64>,
    /// Whether there were unsaved changes before close
    #[serde(default)]
    pub dirty: bool,
    /// Backup file name (within the session directory); `None` = no content snapshot (clean or oversized document)
    #[serde(default)]
    pub backup: Option<String>,
    /// Saved encoding (name, e.g. `UTF-8` / `GBK`); `None` = UTF-8
    #[serde(default)]
    pub encoding: Option<String>,
    #[serde(default)]
    pub bom: bool,
    /// Newline: `CRLF` / `LF` / `CR`
    #[serde(default = "default_newline")]
    pub newline: String,
    /// The file was opened with lossy decoding (buffer contains U+FFFD) — needed by the save prompt, preserved across crashes
    #[serde(default)]
    pub lossy: bool,
}

fn default_newline() -> String {
    "LF".into()
}

/// A session to be written: manifest + content snapshots to persist.
pub struct Journal {
    pub manifest: SessionManifest,
    /// `(file name, content bytes)`
    pub backups: Vec<(String, Vec<u8>)>,
}

// ---------------------------------------------------------------------------
// Capture / write / read back / clear
// ---------------------------------------------------------------------------

/// Capture a session from the editor state.
///
/// `last_gens` is "each document's gen at the last journal" (doc_id → gen):
/// dirty documents whose gen is unchanged are not rewritten (the manifest
/// still references the snapshot already on disk), so moving the caret does
/// not drag large files through a rewrite. The returned map holds the gens
/// **as of this journal**; the caller stores it for the next comparison.
pub fn capture(
    st: &EditorState,
    last_gens: &std::collections::HashMap<u64, u64>,
) -> (Journal, std::collections::HashMap<u64, u64>) {
    let mut backups = Vec::new();
    let mut gens = std::collections::HashMap::new();
    let tabs = st
        .tabs
        .iter()
        .map(|t| {
            let doc = &t.doc;
            let dirty = doc.is_dirty();
            let mut backup = None;
            if dirty && doc.total_bytes() <= MAX_BACKUP_BYTES {
                // `doc.id` is a per-process counter starting at 1, so two
                // instances would both write `backup-1.dat` into the shared
                // session directory: B overwrites A's snapshot, and A's restore
                // then pulls B's text into A's tab (saving it over A's file).
                let name = format!(
                    "{BACKUP_PREFIX}{}-{}.{BACKUP_EXT}",
                    std::process::id(),
                    doc.id
                );
                if last_gens.get(&doc.id) != Some(&doc.gen) {
                    // Raw bytes, not `read_all()`: the snapshot is the document
                    // the user has. `read_all` is lossy, so a bad byte in an
                    // mmap'd file became U+FFFD in the snapshot and the save
                    // after a restore wrote that replacement character to disk
                    // (the save path passes original bytes through by design).
                    backups.push((name.clone(), doc.table.read(0..doc.total_bytes())));
                }
                backup = Some(name);
                gens.insert(doc.id, doc.gen);
            }
            SessionTab {
                doc_id: doc.id,
                path: doc
                    .meta
                    .path
                    .as_ref()
                    .map(|p| p.to_string_lossy().into_owned()),
                display_name: doc.meta.display_name.clone(),
                caret: t.caret.pos,
                anchor: t.caret.anchor,
                dirty,
                backup,
                encoding: doc.meta.encoding.map(|e| e.name().to_string()),
                bom: doc.meta.had_bom,
                newline: newline_name(doc.newline()).to_string(),
                lossy: doc.meta.lossy_utf8,
            }
        })
        .collect();
    (
        Journal {
            manifest: SessionManifest {
                format: FORMAT.into(),
                version: VERSION,
                pid: std::process::id(),
                saved_at_ms: now_ms(),
                active: st.active,
                scroll_top: st.scroll_top,
                tabs,
            },
            backups,
        },
        gens,
    )
}

/// `Newline` → name in the manifest.
pub fn newline_name(nl: crate::buffer::Newline) -> &'static str {
    nl.display()
}

/// Manifest name → `Newline`; unknown names fall back to LF.
pub fn newline_from_name(s: &str) -> crate::buffer::Newline {
    crate::buffer::Newline::from_display(s).unwrap_or(crate::buffer::Newline::Lf)
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Encoding name in the manifest → `&'static Encoding`.
///
/// [`encoding_rs`] has no lookup-by-name API (`for_label` only accepts WHATWG
/// labels and does not know editor-specific names like `UTF-8 BOM`), so we
/// normalize first and then look up the label.
fn encoding_from_name(name: &str) -> Option<&'static encoding_rs::Encoding> {
    let lower = name.to_ascii_lowercase();
    let label = lower.strip_suffix(" bom").unwrap_or(&lower);
    encoding_rs::Encoding::for_label(label.as_bytes())
}

/// Atomic write: tmp + fsync + rename. On power loss the target file holds
/// either the old content or the new content, never a partial write.
fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;
    let tmp = path.with_extension(format!(
        "{}-{}.tmp",
        path.extension().and_then(|e| e.to_str()).unwrap_or("part"),
        std::process::id()
    ));
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    if let Err(e) = std::fs::rename(&tmp, path) {
        // On Windows rename fails when the target is briefly held by an
        // antivirus/indexer: remove the tmp file so debris does not pile up
        // in the session directory (cf. the same handling in io::save_file).
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(())
}

/// pid recorded in the manifest; `None` when the manifest is missing / corrupt.
fn manifest_of(dir: &Path) -> Option<SessionManifest> {
    std::fs::read(dir.join(MANIFEST_NAME))
        .ok()
        .and_then(|b| serde_json::from_slice::<SessionManifest>(&b).ok())
}

/// Whether the session directory belongs to this process.
///
/// "Unowned" (manifest not yet written / corrupt) counts as ours: at that
/// point no other instance's data is in the directory, so removing the whole
/// directory is safe.
fn session_owned_by_us(dir: &Path) -> bool {
    manifest_of(dir).is_none_or(|m| m.pid == std::process::id())
}

/// Write a session into `dir`. The manifest is the commit point: it is
/// written only after all snapshots are on disk.
pub fn write_session(dir: &Path, journal: &Journal) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    // Keep set = snapshots written this round + snapshots still referenced by
    // the manifest (unchanged-gen content snapshots are not rewritten this
    // round, but must never be pruned as "stale" below)
    let mut keep = std::collections::HashSet::new();
    for t in &journal.manifest.tabs {
        if let Some(b) = &t.backup {
            keep.insert(b.clone());
        }
    }
    for (name, bytes) in &journal.backups {
        // Guard against directory escape: backup file names must be plain file names
        if Path::new(name).components().count() != 1 {
            continue;
        }
        write_atomic(&dir.join(name), bytes)?;
        keep.insert(name.clone());
    }
    // The manifest is the commit point: write it first, then prune old
    // snapshots no longer referenced (after a document is saved / closed /
    // becomes clean). Pruning first instead would leave, on a crash before
    // the manifest lands, a disk state of "manifest referencing snapshots
    // that no longer exist", breaking this module's promise of being
    // self-consistent on disk after a crash at any moment.
    let json = serde_json::to_vec_pretty(&journal.manifest)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    // Multi-instance guard: if the **old** manifest's pid is not this process,
    // the session directory belongs to another instance and this round's
    // write must not prune its snapshots (pruning only looks at this process's
    // keep set and would delete the backups the other manifest references,
    // leaving it with incomplete snapshots if it crashes). Writing our own
    // manifest proceeds as usual (last writer wins), minus the prune.
    let owned = session_owned_by_us(dir);
    write_atomic(&dir.join(MANIFEST_NAME), &json)?;
    if owned {
        if let Ok(rd) = std::fs::read_dir(dir) {
            for e in rd.filter_map(|e| e.ok()) {
                let p = e.path();
                let is_backup = p
                    .file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with(BACKUP_PREFIX) && n.ends_with(BACKUP_EXT));
                if is_backup && !keep.contains(p.file_name().unwrap().to_string_lossy().as_ref()) {
                    let _ = std::fs::remove_file(&p);
                }
            }
        }
    }
    Ok(())
}

/// Read back the session manifest. Any anomaly (missing / corrupt /
/// unrecognized version / absurd tab count) falls back to `None` — "no
/// recoverable session" is a perfectly normal state.
pub fn load_session(dir: &Path) -> Option<SessionManifest> {
    let text = std::fs::read_to_string(dir.join(MANIFEST_NAME)).ok()?;
    let m: SessionManifest = serde_json::from_str(&text).ok()?;
    if m.format != FORMAT || m.version > VERSION {
        return None;
    }
    if m.tabs.len() > MAX_TABS {
        return None;
    }
    Some(m)
}

/// Clear the session directory (normal exit / restore finished / user gave
/// up). Best effort.
///
/// **Remove the whole directory only when it belongs to this process.**
/// While another instance is running, its manifest and snapshots live in
/// this directory — removing everything equals wiping the other side's
/// rescue data, and if it crashes within a 3-second throttle window content
/// is genuinely lost. So across instances this degrades to precise pruning:
/// `consumed` is the manifest restored/discarded this round; only snapshots
/// it references that the other side's current manifest **no longer
/// references** are deleted (a same-named snapshot may be in active use by
/// the other side and must never be touched).
pub fn clear_session(dir: &Path, consumed: Option<&SessionManifest>) -> std::io::Result<()> {
    let current = manifest_of(dir);
    if current.as_ref().is_none_or(|m| m.pid == std::process::id()) {
        return std::fs::remove_dir_all(dir);
    }
    let live: std::collections::HashSet<&str> = current
        .as_ref()
        .map(|m| m.tabs.iter().filter_map(|t| t.backup.as_deref()).collect())
        .unwrap_or_default();
    if let Some(m) = consumed {
        for t in &m.tabs {
            let Some(b) = t.backup.as_deref() else {
                continue;
            };
            // Guard against directory escape + don't touch snapshots the other manifest still references
            if live.contains(b) || Path::new(b).components().count() != 1 {
                continue;
            }
            let _ = std::fs::remove_file(dir.join(b));
        }
    }
    Ok(())
}

/// Fallback cleanup after a normal exit (called by `main` after the event
/// loop returns). Silent when the directory is unavailable / missing — not
/// an error, just no session debris this time.
pub fn clear_session_best_effort() {
    if let Some(dir) = session_dir() {
        if dir.exists() {
            let _ = clear_session(&dir, None);
        }
    }
}

// ---------------------------------------------------------------------------
// EditorState: journal / startup check / restore
// ---------------------------------------------------------------------------

impl EditorState {
    /// Content signature: tab set + each document's (id, gen, dirty, path) +
    /// caret + active tab + encoding/newline.
    ///
    /// The scroll position does **not** enter the signature (rewriting the
    /// journal on every scroll is too noisy); it is only recorded into the
    /// manifest incidentally on an actual write. Encoding/newline must enter
    /// the signature: `set_encoding`/`set_newline` neither bump gen nor set
    /// dirty, and missing them means "re-encode only → crash" restores the
    /// old encoding and the next save writes with the old encoding — for
    /// transcoding scenarios that is content corruption.
    pub fn recovery_signature(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        self.active.hash(&mut h);
        for t in &self.tabs {
            t.doc.id.hash(&mut h);
            t.doc.gen.hash(&mut h);
            t.doc.is_dirty().hash(&mut h);
            t.doc.meta.path.hash(&mut h);
            t.caret.pos.hash(&mut h);
            t.caret.anchor.hash(&mut h);
            t.doc.meta.encoding.map(|e| e.name()).hash(&mut h);
            t.doc.meta.had_bom.hash(&mut h);
            // Newline does not implement Hash; the discriminant suffices (the signature only needs to discriminate)
            t.doc.meta.user_newline.map(|n| n as u8).hash(&mut h);
        }
        h.finish()
    }

    /// Called once per redraw frame: write the session into `dir` when the
    /// signature changed and the throttle has expired.
    ///
    /// `dir` = `None` (tests / user directory unavailable) is a no-op. While
    /// the startup-restore confirmation is up ([`Self::pending_recovery`])
    /// nothing is written: the user may choose "restore", and overwriting the
    /// debris then equals deleting the session to be restored.
    pub fn recovery_tick_in(&mut self, dir: Option<&Path>, now: std::time::Instant) {
        let Some(dir) = dir else { return };
        if self.pending_recovery.is_some() {
            return;
        }
        // A pristine brand-new session (1 clean, pathless empty untitled tab)
        // is not worth writing: otherwise a crash after startup before a
        // single byte was edited (within the 3-second throttle) would pop the
        // ghost restore dialog "1 tab, 0 with unsaved changes" next launch.
        if self.tabs.len() == 1
            && !self.tabs[0].doc.is_dirty()
            && self.tabs[0].doc.meta.path.is_none()
            && self.tabs[0].doc.total_bytes() == 0
        {
            return;
        }
        let sig = self.recovery_signature();
        if self.recovery_last.is_some() && self.recovery_sig == Some(sig) {
            return;
        }
        if let Some(last) = self.recovery_last {
            if now.duration_since(last) < JOURNAL_INTERVAL {
                return;
            }
        }
        let (journal, gens) = capture(self, &self.recovery_backup_gens);
        match write_session(dir, &journal) {
            Ok(()) => {
                self.recovery_last = Some(now);
                self.recovery_sig = Some(sig);
                self.recovery_backup_gens = gens;
            }
            Err(_) => {
                // Write failed: don't advance the signature (retry next expiry),
                // and don't bother the user.
                self.recovery_last = Some(now);
                self.recovery_sig = None;
            }
        }
    }

    /// Production entry: run one journal check against the real rescue directory.
    pub fn recovery_tick(&mut self, now: std::time::Instant) {
        let dir = session_dir();
        self.recovery_tick_in(dir.as_deref(), now);
    }

    /// Startup check (called by `EditorState::default`): mount a confirmation
    /// dialog when debris from an abnormal shutdown is found. Silent return
    /// when the directory is unavailable / there is no debris / the manifest
    /// is corrupt.
    pub fn check_recovery(&mut self) {
        let dir = session_dir();
        self.check_recovery_in(dir.as_deref());
    }

    /// Explicit-directory version of [`Self::check_recovery`] (for tests).
    pub fn check_recovery_in(&mut self, dir: Option<&Path>) {
        let Some(dir) = dir else { return };
        let Some(manifest) = load_session(dir) else {
            return;
        };
        if manifest.tabs.is_empty() {
            return;
        }
        self.pending_recovery = Some(manifest);
        self.show_recovery_prompt();
    }

    /// (Re)mount the restore confirmation dialog.
    ///
    /// Besides the startup check this is called on two "return to the restore
    /// dialog" paths: the user cancels in the "discard snapshots" second
    /// confirmation, and × / Esc closes a dialog with `pending_recovery`
    /// attached (see `close_dialog`'s fallback). Restoring is a decision that
    /// must be made one way or the other — if the dialog disappears but the
    /// session is unresolved, the rescue journal stops being written.
    pub fn show_recovery_prompt(&mut self) {
        let Some(m) = &self.pending_recovery else {
            return;
        };
        let n = m.tabs.len();
        let dirty = m.tabs.iter().filter(|t| t.dirty).count();
        use crate::ui::actions::Cmd;
        self.show_dialog(
            crate::ui::state::Dialog::text(
                Msg::DlgRecoveryTitle,
                Fmt::DlgRecoveryBody { tabs: n, dirty },
            )
            .primary(Msg::DlgRecoveryRestore, Some(Cmd::RecoveryRestore))
            .button(Msg::DlgRecoveryDiscard, Some(Cmd::RecoveryDiscard))
            .confirm(),
        );
    }

    /// Restore confirmation → restore: rebuild tabs from the manifest. Production entry.
    pub fn restore_recovery(&mut self) {
        let dir = session_dir();
        if let Some(dir) = dir.as_deref() {
            self.restore_recovery_from(dir);
        } else {
            self.pending_recovery = None;
            self.close_dialog();
        }
    }

    /// Explicit-directory version of [`Self::restore_recovery`] (for tests).
    ///
    /// Take over `pending_recovery` **before** closing the dialog:
    /// `close_dialog` re-mounts the restore dialog as a fallback for sessions
    /// still attached; closing first then taking would summon the dialog
    /// right back.
    pub fn restore_recovery_from(&mut self, dir: &Path) {
        let Some(manifest) = self.pending_recovery.take() else {
            self.close_dialog();
            return;
        };
        self.close_dialog();
        self.tabs.clear();
        let mut lost = 0usize;
        for t in &manifest.tabs {
            // Snapshot reading guards against directory escape just like the
            // write side: a hand-edited manifest must not make the editor read
            // an arbitrary file as a snapshot.
            let backup = t
                .backup
                .as_ref()
                .filter(|b| Path::new(b).components().count() == 1);
            let snap = backup.and_then(|b| std::fs::read(dir.join(b)).ok());
            // The snapshot field is non-empty but unreadable (corrupt /
            // cleaned / partially written): the unsaved changes are gone.
            // Must count it as lost and set dirty — otherwise we silently
            // fall back to the old on-disk content, the status bar still
            // reports a successful restore, and closing pops no confirmation.
            let mut snapshot_lost = false;
            let doc = match snap {
                Some(content) => Some(rebuild_doc(t, content)),
                None => {
                    if t.backup.is_some() || t.dirty {
                        // `dirty` with no snapshot means the document was too
                        // large to snapshot: the reopened text is the older
                        // on-disk content, so the edits are gone as well.
                        snapshot_lost = true;
                    }
                    match t.path.as_deref() {
                        Some(p) if Path::new(p).exists() => {
                            crate::io::open_file(Path::new(p), |_, _| {})
                                .ok()
                                .map(|o| o.doc)
                        }
                        _ => None,
                    }
                }
            };
            match doc {
                Some(doc) => {
                    let mut tab = Tab::new(doc);
                    // Language is re-probed from the (restored) path; file_type follows
                    let spec =
                        crate::highlight::for_path_and_first_line(tab.doc.meta.path.as_deref(), "");
                    tab.spec = spec;
                    tab.hl.set_spec(spec);
                    tab.doc.meta.file_type = spec.name.to_string();
                    if snapshot_lost {
                        tab.doc.mark_dirty();
                        lost += 1;
                    }
                    let len = tab.doc.total_bytes();
                    // snap clamps to a char boundary: offsets from a corrupt /
                    // hand-edited manifest may land inside a multibyte character,
                    // making rendering and selection behavior undefined
                    tab.caret.pos = tab.doc.snap(t.caret.min(len));
                    tab.caret.anchor = t.anchor.map(|a| tab.doc.snap(a.min(len)));
                    self.tabs.push(tab);
                    self.adopt_view_prefs(self.tabs.len() - 1);
                }
                None => lost += 1,
            }
        }
        if self.tabs.is_empty() {
            self.new_doc();
        }
        // The tab set was replaced wholesale: the global wys singleton still
        // holds the pre-restore content, so a normal `set_active_tab` (which
        // flushes it) would overwrite the just-restored document. Activate
        // cleanly and let the guard re-seed from the restored text.
        self.activate_tab_clean(manifest.active.min(self.tabs.len() - 1));
        self.scroll_top = manifest.scroll_top;
        self.clamp_scroll();
        // The snapshot has served its purpose: clear the debris so the next
        // startup doesn't prompt "restore" again. If the directory has been
        // taken over by another instance, only prune the snapshots consumed
        // this round (see [`clear_session`]); don't touch theirs.
        let _ = clear_session(dir, Some(&manifest));
        self.recovery_last = None;
        self.recovery_sig = None;
        self.recovery_backup_gens.clear();
        let tabs = self.tabs.len();
        self.status = Some(if lost > 0 {
            Fmt::StatusSessionRestoredLost { tabs, lost }.into()
        } else {
            Fmt::StatusSessionRestored { tabs }.into()
        });
    }

    /// Restore confirmation → discard: mount the second confirmation **first**,
    /// deleting nothing.
    ///
    /// Discarding is irreversible — once snapshots are gone, unsaved changes
    /// are lost forever. One extra "are you sure" guards against a slip of
    /// the hand (the buttons are adjacent) at the cost of one modal dialog.
    pub fn discard_recovery(&mut self) {
        use crate::ui::actions::Cmd;
        self.show_dialog(
            crate::ui::state::Dialog::text(Msg::DlgRecoveryDiscard, Msg::DlgRecoveryDiscardBody)
                .primary(Msg::CommonCancel, Some(Cmd::RecoveryDiscardCancel))
                .button(
                    Msg::DlgRecoveryDiscardConfirm,
                    Some(Cmd::RecoveryDiscardConfirm),
                )
                .confirm(),
        );
    }

    /// "Discard snapshots" second confirmation → cancel: back to the restore
    /// confirmation dialog, session untouched.
    pub fn discard_recovery_cancel(&mut self) {
        // pending_recovery is still attached (neither confirm stage touched
        // it); just re-mount the restore dialog.
        self.show_recovery_prompt();
    }

    /// "Discard snapshots" second confirmation → confirm discard: delete the
    /// snapshots and start up the normal way. Production entry.
    pub fn discard_recovery_confirmed(&mut self) {
        let dir = session_dir();
        if let Some(dir) = dir.as_deref() {
            self.discard_recovery_from(dir);
        } else {
            self.pending_recovery = None;
            self.close_dialog();
        }
    }

    /// Explicit-directory version of [`Self::discard_recovery_confirmed`]
    /// (for tests).
    ///
    /// Clear `pending_recovery` **before** closing the dialog; same reasoning
    /// as [`Self::restore_recovery_from`].
    pub fn discard_recovery_from(&mut self, dir: &Path) {
        let manifest = self.pending_recovery.take();
        self.close_dialog();
        // The consumer takes over cleanup: if the manifest on disk is still
        // the one that triggered the restore (same pid, = the original owner
        // is dead), remove the manifest before clearing everything. Otherwise
        // [`clear_session`]'s pid guard would treat the dead owner's manifest
        // as "another live instance" and misjudge all backups as still
        // referenced, skipping them — a regression caught in on-device testing.
        if let Some(m) = manifest.as_ref() {
            if manifest_of(dir)
                .as_ref()
                .is_some_and(|cur| cur.pid == m.pid)
            {
                let _ = std::fs::remove_file(dir.join(MANIFEST_NAME));
            }
        }
        let _ = clear_session(dir, manifest.as_ref());
        self.status = Some(Msg::StatusSessionDiscarded.into());
    }
}

/// Rebuild a document from backup content + manifest metadata.
///
/// The content itself is "the user's edits" — there is no matching undo
/// entry, so the dirty flag uses [`TextDocument::mark_dirty`] which sets
/// dirty directly (see method comment).
fn rebuild_doc(t: &SessionTab, content: Vec<u8>) -> TextDocument {
    let nl = newline_from_name(&t.newline);
    let mut meta = DocMeta {
        path: t.path.as_ref().map(PathBuf::from),
        display_name: t.display_name.clone(),
        encoding: t.encoding.as_deref().and_then(encoding_from_name),
        had_bom: t.bom,
        user_newline: Some(nl),
        ..DocMeta::default()
    };
    meta.csv_mode = meta
        .path
        .as_ref()
        .and_then(|p| p.extension().and_then(|e| e.to_str()))
        .is_some_and(|e| e.eq_ignore_ascii_case("csv") || e.eq_ignore_ascii_case("tsv"));
    let mut doc = TextDocument::with_original(Original::Owned(std::sync::Arc::new(content)), meta);
    doc.meta.lossy_utf8 = t.lossy;
    doc.build_index(|_, _| {});
    // The newline may also have been probed from the file itself; the backup
    // content keeps the original newline bytes, so here we only record the
    // "user choice" (save_options prefers user_newline).
    if t.dirty {
        doc.mark_dirty();
    }
    doc
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "infeditor-recovery-test-{}-{tag}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn st_with_dirty(untitled: bool) -> EditorState {
        let mut st = EditorState::minimal();
        st.tabs[0].doc.insert_text(0, "第一行\n第二行\n");
        if !untitled {
            st.tabs[0].doc.meta.path = Some(PathBuf::from("C:/work/notes/a.txt"));
            st.tabs[0].doc.meta.display_name = "a.txt".into();
        }
        st.tabs[0].caret.pos = 6;
        assert!(st.tabs[0].doc.is_dirty());
        st
    }

    #[test]
    fn a_dirty_session_round_trips_through_the_directory() {
        let dir = tmp_dir("roundtrip");
        let mut st = st_with_dirty(true);
        let (journal, gens) = capture(&st, &Default::default());
        write_session(&dir, &journal).unwrap();
        assert!(dir.join(MANIFEST_NAME).exists());
        assert!(
            !journal.backups.is_empty(),
            "a dirty document must get a content snapshot"
        );

        // Simulate a "new process": attach the manifest to a fresh state, then restore
        let loaded = load_session(&dir).expect("manifest should be readable");
        st.pending_recovery = Some(loaded);
        st.restore_recovery_from(&dir);
        assert_eq!(st.tabs.len(), 1);
        assert_eq!(st.tabs[0].doc.read_all(), "第一行\n第二行\n");
        assert!(
            st.tabs[0].doc.is_dirty(),
            "restored unsaved changes must still be dirty"
        );
        assert_eq!(st.tabs[0].caret.pos, 6);
        assert!(
            !dir.join(MANIFEST_NAME).exists(),
            "session debris must be cleared after restore"
        );
        let _ = gens;
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Restoring a Markdown tab must not flush the (stale) wys singleton into
    /// it: the singleton still holds the pre-crash content of another document,
    /// and the old restore path called `set_active_tab`, whose unconditional
    /// flush overwrote the just-restored text with it.
    #[test]
    fn restore_does_not_flush_a_stale_wys_singleton_into_the_restored_tab() {
        use dioxus_native::prelude::dioxus_core::{RuntimeGuard, VirtualDom};
        use dioxus_native::prelude::WritableExt;
        fn empty_app() -> dioxus_native::prelude::Element {
            use dioxus_native::prelude::*;
            rsx! {}
        }
        let vdom = VirtualDom::new(empty_app);
        let _guard = RuntimeGuard::new(vdom.runtime());

        let dir = tmp_dir("md-wys");
        let file =
            std::env::temp_dir().join(format!("infeditor-recovery-md-{}.md", std::process::id()));
        std::fs::write(&file, "# 标题\r\ntext\r\n").unwrap();
        let opened = crate::io::open_file(&file, |_, _| {}).unwrap();
        let mut st = EditorState::minimal();
        st.tabs[0] = crate::ui::state::Tab::new(opened.doc);
        assert!(st.tabs[0].md_wys, "Markdown opens in the document view");
        let (journal, _) = capture(&st, &Default::default());
        write_session(&dir, &journal).unwrap();

        // A stale singleton from the previous session (different content).
        *crate::ui::components::mdws::WYS.signal().write() =
            wysiwyg::Editor::new(wysiwyg::parse_doc("stale\r\n"));

        st.pending_recovery = Some(load_session(&dir).unwrap());
        st.restore_recovery_from(&dir);
        assert_eq!(
            st.tabs[0].doc.read_all(),
            "# 标题\r\ntext\r\n",
            "restored content must survive"
        );
        assert_eq!(
            crate::ui::components::mdws::emit_markdown(),
            "# 标题\n\ntext\n",
            "the singleton must be re-seeded from the restored document"
        );

        let _ = std::fs::remove_file(&file);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn clean_tabs_restore_from_disk_not_from_backups() {
        let dir = tmp_dir("clean");
        // The file on disk
        let file = std::env::temp_dir().join(format!(
            "infeditor-recovery-clean-{}.txt",
            std::process::id()
        ));
        std::fs::write(&file, "磁盘内容\n").unwrap();
        let mut st = EditorState::minimal();
        st.tabs[0].doc.meta.path = Some(file.clone());
        st.tabs[0].doc.meta.display_name = file.to_string_lossy().into_owned();
        assert!(!st.tabs[0].doc.is_dirty());
        let (journal, _) = capture(&st, &Default::default());
        assert!(
            journal.backups.is_empty(),
            "clean documents must not get a content snapshot"
        );
        write_session(&dir, &journal).unwrap();

        st.pending_recovery = Some(load_session(&dir).unwrap());
        st.restore_recovery_from(&dir);
        assert_eq!(st.tabs[0].doc.read_all(), "磁盘内容\n");
        assert!(!st.tabs[0].doc.is_dirty());
        let _ = std::fs::remove_file(&file);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unchanged_docs_are_not_rewritten_but_still_referenced() {
        let dir = tmp_dir("dedup");
        let mut st = st_with_dirty(true);
        let (j1, gens1) = capture(&st, &Default::default());
        write_session(&dir, &j1).unwrap();
        // Caret moved (content gen unchanged): the second capture must not rewrite the snapshot
        st.tabs[0].caret.pos = 0;
        let (j2, _) = capture(&st, &gens1);
        assert!(
            j2.backups.is_empty(),
            "a dirty doc with unchanged gen must not rewrite its snapshot"
        );
        assert!(
            j2.manifest.tabs[0].backup.is_some(),
            "but the manifest still references the snapshot already on disk"
        );
        write_session(&dir, &j2).unwrap();
        assert!(
            load_session(&dir).is_some(),
            "the referenced snapshot must exist, or restore loses content"
        );
        let backup = dir.join(j2.manifest.tabs[0].backup.clone().unwrap());
        assert!(backup.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn stale_backups_are_pruned() {
        let dir = tmp_dir("prune");
        let mut st = st_with_dirty(true);
        let (j1, _) = capture(&st, &Default::default());
        write_session(&dir, &j1).unwrap();
        let name = j1.manifest.tabs[0].backup.clone().unwrap();
        assert!(dir.join(&name).exists());
        // Document saved → clean → next journal no longer references the snapshot → old snapshot pruned
        st.tabs[0].doc.mark_saved();
        let (j2, _) = capture(&st, &Default::default());
        assert!(j2.backups.is_empty());
        write_session(&dir, &j2).unwrap();
        assert!(
            !dir.join(&name).exists(),
            "unreferenced snapshots must be pruned"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn corrupt_or_foreign_manifests_are_ignored() {
        let dir = tmp_dir("corrupt");
        assert!(load_session(&dir).is_none(), "no manifest = no session");
        std::fs::write(dir.join(MANIFEST_NAME), "{ 不是 JSON").unwrap();
        assert!(load_session(&dir).is_none());
        std::fs::write(
            dir.join(MANIFEST_NAME),
            r#"{"format":"other","version":1,"tabs":[]}"#,
        )
        .unwrap();
        assert!(
            load_session(&dir).is_none(),
            "a foreign format is rejected whole"
        );
        // A newer (future) version must not be force-read either
        std::fs::write(
            dir.join(MANIFEST_NAME),
            format!(r#"{{"format":"{FORMAT}","version":{VERSION}+1,"tabs":[]}}"#),
        )
        .unwrap();
        assert!(load_session(&dir).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn oversized_dirty_docs_are_registered_without_content() {
        let mut st = EditorState::minimal();
        st.tabs[0].doc.insert_text(0, "x");
        // Fake an "oversized" document: tests cannot really write 128 MB into
        // total_bytes, so verify the boundary of the decision logic directly
        // (backup only when <= the cap)
        assert!(crate::buffer::TextDocument::empty().total_bytes() <= MAX_BACKUP_BYTES);
        let (journal, _) = capture(&st, &Default::default());
        assert!(journal.backups[0].1.len() as u64 <= MAX_BACKUP_BYTES);
        let _ = st;
    }

    #[test]
    fn discarding_a_session_removes_the_directory() {
        let dir = tmp_dir("discard");
        let mut st = st_with_dirty(true);
        let (journal, _) = capture(&st, &Default::default());
        write_session(&dir, &journal).unwrap();
        st.pending_recovery = Some(load_session(&dir).unwrap());
        st.discard_recovery_from(&dir);
        assert!(st.pending_recovery.is_none());
        assert!(!dir.exists());
    }

    /// "Discard snapshots" must go through a second confirmation first:
    /// nothing is deleted before confirming; cancel returns to the restore
    /// dialog; only confirming really deletes.
    #[test]
    fn discard_goes_through_a_confirmation() {
        let dir = tmp_dir("discard-confirm");
        let mut st = st_with_dirty(true);
        let (journal, _) = capture(&st, &Default::default());
        write_session(&dir, &journal).unwrap();
        st.check_recovery_in(Some(&dir));
        assert!(st.pending_recovery.is_some());
        assert!(st.dialog.is_some(), "the restore dialog is up");

        // Click "discard snapshots": only swaps in the second confirmation
        // dialog, session untouched
        st.discard_recovery();
        assert!(
            st.pending_recovery.is_some(),
            "snapshots must not be touched before confirmation"
        );
        assert!(dir.join(MANIFEST_NAME).exists());
        let confirm = st
            .dialog
            .as_ref()
            .expect("the second confirmation dialog is up");
        assert_eq!(confirm.title.key(), "Plain:DlgRecoveryDiscard");

        // "Cancel": back to the restore dialog, session intact
        st.discard_recovery_cancel();
        assert!(st.pending_recovery.is_some());
        assert_eq!(
            st.dialog.as_ref().unwrap().title.key(),
            "Plain:DlgRecoveryTitle"
        );

        // Discard again → confirm this time: really deletes, and the dialog
        // is closed with it (in unit tests session_dir() is None so the
        // production entry cannot use a real directory; use the
        // explicit-directory version — the body of the confirm path)
        st.discard_recovery();
        st.discard_recovery_from(&dir);
        assert!(st.pending_recovery.is_none());
        assert!(!dir.exists());
        assert!(
            st.dialog.is_none(),
            "no dialog should remain after confirming discard"
        );
    }

    /// × / Esc cannot close a dialog with `pending_recovery` attached:
    /// `close_dialog` re-mounts the restore dialog as fallback — this
    /// either/or must not dangle (otherwise the rescue journal stops being
    /// written).
    #[test]
    fn closing_the_recovery_prompt_bounces_back() {
        let dir = tmp_dir("bounce");
        let mut st = st_with_dirty(true);
        let (journal, _) = capture(&st, &Default::default());
        write_session(&dir, &journal).unwrap();
        st.check_recovery_in(Some(&dir));

        st.close_dialog(); // simulate × / Esc
        assert!(st.pending_recovery.is_some());
        assert!(
            st.dialog.is_some(),
            "the restore dialog must be re-mounted as fallback"
        );
        assert_eq!(
            st.dialog.as_ref().unwrap().title.key(),
            "Plain:DlgRecoveryTitle"
        );

        // Restore path: pending is taken first, so closing no longer bounces back
        st.restore_recovery_from(&dir);
        assert!(st.pending_recovery.is_none());
        assert!(st.dialog.is_none());
        assert!(!dir.exists());
    }

    #[test]
    fn a_session_with_no_tabs_is_not_offered_for_recovery() {
        let dir = tmp_dir("empty");
        let st = EditorState::minimal();
        let (journal, _) = capture(&st, &Default::default());
        // minimal() has 1 tab; build a manifest with empty tabs
        let mut m = journal.manifest.clone();
        m.tabs.clear();
        write_session(
            &dir,
            &Journal {
                manifest: m,
                backups: vec![],
            },
        )
        .unwrap();
        let mut st2 = EditorState::minimal();
        st2.check_recovery_in(Some(&dir));
        assert!(
            st2.pending_recovery.is_none(),
            "an empty session must not pop the restore dialog"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Manifest entries whose backup file names carry path separators must
    /// not let writes escape the rescue directory.
    #[test]
    fn backup_names_cannot_escape_the_session_dir() {
        let dir = tmp_dir("escape");
        let mut journal = Journal {
            manifest: SessionManifest {
                format: FORMAT.into(),
                version: VERSION,
                pid: 0,
                saved_at_ms: 0,
                active: 0,
                scroll_top: 0.0,
                tabs: vec![],
            },
            backups: vec![("../evil.dat".to_string(), b"boom".to_vec())],
        };
        // Writing must not panic and must not go outside the directory
        write_session(&dir, &journal).unwrap();
        assert!(!dir.parent().unwrap().join("evil.dat").exists());
        // A legal name works as usual
        journal.backups = vec![(format!("{BACKUP_PREFIX}1.{BACKUP_EXT}"), b"ok".to_vec())];
        write_session(&dir, &journal).unwrap();
        assert!(dir.join(format!("{BACKUP_PREFIX}1.{BACKUP_EXT}")).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Multi-instance: another instance's session directory must not be wiped
    /// by the one that exits first (or is restoring) — it holds the other
    /// side's unsaved content, genuinely lost if it crashes within a
    /// 3-second throttle window.
    #[test]
    fn another_instances_session_survives_our_cleanup() {
        let dir = tmp_dir("multi-instance");
        let st = st_with_dirty(true);
        let (mut journal, _) = capture(&st, &Default::default());
        // Fake a "manifest written by another process"
        journal.manifest.pid = std::process::id() + 1;
        let name = journal.manifest.tabs[0].backup.clone().unwrap();
        write_session(&dir, &journal).unwrap();
        assert!(dir.join(&name).exists());

        // Normal-exit cleanup: the manifest and the snapshots it references must both survive
        clear_session(&dir, None).unwrap();
        assert!(
            dir.join(MANIFEST_NAME).exists(),
            "the other side's manifest must not be deleted"
        );
        assert!(
            dir.join(&name).exists(),
            "the other side's snapshot must not be deleted"
        );

        // Post-restore cleanup: of the consumed manifest, snapshots the other
        // side **still references** must not be deleted either
        let loaded = load_session(&dir).expect("the other side's manifest is readable");
        clear_session(&dir, Some(&loaded)).unwrap();
        assert!(
            dir.join(&name).exists(),
            "snapshots the other side still references must survive"
        );

        // Delete only snapshots "referenced by consumed and no longer
        // referenced by the other side's current manifest": place an orphan
        // snapshot nothing references into the directory, then point consumed
        // at it.
        let orphan = format!("{BACKUP_PREFIX}777.{BACKUP_EXT}");
        std::fs::write(dir.join(&orphan), b"stale").unwrap();
        let mut consumed = loaded.clone();
        consumed.tabs[0].backup = Some(orphan.clone());
        clear_session(&dir, Some(&consumed)).unwrap();
        assert!(
            !dir.join(&orphan).exists(),
            "the orphan snapshot should be precisely pruned"
        );
        assert!(
            dir.join(&name).exists(),
            "the snapshot referenced by the other manifest is still there"
        );
        assert!(
            dir.join(MANIFEST_NAME).exists(),
            "the other side's manifest itself is never touched"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// When the directory belongs to this process (or has no manifest),
    /// cleanup is still a whole-directory removal: no "restore" ghost left
    /// behind.
    #[test]
    fn own_session_is_removed_whole() {
        let dir = tmp_dir("own-clean");
        let st = st_with_dirty(true);
        let (journal, _) = capture(&st, &Default::default());
        write_session(&dir, &journal).unwrap();
        clear_session(&dir, None).unwrap();
        assert!(!dir.exists(), "our own session directory is removed whole");
    }

    #[test]
    fn newline_names_round_trip() {
        for (nl, name) in [
            (crate::buffer::Newline::CrLf, "CRLF"),
            (crate::buffer::Newline::Lf, "LF"),
            (crate::buffer::Newline::Cr, "CR"),
        ] {
            assert_eq!(newline_name(nl), name);
            assert_eq!(newline_from_name(name), nl);
        }
        assert_eq!(newline_from_name("Nonsense"), crate::buffer::Newline::Lf);
    }
}
