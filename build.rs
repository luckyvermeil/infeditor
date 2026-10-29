//! Build script: embeds the Windows application manifest and the application
//! icon into the executable.
//!
//! The manifest (`app.manifest`) declares the Common-Controls v6 dependency and
//! PerMonitorV2 DPI awareness — without the former, common dialogs such as
//! open/save-as fall back to legacy comctl32 rendering (no visual styles,
//! blurry on DPI screens); without the latter, the awareness level is only set
//! once winit initializes the event loop.
//!
//! Embedding is done directly via the MSVC linker's `/MANIFEST:EMBED` +
//! `/MANIFESTINPUT`; no rc.exe / windres needed (pure build.rs link args, zero
//! extra dependencies on the MSVC toolchain). Non-Windows or non-MSVC targets
//! always skip.
//!
//! The icon takes the same route: instead of compiling a `.rc` (which needs
//! `rc.exe` from the Windows SDK), this script writes the binary `.res` itself
//! and hands it to the linker as one more input file. Without it the exe has no
//! icon resource at all, so Explorer, the file-association default icon
//! (`"infeditor.exe",0`) and the taskbar all fall back to the generic
//! application icon — the `.ico` next to the exe only serves installer
//! shortcuts.
//!
//! The ICO reader and the `.res` writer live in `src/build_res.rs`, compiled
//! into **both** this script (via `#[path]`) and the library: a build script's
//! own `#[cfg(test)]` code is never run by `cargo test`, so keeping the format
//! code here would have left it untested.

use std::env;
use std::fs;
use std::path::PathBuf;

#[path = "src/build_res.rs"]
mod build_res;

use build_res::{group_icon, parse_ico, ResFile, RT_GROUP_ICON, RT_ICON};

fn main() {
    // `/MANIFEST:EMBED` + `.res` input files are link.exe (MSVC) features; a
    // windows-gnu build would fail on both. The doc comment above promises a
    // non-MSVC skip, so check the target *environment*, not just the OS.
    let target_env = env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    if env::var("CARGO_CFG_WINDOWS").is_err() || target_env != "msvc" {
        return;
    }
    // Only affects linking of bin targets; relink when the manifest changes.
    println!("cargo:rerun-if-changed=app.manifest");
    println!("cargo:rustc-link-arg-bins=/MANIFEST:EMBED");
    println!("cargo:rustc-link-arg-bins=/MANIFESTINPUT:app.manifest");
    // UAC is expressed by requestedExecutionLevel in the manifest; turn off the
    // linker's default UAC section to avoid conflicts when the two manifest
    // fragments are merged.
    println!("cargo:rustc-link-arg-bins=/MANIFESTUAC:NO");

    embed_icon();
}

/// Reads `packaging/windows/infeditor.ico` and links every frame plus the
/// group that binds them together (see [`build_res::group_icon`] for why both
/// resource types are needed).
fn embed_icon() {
    const ICO: &str = "packaging/windows/infeditor.ico";
    println!("cargo:rerun-if-changed={ICO}");
    let bytes = fs::read(ICO).unwrap_or_else(|e| panic!("{ICO}: {e}"));
    let images = parse_ico(&bytes);
    assert!(!images.is_empty(), "{ICO}: no images");
    let group = group_icon(&images);

    let mut res = ResFile::new();
    for (i, img) in images.iter().enumerate() {
        res.push(RT_ICON, (i + 1) as u16, &img.data);
    }
    res.push(RT_GROUP_ICON, 1, &group);

    let out = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR")).join("app-icon.res");
    fs::write(&out, res.bytes()).unwrap_or_else(|e| panic!("{}: {e}", out.display()));
    // A plain input file, not an option: link.exe converts .res files itself.
    println!("cargo:rustc-link-arg-bins={}", out.display());
}
