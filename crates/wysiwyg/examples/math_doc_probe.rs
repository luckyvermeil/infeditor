//! Per-document math diagnostic (not part of the build):
//! `cargo run -p wysiwyg --example math_doc_probe -- <file.md>`
//!
//! Parses a Markdown file through the **real** pipeline (`md::parse`) and runs
//! every math node through `MathSpec::svg()` — the end-to-end path, including
//! the parse-time brace shield and `math::repair_markdown_escapes` — printing
//! OK/FAIL per formula plus a failure count. Use it when a user document renders
//! raw TeX: a FAIL line is a formula that fell back to literal text, and a
//! formula missing from the list never became a math node at all (compare the
//! number of `$` pairs in the source with the count printed at the end).
use std::io::Read;
use wysiwyg::math::MathSpec;
use wysiwyg::model::{Block, Inline};

fn walk(inlines: &[Inline], out: &mut Vec<(String, bool)>) {
    for i in inlines {
        match i {
            Inline::Math { src, display } => out.push((src.clone(), *display)),
            Inline::Strong(v) | Inline::Em(v) | Inline::Del(v) => walk(v, out),
            Inline::Link { inlines, .. } => walk(inlines, out),
            _ => {}
        }
    }
}

fn collect(b: &Block, out: &mut Vec<(String, bool)>) {
    match b {
        Block::Para(v) | Block::Quote(v) | Block::Li { inlines: v, .. } => walk(v, out),
        Block::Head { inlines, .. } | Block::Td { inlines, .. } => walk(inlines, out),
        Block::Quoted { inner, .. } => collect(inner, out),
        _ => {}
    }
}

fn main() {
    let path = std::env::args()
        .nth(1)
        .expect("usage: math_doc_probe <file.md>");
    let mut s = String::new();
    std::fs::File::open(&path)
        .unwrap()
        .read_to_string(&mut s)
        .unwrap();
    let doc = wysiwyg::md::parse(&s);
    let mut nodes = Vec::new();
    for b in &doc.blocks {
        collect(b, &mut nodes);
    }
    let mut fails = 0;
    for (src, display) in &nodes {
        let spec = if *display {
            MathSpec::block(src, "#333333", 16.0)
        } else {
            MathSpec::inline(src, "#333333", 16.0)
        };
        match spec.svg() {
            Some(svg) => println!("OK   [{:5}B] {}", svg.len(), short(src)),
            None => {
                fails += 1;
                println!("FAIL           {}", short(src));
            }
        }
    }
    println!("---- {fails} failures / {} formulas ----", nodes.len());
}

fn short(s: &str) -> String {
    let t: String = s.chars().take(80).collect();
    let t = t.replace('\\', "\\\\").replace('\n', "\\n");
    if s.chars().count() > 80 {
        format!("{t}…")
    } else {
        t
    }
}
