//! The application mark, as inline SVG for the title bar.
//!
//! Same design as `material/infeditor-small.svg` (the 16–24px optical
//! variant: no drop shadow, the page fills most of the tile), carried here as
//! markup because blitz renders it through the same `dangerous_inner_html`
//! path as the toolbar icons and the MD-preview formulas — a bitmap would need
//! a decode step and a second copy of the asset at every DPI.
//!
//! Geometry lives in both places on purpose: the `material/` SVG is the source
//! for the `.ico`/`.png` files, this one is drawn at one fixed box (16 CSS px,
//! so 32 physical px on a 200% display) where the folds and hairlines of the
//! large variant would only alias.

/// The mark at a 16×16 CSS box: an accent tile, a white page with a folded
/// corner, two text lines and the caret that ends the second one.
pub fn svg() -> String {
    "<svg viewBox=\"0 0 256 256\" width=\"16\" height=\"16\" \
     xmlns=\"http://www.w3.org/2000/svg\">\
     <defs>\
     <linearGradient id=\"inf-mark-tile\" x1=\"0.05\" y1=\"0\" x2=\"0.95\" y2=\"1\">\
     <stop offset=\"0\" stop-color=\"#6f97ff\"/>\
     <stop offset=\"0.52\" stop-color=\"#4477f7\"/>\
     <stop offset=\"1\" stop-color=\"#2743b8\"/>\
     </linearGradient>\
     </defs>\
     <rect width=\"256\" height=\"256\" rx=\"56\" fill=\"url(#inf-mark-tile)\"/>\
     <path d=\"M64 18 H156 L210 72 V200 A22 22 0 0 1 188 222 H64 A22 22 0 0 1 42 200 \
     V40 A22 22 0 0 1 64 18 Z\" fill=\"#ffffff\"/>\
     <path d=\"M156 18 L210 72 H172 A16 16 0 0 1 156 56 Z\" fill=\"#bdd0ff\"/>\
     <g fill=\"#3c62dc\">\
     <rect x=\"72\" y=\"104\" width=\"112\" height=\"20\" rx=\"10\"/>\
     <rect x=\"72\" y=\"150\" width=\"66\" height=\"20\" rx=\"10\"/>\
     <rect x=\"72\" y=\"196\" width=\"94\" height=\"20\" rx=\"10\" opacity=\"0.42\"/>\
     <rect x=\"156\" y=\"138\" width=\"26\" height=\"44\" rx=\"13\"/>\
     </g>\
     </svg>"
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Never empty: blitz's mutation writer replaces the children of an empty
    /// `dangerous_inner_html` with a zero node and the next template
    /// placeholder's `load_child` panics out of bounds (the same rule the
    /// toolbar documents for `mdicons::svg`).
    #[test]
    fn mark_renders_non_empty_svg() {
        let s = svg();
        assert!(s.starts_with("<svg"), "must start with an svg root");
        assert!(s.ends_with("</svg>"));
        // The caret is the element that makes the mark read as an editor.
        assert!(s.contains("x=\"156\" y=\"138\""), "caret missing");
        // The tile must be filled, otherwise the white page vanishes on a
        // light title bar.
        assert!(s.contains("url(#inf-mark-tile)"), "tile fill missing");
    }
}
