//! Normalize a Markdown file into the WYSIWYG kernel's emit form (a
//! diagnostics / sample-maintenance tool).
//!
//! ```text
//! cargo run --example md_normalize -- samples/wys-all-features.md [--write]
//! cargo run --example md_normalize -- samples/wys-all-features.md --blocks
//! cargo run --example md_normalize -- --math '\frac{a}{b}' [--inline]
//! ```
//!
//! Without `--write` it only prints the differing lines; with `--write` it
//! writes the normalized result straight back to the file.
//! `--blocks` prints the parsed block tree, `--math` prints the formula SVG
//! (for calling the renderer / checking sizes).
//! Usage: after adding or modifying a sample, run it once to guarantee
//! "open → save" is byte-for-byte identical (the test
//! `md::tests::all_features_sample_is_canonical` locks this down).

use std::path::PathBuf;
use wysiwyg::md;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().is_some_and(|a| a == "--math") {
        let src = args.get(1).cloned().unwrap_or_else(|| "x^2".to_string());
        let display = !args.iter().any(|a| a == "--inline");
        let spec = if display {
            wysiwyg::math::MathSpec::block(&src, "#111111", 16.0)
        } else {
            wysiwyg::math::MathSpec::inline(&src, "#111111", 16.0)
        };
        match spec.svg() {
            Some(svg) => println!("{svg}"),
            None => eprintln!("formula rendering failed (text fallback): {src:?}"),
        }
        return;
    }
    let Some(path) = args.first().map(PathBuf::from) else {
        eprintln!("usage: md_normalize <file.md> [--write|--blocks] | --math <tex> [--inline]");
        std::process::exit(2);
    };
    let rest = &args[1..];
    let write = rest.iter().any(|a| a == "--write");
    let dump = rest.iter().any(|a| a == "--blocks");
    let src = std::fs::read_to_string(&path).expect("failed to read file");
    let doc = md::parse(&src);
    if dump {
        for (i, b) in doc.blocks.iter().enumerate() {
            println!("{i}: {b:?}");
        }
        return;
    }
    let out = md::emit(&doc);
    if out == src {
        println!("already in minimal form: {}", path.display());
        return;
    }
    if write {
        std::fs::write(&path, &out).expect("failed to write back");
        println!("normalized into emit form: {}", path.display());
        return;
    }
    let a: Vec<&str> = src.lines().collect();
    let b: Vec<&str> = out.lines().collect();
    for i in 0..a.len().max(b.len()) {
        let x = a.get(i).copied().unwrap_or("");
        let y = b.get(i).copied().unwrap_or("");
        if x != y {
            println!("{:>4} - {x:?}", i + 1);
            println!("     + {y:?}");
        }
    }
    eprintln!(
        "differences remain ({} lines). Add --write to write back.",
        b.len()
    );
}
