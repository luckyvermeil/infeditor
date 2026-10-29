//! Serialized access to the OS clipboard.
//!
//! The Windows clipboard is a single system-wide resource, and the
//! `arboard`/`clipboard-win` backends open/close it around every operation.
//! Two threads doing that inside one process can interleave one thread's
//! `GetClipboardData`/`GlobalLock` read with the other thread's
//! `CloseClipboard`, releasing the HGLOBAL while it is still being copied
//! out of. The result is a corrupted process heap (`STATUS_HEAP_CORRUPTION`,
//! 0xC0000374) — reproducible with eight threads doing nothing but clipboard
//! reads.
//!
//! The editor itself only touches the clipboard from the UI thread, but the
//! headless test suites run every `#[test]` in parallel (and a future worker
//! thread could join in), so **every OS clipboard operation in every crate**
//! goes through [`lock`]. The critical section is one clipboard round trip;
//! a poisoned lock is recovered instead of propagated — a panic elsewhere
//! must not brick copy/paste.
//!
//! This lives in `platform` (not in the host crate or `wysiwyg`) precisely
//! so that the host's and the component's clipboard paths share **one**
//! process-wide lock instead of two unrelated ones.

use std::sync::{Mutex, MutexGuard};

static CLIPBOARD: Mutex<()> = Mutex::new(());

/// Hold this guard for the whole lifetime of one OS clipboard operation.
///
/// Declare it **before** the `arboard::Clipboard` so it is released after the
/// clipboard handle is closed (locals drop in reverse declaration order).
pub fn lock() -> MutexGuard<'static, ()> {
    CLIPBOARD.lock().unwrap_or_else(|e| e.into_inner())
}
