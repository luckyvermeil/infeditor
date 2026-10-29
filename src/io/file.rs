//! Opening and saving documents.
//!
//! Open strategy: files >= the huge threshold that are UTF-8 without BOM are
//! memory-mapped and indexed in one background pass (memory usage stays at
//! ~ the OS page cache + edits). Everything else is read fully, converted to
//! UTF-8 (BOM stripped, encoding remembered for saving), and indexed.
//!
//! Save strategy: UTF-8 documents without newline conversion are streamed
//! straight from the piece table; otherwise lines are re-encoded / converted.

use std::io::{Read, Seek, Write};
use std::path::{Path, PathBuf};

use crate::buffer::line_index::Newline;
use crate::buffer::{Original, TextDocument};
use crate::io::encoding::{self, Detected};

/// Open a file for memory-mapping. The Windows sharing-mode dance (READ|
/// DELETE but not WRITE, and why) lives in `platform::open_semi_shared` —
/// this is the one call site, and it is platform-neutral.
fn open_for_mmap(path: &Path) -> std::io::Result<std::fs::File> {
    platform::open_semi_shared(path)
}

/// Files at or above this size are memory-mapped instead of read into a heap
/// buffer (UTF-8 without BOM only — editing and saving assume UTF-8 bytes).
///
/// The scan itself has to touch every byte either way; what mmap saves is the
/// `read_to_end` copy *plus* the multi-hundred-megabyte allocation that would
/// otherwise have to be faulted in and then kept alive next to the piece table.
/// The old value (256 MB) meant a 200 MB file paid both costs for no reason.
pub const HUGE_THRESHOLD: u64 = 64 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum OpenError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    /// A read failed *after* encoding detection (today: the mmap). The text is
    /// what the status bar shows (rendered at process-level language; the
    /// engine layer has no EditorState), and `source` keeps the OS error, so
    /// `kind()` and the error chain survive the trip to the UI.
    #[error("{0}")]
    Read(String, #[source] std::io::Error),
}

impl OpenError {
    /// [`Self::Read`] for an mmap failure: localized text plus the OS error it
    /// came from.
    fn mmap(e: std::io::Error) -> Self {
        Self::Read(
            i18n::current()
                .tr(i18n::Fmt::StatusReadFileFailed { err: e.to_string() })
                .into_owned(),
            e,
        )
    }
}

pub struct Opened {
    pub doc: TextDocument,
    /// size of the source file
    pub file_size: u64,
    /// true when the text sits in a read-only mmap of the file
    pub memory_mapped: bool,
    /// false when the line index covers only a prefix and a background scan
    /// is still filling it in (progressive open of a huge file)
    pub index_complete: bool,
    /// Prefix probe reported UTF-8 but the full-file check failed: already
    /// decoded leniently as UTF-8 (invalid bytes → U+FFFD). The status bar
    /// hints based on this, so the user doesn't think the file was always
    /// mojibake.
    pub utf8_lossy: bool,
}

/// Phase 1 of a progressive open indexes this many bytes (or 4096 lines'
/// worth, whichever the scan hits first is irrelevant — the byte bound is
/// what keeps the first paint constant-time).
pub const INDEX_PREFIX_BYTES: u64 = 8 * 1024 * 1024;

fn detect_prefix(f: &mut std::fs::File, file_len: u64) -> std::io::Result<(Vec<u8>, Detected)> {
    let n = file_len.min(64 * 1024) as usize;
    let mut buf = vec![0u8; n];
    let mut read = 0;
    while read < n {
        match f.read(&mut buf[read..])? {
            0 => break,
            k => read += k,
        }
    }
    buf.truncate(read);
    // chardetng works on the sampled prefix; a full read for borderline files
    // would defeat the point of sampling
    let detected = encoding::detect(&buf);
    Ok((buf, detected))
}

/// Open a file into a `TextDocument`. `progress(done_bytes, total_bytes)` is
/// invoked during index building. Runs quickly for normal files and scans at
/// memory speed for huge ones.
pub fn open_file(path: &Path, progress: impl FnMut(u64, u64)) -> Result<Opened, OpenError> {
    open_file_with_limits(path, OpenLimits::default(), progress)
}

/// The byte limits that decide which open path a file takes.
///
/// They are constants in production ([`HUGE_THRESHOLD`] /
/// [`INDEX_PREFIX_BYTES`]), but a branch that only runs above 64 MiB is a
/// branch no test ever takes — the memory-mapped and progressive tests below
/// pass kilobyte limits instead of writing hundreds of megabytes into the temp
/// directory.
#[derive(Clone, Copy)]
pub struct OpenLimits {
    /// Files at or above this size are memory-mapped instead of read.
    pub mmap_threshold: u64,
    /// How much of a progressively opened file the first index pass covers.
    pub index_prefix: u64,
}

impl Default for OpenLimits {
    fn default() -> Self {
        Self {
            mmap_threshold: HUGE_THRESHOLD,
            index_prefix: INDEX_PREFIX_BYTES,
        }
    }
}

/// [`open_file`] with the limits spelled out (see [`OpenLimits`]).
fn open_file_with_limits(
    path: &Path,
    limits: OpenLimits,
    mut progress: impl FnMut(u64, u64),
) -> Result<Opened, OpenError> {
    let mut f = std::fs::File::open(path)?;
    let file_len = f.metadata()?.len();
    if file_len == 0 {
        let mut doc = TextDocument::empty();
        doc.meta.path = Some(path.to_path_buf());
        doc.meta.display_name = file_name(path);
        doc.build_index(|_, _| {});
        return Ok(Opened {
            doc,
            file_size: 0,
            memory_mapped: false,
            index_complete: true,
            utf8_lossy: false,
        });
    }
    let (prefix, detected) = detect_prefix(&mut f, file_len)?;

    let use_mmap = file_len >= limits.mmap_threshold
        && detected.utf8_fast
        && !detected.bom
        && detected.encoding == encoding_rs::UTF_8;

    let mut utf8_lossy = false;
    let mut doc = if use_mmap {
        let file = open_for_mmap(path)?;
        let mmap = unsafe { memmap2::Mmap::map(&file) }.map_err(OpenError::mmap)?;
        // Note: the probe window is only the first 64 KB; the later part of
        // the mapped content **may not be** valid UTF-8. No full-file
        // validation here (that would turn opening a huge file into a full
        // sequential read, exactly what mmap avoids): the read path renders
        // bad bytes as U+FFFD, while saving passes raw bytes straight
        // through — bad bytes stay in place, no data loss (see
        // `huge_invalid_utf8_tail_survives_a_save`).
        let mut doc = TextDocument::with_original(
            Original::Mmap(std::sync::Arc::new(mmap)),
            Default::default(),
        );
        doc.meta.encoding = Some(encoding_rs::UTF_8);
        doc.meta.had_bom = false;
        doc.build_index(&mut progress);
        doc
    } else {
        // read whole file
        f.seek(std::io::SeekFrom::Start(0))?;
        let mut bytes = Vec::with_capacity(file_len as usize);
        f.read_to_end(&mut bytes)?;
        let detected = if prefix.is_empty() {
            Detected::new(encoding_rs::UTF_8, false, true)
        } else {
            detected
        };
        let (text_bytes, enc, bom) = if detected.utf8_fast {
            // Guard: `bom` comes from the 64KB prefix probe while `bytes` is
            // the full file read afterwards — if the file was truncated to <3
            // bytes between the two reads, `bytes[3..]` would panic (same
            // check in encoding.rs::decode_all).
            if detected.bom && bytes.len() >= 3 {
                (bytes[3..].to_vec(), encoding_rs::UTF_8, true)
            } else {
                // A valid 64KB prefix ≠ a valid full file: the middle/later
                // part of concatenated logs or exports with a binary tail can
                // be invalid. TextDocument's "content is always valid UTF-8"
                // invariant must hold — on validation failure fall back to
                // lenient decoding (U+FFFD), so read_str doesn't need repeated
                // lossy fallback on every line's hot path.
                if std::str::from_utf8(&bytes).is_err() {
                    // Re-detect on the **whole** buffer first. Detection only
                    // saw the first 64 KB, so a GBK/Big5/Shift-JIS file whose
                    // head is plain ASCII (SQL dump header, CSV header, a
                    // comment banner) was classified as UTF-8 — decoding it
                    // lossily turned every CJK character into U+FFFD, and the
                    // next save wrote those replacement characters back. Such a
                    // file is losslessly decodable; only genuinely broken bytes
                    // may take the lossy path.
                    let full = encoding::detect(&bytes);
                    if full.utf8_fast || encoding::mostly_valid_utf8(&bytes) {
                        utf8_lossy = true;
                        let (text, _, _) = detected.encoding.decode(&bytes);
                        (text.into_owned().into_bytes(), encoding_rs::UTF_8, false)
                    } else {
                        let (text, replaced) = encoding::decode_all_checked(&bytes, full);
                        utf8_lossy = replaced;
                        let (enc, bom) = if full.utf32.is_some() {
                            (encoding_rs::UTF_8, false)
                        } else {
                            (full.encoding, full.bom)
                        };
                        (text.into_bytes(), enc, bom)
                    }
                } else {
                    (bytes, encoding_rs::UTF_8, false)
                }
            }
        } else {
            let (text, replaced) = encoding::decode_all_checked(&bytes, detected);
            // Replacement characters mean the file cannot be reproduced
            // byte-for-byte on save (the encoder writes U+FFFD as an NCR), so
            // the save prompt has to know — this used to be tracked only for
            // the UTF-8 fallback above.
            utf8_lossy = replaced;
            // UTF-32 sources are decoded manually; saving defaults to UTF-8.
            // Otherwise keep the detected BOM flag: `decode_all` strips the
            // BOM, and dropping the flag here made a save re-encode UTF-16
            // *without* a BOM — other tools then misread the file as ANSI.
            let (enc, bom) = if detected.utf32.is_some() {
                (encoding_rs::UTF_8, false)
            } else {
                (detected.encoding, detected.bom)
            };
            (text.into_bytes(), enc, bom)
        };
        let mut doc = TextDocument::with_original(
            Original::Owned(std::sync::Arc::new(text_bytes)),
            Default::default(),
        );
        doc.meta.encoding = Some(enc);
        doc.meta.had_bom = bom;
        doc.build_index(&mut progress);
        doc
    };

    doc.meta.path = Some(path.to_path_buf());
    doc.meta.display_name = file_name(path);
    doc.meta.detected_newline = doc.index.newline;
    doc.meta.file_type = guess_file_type(path, &doc);
    // The lenient-decode flag travels with the document: saving uses it to
    // hint "The written-back bytes are no longer the original bytes".
    doc.meta.lossy_utf8 = utf8_lossy;
    doc.meta.csv_mode = matches!(
        path.extension().and_then(|e| e.to_str()).map(|s| s.to_ascii_lowercase()),
        Some(ref e) if e == "csv" || e == "tsv"
    );
    let memory_mapped = matches!(doc.table.original(), Original::Mmap(_));
    Ok(Opened {
        doc,
        file_size: file_len,
        memory_mapped,
        index_complete: true,
        utf8_lossy,
    })
}

/// Progressive open for huge files: map the file, index only a small prefix
/// and return immediately — the caller keeps the document interactive while a
/// background thread finishes the index (see `scan_continue`). Anything that
/// is not a huge mmap-able UTF-8 file falls back to the full [`open_file`]
/// path, which is fast enough at those sizes.
pub fn open_file_progressive(
    path: &Path,
    progress: impl FnMut(u64, u64),
) -> Result<Opened, OpenError> {
    open_file_progressive_with_limits(path, OpenLimits::default(), progress)
}

/// [`open_file_progressive`] with the limits spelled out, so the prefix-index
/// branch can be exercised without a 128 MiB file (see [`OpenLimits`]).
fn open_file_progressive_with_limits(
    path: &Path,
    limits: OpenLimits,
    mut progress: impl FnMut(u64, u64),
) -> Result<Opened, OpenError> {
    let mut f = std::fs::File::open(path)?;
    let file_len = f.metadata()?.len();
    let prefix_ok = {
        let (prefix, detected) = detect_prefix(&mut f, file_len)?;
        let _ = prefix;
        detected.utf8_fast && !detected.bom && detected.encoding == encoding_rs::UTF_8
    };
    if file_len < limits.mmap_threshold * 2 || !prefix_ok {
        // not big enough to be worth splitting, or not raw UTF-8: full path
        return open_file_with_limits(path, limits, progress);
    }
    drop(f);
    let file = open_for_mmap(path)?;
    let mmap = unsafe { memmap2::Mmap::map(&file) }.map_err(OpenError::mmap)?;
    let mut doc = TextDocument::with_original(
        Original::Mmap(std::sync::Arc::new(mmap)),
        Default::default(),
    );
    doc.meta.encoding = Some(encoding_rs::UTF_8);
    doc.meta.had_bom = false;
    doc.build_index_prefix(limits.index_prefix, &mut progress);
    doc.meta.path = Some(path.to_path_buf());
    doc.meta.display_name = file_name(path);
    doc.meta.detected_newline = doc.index.newline;
    doc.meta.file_type = guess_file_type(path, &doc);
    doc.meta.csv_mode = matches!(
        path.extension().and_then(|e| e.to_str()).map(|s| s.to_ascii_lowercase()),
        Some(ref e) if e == "csv" || e == "tsv"
    );
    Ok(Opened {
        doc,
        file_size: file_len,
        memory_mapped: true,
        index_complete: false,
        utf8_lossy: false,
    })
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string_lossy().into_owned())
}

fn guess_file_type(path: &Path, doc: &TextDocument) -> String {
    if doc.meta.csv_mode {
        // TSV shares the same table features as CSV, but the type name is
        // shown honestly
        let is_tsv = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("tsv"))
            .unwrap_or(false);
        return if is_tsv { "TSV" } else { "CSV" }.to_string();
    }
    // The single source of truth for extension → type is the highlight
    // language table: the type name shown in the status bar = the name of the
    // highlight language the file actually uses. No/unknown extension gets
    // `LangSpec::plain()`'s "Plain Text".
    crate::highlight::lang::for_path(Some(path))
        .name
        .to_string()
}

// ---- saving ---------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
pub struct SaveOptions {
    pub encoding: &'static encoding_rs::Encoding,
    pub bom: bool,
    pub newline: Newline,
}

#[derive(Debug)]
pub struct SaveReport {
    pub bytes_written: u64,
    /// true when some characters could not be represented in the encoding
    pub lossy: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum SaveError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    /// Human-readable error message (rendered at process-level language; the
    /// engine layer has no EditorState).
    #[error("{0}")]
    EncodeUnsupported(String),
}

impl TextDocument {
    pub fn save_options(&self) -> SaveOptions {
        let enc = self.meta.encoding.unwrap_or(encoding_rs::UTF_8);
        SaveOptions {
            encoding: enc,
            bom: self.meta.had_bom,
            newline: self.newline(),
        }
    }
}

/// Save the document to `path` (write temp + rename). Returns a report.
pub fn save_file(
    doc: &TextDocument,
    path: &Path,
    opts: &SaveOptions,
) -> Result<SaveReport, SaveError> {
    if opts.encoding.name().starts_with("UTF-32") {
        return Err(SaveError::EncodeUnsupported(
            i18n::current()
                .tr(i18n::Fmt::StatusEncodeUnsupported {
                    enc: opts.encoding.name().to_string(),
                })
                .into_owned(),
        ));
    }
    let tmp = temp_path(path);
    // The rename makes the *temp* file the document, so it is created carrying
    // the permissions of the file it replaces and gets the exact bits back
    // before the swap — otherwise the first save handed the file the process
    // default mode (POSIX) or the directory's inherited ACEs (Windows), and a
    // 0600 notes file came back world-readable. See
    // `platform::create_replacement_file` for why this is two calls.
    //
    // Written as one fallible sequence with a single cleanup point: a failed
    // save must not leave `.foo.md.infeditor-1234-5678.tmp` in the user's
    // directory, and no failure after this point is safe to ignore.
    let result = (|| -> Result<SaveReport, SaveError> {
        platform::create_replacement_file(path, &tmp)?;
        let report = write_out(doc, &tmp, opts)?;
        platform::finalize_replacement_file(path, &tmp)?;
        // rename can still fail (target in use / cross-drive): same cleanup.
        std::fs::rename(&tmp, path)?;
        Ok(report)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

fn temp_path(path: &Path) -> PathBuf {
    use std::time::{SystemTime, UNIX_EPOCH};
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let name = path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    // pid + ms timestamp: two instances (or a macro batch) saving the same
    // path concurrently must never write each other's temp file
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    dir.join(format!(
        ".{}.infeditor-{}-{}.tmp",
        name,
        std::process::id(),
        ms
    ))
}

fn write_out(doc: &TextDocument, tmp: &Path, opts: &SaveOptions) -> Result<SaveReport, SaveError> {
    let mut out = std::io::BufWriter::new(std::fs::File::create(tmp)?);
    out.write_all(encoding::bom_for(opts.encoding, opts.bom))?;
    let needs_newline_conv = opts.newline != doc.index.newline;
    let utf8 = opts.encoding == encoding_rs::UTF_8;

    if utf8 && !needs_newline_conv {
        // fast path: stream raw bytes
        for (_abs, bytes) in doc.table.iter_from(0) {
            out.write_all(bytes)?;
        }
        let n = doc.table.len();
        // fsync before the caller renames: flush() only reaches the OS page
        // cache, and a power loss after the rename could otherwise leave an
        // empty / truncated file behind the new directory entry
        out.into_inner().map_err(|e| e.into_error())?.sync_all()?;
        return Ok(SaveReport {
            bytes_written: n,
            lossy: false,
        });
    }

    // line-by-line path (encoding conversion and/or newline conversion)
    let mut lossy = false;
    // With non-UTF-8 encodings the line terminator must also go through the
    // encoder: in UTF-16 `\n` must be `0A 00`, not the bare byte `0A` — bare
    // bytes shift the rest of the file into mojibake (a byte-stream shift for
    // UTF-16, literal mis-writing for single/double-byte encodings like GBK).
    let term_encoded: Vec<u8>;
    let term: &[u8] = if utf8 {
        opts.newline.terminator().as_bytes()
    } else {
        let (bytes, err) = encoding::encode(opts.newline.terminator(), opts.encoding);
        lossy |= err;
        term_encoded = bytes;
        &term_encoded
    };
    let mut written = 0u64;
    let mut walker = crate::buffer::line_index::LineWalker::new_at(&doc.table, 0, 0);
    while let Some(span) = walker.next_span(&doc.table) {
        let content = doc.table.read(span.start..span.end);
        if utf8 {
            out.write_all(&content)?;
            written += content.len() as u64;
        } else {
            let s = String::from_utf8_lossy(&content);
            let (bytes, err) = encoding::encode(&s, opts.encoding);
            lossy |= err;
            out.write_all(&bytes)?;
            written += bytes.len() as u64;
        }
        if span.term != 0 {
            out.write_all(term)?;
            written += term.len() as u64;
        }
    }
    out.into_inner().map_err(|e| e.into_error())?.sync_all()?;
    Ok(SaveReport {
        bytes_written: written,
        lossy,
    })
}

/// Create a new empty file at `path`. Fails with `AlreadyExists` when the
/// path is taken — `File::create` would silently truncate an existing file.
pub fn create_file(path: &Path) -> std::io::Result<()> {
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buffer::Newline;

    fn tmp_file(name: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("infeditor-test-{}-{}", std::process::id(), name));
        p
    }

    /// Save back over the mmap document's original path: write-temp +
    /// rename(REPLACE_EXISTING) needs DELETE access to the target file, so the
    /// mmap handle's share mode must include FILE_SHARE_DELETE. Regression:
    /// after changing to READ-only, this test failed with os error 5.
    #[test]
    #[cfg(windows)]
    fn save_back_over_mmap_original() {
        let path = tmp_file("mmap-saveback.txt");
        std::fs::write(&path, b"original-content").unwrap();
        let file = open_for_mmap(&path).unwrap();
        let mmap = unsafe { memmap2::Mmap::map(&file) }.unwrap();
        // Simulate save_file's write-temp + rename-over-original
        let tmp = tmp_file("mmap-saveback.txt.tmp");
        std::fs::write(&tmp, b"new-content").unwrap();
        std::fs::rename(&tmp, &path).expect("rename over mapped file must succeed");
        assert_eq!(std::fs::read(&path).unwrap(), b"new-content");
        // The old mapping lives on via the section reference; content unchanged
        // (piece table offsets stay valid)
        assert_eq!(&mmap[..], b"original-content");
        drop(mmap);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn open_save_roundtrip_crlf() {
        let path = tmp_file("crlf.txt");
        std::fs::write(&path, b"line1\r\nline2\r\nline3").unwrap();
        let opened = open_file(&path, |_, _| {}).unwrap();
        let d = &opened.doc;
        assert_eq!(d.line_count(), 3);
        assert_eq!(d.index.newline, Newline::CrLf);
        assert_eq!(d.line_text(1), "line2");
        // save elsewhere preserving encoding/newline
        let out = tmp_file("crlf-out.txt");
        let opts = d.save_options();
        save_file(d, &out, &opts).unwrap();
        let back = std::fs::read(&out).unwrap();
        assert_eq!(back, b"line1\r\nline2\r\nline3");
        let _ = std::fs::remove_file(&out);
        let _ = std::fs::remove_file(&path);
    }

    /// An atomic save replaces the file *object*, so its permissions have to
    /// travel with it: `File::create` hands the temp file the process default
    /// (0644 under the usual umask) and the rename makes that file the
    /// document. A private 0600 file coming back as 0644 is a real disclosure;
    /// an executable script losing +x breaks it for its user.
    #[test]
    #[cfg(unix)]
    fn save_preserves_the_file_mode() {
        use std::os::unix::fs::PermissionsExt;
        for mode in [0o600u32, 0o640, 0o700] {
            let path = tmp_file(&format!("mode-{mode:o}.sh"));
            std::fs::write(&path, b"echo old\n").unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();

            let opened = open_file(&path, |_, _| {}).unwrap();
            let opts = opened.doc.save_options();
            save_file(&opened.doc, &path, &opts).unwrap();

            let got = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(got, mode, "saving rewrote the file's permissions");
            assert_eq!(std::fs::read(&path).unwrap(), b"echo old\n");
            let _ = std::fs::remove_file(&path);
        }
    }

    /// A brand-new document must not leave a permission placeholder behind:
    /// the save creates its temp file, and nothing else in the directory.
    #[test]
    fn a_new_document_save_leaves_only_the_document() {
        let path = tmp_file("brand-new-save.txt");
        let _ = std::fs::remove_file(&path);
        let mut doc = TextDocument::from("hello\n");
        let opts = doc.save_options();
        save_file(&doc, &path, &opts).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"hello\n");
        let dir = path.parent().unwrap();
        let stem = path.file_name().unwrap().to_string_lossy().into_owned();
        let debris: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with(&format!(".{stem}.infeditor-")))
            .collect();
        assert!(debris.is_empty(), "temp files left behind: {debris:?}");
        doc.meta.path = None;
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn open_gbk_convert_and_save_utf8() {
        let path = tmp_file("gbk.txt");
        let (bytes, _) = encoding::encode("你好\n世界\n", encoding_rs::GBK);
        std::fs::write(&path, &bytes).unwrap();
        let mut opened = open_file(&path, |_, _| {}).unwrap();
        assert_eq!(opened.doc.read_all(), "你好\n世界\n");
        assert_eq!(opened.doc.index.newline, Newline::Lf);
        // save as GBK again
        let opts = SaveOptions {
            encoding: encoding_rs::GBK,
            bom: false,
            newline: Newline::Lf,
        };
        let out = tmp_file("gbk-out.txt");
        save_file(&opened.doc, &out, &opts).unwrap();
        let back = std::fs::read(&out).unwrap();
        assert_eq!(back, bytes);
        // save as UTF-8 with CRLF
        let opts2 = SaveOptions {
            encoding: encoding_rs::UTF_8,
            bom: false,
            newline: Newline::CrLf,
        };
        save_file(&opened.doc, &out, &opts2).unwrap();
        let back2 = std::fs::read(&out).unwrap();
        assert_eq!(back2, "你好\r\n世界\r\n".as_bytes());
        opened.doc.meta.path = None;
        let _ = std::fs::remove_file(&out);
        let _ = std::fs::remove_file(&path);
    }

    /// Detection only samples the first 64 KB, so a GBK file whose head is
    /// plain ASCII looked like a valid UTF-8 prefix; the full-file validation
    /// then failed and the "lossy UTF-8" fallback replaced every CJK character
    /// with U+FFFD — saving wrote those replacement characters over the
    /// original bytes. Re-detecting on the whole buffer keeps the file
    /// losslessly decodable.
    ///
    /// Escapes instead of literals: the CJK ratchet counts non-ASCII in `src/`.
    #[test]
    fn gbk_after_a_long_ascii_prefix_stays_lossless() {
        let path = tmp_file("gbk-ascii-prefix.txt");
        let cjk_text = "\u{4f60}\u{597d}\u{ff0c}\u{4e16}\u{754c}\n";
        let mut bytes = Vec::new();
        // > 64 KB of pure ASCII so the probe window only ever sees UTF-8
        for _ in 0..4000 {
            bytes.extend_from_slice(b"-- a plain ascii comment line\n");
        }
        let (cjk, _) = encoding::encode(cjk_text, encoding_rs::GBK);
        bytes.extend_from_slice(&cjk);
        std::fs::write(&path, &bytes).unwrap();

        let opened = open_file(&path, |_, _| {}).unwrap();
        assert!(
            !opened.utf8_lossy,
            "the file is valid in its detected legacy encoding, not broken UTF-8"
        );
        assert!(
            opened.doc.read_all().ends_with(cjk_text),
            "the CJK tail must survive the open"
        );
        // Saving with the detected options reproduces the original bytes
        let out = tmp_file("gbk-ascii-prefix-out.txt");
        let opts = opened.doc.save_options();
        save_file(&opened.doc, &out, &opts).unwrap();
        assert_eq!(std::fs::read(&out).unwrap(), bytes);
        let _ = std::fs::remove_file(&out);
        let _ = std::fs::remove_file(&path);
    }

    /// The read path renders a localized message for the status bar, but the OS
    /// error must stay reachable: `source()` is what logging and `kind()`
    /// matching need, and the UI keeps this error whole until draw time.
    #[test]
    fn a_read_failure_keeps_its_io_source() {
        use std::error::Error as _;
        let err = OpenError::mmap(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "denied",
        ));
        assert!(err.to_string().contains("denied"), "{err}");
        let src = err.source().expect("the OS error must stay in the chain");
        assert_eq!(
            src.downcast_ref::<std::io::Error>()
                .map(std::io::Error::kind),
            Some(std::io::ErrorKind::PermissionDenied)
        );
    }

    #[test]
    fn open_utf8_bom_and_edit() {
        let path = tmp_file("bom.txt");
        let mut v = vec![0xEF, 0xBB, 0xBF];
        v.extend_from_slice("abc\ndef\n".as_bytes());
        std::fs::write(&path, &v).unwrap();
        let opened = open_file(&path, |_, _| {}).unwrap();
        assert!(opened.doc.meta.had_bom);
        assert_eq!(opened.doc.read_all(), "abc\ndef\n");
        assert_eq!(opened.doc.line_count(), 2);
        let _ = std::fs::remove_file(&path);
    }

    /// Below the threshold the bytes are read into a heap buffer: the piece
    /// table must own them, since the file on disk is about to be replaced by
    /// every save.
    #[test]
    fn a_file_under_the_threshold_is_not_memory_mapped() {
        let path = tmp_file("small.txt");
        std::fs::write(&path, "x\n".as_bytes()).unwrap();
        let opened = open_file(&path, |_, _| {}).unwrap();
        assert!(!opened.memory_mapped);
        let _ = std::fs::remove_file(&path);
    }

    /// The memory-mapped branch, actually taken. The threshold is a parameter
    /// (`HUGE_THRESHOLD` in production), so a 200 KB file plus a 1 KB
    /// threshold exercises exactly the code a 100 MB file would — the map, the
    /// absent full-file validation, and the piece table built over
    /// `Original::Mmap` — without writing hundreds of megabytes to disk.
    ///
    /// Saving is part of the assertion: the mmap is read-only and the save is
    /// temp+rename, so a mapped document must round-trip its content and
    /// survive the rename that unlinks the mapped inode (POSIX) / the
    /// share-mode dance (Windows, see `save_back_over_mmap_original`).
    #[test]
    fn a_file_over_the_threshold_is_memory_mapped_and_still_saves() {
        let path = tmp_file("mapped.txt");
        let line = "0123456789abcdef\n";
        let body: String = std::iter::repeat_n(line, 10_000).collect();
        std::fs::write(&path, body.as_bytes()).unwrap();

        let mut progress_calls = 0u32;
        let limits = OpenLimits {
            mmap_threshold: 1024,
            ..OpenLimits::default()
        };
        let opened = open_file_with_limits(&path, limits, |_, _| progress_calls += 1).unwrap();
        assert!(opened.memory_mapped, "a 170 KB file with a 1 KB threshold");
        assert_eq!(opened.file_size, body.len() as u64);
        assert!(opened.index_complete);
        assert_eq!(opened.doc.line_count(), 10_000);
        assert_eq!(opened.doc.read_all(), body);
        assert_eq!(
            opened.doc.meta.encoding.map(|e| e.name()),
            Some("UTF-8"),
            "the mmap branch is UTF-8 only"
        );
        assert!(
            progress_calls > 0,
            "index building must report progress on the mapped path too"
        );

        // Edit + save through the mapped original.
        let mut doc = opened.doc;
        doc.insert_text(0, "header\n");
        let opts = doc.save_options();
        save_file(&doc, &path, &opts).unwrap();
        let back = std::fs::read_to_string(&path).unwrap();
        assert!(back.starts_with("header\n"));
        assert_eq!(back.len(), body.len() + "header\n".len());
        assert_eq!(doc.read_all(), back);
        doc.meta.path = None;
        let _ = std::fs::remove_file(&path);
    }

    /// The progressive-open branch: index a prefix, return immediately with
    /// `index_complete == false`, and let the caller finish the scan. Also
    /// never taken by a test before, because it only starts at twice the huge
    /// threshold.
    #[test]
    fn progressive_open_indexes_only_a_prefix() {
        let path = tmp_file("progressive.txt");
        let line = "0123456789abcdef\n";
        let lines = 20_000u64;
        let body: String = std::iter::repeat_n(line, lines as usize).collect();
        std::fs::write(&path, body.as_bytes()).unwrap();

        let limits = OpenLimits {
            mmap_threshold: 1024,
            // The prefix pass must stop well before the end of the file, so the
            // "partial index" state is observable on a 340 KB fixture.
            index_prefix: 4096,
        };
        let opened = open_file_progressive_with_limits(&path, limits, |_, _| {}).unwrap();
        assert!(opened.memory_mapped);
        assert!(
            !opened.index_complete,
            "a progressive open must hand back a partial index to finish in the background"
        );
        let scanned = opened.doc.index_scanned_to();
        assert!(
            scanned >= limits.index_prefix,
            "the first pass must cover the requested prefix, stopped at {scanned}"
        );
        assert!(
            scanned < body.len() as u64,
            "the prefix scan stopped early, but the index covers all {} bytes",
            body.len()
        );
        // What the first paint gets to work with: an estimate from the scanned
        // sample. Uniform lines must land within a few percent, otherwise the
        // scrollbar jumps once the background scan lands.
        let est = opened.doc.line_count();
        let error = (est as i64 - lines as i64).abs();
        assert!(
            error <= (lines as i64) / 20,
            "line-count estimate {est} is too far from {lines}"
        );
        // The bytes are all there even though the index is not: reads must not
        // depend on how far the scan got.
        assert_eq!(opened.doc.read_all().len(), body.len());
        let _ = std::fs::remove_file(&path);
    }

    /// The tail of a huge file can also be invalid UTF-8 (invisible to the
    /// 64 KB probe window). Such files get **no full-file validation**:
    /// opening keeps mmap's immediacy, bad bytes display as U+FFFD on the
    /// read path, but saving must pass the original bytes straight through —
    /// the editor must not quietly replace bad bytes.
    ///
    /// Opt-in (writes a temp file > [`HUGE_THRESHOLD`]), same switch as
    /// [`huge_file_bench`]:
    ///
    /// ```text
    /// cargo test --release -- --ignored --nocapture huge_invalid_utf8
    /// ```
    #[test]
    #[ignore = "writes a multi-hundred-MB temp file"]
    fn huge_invalid_utf8_tail_survives_a_save() {
        use std::io::Write as _;

        let path = tmp_file("huge-invalid.txt");
        let tail = [0xFFu8, 0xFE, 0xFD];
        {
            let mut out =
                std::io::BufWriter::with_capacity(1 << 20, std::fs::File::create(&path).unwrap());
            // All valid UTF-8 (the probe window passes); only the last three
            // bytes are not
            let row = "0123456789abcdefghijklmnopqrstuvwxyz 一行中文\n".repeat(1024);
            let mut written = 0u64;
            while written < HUGE_THRESHOLD + (1 << 20) {
                out.write_all(row.as_bytes()).unwrap();
                written += row.len() as u64;
            }
            out.write_all(&tail).unwrap();
            out.flush().unwrap();
        }
        let original = std::fs::read(&path).unwrap();

        let mut opened = open_file(&path, |_, _| {}).unwrap();
        assert!(opened.memory_mapped, "huge files should use mmap");
        assert!(
            !opened.doc.meta.lossy_utf8,
            "content untouched, should not be flagged lenient-decode"
        );
        assert!(
            opened.doc.read_all().ends_with('\u{FFFD}'),
            "read path shows invalid bytes as replacement chars"
        );

        // Save back to the original path: content unedited, bytes must be
        // equal one by one (bad bytes preserved as-is)
        let out = tmp_file("huge-invalid-out.txt");
        let opts = opened.doc.save_options();
        save_file(&opened.doc, &out, &opts).unwrap();
        assert_eq!(
            std::fs::read(&out).unwrap(),
            original,
            "saving must not rewrite bad bytes"
        );
        opened.doc.meta.path = None;
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&out);
    }

    /// End-to-end check of the huge path: generate a file well past
    /// [`HUGE_THRESHOLD`], open it through the real `open_file`, and assert the
    /// document is (a) memory-mapped, (b) indexed to the exact line count, and
    /// (c) random-access correct.
    ///
    /// Opt-in because writing hundreds of megabytes is not something a normal
    /// `cargo test` should do:
    ///
    /// ```text
    /// cargo test --release -- --ignored --nocapture huge_file_bench
    /// ```
    ///
    /// `INFEDITOR_BENCH_MB` overrides the size (default 256).
    #[test]
    #[ignore = "writes a multi-hundred-MB temp file"]
    fn huge_file_bench() {
        use std::io::Write as _;

        let mb: usize = std::env::var("INFEDITOR_BENCH_MB")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(256);
        let target = mb * 1024 * 1024;

        let path = tmp_file("huge.txt");
        let t_write = std::time::Instant::now();
        {
            // One realistic fixed-width row per line, so the file also looks
            // like something a log/session dump would produce.
            let mut row = String::with_capacity(80);
            let mut out =
                std::io::BufWriter::with_capacity(1 << 20, std::fs::File::create(&path).unwrap());
            let mut line = 0u64;
            let mut written = 0usize;
            while written < target {
                row.clear();
                use std::fmt::Write as _;
                let _ = writeln!(row, "{line:>9} | {:>60} | done", "payload");
                out.write_all(row.as_bytes()).unwrap();
                written += row.len();
                line += 1;
            }
            out.flush().unwrap();
            println!(
                "[bench] generated {mb} MB / {line} lines in {:?}",
                t_write.elapsed()
            );
        }
        let size = std::fs::metadata(&path).unwrap().len();
        assert!(size >= HUGE_THRESHOLD, "fixture too small: {size}");

        // Opening must report progress monotonically and reach 100%.
        let mut last = 0u64;
        let mut ticks = 0u32;
        let t_open = std::time::Instant::now();
        let opened = open_file(&path, |done, total| {
            assert!(done >= last, "progress went backwards: {last} -> {done}");
            assert!(done <= total);
            last = done;
            ticks += 1;
        })
        .unwrap();
        let elapsed = t_open.elapsed();

        println!(
            "[bench] open {} ({} bytes) in {:?}  =>  {:.1} MB/s, {} progress ticks, mmap={}",
            opened.doc.meta.display_name,
            size,
            elapsed,
            (size as f64 / 1024.0 / 1024.0) / elapsed.as_secs_f64(),
            ticks,
            opened.memory_mapped,
        );

        assert!(
            opened.memory_mapped,
            "a {size}-byte UTF-8 file should be mapped"
        );
        assert_eq!(opened.doc.total_bytes(), size);
        assert!(ticks > 0, "index build reported no progress");
        assert_eq!(last, size, "index build did not reach the end of the file");

        // The line count must agree with an independent count.
        let expected_lines = {
            use std::io::Read as _;
            let mut f = std::fs::File::open(&path).unwrap();
            let mut buf = vec![0u8; 1 << 20];
            let mut n = 0u64;
            loop {
                match f.read(&mut buf).unwrap() {
                    0 => break,
                    k => n += buf[..k].iter().filter(|&&b| b == b'\n').count() as u64,
                }
            }
            n
        };
        assert_eq!(opened.doc.line_count(), expected_lines);

        // Random-access reads at scale: first, last and a middle line must come
        // back intact and cheaply.
        let t_read = std::time::Instant::now();
        let first = opened.doc.line_text(0);
        let mid = opened.doc.line_text(expected_lines / 2);
        let last_line = opened.doc.line_text(expected_lines - 1);
        println!("[bench] 3 random line reads in {:?}", t_read.elapsed());
        assert!(
            first.starts_with("        0 | "),
            "unexpected first line: {first:?}"
        );
        assert!(
            last_line.contains(" | done"),
            "unexpected last line: {last_line:?}"
        );
        assert_eq!(
            mid.len(),
            first.len(),
            "fixed-width rows should be equal length"
        );

        // Jumping to an arbitrary offset and back must be O(log n).
        let p = opened.doc.pos_of_line(expected_lines / 3);
        assert!(p > 0 && p < size);
        assert_eq!(opened.doc.line_of_pos(p), expected_lines / 3);

        let _ = std::fs::remove_file(&path);
    }
}
