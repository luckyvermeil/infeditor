// ---- XML ------------------------------------------------------------------

/// Indent XML by tag nesting.
///
/// Best effort by design: a stray close tag never panics and never loses text,
/// it just mis-indents. Self-closing tags, processing instructions, comments
/// and DOCTYPE are kept verbatim on their own line. Text content is trimmed;
/// element-per-line formatting is what a formatter is for.
pub fn xml_pretty(src: &str) -> String {
    let mut tokens: Vec<&str> = Vec::new();
    let mut rest = src;
    while let Some(lt) = rest.find('<') {
        if lt > 0 {
            tokens.push(&rest[..lt]);
        }
        let after = &rest[lt..];
        let Some(gt_rel) = find_tag_end(after) else {
            tokens.push(after);
            rest = "";
            break;
        };
        let end = lt + gt_rel + 1;
        tokens.push(&rest[lt..end]);
        rest = &rest[end..];
    }
    if !rest.is_empty() {
        tokens.push(rest);
    }

    let mut out = String::with_capacity(src.len() + src.len() / 4);
    let mut depth: usize = 0;
    let mut k = 0;
    while k < tokens.len() {
        // Collapse `open + short text + close` into one line, so `<x>t</x>`
        // stays one line instead of growing three.
        if k + 2 < tokens.len() {
            if let (Some(open), Some(text), Some(close)) = (
                strip_tag(tokens[k]),
                text_token(tokens[k + 1]),
                strip_tag(tokens[k + 2]),
            ) {
                // is_close_tag must be checked first: `close` may be any non-ASCII tag body
                // (e.g. `中`), and slicing `&close[1..]` for a body not starting with '/'
                // would panic mid-multibyte-character. '/' is ASCII, so the slice is safe
                // after the check.
                if opens_element(open)
                    && is_close_tag(close)
                    && tag_name(open) == tag_name(&close[1..])
                    && !tokens[k + 1].contains('\n')
                    && tokens[k + 1].trim().len() <= 60
                {
                    for _ in 0..depth {
                        out.push_str("  ");
                    }
                    out.push_str(tokens[k].trim());
                    out.push_str(text);
                    out.push_str(tokens[k + 2].trim());
                    out.push('\n');
                    k += 3;
                    continue;
                }
            }
        }
        let tok = tokens[k];
        k += 1;
        if let Some(tag) = strip_tag(tok) {
            let body = tag.trim();
            if is_close_tag(body) {
                depth = depth.saturating_sub(1);
                push_line(&mut out, depth, tok);
            } else {
                push_line(&mut out, depth, tok);
                if opens_element(body) {
                    depth += 1;
                }
            }
        } else {
            let text = tok.trim();
            if !text.is_empty() {
                push_line(&mut out, depth, tok);
            }
        }
    }
    out.trim_end_matches('\n').to_string()
}

/// Trimmed text of a token that is *not* a tag; `None` for tags/empties.
fn text_token(tok: &str) -> Option<&str> {
    if strip_tag(tok).is_some() {
        return None;
    }
    let t = tok.trim();
    if t.is_empty() {
        None
    } else {
        Some(t)
    }
}

/// The element name of a tag body: `"root a=\"1\"" → "root"`.
fn tag_name(body: &str) -> &str {
    body.split_whitespace().next().unwrap_or(body)
}

/// Index of the `>` closing a tag starting at `<`, `>`-in-attribute and
/// `-->`-comment aware.
fn find_tag_end(after_lt: &str) -> Option<usize> {
    let bytes = after_lt.as_bytes();
    let mut in_quote = 0u8; // 0 none, b'"', b'\''
    let comment = after_lt.starts_with("<!--");
    for (i, &b) in bytes.iter().enumerate().skip(1) {
        match b {
            b'>' if comment => {
                // Byte-level comparison: slicing `i-2..=i` as &str requires i-2 on a char
                // boundary; a comment with a multibyte character + `>` (e.g. `<!--中>-->`)
                // would panic on a continuation byte. `-->` is pure ASCII, compare bytes
                // directly.
                if i >= 2 && &bytes[i - 2..=i] == b"-->" {
                    return Some(i);
                }
            }
            b'>' if in_quote == 0 => return Some(i),
            b'"' if in_quote != b'\'' => in_quote = if in_quote == b'"' { 0 } else { b'"' },
            b'\'' if in_quote != b'"' => in_quote = if in_quote == b'\'' { 0 } else { b'\'' },
            _ => {}
        }
    }
    None
}

/// `Some(tag_body)` if the token is exactly one `<…>` tag.
fn strip_tag(tok: &str) -> Option<&str> {
    let t = tok.trim();
    if t.starts_with('<') && t.ends_with('>') && t.matches('<').count() == 1 && t.len() >= 3 {
        Some(&t[1..t.len() - 1])
    } else {
        None
    }
}

fn is_close_tag(body: &str) -> bool {
    body.starts_with('/')
}

/// A tag that increases nesting: not a close, not self-closing, not a PI,
/// comment or declaration.
fn opens_element(body: &str) -> bool {
    if is_close_tag(body) || body.ends_with('/') {
        return false;
    }
    if body.starts_with('?') || body.starts_with('!') {
        return false;
    }
    true
}

fn push_line(out: &mut String, depth: usize, tok: &str) {
    for _ in 0..depth {
        out.push_str("  ");
    }
    // Tags and text tokens are treated the same here; both are trimmed and
    // re-indented. (Kept as plain `trim` — a future distinction would only
    // affect the text case.)
    let line = tok.trim();
    out.push_str(line);
    out.push('\n');
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xml_pretty_multibyte_close_tag_does_not_panic() {
        let got = xml_pretty("<a>x<中></中></a>");
        assert!(got.contains("中"), "content should be preserved: {got}");
        assert!(got.contains("<a>"));
    }

    /// Regression: a comment containing a multibyte character + `>` used to panic when backing
    /// up two bytes to compare `-->` (`&after_lt[i-2..=i]` landed on a continuation byte).

    #[test]
    fn xml_pretty_comment_with_multibyte_does_not_panic() {
        let got = xml_pretty("<r><!--中>--><x/></r>");
        assert!(
            got.contains("<!--中>-->"),
            "comment should be preserved: {got}"
        );
        assert!(got.contains("<x/>"));
    }

    /// Regression: fenced code blocks used to write the opening tags twice
    /// (`<pre><code><pre><code>`).

    #[test]
    fn xml_pretty_nests_and_keeps_declarations() {
        let got = xml_pretty(r#"<?xml version="1.0"?><root a="1"><x>t</x><y/></root>"#);
        assert_eq!(
            got,
            "<?xml version=\"1.0\"?>\n<root a=\"1\">\n  <x>t</x>\n  <y/>\n</root>"
        );
    }

    #[test]
    fn xml_pretty_survives_attributes_with_gt_and_comments() {
        let got = xml_pretty(r#"<!-- a>b --><r t="i>j"><v/></r>"#);
        assert_eq!(got, "<!-- a>b -->\n<r t=\"i>j\">\n  <v/>\n</r>");
    }

    #[test]
    fn xml_pretty_stray_close_does_not_panic() {
        let got = xml_pretty("</a><b/>");
        assert!(got.contains("<b/>"));
    }
}
