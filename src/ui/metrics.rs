//! Character-width metrics for the editor viewport.
//!
//! Blitz lays out and paints text itself, but the editor needs pixel math for
//! caret placement, click-to-column mapping and IME cursor areas. We measure
//! with the *same* font file the CSS `font-family` resolves to (via fontdue)
//! and treat missing glyphs by unicode-width class: wide (CJK) = 1em,
//! narrow = half the Latin advance. Tab stops follow the common editor rule:
//! advance to the next multiple of `tab_size * latin_advance` in pixels.

pub struct Metrics {
    /// The parsed font face. `Arc` because different sizes of the same family
    /// share one parse result — a size change (Ctrl+wheel / settings) only
    /// rebuilds the 128-entry ASCII width table, no longer re-reading and
    /// re-parsing the 450KB font from disk (see [`face_for_family`]).
    latin: Option<std::sync::Arc<fontdue::Font>>,
    /// font size in logical px
    pub em: f32,
    /// per-character advance for the monospace Latin font (px)
    pub adv: f32,
    /// line height (px), integer for crisp rows
    pub lh: f32,
    /// ASCII advance lookup table (filled once in `new`). Display widths of
    /// code/English almost entirely fall within ASCII, so `char_w` saves two
    /// fontdue lookups per call — `width_of` is the main caller, in the
    /// per-frame horizontal width sweep and wrap measurement.
    ascii: [f32; 128],
}

/// Advance per em when the font file cannot be read.
///
/// Consolas is 0.55em (Windows always has the file, so this is the
/// last-resort path there); the generic monospace face on the other platforms
/// (DejaVu Sans Mono, Liberation Mono, Noto Sans Mono, Menlo) is ~0.6em.
/// Using the Windows ratio everywhere under-measured every line by ~9% on
/// Linux/macOS, which drifted carets and selections to the right of the
/// glyphs.
#[cfg(windows)]
const FALLBACK_RATIO: f32 = 0.55;
#[cfg(not(windows))]
const FALLBACK_RATIO: f32 = 0.6;

// ---------------------------------------------------------------------------
// (family, size) → Metrics cache.
//
// `Metrics::new` reads the 453KB font file from disk and fully parses it with
// fontdue every time, while the (family, size) combinations the callers
// (startup, applying settings, Ctrl+wheel zoom, resetting zoom) repeatedly
// request are few. Here we cache `Arc<Metrics>` keyed by (lowercased family,
// size×100 rounded): on a hit there is zero IO and zero parsing; the `Arc`
// also lets the metrics held by `EditorState` share the same parse result
// with the cache, so cloning the state no longer deep-copies the font.
// ---------------------------------------------------------------------------

type MetricsCache = std::collections::HashMap<(String, i64), std::sync::Arc<Metrics>>;

fn metrics_cache() -> &'static std::sync::Mutex<MetricsCache> {
    static CACHE: std::sync::OnceLock<std::sync::Mutex<MetricsCache>> = std::sync::OnceLock::new();
    CACHE.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

/// Cache of parsed font faces: family (normalized lowercase) → `Arc<Font>`.
///
/// Reading the font file + fully parsing it with fontdue is the **only**
/// application-side startup hotspot (measured ~13ms). A size change used to
/// walk the whole chain again; with the face cached only the first request
/// pays, and every later zoom only rebuilds the 128-entry ASCII width table
/// (microseconds).
///
/// `None` is cached too: a missing font file does not hit the disk again on
/// every call.
type FaceCache = std::collections::HashMap<String, Option<std::sync::Arc<fontdue::Font>>>;

fn face_cache() -> &'static std::sync::Mutex<FaceCache> {
    static CACHE: std::sync::OnceLock<std::sync::Mutex<FaceCache>> = std::sync::OnceLock::new();
    CACHE.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

/// Fetch (and cache) a family's parsed face.
pub fn face_for_family(family: &str) -> Option<std::sync::Arc<fontdue::Font>> {
    let key = family.trim().to_ascii_lowercase();
    if let Ok(map) = face_cache().lock() {
        if let Some(hit) = map.get(&key) {
            return hit.clone();
        }
    }
    let face = font_bytes_for_family(family).and_then(|b| {
        fontdue::Font::from_bytes(b, fontdue::FontSettings::default())
            .ok()
            .map(std::sync::Arc::new)
    });
    if let Ok(mut map) = face_cache().lock() {
        // The number of font families is single-digit; the cap only defends
        // against pathological input (a family change every frame).
        if map.len() >= 32 {
            map.clear();
        }
        map.insert(key, face.clone());
    }
    face
}

/// Fetch cached [`Metrics`] by (family, size); only on a miss is the font
/// file read and parsed.
///
/// `Metrics::new`'s behavior is unchanged, merely wrapped in (family, size)
/// memoization.
pub fn metrics_for_family(family: &str, em: f32) -> std::sync::Arc<Metrics> {
    // The size is discretized at 0.01px granularity (zoom steps by 1.0,
    // settings enforce a lower bound, so the real number of combinations is
    // tiny); family is lowercased just like in `font_bytes_for_family`.
    let key = (
        family.trim().to_ascii_lowercase(),
        (em * 100.0).round() as i64,
    );
    if let Ok(map) = metrics_cache().lock() {
        if let Some(m) = map.get(&key) {
            return m.clone();
        }
    }
    let m = std::sync::Arc::new(Metrics::from_face(face_for_family(family), em));
    if let Ok(mut map) = metrics_cache().lock() {
        // Capacity guardrail: the theoretical bound = number of families ×
        // number of size steps, normally far below this; exceeding it means
        // extreme usage — simply clear and rebuild (combinations can be
        // recomputed deterministically).
        if map.len() >= 64 {
            map.clear();
        }
        map.insert(key, m.clone());
    }
    m
}

impl Metrics {
    /// Construct from raw font bytes (bypassing the face cache; for tests and
    /// cold paths).
    pub fn new(font_bytes: Option<&[u8]>, em: f32) -> Self {
        let face = font_bytes
            .and_then(|b| fontdue::Font::from_bytes(b, fontdue::FontSettings::default()).ok())
            .map(std::sync::Arc::new);
        Self::from_face(face, em)
    }

    /// Construct from an already-parsed face: only computes advance/line-
    /// height and the 128-entry ASCII table.
    pub fn from_face(face: Option<std::sync::Arc<fontdue::Font>>, em: f32) -> Self {
        let latin = face;
        let adv = latin
            .as_ref()
            .and_then(|f| {
                let g = f.lookup_glyph_index('M');
                if g == 0 {
                    return None;
                }
                let m = f.metrics_indexed(g, em);
                (m.advance_width > 0.0).then_some(m.advance_width)
            })
            .unwrap_or(em * FALLBACK_RATIO);
        let lh = (em * 1.45).round().max(em + 2.0);
        let mut m = Metrics {
            latin,
            em,
            adv,
            lh,
            ascii: [0.0; 128],
        };
        for i in 0..128u32 {
            m.ascii[i as usize] = m.char_w_uncached(char::from_u32(i).unwrap());
        }
        m
    }

    /// Width of a single character (tab handled by the caller).
    pub fn char_w(&self, ch: char) -> f32 {
        if ch == '\t' {
            return 0.0;
        }
        let c = ch as u32;
        if c < 128 {
            return self.ascii[c as usize];
        }
        self.char_w_uncached(ch)
    }

    fn char_w_uncached(&self, ch: char) -> f32 {
        if let Some(f) = &self.latin {
            let g = f.lookup_glyph_index(ch);
            if g != 0 {
                let w = f.metrics_indexed(g, self.em).advance_width;
                if w > 0.0 {
                    return w;
                }
            }
        }
        // Missing glyph: classify by unicode width.
        match unicode_width::UnicodeWidthChar::width(ch) {
            Some(0) | None => 0.0,
            Some(1) => self.em * 0.5,
            _ => self.em, // wide / CJK fallback renders at 1em
        }
    }

    pub(crate) fn tab_stop_after(&self, x: f32, tab_size: usize) -> f32 {
        let step = self.adv * tab_size.max(1) as f32;
        if step <= 0.0 {
            return x + self.adv;
        }
        (x.floor() / step).floor() * step + step
    }

    /// Total display width of `text` starting at x = 0.
    pub fn width_of(&self, text: &str, tab_size: usize) -> f32 {
        let mut x = 0.0f32;
        for ch in text.chars() {
            if ch == '\t' {
                x = self.tab_stop_after(x, tab_size);
            } else {
                x += self.char_w(ch);
            }
        }
        x
    }

    /// Display x of `byte_off` within `line` (clamped to the line).
    pub fn x_of_offset(&self, line: &str, byte_off: usize, tab_size: usize) -> f32 {
        let mut x = 0.0f32;
        let mut acc = 0usize;
        for ch in line.chars() {
            if acc >= byte_off {
                break;
            }
            if ch == '\t' {
                x = self.tab_stop_after(x, tab_size);
            } else {
                x += self.char_w(ch);
            }
            acc += ch.len_utf8();
        }
        x
    }

    /// Map a pixel x inside a line to a byte offset (snapped to a char
    /// boundary; rounds to the nearest boundary by half-advance).
    pub fn offset_of_x(&self, line: &str, target_x: f32, tab_size: usize) -> usize {
        if target_x <= 0.0 {
            return 0;
        }
        let mut x = 0.0f32;
        for (i, ch) in line.char_indices() {
            let w = if ch == '\t' {
                self.tab_stop_after(x, tab_size) - x
            } else {
                self.char_w(ch)
            };
            if target_x < x + w / 2.0 {
                return i;
            }
            if target_x < x + w {
                return i + ch.len_utf8();
            }
            x += w;
        }
        line.len()
    }

    /// Horizontal extent of the widest prefix of `line` we care about
    /// (helper for scroll width estimates).
    pub fn line_width(&self, line: &str, tab_size: usize) -> f32 {
        self.width_of(line, tab_size)
    }
}

/// Locate a font file for a CSS font family. `None` when it cannot be
/// resolved — callers fall back to `FALLBACK_RATIO`.
///
/// Windows reads known file names from `C:\Windows\Fonts` (Consolas ships with
/// every install). Linux resolves the family through fontique — the same
/// fontconfig-backed discovery stack blitz renders with — so the metrics use
/// the **real** face; otherwise the editor measured a fabricated 0.55em advance
/// while the glyphs were drawn at the generic monospace's ~0.6em, and every
/// caret/selection past the first few characters drifted. Unknown names fall
/// back to the generic `monospace` family, exactly what the CSS stack does.
///
/// macOS has no file lookup: fontique is not a dependency there, and Menlo (the
/// default monospace) is 0.602em, which `FALLBACK_RATIO` already uses.
pub fn font_bytes_for_family(family: &str) -> Option<Vec<u8>> {
    // Fontconfig (behind fontique on Linux) initializes process-wide state on
    // first use. The headless suites run their tests in parallel, so several
    // threads can miss the face cache together and enter that initialization
    // at once — a data race that showed up as an occasional SIGSEGV in
    // `tests/commands` on Linux. One process-wide lock costs nothing (a lookup
    // happens once per family; the parsed face is cached).
    static LOOKUP: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _guard = LOOKUP.lock().unwrap_or_else(|e| e.into_inner());
    imp::font_bytes_for_family(family)
}

/// Windows: known file names under `C:\Windows\Fonts` (Consolas ships with
/// every install).
#[cfg(windows)]
mod imp {
    pub fn font_bytes_for_family(family: &str) -> Option<Vec<u8>> {
        let fonts = std::path::Path::new(r"C:\Windows\Fonts");
        let name = family.trim().to_ascii_lowercase();
        let candidates: &[&str] = match name.as_str() {
            "consolas" => &["consola.ttf"],
            "cascadia code" | "cascadia mono" | "cascadia code se" => {
                &["CascadiaCode.ttf", "CascadiaMono.ttf", "CascadiaCodePL.ttf"]
            }
            "courier new" => &["cour.ttf"],
            "lucida console" => &["lucon.ttf"],
            "cascadia mono pl" => &["CascadiaMonoPL.ttf"],
            "dejavu sans mono" => &["DejaVuSansMono.ttf"],
            "liberation mono" => &["liberationmono-Regular.ttf"],
            _ => &["consola.ttf"],
        };
        for c in candidates {
            let p = fonts.join(c);
            if let Ok(b) = std::fs::read(&p) {
                return Some(b);
            }
        }
        // last resort: any known monospace file
        std::fs::read(fonts.join("consola.ttf")).ok()
    }
}

/// Linux: fontique/fontconfig resolution (see the parent function's docs).
#[cfg(target_os = "linux")]
mod imp {
    pub fn font_bytes_for_family(family: &str) -> Option<Vec<u8>> {
        super::linux_font_bytes(family)
    }
}

/// macOS and other targets: no file lookup — `super::FALLBACK_RATIO` covers
/// the platform's default monospace closely enough.
#[cfg(not(any(windows, target_os = "linux")))]
mod imp {
    pub fn font_bytes_for_family(_family: &str) -> Option<Vec<u8>> {
        None
    }
}

/// Linux face lookup through fontique (fontconfig): the requested family when
/// installed, otherwise the generic monospace family.
#[cfg(target_os = "linux")]
fn linux_font_bytes(family: &str) -> Option<Vec<u8>> {
    use fontique::{Collection, CollectionOptions, GenericFamily};

    let mut collection = Collection::new(CollectionOptions::default());
    // A snapshot (owned), not a borrow: the generic-family iterator below needs
    // `&mut collection`.
    let ids: Vec<_> = match collection.family_by_name(family) {
        Some(f) => vec![f.id()],
        None => collection
            .generic_families(GenericFamily::Monospace)
            .collect(),
    };
    for id in ids {
        let Some(fam) = collection.family(id) else {
            continue;
        };
        let Some(font) = fam.default_font() else {
            continue;
        };
        // fontdue parses face 0 of a collection; any other index is a
        // different face and cannot be selected.
        if font.index() != 0 {
            continue;
        }
        if let Some(blob) = font.load(None) {
            return Some(blob.data().to_vec());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The ASCII lookup table must be bit-for-bit identical to the per-char
    /// slow path (the cache must not change behavior).
    #[test]
    fn ascii_lookup_matches_uncached_path() {
        let m = Metrics::new(None, 14.0);
        for c in 0u32..128 {
            let ch = char::from_u32(c).unwrap();
            if ch == '\t' {
                assert_eq!(m.char_w(ch), 0.0);
                continue;
            }
            assert_eq!(m.char_w(ch), m.char_w_uncached(ch), "char {c:#x}");
        }
        // Non-ASCII goes through the slow path; behavior unchanged
        assert_eq!(m.char_w('中'), m.char_w_uncached('中'));
        assert_eq!(m.char_w('δ'), m.char_w_uncached('δ'));
        // In a monospace font every printable ASCII char has the same width
        // (the lookup table must not introduce deviation)
        let w = m.char_w('x');
        assert!(w > 0.0);
        assert!(
            (m.char_w(' ') - w).abs() < 1e-3,
            "space width should match the char width"
        );
    }

    /// A cache hit must return the same instance (after lowercasing the
    /// family), with values identical to a fresh build.
    #[test]
    fn cached_metrics_reuses_instance_and_matches_fresh() {
        let a = metrics_for_family("Consolas", 14.0);
        let b = metrics_for_family("consolas", 14.0); // lowercased → hit
        assert!(
            std::sync::Arc::ptr_eq(&a, &b),
            "the same key should share one Arc"
        );

        let fresh = Metrics::new(font_bytes_for_family("Consolas").as_deref(), 14.0);
        assert_eq!(a.em, fresh.em);
        assert_eq!(a.adv, fresh.adv);
        assert_eq!(a.lh, fresh.lh);
    }

    /// Different sizes are different cache entries, with em correct; asking
    /// for the same size again hits again.
    #[test]
    fn cached_metrics_keys_differ_by_size() {
        let s1 = metrics_for_family("Consolas", 15.0);
        let s2 = metrics_for_family("Consolas", 16.0);
        assert!(!std::sync::Arc::ptr_eq(&s1, &s2));
        assert_eq!(s1.em, 15.0);
        assert_eq!(s2.em, 16.0);
        let again = metrics_for_family("Consolas", 15.0);
        assert!(
            std::sync::Arc::ptr_eq(&s1, &again),
            "repeat requests should hit the cache"
        );
    }

    /// Different sizes of the same family must share one parsed face: a size
    /// change only rebuilds the ASCII width table, no disk read/font parse
    /// (a guardrail for zoom performance).
    #[test]
    fn font_face_is_parsed_once_per_family() {
        let a = metrics_for_family("Consolas", 21.0);
        let b = metrics_for_family("Consolas", 22.0);
        match (a.latin.as_ref(), b.latin.as_ref()) {
            (Some(fa), Some(fb)) => {
                assert!(
                    std::sync::Arc::ptr_eq(fa, fb),
                    "the two sizes should share one face"
                );
            }
            // On a machine without the font file both degrade to the
            // proportional fallback — equally legal.
            (None, None) => {}
            other => panic!("the same family should not have a face on one side only: {other:?}"),
        }
    }
}
