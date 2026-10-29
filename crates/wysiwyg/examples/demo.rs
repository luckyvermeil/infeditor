//! Demo: left edit pane / right live-source split screen + tooltips.
//!
//! Verification goals (design doc §5):
//! 1. rendering + click-to-place cursor + live source;
//! 2. typing/deleting/Enter/autocomplete/undo-redo;
//! 3. block semantics (list continuation, un-indent, Tab indent, heading/
//!    quote/code-fence toggling);
//! 4. inline-format shortcuts (Ctrl+B/I/K/D/E), drag-select / double-click
//!    word select, clipboard;
//! 5. table / task list / formula / image / link rendering and interaction
//!    (checkbox click, Ctrl+click to open a link).
//!
//! Run: `cargo run -p wysiwyg --example demo`

use dioxus_native::prelude::*;

use wysiwyg::{md, Editor, Wysiwyg};

const SAMPLE: &str = r#"# WYSIWYG Editor

This paragraph mixes **bold**, *italic*, ~~strikethrough~~ and `inline code`,
with a [link](https://github.com) (Ctrl+click to open), an image ![logo](https://www.rust-lang.org/logos/rust-logo-64x64.png) and a formula $e^{i\pi}+1=0$.

## Try editing

1. Just type — `# `, `- `, `> `, `- `, `--- ` auto-complete at the block level;
2. Double-click a word, drag to select then Ctrl+B to bold, Ctrl+C/X/V for the clipboard;
3. Tab/Shift+Tab indent in lists, Enter on an empty item exits;
4. Click the checkbox below to toggle it;
5. Ctrl+Z / Ctrl+Y to undo/redo.

- [ ] A todo item (click the box to toggle)
- [x] A finished item

| Feature | Status |
|:--------|-------:|
| Tables  | ready  |
| Formulas| ready  |

> Block quote: Backspace on a blank line exits the quote.

```rust
fn main() {
    println!("code blocks are editable; Backspace at the start unwraps");
}
```

---"#;

const DEMO_CSS: &str = "
html, body { margin: 0; padding: 0; width: 100%; height: 100%; }
/* Layout note (a pitfall found in real-machine testing): under blitz/taffy a
   flex item's height:100% chained with flex-basis:auto height constraints is
   unreliable (wys-root once failed to establish a scroll container this way,
   wheel overflow leaked to the body and body scrolling did not render).
   Absolute positioning with explicit top/bottom stretching is the
   proven-reliable way. */
.app { display: flex; width: 100%; height: 100%; font-family: sans-serif; }
.pane { flex: 1; height: 100%; overflow: hidden; }
.pane.edit { border-right: 1px solid rgba(128,128,128,.35); }
.pane.src { background: rgba(128,128,128,.08); }
.src pre { margin: 0; padding: 14px 16px; font-family: monospace; font-size: 13px; line-height: 1.5; white-space: pre-wrap; color: rgba(128,128,128,.9); }
.src h3 { margin: 0; padding: 10px 16px 0; font-size: 12px; font-weight: 500; color: rgba(128,128,128,.6); }
";

fn main() {
    dioxus_native::launch(app);
}

/// Path where the test image lands on disk (returns an absolute path usable as
/// a Markdown destination, forward slashes). `<img>` goes through the
/// `safe_image_url` allowlist (`data:` is rejected and falls back to the alt
/// placeholder); to verify the `<img>` rendering chain the demo needs a real
/// file path; the embedded logo.png lands in the project tmp directory
/// (AGENTS.md: temp files go in tmp).
fn logo_file_path() -> String {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("manifest dir two levels up = project root");
    let dir = root.join("tmp");
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("wys_demo_logo.png");
    let _ = std::fs::write(&path, include_bytes!("logo.png"));
    // Markdown destinations use forward slashes: backslashes would have to pass
    // through CommonMark's escaping surface, which is not intuitive.
    path.to_string_lossy().replace('\\', "/")
}

fn app() -> Element {
    // IME: always on at window level after window creation (the winit fork
    // keeps IME off when creating a window; blitz's only enable point serves
    // TextInput only, so a non-<input> editing surface must enable it manually —
    // same as infeditor shell's enable_ime). Committed text reaches the
    // component's onkeydown via vendor patch seven (Ime::Commit → synthetic
    // keydown). The candidate window anchor is simplified to (0,0); once
    // integrated into a host, the host refreshes it with the cursor.
    // INFEDITOR_IME=0 skips it (escape hatch for GUI live testing: synthetic
    // keys get composed by the real IME and Enter gets eaten — same convention
    // as infeditor).
    if std::env::var_os("INFEDITOR_IME").is_some_and(|v| v == "0") {
        // Skip enabling
    } else {
        use dioxus_native::winit::dpi::{LogicalPosition, LogicalSize};
        use dioxus_native::winit::window::{
            ImeCapabilities, ImeEnableRequest, ImeRequest, ImeRequestData,
        };
        let win = dioxus_native::use_window();
        let _ = win.request_ime_update(ImeRequest::Enable(
            ImeEnableRequest::new(
                ImeCapabilities::new().with_cursor_area(),
                ImeRequestData::default().with_cursor_area(
                    LogicalPosition::new(0.0, 0.0).into(),
                    LogicalSize::new(0.0, 0.0).into(),
                ),
            )
            .unwrap(),
        ));
    }

    // Network images are unreachable in this test environment (rust-lang.org),
    // so we swap in an absolute path written to the project tmp to verify the
    // <img> rendering chain (data: URIs are rejected by the image allowlist and
    // fall back to the alt placeholder); the network loading capability itself
    // comes from the net feature + blitz-net.
    let sample = SAMPLE.replace(
        "https://www.rust-lang.org/logos/rust-logo-64x64.png",
        &logo_file_path(),
    );
    let editor = use_signal(move || Editor::new(md::parse(&sample)));

    // Temporary probe: when the editor changes, dump the emitted source to a
    // file (INF_WYS_DUMP=path) for lightweight read-back in live tests (a full
    // UIA tree walk is too slow).
    use_effect(move || {
        let doc = editor.read().doc.clone();
        if let Ok(path) = std::env::var("INF_WYS_DUMP") {
            let md = wysiwyg::md::emit(&doc);
            let _ = std::fs::write(&path, md);
        }
    });

    let open_link = move |url: String| {
        // Demo environment: rundll32's FileProtocolHandler opens the system
        // default browser.
        // **Not through cmd**: `cmd /C start "" <url>` re-parses & | " etc. in
        // the URL as shell syntax, so a legitimate Markdown link could inject
        // commands; CreateProcess passes arguments straight through with no
        // second parsing. The URL has already passed ui.rs's safe_link_url
        // allowlist.
        let _ = std::process::Command::new("rundll32")
            .args(["url.dll,FileProtocolHandler", &url])
            .spawn();
    };

    rsx! {
        style { {DEMO_CSS} }
        div { class: "app",
            div { class: "pane edit",
                Wysiwyg { editor, on_open_link: open_link }
            }
            div { class: "pane src",
                h3 { "Markdown (the controlled Editor's emit output)" }
                pre { "{md::emit(&editor.read().doc)}" }
            }
        }
    }
}
