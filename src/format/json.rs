// ---- JSON -----------------------------------------------------------------

/// Pretty-print JSON with 2-space indent, **preserving key order**.
///
/// serde_json's `Value` is a `BTreeMap`, so `to_string_pretty` silently sorts
/// object keys — a formatter must not reorder the document. The text is first
/// validated with a streaming `IgnoredAny` parse (no DOM, so huge documents
/// don't pay a tree), then re-indented by a byte-level pass that keeps every
/// byte of the original tokens. UTF-8 is self-synchronizing and every JSON
/// structural character is ASCII, so the formatter can copy non-ASCII bytes
/// through verbatim without decoding.
pub fn json_pretty(src: &str) -> Result<String, String> {
    serde_json::from_str::<serde::de::IgnoredAny>(src).map_err(|e| e.to_string())?;
    Ok(pretty_json(src))
}

fn pretty_json(src: &str) -> String {
    let mut out = String::with_capacity(src.len() * 2);
    let mut depth = 0usize;
    let mut in_str = false;
    let mut esc = false;
    let bytes = src.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if in_str {
            // Inside a string every byte is literal, including { } , : \n.
            if c < 0x80 {
                out.push(c as char);
                i += 1;
                if esc {
                    esc = false;
                } else if c == b'\\' {
                    esc = true;
                } else if c == b'"' {
                    in_str = false;
                }
            } else {
                // multi-byte UTF-8 sequence: copy verbatim (never structural)
                let end = (i + utf8_seq_len(c)).min(bytes.len());
                out.push_str(&src[i..end]);
                i = end;
            }
            continue;
        }
        match c {
            b'"' => {
                in_str = true;
                out.push('"');
                i += 1;
            }
            b'{' | b'[' => {
                // Empty containers stay inline: {} and [].
                let close = if c == b'{' { b'}' } else { b']' };
                let mut j = i + 1;
                while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                    j += 1;
                }
                if j < bytes.len() && bytes[j] == close {
                    out.push(c as char);
                    out.push(close as char);
                    i = j + 1;
                } else {
                    out.push(c as char);
                    depth += 1;
                    push_nl(&mut out, depth);
                    i += 1;
                }
            }
            b'}' | b']' => {
                depth = depth.saturating_sub(1);
                push_nl(&mut out, depth);
                out.push(c as char);
                i += 1;
            }
            b',' => {
                out.push(',');
                push_nl(&mut out, depth);
                i += 1;
            }
            b':' => {
                out.push_str(": ");
                i += 1;
            }
            c if c.is_ascii_whitespace() => i += 1,
            // Non-ASCII (multi-byte) or any other literal: copy the full
            // UTF-8 sequence through verbatim.
            _ => {
                let ch_len = utf8_seq_len(c);
                let end = (i + ch_len).min(bytes.len());
                out.push_str(&src[i..end]);
                i = end;
            }
        }
    }
    out
}

/// Length of the UTF-8 sequence starting with lead byte `b` (1 for ASCII).
///
/// Always goes through [`crate::textops::utf8_seq_len`]; the input is a valid
/// `&str`, so the stray branch is unreachable in practice.
fn utf8_seq_len(b: u8) -> usize {
    crate::textops::utf8_seq_len(b)
}

fn push_nl(out: &mut String, depth: usize) {
    out.push('\n');
    for _ in 0..depth {
        out.push_str("  ");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_pretty_indents_and_sorts_nothing() {
        let got = json_pretty(r#"{"b":1,"a":[1,2]}"#).unwrap();
        assert_eq!(got, "{\n  \"b\": 1,\n  \"a\": [\n    1,\n    2\n  ]\n}");
        assert!(json_pretty("{bad").is_err());
    }
}
