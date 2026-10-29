//! The NetProvider for MD previews: `<img>` resource fetching for blitz
//! documents.
//!
//! Upstream `blitz-net` depends on reqwest + tokio (needs a runtime context;
//! `tokio::spawn` panics on our event-loop thread), so we build our own
//! lightweight implementation:
//!
//! * `file://` — reads local files (relative-path images in an MD preview have
//!   already been resolved to absolute file:// URLs at render time, see
//!   `safe_image_url` in `wysiwyg::preview`);
//! * `http` / `https` — ureq (rustls) blocks and fetches on a std thread;
//! * any other scheme is rejected (`data:`/`javascript:` etc. do not render;
//!   the preview falls back to the alt text).
//!
//! Safety bounds: a global concurrency cap + a 20 MiB per-resource cap + a
//! 20s total timeout (both download and disk reads are bounded, so no thread/
//! handle can be held forever); ureq sends no Cookies and auto-follows ≤5
//! redirects. On failure it **returns empty bytes**: when blitz's
//! `ImageHandler` fails to decode it records the URL into the document's
//! `failed_images` (vendor patch), and the host re-renders that image as an
//! alt placeholder (same semantics as a browser's broken-image alt); without
//! the callback the image would stay forever in the not-yet-loaded state and
//! the alt would never appear.

use std::io::Read;
use std::sync::{Condvar, Mutex, MutexGuard};
use std::time::Duration;

use blitz_traits::net::{Bytes, NetHandler, NetProvider, Request, Url};

/// Size cap for a single resource (so a huge file / decompression bomb
/// cannot exhaust memory).
const MAX_BYTES: usize = 20 * 1024 * 1024;
/// Global concurrent-download cap (aligned with a browser's ~6 connections
/// per origin, so a long article with dozens of images fetching at once does
/// not saturate threads/connections).
const MAX_CONCURRENT: usize = 8;
/// Total timeout for one request (connect + transfer).
const TIMEOUT: Duration = Duration::from_secs(20);

pub struct InfNetProvider;

impl NetProvider for InfNetProvider {
    fn fetch(&self, _doc_id: usize, request: Request, handler: Box<dyn NetHandler>) {
        let url = request.url;
        std::thread::Builder::new()
            .name("md-image-fetch".into())
            .spawn(move || {
                // Image decode/network work runs on user content: a panic here must
                // fail this one image, not terminate the editor.
                crate::ui::guard::worker("ui::mdnet", move || {
                    let _permit = Permit::acquire();
                    match fetch_bytes(&url) {
                        Ok(bytes) => {
                            crate::ui::shell::debug_log(&format!(
                                "mdnet: fetched {url} ({} bytes)",
                                bytes.len()
                            ));
                            handler.bytes(url.to_string(), bytes);
                        }
                        Err(err) => {
                            crate::ui::shell::debug_log(&format!(
                                "mdnet: fetch failed {url}: {err}"
                            ));
                            // Empty bytes → blitz fails to decode → the document
                            // records failed_images → the host swaps that URL for
                            // an alt placeholder (see the module docs).
                            handler.bytes(url.to_string(), Bytes::from(Vec::new()));
                        }
                    }
                });
            })
            .ok(); // failing to create the thread equals a failed fetch; the
                   // image does not load
    }

    fn is_noop(&self) -> bool {
        false
    }
}

fn fetch_bytes(url: &Url) -> Result<Bytes, String> {
    match url.scheme() {
        "file" => {
            // UNC paths (file://server/share/…) are rejected: a network drive
            // can hang, `File::open`/`read_to_end` have no timeout, and one
            // hung path would hold a concurrency permit forever; after 8 hangs
            // all image loading stalls.
            if url.host_str().is_some() {
                return Err("file:// does not allow UNC network paths".to_string());
            }
            let path = url
                .to_file_path()
                .map_err(|_| "invalid file:// URL".to_string())?;
            let file =
                std::fs::File::open(&path).map_err(|e| format!("{}: {e}", path.display()))?;
            let len = file.metadata().map_err(|e| e.to_string())?.len();
            if len as usize > MAX_BYTES {
                return Err(format!("file exceeds the {MAX_BYTES}-byte cap"));
            }
            let mut buf = Vec::with_capacity(len.min(MAX_BYTES as u64) as usize);
            file.take(MAX_BYTES as u64)
                .read_to_end(&mut buf)
                .map_err(|e| e.to_string())?;
            Ok(Bytes::from(buf))
        }
        "http" | "https" => {
            let resp = agent()
                .get(url.as_str())
                .call()
                .map_err(|e| e.to_string())?;
            let declared = resp
                .header("Content-Length")
                .and_then(|v| v.parse::<usize>().ok());
            if declared.is_some_and(|len| len > MAX_BYTES) {
                return Err(format!("resource exceeds the {MAX_BYTES}-byte cap"));
            }
            // take(cap+1): read one extra byte to distinguish "exactly at the
            // cap" from "over the cap"
            let mut reader = resp.into_reader().take((MAX_BYTES + 1) as u64);
            let mut buf = Vec::new();
            reader.read_to_end(&mut buf).map_err(|e| e.to_string())?;
            if buf.len() > MAX_BYTES {
                return Err(format!("resource exceeds the {MAX_BYTES}-byte cap"));
            }
            Ok(Bytes::from(buf))
        }
        other => Err(format!("scheme '{other}' is not on the allowlist")),
    }
}

fn agent() -> &'static ureq::Agent {
    static AGENT: std::sync::OnceLock<ureq::Agent> = std::sync::OnceLock::new();
    AGENT.get_or_init(|| {
        ureq::AgentBuilder::new()
            .timeout(TIMEOUT)
            .redirects(5)
            .user_agent("InfEditor/0.1 (markdown-preview)")
            .build()
    })
}

/// A simple global concurrency gate: a Mutex counter + a Condvar; download
/// threads wait when they cannot get one.
struct Permit;

impl Permit {
    fn state() -> &'static (Mutex<usize>, Condvar) {
        static STATE: std::sync::OnceLock<(Mutex<usize>, Condvar)> = std::sync::OnceLock::new();
        STATE.get_or_init(|| (Mutex::new(0), Condvar::new()))
    }

    fn acquire() -> PermitGuard {
        let (lock, cv) = Self::state();
        let mut n = lock.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if *n < MAX_CONCURRENT {
                *n += 1;
                break;
            }
            n = cv.wait(n).unwrap_or_else(|e| e.into_inner());
        }
        PermitGuard { _guard: n }
    }
}

struct PermitGuard {
    /// Holds the MutexGuard until drop, keeping the counter's increments and
    /// decrements paired (-1 + notify on drop).
    _guard: MutexGuard<'static, usize>,
}

impl Drop for PermitGuard {
    fn drop(&mut self) {
        let (_, cv) = Permit::state();
        cv.notify_one();
    }
}
