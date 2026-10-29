//! LaTeX math → self-contained SVG (the proven pattern from infeditor's
//! former `format/math.rs`; this module is shared by the preview and the edit
//! view).
//!
//! [`latex_rust`] parses LaTeX into an AST, lays it out with the TeX box
//! model, and outputs a **self-contained SVG** (glyphs are `<path>`s of the
//! STIX Two Math outlines, rules are `<rect>`s; no embedded fonts, no `<text>`
//! nodes) — Blitz renders an inline `<svg>` through usvg
//! (`blitz-dom`'s `layout/construct.rs`; `dioxus-native` includes the `svg`
//! feature by default), so a formula can be inserted into the preview HTML as
//!-is.
//!
//! **Module boundary: produce only an SVG string, no HTML wrapping.** The
//! inline/block-level wrapping differs per view (the edit view wants a
//! `data-vlen` carrier, the preview wants a `<div>`/phrasing `<span>` — see
//! `render::push_math` and `preview::wrap_math`); putting it here would only
//! breed a family of twin functions differing in the outer tag.
//!
//! Three key constraints:
//! 1. **The glyph fill is baked into the SVG**, so the theme foreground must be
//!    passed in, otherwise a black formula is invisible on a dark theme. Only
//!    `#rrggbb` is accepted (this project's `Theme::menu_fg` only has two
//!    values).
//! 2. latex-rust **supports only math mode**; text mode / TikZ returns Err —
//!    the caller must fall back to escaped-text rendering and must never
//!    silently swallow content.
//! 3. Output length cap [`MAX_MATH_LEN`]: overly long input (abnormal input /
//!    a pathological document) is abandoned outright, to keep the typesetting
//!    engine from being run like an FFT.

use latex_rust::{latex_to_svg, Color, Dim, MathFont, SvgOptions};
use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

/// Per-formula cache: key = [`MathSpec::key`], value = the bare SVG.
///
/// Both the preview and the edit view go through the process-global cache
/// ([`MathSpec::cached`]); this type + [`MathSpec::svg_cached`] gather the
/// "lookup → typeset on miss → backfill/clear" read/write semantics in one
/// place, so an independent cache scope (tests / future per-document
/// partitioning) can reuse it directly.
pub type MathCache = HashMap<String, String>;

/// LaTeX-source byte cap for a single formula. Over the cap falls back as a
/// parse failure.
///
/// 512 (the old value) was measured against short display formulas; CJK
/// flowcharts in `\boxed{ \begin{gathered} … }` legitimately run longer
/// (CJK is 3 bytes/char — the seven-step sample document hits ~500 already),
/// and the cost of a miss is the whole formula degrading to plain text.
/// 1024 keeps the "pathological input" guard while leaving real documents
/// headroom.
pub const MAX_MATH_LEN: usize = 1024;
/// Formula SVG cache capacity cap: past it the whole table is cleared (the
/// number of formulas is far below the number of document edits, so clearing
/// only costs one re-render pass).
pub const CACHE_CAP: usize = 1000;
/// Default foreground when there is no theme context (a light theme's
/// menu_fg).
pub const DEFAULT_FG: &str = "#333333";
/// Block-level formula scale relative to the body font size (MathJax's
/// display look).
const DISPLAY_SCALE: f32 = 1.2;

/// Typesetting parameters for one formula: the **only** typesetting-entry
/// credential.
///
/// The font-size interpretation lives only here: `px` is always the **body
/// font size**, and display multiplies by `DISPLAY_SCALE` on top. Callers must
/// not pre-multiply (in the old implementation `render.rs` and the preview
/// each multiplied by 1.2, then this multiplied again, so a block-level
/// formula ended up scaled to 1.44×).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MathSpec<'a> {
    pub src: &'a str,
    /// Theme foreground (`#rrggbb`). The formula fill is baked into the SVG, so
    /// it must be part of the cache key.
    pub fg: &'a str,
    /// Body font size (CSS px).
    pub px: f32,
    /// Block-level (`$$…$$`) formula: TeX display style.
    pub display: bool,
}

impl<'a> MathSpec<'a> {
    /// An inline formula.
    pub fn inline(src: &'a str, fg: &'a str, px: f32) -> Self {
        Self {
            src,
            fg,
            px,
            display: false,
        }
    }

    /// A block-level (display) formula: occupies its own line, with larger
    /// fractions and limits placed above/below.
    pub fn block(src: &'a str, fg: &'a str, px: f32) -> Self {
        Self {
            src,
            fg,
            px,
            display: true,
        }
    }

    /// Final typesetting font size (px).
    fn em_px(&self) -> f32 {
        if self.display {
            self.px * DISPLAY_SCALE
        } else {
            self.px
        }
    }

    /// Cache key: display + foreground + body font size + source. The same
    /// formula is two different SVGs under light/dark themes (different fill),
    /// so the fill must be part of the key. `\u{1}` is the separator — the
    /// source can never contain it.
    pub fn key(&self) -> String {
        format!(
            "{}\u{1}{}\u{1}{:.2}\u{1}{}",
            u8::from(self.display),
            self.fg,
            self.px,
            self.src
        )
    }

    /// LaTeX → SVG. Returns `None` on failure (the caller falls back to text;
    /// content is never swallowed).
    pub fn svg(&self) -> Option<String> {
        svg_body(self)
    }

    /// Same as [`svg`](Self::svg), but uses a **caller-held cache**. An
    /// unchanged formula hits the cache and skips re-typesetting
    /// (`latex_to_svg` is a pure function).
    ///
    /// Only successful results are cached: a failure is usually over-long/bad
    /// input (intercepted up front, cheap to detect), so caching those costs
    /// more than it saves. Past [`CACHE_CAP`] the whole table is cleared (the
    /// number of formulas is far below the keystroke count, so clearing only
    /// costs one re-render pass).
    pub fn svg_cached(&self, cache: &mut MathCache) -> Option<String> {
        let key = self.key();
        if let Some(svg) = cache.get(&key) {
            return Some(svg.clone());
        }
        let svg = self.svg()?;
        if cache.len() >= CACHE_CAP {
            cache.clear();
        }
        cache.insert(key, svg.clone());
        Some(svg)
    }

    /// Same as [`svg_cached`](Self::svg_cached), but uses the **process-global
    /// cache**: the edit view re-splices fragments on every keystroke and the
    /// preview re-runs on every gen, so an unchanged formula hits the cache and
    /// skips re-typesetting. The global cache lets both views share one
    /// typesetting result (`latex_to_svg` is a pure function).
    pub fn cached(&self) -> Option<String> {
        static CACHE: OnceLock<Mutex<MathCache>> = OnceLock::new();
        let cache = CACHE.get_or_init(|| Mutex::new(MathCache::new()));
        // A poisoned lock (the holding thread panicked) is treated as a miss, so
        // rendering does not crash along with it.
        let mut map = cache.lock().unwrap_or_else(|e| e.into_inner());
        self.svg_cached(&mut map)
    }
}

/// Embedded STIX Two Math font: parsed once, shared within the process.
/// `OnceLock<Option<_>>` — even a parse failure is only attempted once, and
/// after that everything uniformly falls back to text.
///
/// **Known limitation** (the local vendor patch that supplied a CJK fallback
/// face was removed together with the vendor directory): the embedded STIX
/// Two Math has no CJK outlines, so a CJK character inside `\text{…}` /
/// `\mathrm{…}` dies with `missing glyph` and the whole formula takes the
/// literal-text fallback. Pure-ASCII formulas are unaffected.
static FONT: OnceLock<Option<MathFont>> = OnceLock::new();

fn font() -> Option<&'static MathFont> {
    FONT.get_or_init(|| MathFont::stix_two_math().ok()).as_ref()
}

/// Shared render body: strip the XML declaration + shape validation.
///
/// **Size convention**: blitz parses the SVG root's `width`/`height`
/// attributes as f32 (with a unit it fails → the intrinsic size is 0), so
/// below the `pt` suffix is stripped and the value is used directly as px.
/// Therefore the em font size must be given in **pixels**: inline = the body
/// font size, block × [`DISPLAY_SCALE`]. It used to be emitted as 12 (the pt
/// number); after stripping the unit the formula was only 12px — visibly a
/// size smaller than the body text.
fn svg_body(spec: &MathSpec<'_>) -> Option<String> {
    let src = spec.src.trim();
    if src.is_empty() || src.len() > MAX_MATH_LEN {
        return None;
    }
    let font = font()?;
    let mut opts = SvgOptions::new();
    // blitz strips the unit and parses as px, so this value is the final CSS
    // pixel font size.
    opts.font_size_pt = Dim::ratio(spec.em_px().round() as i64, 1);
    opts.display = spec.display;
    opts.color = hex_color(spec.fg).unwrap_or(Color::rgb(0, 0, 0));
    // Two repair passes, in this order, both before typesetting (they take
    // effect for the preview and the edit view from the same source; the cache
    // key is still the original src, so two sources each occupying their own
    // cache entry is harmless):
    // 1. undo a Markdown escaper that ran over the LaTeX (`x\_n`, `\\geq`,
    //    `\left{`) — see [`repair_markdown_escapes`];
    // 2. non-standard macro aliases (`\lt` → `<`, `\pmod`, `gathered`) — see
    //    [`normalize_tex`]. It must run *second*: a repaired `\lt` is an alias
    //    input like any other.
    let escaped = repair_markdown_escapes(src);
    let src = normalize_tex(&escaped);
    let svg = latex_to_svg(&src, font, &opts).ok()?;
    // The output starts with an `<?xml …?>` declaration — strip it, so the
    // declaration does not leak into the HTML body and the shape validation
    // below lands on the `<svg` root element.
    let svg = &svg[svg.find("<svg ")?..];
    // Defensive shape validation: the output must be an SVG root element.
    // latex-rust is a trusted dependency; this only catches "an unexpected
    // output shape".
    if !svg.starts_with("<svg xmlns=\"http://www.w3.org/2000/svg\"") {
        return None;
    }
    // When blitz parses the SVG's intrinsic size it parses the width/height
    // attributes as f32; latex-rust's `width="12.3pt"` carries a unit, so the
    // parse fails → the intrinsic size is lost → the inline box has zero
    // width/height → the caret is misaligned before/after the formula. Strip
    // the `pt` suffix so it parses as px; therefore `font_size_pt`'s value is
    // the final px font size (see [`svg_body`]'s size convention).
    //
    // The **trailing newline must go**: latex-rust ends its output with
    // `</svg>\n`, and the carriers wrap the SVG as `<span …>{svg}</span>`. That
    // `\n` becomes a whitespace-only text node between `</svg>` and `</span>`,
    // and the shell is `white-space:pre-wrap` — so every formula drew one extra
    // blank line under itself. Trimming is presentation-only: the model's
    // formula source (what `data-vlen` counts and what the caret edits) is
    // untouched.
    Some(svg.trim_end().replace("pt\"", "\""))
}

/// `#rrggbb` → [`Color`]. This project's theme colors are all this shape.
fn hex_color(s: &str) -> Option<Color> {
    let hex = s.strip_prefix('#')?;
    if hex.len() != 6 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
    let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
    let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
    Some(Color::rgb(r, g, b))
}

/// Non-standard macro / alias table: commands that **the probe measured as
/// unrecognized by latex-rust** but that are common in MathJax / academic
/// documents → an already-supported equivalent. Standard TeX macros
/// (`\land`/`\lor`/\ne`/`\to`/`\dfrac`/`\lvert`…) are already recognized by
/// latex-rust (of the 130-command survey only the 4+5 below fail) and are not
/// in the table — one less rewriting layer is one less class of surprise.
///
/// A replacement value can be a bare symbol (`<`) or another supported
/// command (`\circ`). The rewriting only happens before typesetting
/// ([`svg_body`]); the formula source in the model is untouched, and the cache
/// key need not change.
fn tex_alias(name: &[u8]) -> Option<&'static str> {
    Some(match name {
        // MathJax non-standard macros: less-than/greater-than (guarded against
        // hitting `\ltimes` by whole-command-boundary scanning)
        b"lt" => "<",
        b"gt" => ">",
        // `\degree` → `\circ` (`90^\degree` → `90^\circ`)
        b"degree" => "\\circ",
        // amsmath variant: \bm → \boldsymbol
        b"bm" => "\\boldsymbol",
        // `\bmod` (binary mod): the spacing macro `\;` was verified by the probe
        b"bmod" => "\\;\\mathrm{mod}\\;",
        // mathtools definition symbol: x := y
        b"coloneqq" => ":=",
        b"eqqcolon" => "=:",
        // \argmax/\argmin (non-standard macros customary in auctex/optimization
        // literature)
        b"argmax" => "\\operatorname{argmax}",
        b"argmin" => "\\operatorname{argmin}",
        _ => return None,
    })
}

/// Undo a **Markdown escaper that ran over a LaTeX formula**.
///
/// Some converters (LaTeX/OCR → Markdown → HTML → Markdown pipelines) write the
/// formula source as if it were ordinary prose, so the saved file carries
/// `x\_n` (subscript), `\\geq` / `\\frac` (an escaped command backslash) and
/// `\left{` / `\right}` (the escape pair `\{` lost its backslash while the
/// command's own backslash survived). 25 of the 41 formulas in the 2019 SUFE
/// analysis exam sample fail to typeset for exactly this reason; repairing the
/// source at display time turns 25 failures into 0 without touching the document
/// text. (The one formula whose surviving braces keep pulldown from recognizing
/// it as a formula at all is handled earlier — see `md::shield_math_braces`.)
///
/// The pass is **all-or-nothing per formula**: it only runs when the source
/// shows at least one escape artifact (`has_markdown_escapes`), and then
/// rewrites every artifact in it. A formula with no artifact is returned
/// borrowed and byte-identical, so perfectly normal documents (`\begin{array}
/// {l} a \\ b \end{array}`) are untouched.
///
/// Two decisions worth knowing:
/// - `\_` is read as a subscript, not as TeX's literal underscore. Inside a
///   formula that is already showing escape artifacts, the subscript reading is
///   the only one that makes the document work; a genuine `\_` (literal
///   underscore in math) is vanishingly rare, and would have to appear alone
///   (with no other artifact) to be misread.
/// - A doubled backslash is collapsed **only when a command name follows**
///   (`\\geq` → `\geq`) or when it guards a brace (`\\{` → `\{`). `\\` + space /
///   `&` / end of input is the legitimate array row separator and stays. The
///   known false positive is a hand-written `a\\frac` (a row break immediately
///   followed by a command, which TeX itself reads as an undefined `\frac`
///   anyway).
///
/// The pass is deliberately conservative: a *uniformly* doubled formula (row
/// separators also written as `\\\\`) is not un-doubled — TeX reads that as two
/// row breaks, and folding it would equally fold the legitimate row break of a
/// partially escaped formula. Only the artifact classes above count as evidence.
fn repair_markdown_escapes(src: &str) -> Cow<'_, str> {
    let b = src.as_bytes();
    if !has_markdown_escapes(b) {
        return Cow::Borrowed(src);
    }
    let mut out = String::with_capacity(src.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] != b'\\' {
            let n = utf8_len(b[i]);
            out.push_str(&src[i..i + n]);
            i += n;
            continue;
        }
        // `\\` + command name (`\\geq`) = one escaped backslash. `\\` followed
        // by anything else stays: that is the array row separator.
        if i + 1 < b.len() && b[i + 1] == b'\\' {
            let letters = i + 2;
            if command_len(b, letters) >= 2 {
                out.push('\\');
                i = emit_command(&mut out, src, b, letters);
                continue;
            }
            // `\\{` / `\\}`: the delimiter pair of an escaped formula (`\{` came
            // out one level deeper). A row separator is never written straight
            // against a brace, so this cannot eat a line break.
            if letters < b.len() && (b[letters] == b'{' || b[letters] == b'}') {
                out.push('\\');
                i = letters;
                continue;
            }
            out.push_str("\\\\");
            i += 2;
            continue;
        }
        // Markdown's escaped punctuation, restricted to the set that has no
        // legitimate math meaning: `\_` `\[` `\]` `\<` `\>`. Spacing commands
        // (`\,` `\;` `\!`) and the escape pairs (`\{` `\}` `\|` `\(` `\)`) are
        // real TeX and are kept as-is.
        if i + 1 < b.len() && matches!(b[i + 1], b'_' | b'[' | b']' | b'<' | b'>') {
            out.push(b[i + 1] as char);
            i += 2;
            continue;
        }
        // A command: `\name`, plus the `\left{` / `\right}` repair (see
        // [`emit_command`]).
        if i + 1 < b.len() && b[i + 1].is_ascii_alphabetic() {
            out.push('\\');
            i = emit_command(&mut out, src, b, i + 1);
            continue;
        }
        let n = if i + 1 < b.len() && b[i + 1].is_ascii() {
            2
        } else {
            1
        };
        out.push_str(&src[i..i + n]);
        i += n;
    }
    Cow::Owned(out)
}

/// Copy the command name starting at `at` (its first letter) and — for a
/// delimiter-sizing command written straight against a brace (`\left{` /
/// `\right}`) — restore the delimiter's backslash. The leading backslash has
/// already been written by the caller. Returns the index past what it consumed.
///
/// Only sizing commands are repaired: `{` after any other command is an
/// ordinary argument group.
fn emit_command(out: &mut String, src: &str, b: &[u8], at: usize) -> usize {
    let n = command_len(b, at);
    let end = at + n;
    if end < b.len() && is_sizing_cmd(&b[at..end]) && (b[end] == b'{' || b[end] == b'}') {
        out.push_str(&src[at..end]);
        out.push('\\');
        out.push(b[end] as char);
        return end + 1;
    }
    out.push_str(&src[at..end]);
    end
}

/// Length of the ASCII letter run at `at` (a TeX command name's body).
fn command_len(b: &[u8], at: usize) -> usize {
    let mut k = at;
    while k < b.len() && b[k].is_ascii_alphabetic() {
        k += 1;
    }
    k - at
}

/// Does this formula carry at least one Markdown-escape artifact? The gate that
/// keeps [`repair_markdown_escapes`] away from ordinary documents: `\_`, an
/// escaped `[`/`]`/`<`/`>`, `\\` + a command name, or a `\left{`-shaped sizing
/// command whose brace lost its backslash.
fn has_markdown_escapes(b: &[u8]) -> bool {
    let mut i = 0;
    while i + 1 < b.len() {
        if b[i] != b'\\' {
            i += 1;
            continue;
        }
        match b[i + 1] {
            b'_' | b'[' | b']' | b'<' | b'>' => return true,
            b'\\' => {
                if command_len(b, i + 2) >= 2 {
                    return true;
                }
                i += 2;
                continue;
            }
            c if c.is_ascii_alphabetic() => {
                let end = i + 1 + command_len(b, i + 1);
                // `\left{` — a sizing command directly against a brace
                if end < b.len()
                    && is_sizing_cmd(&b[i + 1..end])
                    && (b[end] == b'{' || b[end] == b'}')
                {
                    return true;
                }
                i = end;
                continue;
            }
            _ => {}
        }
        i += 1;
    }
    false
}

/// TeX's delimiter-sizing commands — the ones that take a delimiter and would
/// legitimately be followed by `\{`/`\}`.
fn is_sizing_cmd(name: &[u8]) -> bool {
    matches!(
        name,
        b"left"
            | b"right"
            | b"middle"
            | b"big"
            | b"Big"
            | b"bigg"
            | b"Bigg"
            | b"bigl"
            | b"bigr"
            | b"Bigl"
            | b"Bigr"
            | b"biggl"
            | b"biggr"
            | b"Biggl"
            | b"Biggr"
    )
}

/// Non-standard macro normalization: `\`+letters matches the alias table
/// **whole-word** before replacing (a plain `replace` would chew `\ltimes`
/// into `<imes`). `\pmod` with an argument (`\pmod{n}` / `\pmod n`)
/// is specially handled into `(\mathrm{mod}\ …)`. Everything else is kept
/// verbatim; a command with no alias passes through unchanged.
///
/// Probe survey (130+12 items, 2026-10-21): of the latex-rust failures only
/// the ten `lt/gt/degree/bm/bmod/pmod/argmax/argmin/coloneqq/eqqcolon` can be
/// rescued by an alias; the other failing shapes (with no known equivalent
/// replacement) are handed to the existing `<code>` text fallback, and content
/// is never swallowed.
pub fn normalize_tex(src: &str) -> Cow<'_, str> {
    let b = src.as_bytes();
    if !b.contains(&b'\\') {
        return Cow::Borrowed(src);
    }
    let mut out = String::with_capacity(src.len() + 16);
    let mut i = 0;
    while i < b.len() {
        if b[i] != b'\\' {
            let ch_len = utf8_len(b[i]);
            out.push_str(&src[i..i + ch_len]);
            i += ch_len;
            continue;
        }
        // `\\` (the newline command) or `\`+ASCII non-letter (`\;` `\,`
        // `\{`…): pass the two bytes through as-is. **If what follows is not
        // ASCII, consume only the backslash itself** — for input where a
        // backslash is immediately followed by a multibyte character, a 2-byte
        // slice would cut into the middle of a UTF-8 sequence and panic.
        if i + 1 >= b.len() || !b[i + 1].is_ascii_alphabetic() {
            let take = if i + 1 < b.len() && b[i + 1].is_ascii() {
                2
            } else {
                1
            };
            out.push_str(&src[i..i + take]);
            i += take;
            continue;
        }
        // The command name = the maximal letter run after `\` (the command
        // boundary)
        let name_start = i + 1;
        let mut name_end = name_start;
        while name_end < b.len() && b[name_end].is_ascii_alphabetic() {
            name_end += 1;
        }
        let name = &b[name_start..name_end];
        if name == b"pmod" {
            if let Some((repl, next)) = rewrite_pmod(src, b, name_end) {
                out.push_str(&repl);
                i = next;
                continue;
            }
        }
        if name == b"begin" || name == b"end" {
            if let Some((repl, next)) = rewrite_env(src, b, name, name_end) {
                out.push_str(&repl);
                i = next;
                continue;
            }
        }
        if let Some(repl) = tex_alias(name) {
            out.push_str(repl);
            i = name_end;
            continue;
        }
        // A command with no alias: keep `\`+name as-is
        out.push_str(&src[i..name_end]);
        i = name_end;
    }
    Cow::Owned(out)
}

/// `\pmod` → `(\mathrm{mod}\ <arg>)`. The argument is a brace group (the
/// balance scan skips `\{` escapes) or a single token (one command / one
/// character). When the argument cannot be recognized it degrades to
/// `\mathrm{mod}` (content is not lost, only the parentheses). Returns
/// `(replacement text, consumed end)`.
fn rewrite_pmod(src: &str, b: &[u8], name_end: usize) -> Option<(String, usize)> {
    let mut j = name_end;
    while j < b.len() && (b[j] == b' ' || b[j] == b'\t') {
        j += 1;
    }
    let arg = if j < b.len() && b[j] == b'{' {
        let end = scan_brace_group(b, j)?;
        let inner = &src[j + 1..end - 1];
        j = end;
        inner.to_string()
    } else if j < b.len() && b[j] == b'\\' {
        // A single-command argument: `\`+letter run, or `\`+one character
        let k = if j + 1 < b.len() && b[j + 1].is_ascii_alphabetic() {
            let mut k = j + 1;
            while k < b.len() && b[k].is_ascii_alphabetic() {
                k += 1;
            }
            k
        } else {
            (j + 2).min(b.len())
        };
        let tok = src[j..k].to_string();
        j = k;
        tok
    } else if j < b.len() && b[j] != b'{' && !b[j].is_ascii_whitespace() {
        let ch_len = utf8_len(b[j]);
        let tok = src[j..j + ch_len].to_string();
        j += ch_len;
        tok
    } else {
        // No argument (`\pmod` at the end): degrades to a bare mod
        return Some(("\\mathrm{mod}".to_string(), name_end));
    };
    Some((format!("(\\mathrm{{mod}}\\ {arg})"), j))
}

/// From `at` (which must point at `{`) scan the balanced brace group, with
/// `\` escape pairs skipped whole. Returns the index just past the closing
/// brace, or `None` when unbalanced (caller passes the text through verbatim).
fn scan_brace_group(b: &[u8], at: usize) -> Option<usize> {
    let mut depth = 0usize;
    let mut k = at;
    while k < b.len() {
        if b[k] == b'\\' {
            k += 2; // skip escape pairs like `\{`/`\}`
            continue;
        }
        match b[k] {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(k + 1);
                }
            }
            _ => {}
        }
        k += 1;
    }
    None
}

/// Environment-name rewrite for shapes latex-rust cannot parse (probe
/// 2026-10-23: `unsupported: environment gathered`). Only the *name* is
/// rewritten; the body is untouched, so any parse failure inside still lands
/// in the text fallback with content intact.
///
/// `gathered` (amsmath "centered multi-line") is semantically a single
/// centered column — exactly `\begin{array}{c}` — and the array shape is
/// probe-verified to typeset (`\boxed` around it works too). `\\` line breaks
/// carry over unchanged. `cases` could get the same treatment
/// (`\left\{\begin{array}{ll}`), but its `&` column count varies in the wild;
/// left alone until there is a real document that needs it.
///
/// Anything else (or an unreadable `{name}` group) returns `None` and the
/// `\begin`/`\end` plus its argument pass through verbatim.
fn rewrite_env(src: &str, b: &[u8], name: &[u8], name_end: usize) -> Option<(String, usize)> {
    let mut j = name_end;
    while j < b.len() && (b[j] == b' ' || b[j] == b'\t') {
        j += 1;
    }
    if j >= b.len() || b[j] != b'{' {
        return None;
    }
    let end = scan_brace_group(b, j)?;
    let env = &src[j + 1..end - 1];
    match (name, env) {
        (b"begin", "gathered") => Some(("\\begin{array}{c}".to_string(), end)),
        (b"end", "gathered") => Some(("\\end{array}".to_string(), end)),
        _ => None,
    }
}

/// UTF-8 first byte → sequence length (the first byte is never a
/// continuation byte; out-of-range values step by 1 to avoid an infinite
/// loop).
fn utf8_len(first: u8) -> usize {
    match first {
        0x00..=0x7F => 1,
        0xC0..=0xDF => 2,
        0xE0..=0xEF => 3,
        0xF0..=0xF7 => 4,
        _ => 1,
    }
}

/// The user document that drove `gathered` support: a boxed gathered CJK
/// flowchart (`\boxed{ \begin{gathered} \text{…} \\ \downarrow \\ … }`).
/// Test-only shared fixture: the WYSIWYG and preview layers assert on the
/// *full* seven-row version, not a truncation.
#[cfg(test)]
pub(crate) const BOXED_GATHERED_FLOWCHART: &str =
    "\\boxed{ \\begin{gathered} \\text{测量碱性岩的 F、Cl、C、S}\\\\ \
    \\downarrow\\\\ \\text{评估蚀变、脱气及岩浆分异的影响}\\\\ \\downarrow\\\\ \
    \\text{利用 F/Cl、F/Nd、Cl/Nb 追踪卤素行为}\\\\ \\downarrow\\\\ \
    \\text{结合 Th/Yb、Ba/La 判断交代来源}\\\\ \\downarrow\\\\ \
    \\text{用非模态部分熔融模型反推地幔源区}\\\\ \\downarrow\\\\ \
    \\text{区分沉积物熔融与板片脱水的作用}\\\\ \\downarrow\\\\ \
    \\text{讨论卤素向深部地幔的再循环} \\end{gathered} }";

#[cfg(test)]
mod tests {
    use super::*;

    fn svg_height(svg: &str) -> f32 {
        let i = svg.find("height=\"").expect("height attr") + 8;
        let j = svg[i..].find('"').expect("closing quote") + i;
        svg[i..j].parse().expect("numeric height")
    }

    #[test]
    fn fraction_renders_svg() {
        let svg = MathSpec::inline(r"\frac{1}{2}", "#333333", 16.0)
            .svg()
            .expect("frac");
        assert!(svg.starts_with("<svg xmlns=\"http://www.w3.org/2000/svg\""));
        assert!(svg.contains("<path"), "glyphs should be paths: {svg}");
    }

    /// The SVG must not end with whitespace: both carriers splice it as
    /// `<span …>{svg}</span>` into a `white-space:pre-wrap` shell, where a
    /// trailing `\n` becomes a whitespace-only text node and renders as a real
    /// line break — one blank line under every formula.
    #[test]
    fn math_svg_has_no_trailing_whitespace() {
        for spec in [
            MathSpec::inline(r"\frac{1}{2}", DEFAULT_FG, 16.0),
            MathSpec::block(r"\int_0^1 x^2\,dx", DEFAULT_FG, 16.0),
        ] {
            let svg = spec.svg().unwrap();
            assert!(svg.ends_with("</svg>"), "…{:?}", &svg[svg.len() - 24..]);
            assert_eq!(svg.trim_end(), svg, "trailing whitespace in the SVG");
        }
    }

    /// The formula SVG's size must follow the body font size (the caller
    /// passes px), with block-level at 1.2×: the old implementation was fixed
    /// at 12 (the pt number treated as px) → in 14px body text the formula was
    /// only 12px; later three places each multiplied by 1.2, scaling it to
    /// 1.44×.
    #[test]
    fn math_svg_scales_with_font_px_and_display_once() {
        let h14 = svg_height(&MathSpec::inline("x^2", "#333333", 14.0).svg().unwrap());
        let h20 = svg_height(&MathSpec::inline("x^2", "#333333", 20.0).svg().unwrap());
        assert!(
            h20 > h14 * 1.3,
            "a 20px formula should be clearly taller than 14px: {h14} vs {h20}"
        );
        let hd = svg_height(&MathSpec::block("x^2", "#333333", 14.0).svg().unwrap());
        let ratio = hd / h14;
        assert!(
            (1.15..1.25).contains(&ratio),
            "a block-level formula should scale only 1.2× (not 1.44×): {h14} vs {hd}"
        );
    }

    #[test]
    fn math_color_follows_theme() {
        let dark = MathSpec::inline("x", "#c9d1d9", 16.0).svg().expect("dark");
        let light = MathSpec::inline("x", "#333333", 16.0).svg().expect("light");
        assert!(dark.contains("c9d1d9"), "{dark}");
        assert!(light.contains("333333"), "{light}");
        assert_ne!(dark, light, "the fill goes into the SVG with the theme");
    }

    #[test]
    fn invalid_latex_returns_none() {
        assert!(MathSpec::inline(r"\unknownmacro{", "#333333", 16.0)
            .svg()
            .is_none());
        assert!(MathSpec::inline(r"\notacommand", DEFAULT_FG, 16.0)
            .svg()
            .is_none());
        assert!(MathSpec::inline("", DEFAULT_FG, 16.0).svg().is_none());
        assert!(MathSpec::inline("   ", DEFAULT_FG, 16.0).svg().is_none());
    }

    // ---- Non-standard macro / alias normalization (normalize_tex) ----

    /// Sample A problem 7 (the probe's only failing item): rescued by the
    /// `\lt` alias.
    #[test]
    fn nonstandard_lt_alias_renders() {
        let src = r"0\lt f(b)-f(a)\leq \frac{1}{4}(b-a)";
        let svg = MathSpec::inline(src, DEFAULT_FG, 16.0)
            .svg()
            .expect("alias lt should rescue the formula");
        assert!(svg.starts_with("<svg xmlns=\"http://www.w3.org/2000/svg\""));
    }

    /// Every alias target renders to SVG.
    #[test]
    fn every_alias_target_renders() {
        for src in [
            r"a\gt b",
            r"90^\degree",
            r"\bm{\alpha}",
            r"x\coloneqq y",
            r"x\eqqcolon y",
            r"a\bmod n",
            r"a\equiv b\pmod{n}",
            r"a\equiv b \pmod n",
            r"\argmax_x f",
            r"\argmin_x f",
        ] {
            assert!(
                MathSpec::inline(src, DEFAULT_FG, 16.0).svg().is_some(),
                "should be renderable: {src}"
            );
        }
    }

    /// Command boundary: longer-prefix commands like
    /// `\ltimes`/`\lnot`/`\neq` are unaffected by the `\lt` alias.
    #[test]
    fn alias_respects_command_boundary() {
        assert_eq!(normalize_tex(r"a\ltimes b"), r"a\ltimes b");
        assert_eq!(normalize_tex(r"a\lnot b"), r"a\lnot b");
        assert_eq!(normalize_tex(r"a\neq b"), r"a\neq b");
        assert_eq!(normalize_tex(r"\gtrsim"), r"\gtrsim");
        // `\\lt` = the newline command + the letter l; must not be aliased
        assert_eq!(normalize_tex(r"a\\lt b"), r"a\\lt b");
    }

    /// Normalized-shape assertions: alias substitution, pmod's three argument
    /// shapes, everything else kept verbatim.
    #[test]
    fn normalize_tex_shapes() {
        assert_eq!(normalize_tex(r"0\lt 1"), "0< 1"); // the original space after the command name is kept; rendering-equivalent
        assert_eq!(normalize_tex(r"\bm{\alpha}"), r"\boldsymbol{\alpha}");
        assert_eq!(normalize_tex(r"90^\degree"), r"90^\circ");
        assert_eq!(normalize_tex(r"x\coloneqq y"), "x:= y");
        assert_eq!(
            normalize_tex(r"a\equiv b\pmod{n}"),
            r"a\equiv b(\mathrm{mod}\ n)"
        );
        // A space before the brace; the argument contains nested/escaped parens
        assert_eq!(normalize_tex(r"a \pmod {n}"), r"a (\mathrm{mod}\ n)");
        assert_eq!(normalize_tex(r"\pmod{f(n)}"), r"(\mathrm{mod}\ f(n))");
        // A single-command argument
        assert_eq!(normalize_tex(r"a\pmod\pi"), r"a(\mathrm{mod}\ \pi)");
        // No-argument degradation (content is not lost)
        assert_eq!(normalize_tex(r"a\pmod"), r"a\mathrm{mod}");
        // Unbalanced braces pass through whole
        assert_eq!(normalize_tex(r"a\pmod{n"), r"a\pmod{n");
        // The no-backslash fast path allocates nothing
        assert!(matches!(normalize_tex("plain 1<2"), Cow::Borrowed(_)));
    }

    /// A backslash immediately followed by a multibyte character must not be
    /// sliced as 2 bytes — it once panicked with "byte index is not a char
    /// boundary".
    #[test]
    fn normalize_tex_multibyte_after_backslash_no_panic() {
        assert_eq!(normalize_tex("\\中x"), "\\中x");
        assert_eq!(normalize_tex(r"\中文 \alpha"), r"\中文 \alpha");
        assert_eq!(normalize_tex("\\"), "\\");
    }

    /// Rescuing non-standard macros does not affect the failure fallback: a
    /// truly unknown command still returns None.
    #[test]
    fn alias_does_not_break_fallback() {
        assert!(MathSpec::inline(r"\notacommand x", DEFAULT_FG, 16.0)
            .svg()
            .is_none());
    }

    // ---- Markdown-escaped LaTeX repair (repair_markdown_escapes) ----

    /// The artifact shapes a Markdown escaper leaves in a formula, one test per
    /// class: escaped subscript, escaped command backslash, escaped bracket /
    /// angle bracket, and a sizing command whose delimiter lost its backslash.
    #[test]
    fn repair_markdown_escapes_shapes() {
        assert_eq!(repair_markdown_escapes(r"x\_n"), r"x_n");
        assert_eq!(repair_markdown_escapes(r"a \\geq b"), r"a \geq b");
        assert_eq!(repair_markdown_escapes(r"\\frac{1}{2}"), r"\frac{1}{2}");
        assert_eq!(
            repair_markdown_escapes(r"\left\[a\right\]"),
            r"\left[a\right]"
        );
        assert_eq!(repair_markdown_escapes(r"1\<a\<b"), r"1<a<b");
        assert_eq!(
            repair_markdown_escapes(r"\left{x\_1\right}"),
            r"\left\{x_1\right\}"
        );
        // The delimiter pair one level deeper (`\{` escaped again): only folded
        // *inside* an already-escaped formula, never on its own.
        assert_eq!(repair_markdown_escapes(r"\\{x\_1\\}"), r"\{x_1\}");
    }

    /// No artifact → borrowed and byte-identical. Ordinary formulas must never
    /// be rewritten: `\\` before a command is also the legitimate row break, and
    /// `\{`, `\,`, `\|`, `\%` are real TeX.
    #[test]
    fn clean_tex_is_not_repaired() {
        for src in [
            r"\begin{array}{l} a \\ b \end{array}",
            r"\left\{x\right\}",
            r"x_n + y^2",
            r"a\,b\;c\!d\quad e",
            r"\{a\}\ \|",
            r"\text{50\%}",
        ] {
            assert!(
                matches!(repair_markdown_escapes(src), Cow::Borrowed(_)),
                "should stay untouched: {src}"
            );
            assert_eq!(repair_markdown_escapes(src), src);
        }
    }

    /// Inside a repaired formula the array row separator survives: only a
    /// doubled backslash that introduces a command name is collapsed.
    #[test]
    fn repair_keeps_row_separators() {
        assert_eq!(
            repair_markdown_escapes(r"\begin{array}{l}x\_1\\ y\_2\end{array}"),
            r"\begin{array}{l}x_1\\ y_2\end{array}"
        );
    }

    /// Idempotent (the edit view re-splices on every keystroke) and safe next to
    /// multibyte text — a backslash directly before a CJK char must not be
    /// sliced in half.
    #[test]
    fn repair_is_idempotent_and_multibyte_safe() {
        let once = repair_markdown_escapes(r"\left{x\_1\\ y\right}").into_owned();
        assert_eq!(repair_markdown_escapes(&once), once);
        assert_eq!(repair_markdown_escapes("\\_中\\_文"), "_中_文");
        assert_eq!(repair_markdown_escapes("\\"), "\\");
    }

    /// The end-to-end assertion: the sample exam's longest formula, taken
    /// verbatim from the mangled file, must typeset **identically** to the
    /// hand-cleaned source kept in `math_probe.rs`. A one-byte drift in the
    /// repair changes the SVG and fails here.
    #[test]
    fn repaired_exam_formula_typesets_like_the_clean_source() {
        let mangled = r"\\left{\\begin{array}{l}x\_n^{\\frac{4 n+5}{n}}+3 x\_n+y\_n=1+\\frac{1}{n} \\ y\_n^{\\frac{n+2}{n}}+4 x\_n+3 y\_n=4-\\frac{1}{n}\\end{array}\\right.";
        let clean = r"\left\{\begin{array}{l}x_n^{\frac{4 n+5}{n}}+3 x_n+y_n=1+\frac{1}{n} \\ y_n^{\frac{n+2}{n}}+4 x_n+3 y_n=4-\frac{1}{n}\end{array}\right.";
        assert_eq!(repair_markdown_escapes(mangled), clean);
        let a = MathSpec::inline(mangled, DEFAULT_FG, 16.0)
            .svg()
            .expect("the mangled formula typesets");
        let b = MathSpec::inline(clean, DEFAULT_FG, 16.0).svg().unwrap();
        assert_eq!(a, b);
    }

    /// Every formula of the 2019 SUFE exam sample that used to fall back to
    /// literal text (17 of the document's 41). Without the repair pass 25 of the
    /// 41 fail; the point of the pass is that the whole document renders.
    #[test]
    fn escaped_exam_formulas_all_typeset() {
        let mangled = [
            r"x\_n \\geq 0, y\_n \\geq 0, n=1,2,3, \\ldots",
            r"\\left{x\_n\\right},\\left{y\_n\\right}",
            r"(-\\infty,+\\infty)",
            r"f^{\\prime}(x)",
            r"\\int\_a^b f^{\\prime}(x) d x=f(b)-f(a)",
            r"f(0)=1, f\\left(\\frac{1}{2}\\right)=\\frac{1}{e}",
            r"\\int\_L\\left\[f^{\\prime}(x)+6 f(x)\\right\] y d x+f^{\\prime}(x) d y",
            r"D: x^2+y^2 \\leq 1",
            r"z=\\left(x^2+y^2\\right) \\frac{\\partial z}{\\partial x}+2018\\left(\\frac{\\partial z}{\\partial y}\\right)^2",
            r"\\sum\_{n=0}^{\\infty} \\frac{x^{2 n+1}}{(2 n+1) !!}",
            r"\\lim \_{x \\rightarrow+\\infty} e^{-\\frac{x^2}{2}} S(x)=\\sqrt{\\frac{\\pi}{2}}",
            r"0\\lt f(b)-f(a)\\leq \\frac{1}{4}(b-a)",
            r"f(x)=|x|, x \\in\[-\\pi, \\pi\]",
            r"\\int\_0^1 \\frac{\\ln (1-x)}{x} d x",
            r"f(x)=\\int\_0^x\\left(t-t^2\\right) \\sin ^{2 n} t d t",
            r"\\iint\_{x^2+y^2 \\leq z^2}\\left(x^2+y^2\\right) f\\left(\\sqrt{x^2+y^2}\\right) d x d y",
            r"\[0,+\infty)",
        ];
        for src in mangled {
            assert!(
                MathSpec::inline(src, DEFAULT_FG, 16.0).svg().is_some(),
                "should be repaired into a typesettable formula: {src}"
            );
        }
    }

    // ---- Environment rewrite (rewrite_env) & CJK fallback ----

    /// `gathered` rewrites to the probe-verified equivalent shape; other
    /// environments pass through verbatim.
    #[test]
    fn normalize_tex_gathered_env() {
        assert_eq!(
            normalize_tex(r"\begin{gathered} a \\ b \end{gathered}"),
            r"\begin{array}{c} a \\ b \end{array}"
        );
        // Whitespace between \begin and the brace is tolerated
        assert_eq!(
            normalize_tex(r"\begin {gathered} x \end{gathered}"),
            r"\begin{array}{c} x \end{array}"
        );
        // Unknown environments untouched (the argument stays a plain group)
        assert_eq!(
            normalize_tex(r"\begin{cases} x \end{cases}"),
            r"\begin{cases} x \end{cases}"
        );
        // Unbalanced brace: pass through whole
        assert_eq!(normalize_tex(r"\begin{gathered"), r"\begin{gathered");
    }

    /// CJK inside `\text` cannot typeset (no CJK outlines in the embedded
    /// font, no fallback face since the vendor directory was dropped): the
    /// formula returns `None` and the callers show the LaTeX source as
    /// literal text — content is never swallowed.
    #[test]
    fn cjk_text_takes_the_text_fallback() {
        assert!(
            MathSpec::inline(r"\text{测量碱性岩的 F、Cl、C、S}", DEFAULT_FG, 16.0)
                .svg()
                .is_none(),
            "a CJK formula must fall back to text, not render boxes"
        );
    }

    /// The user document that started the gathered support: a boxed gathered
    /// flowchart. With the CJK fallback gone the CJK rows fall back to text —
    /// the pure-ASCII skeleton still typesets.
    #[test]
    fn boxed_gathered_ascii_skeleton_renders() {
        // An ASCII-only gathered formula typesets (the CJK rows would kill it
        // at the engine level — missing glyph — and fall back to text).
        let ascii = MathSpec::block(
            r"\boxed{ \begin{gathered} F, Cl\\ \downarrow\\ Hg \end{gathered} }",
            DEFAULT_FG,
            16.0,
        )
        .svg()
        .expect("an ASCII gathered formula typesets");
        assert!(ascii.starts_with("<svg xmlns=\"http://www.w3.org/2000/svg\""));
        assert!(ascii.matches("<path").count() > 5);
        // The CJK rows kill it at the engine level (missing glyph)
        let cjk = MathSpec::block(
            r"\boxed{ \begin{gathered} F、Cl\\ \downarrow\\ Hg }",
            DEFAULT_FG,
            16.0,
        )
        .svg();
        assert!(cjk.is_none(), "CJK inside the formula must fall back");
    }

    /// Plain math is unaffected by any of the above: no CJK, no fallback.
    #[test]
    fn plain_math_typesets() {
        let a = MathSpec::inline(r"\frac{1}{2}+\sum_{i=1}^n i", DEFAULT_FG, 16.0)
            .svg()
            .expect("plain math");
        assert!(a.contains("<path"));
    }

    #[test]
    fn too_long_returns_none() {
        let long = "x".repeat(MAX_MATH_LEN + 1);
        assert!(MathSpec::inline(&long, "#333333", 16.0).svg().is_none());
    }

    #[test]
    fn math_hex_color_rejects_garbage() {
        assert!(hex_color("#c9d1d9").is_some());
        assert!(hex_color("#333").is_none());
        assert!(hex_color("red").is_none());
        assert!(hex_color("#gggggg").is_none());
    }

    /// Deep nesting must not panic (1.0.x once stack-overflowed; fixed in 2.x;
    /// here a deep fraction is used to press on it)
    #[test]
    fn math_deep_nesting_no_panic() {
        let deep = format!("{}1{}", "\\frac{".repeat(40), "}".repeat(40));
        let _ = MathSpec::inline(&deep, DEFAULT_FG, 16.0).svg(); // Ok or Err is fine, it must not crash
    }

    #[test]
    fn math_cache_key_distinguishes_theme_display_and_size() {
        let base = MathSpec::inline("x", "#333333", 16.0);
        assert_ne!(base.key(), MathSpec::block("x", "#333333", 16.0).key());
        assert_ne!(base.key(), MathSpec::inline("x", "#c9d1d9", 16.0).key());
        assert_ne!(base.key(), MathSpec::inline("x", "#333333", 18.0).key());
        assert_eq!(base.key(), MathSpec::inline("x", "#333333", 16.0).key());
    }

    /// The caller cache and the global cache share one implementation: output
    /// is byte-identical, a hit does not re-typeset, and a failure does not
    /// enter the table.
    #[test]
    fn svg_cached_matches_uncached_and_caller_cache() {
        let spec = MathSpec::inline(r"\frac{1}{2}", "#333333", 16.0);
        let mut cache = MathCache::new();
        let a = spec.svg_cached(&mut cache).expect("frac");
        assert_eq!(cache.len(), 1);
        let b = spec.svg_cached(&mut cache).expect("frac hit");
        assert_eq!(a, b);
        assert_eq!(a, spec.svg().unwrap());
        assert_eq!(a, spec.cached().unwrap());
        // The theme goes into the key: a different SVG, the original entry is
        // not overwritten
        let dark = MathSpec::inline(r"\frac{1}{2}", "#dddddd", 16.0)
            .svg_cached(&mut cache)
            .unwrap();
        assert_ne!(a, dark);
        assert_eq!(cache.len(), 2);
        // A failure is not cached
        let before = cache.len();
        assert!(MathSpec::inline(r"\unknownmacro{", "#333333", 16.0)
            .svg_cached(&mut cache)
            .is_none());
        assert_eq!(cache.len(), before);
    }

    /// The cache path (hit/miss) output is byte-identical to direct typesetting;
    /// the theme goes into the key; a failure is not cached (returns None and
    /// does not enter the table).
    #[test]
    fn cached_matches_uncached() {
        let spec = MathSpec::inline(r"\frac{1}{2}", "#333333", 16.0);
        let a = spec.cached().expect("frac");
        let b = spec.cached().expect("frac cached");
        assert_eq!(a, b);
        assert_eq!(a, spec.svg().unwrap());
        assert_ne!(
            a,
            MathSpec::inline(r"\frac{1}{2}", "#dddddd", 16.0)
                .cached()
                .unwrap()
        );
        assert!(MathSpec::inline(r"\unknownmacro{", "#333333", 16.0)
            .cached()
            .is_none());
    }
}
