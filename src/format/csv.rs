// ---- CSV ------------------------------------------------------------------

/// Split a CSV/TSV document into rows (RFC 4180 quoting: `""` inside a quoted
/// field is a literal quote; quoted fields may contain delimiters, newlines).
/// Blank lines are skipped.
pub fn parse_csv(src: &str, delim: u8) -> Vec<Vec<String>> {
    parse_csv_spans(src, delim)
        .into_iter()
        .map(|(_, _, row)| row)
        .collect()
}

/// Like [`parse_csv`] but each row also carries its **starting document line**
/// (0-based, counting source lines by `\n`). Quoted fields may span lines, so
/// a row can start anywhere; the mapping lets the table view jump back to the
/// text view at the right line.
pub fn parse_csv_indexed(src: &str, delim: u8) -> Vec<(u64, Vec<String>)> {
    parse_csv_spans(src, delim)
        .into_iter()
        .map(|(start, _, row)| (start, row))
        .collect()
}

/// Like [`parse_csv_indexed`] but with the row's **exclusive end line** too.
///
/// `[start, end)` covers the row's own lines (including newlines inside a
/// quoted field and the row terminator when present), but **not** blank lines
/// between records. That is exactly what an in-place row rewrite needs: a
/// quoted multi-line note must be replaced as one block instead of being
/// refused for the whole document.
pub fn parse_csv_spans(src: &str, delim: u8) -> Vec<(u64, u64, Vec<String>)> {
    let d = delim as char;
    let mut rows: Vec<(u64, u64, Vec<String>)> = Vec::new();
    let mut row: Vec<String> = Vec::new();
    let mut field = String::new();
    let mut in_quotes = false;
    let mut row_line = 0u64;
    let mut line = 0u64;
    let mut chars = src.chars().peekable();
    while let Some(c) = chars.next() {
        if in_quotes {
            match c {
                '"' => {
                    if chars.peek() == Some(&'"') {
                        field.push('"');
                        chars.next();
                    } else {
                        in_quotes = false;
                    }
                }
                // a quoted field may contain newlines (RFC 4180) — they are
                // real document lines, so the line counter must advance too,
                // otherwise every following row's line mapping is off
                '\n' => {
                    line += 1;
                    field.push(c);
                }
                '\r' => {
                    if chars.peek() == Some(&'\n') {
                        chars.next();
                        line += 1;
                        // CRLF is **two bytes**: keeping only '\r' would turn the field content from
                        // "a\r\nb" into "a\rb"; writing an edit back corrupts the data
                        field.push('\r');
                        field.push('\n');
                    } else {
                        line += 1;
                        field.push(c);
                    }
                }
                _ => field.push(c),
            }
        } else if c == '"' && field.is_empty() {
            in_quotes = true;
        } else if c == d {
            row.push(std::mem::take(&mut field));
        } else if c == '\n' || c == '\r' {
            if c == '\r' && chars.peek() == Some(&'\n') {
                chars.next();
            }
            line += 1;
            row.push(std::mem::take(&mut field));
            if row.len() > 1 || !row[0].is_empty() {
                rows.push((row_line, line, std::mem::take(&mut row)));
            } else {
                row.clear();
            }
            row_line = line;
        } else {
            field.push(c);
        }
    }
    if in_quotes || !field.is_empty() || !row.is_empty() {
        // Final field (no trailing delimiter/newline), possibly unterminated.
        row.push(field);
        if row.len() > 1 || !row[0].is_empty() {
            // No trailing newline: the row occupies the current line, so the
            // exclusive end is `line + 1`.
            rows.push((row_line, line + 1, row));
        }
    }
    rows
}

/// Minimal RFC 4180 serialization: a field is quoted only when it has to be
/// (contains the delimiter, a quote, CR or LF); `""` doubles inner quotes.
/// Used by the table view's cell write-back and its structural column edits —
/// those rewrite the document, so the output stays as close to the user's
/// original formatting as quoting rules allow.
pub fn serialize_csv_row(row: &[String], delim: u8) -> String {
    let d = delim as char;
    let mut out = String::new();
    for (i, field) in row.iter().enumerate() {
        if i > 0 {
            out.push(d);
        }
        if field.contains(d) || field.contains('"') || field.contains('\n') || field.contains('\r')
        {
            out.push('"');
            out.push_str(&field.replace('"', "\"\""));
            out.push('"');
        } else {
            out.push_str(field);
        }
    }
    out
}

/// Whether every quoted field in `src` is closed before the end of the text.
///
/// Structural table edits rewrite one record at a time; an unterminated quote
/// means the parser swallowed the rest of the file into that record. Rewriting
/// it would "repair" the quote and silently change unrelated lines, so those
/// edits stay refused. Multi-line **closed** fields are fine.
pub fn csv_quotes_balanced(src: &str, delim: u8) -> bool {
    let d = delim as char;
    let mut chars = src.chars().peekable();
    let mut in_quotes = false;
    // A quote only opens a field at its start, matching `parse_csv_spans`.
    let mut at_field_start = true;
    while let Some(c) = chars.next() {
        if in_quotes {
            if c == '"' {
                if chars.peek() == Some(&'"') {
                    chars.next();
                } else {
                    in_quotes = false;
                }
            }
            continue;
        }
        if c == '"' && at_field_start {
            in_quotes = true;
        } else {
            at_field_start = c == d || c == '\n' || c == '\r';
        }
    }
    !in_quotes
}

/// Comparison key for table-view sorting: numeric when both ends parse as
/// numbers, byte order otherwise.
pub fn csv_sort_key(s: &str) -> (f64, String) {
    match s.trim().parse::<f64>() {
        Ok(v) => (v, String::new()),
        Err(_) => (f64::NEG_INFINITY, s.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn csv_quoted_crlf_round_trips() {
        let src = "\"a\r\nb\",c\n";
        let rows = parse_csv(src, b',');
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0][0], "a\r\nb", "CRLF must be preserved intact");
        // Minimal-quoting serialization should round-trip to the original text
        let out = serialize_csv_row(&rows[0], b',');
        assert_eq!(out, "\"a\r\nb\",c");
    }

    #[test]
    fn csv_respects_quotes_and_newlines() {
        let rows = parse_csv("a,b\n\"x,1\",\"he said \"\"hi\"\"\"\n\nz,w", b',');
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[1], vec!["x,1", "he said \"hi\""]);
        assert_eq!(rows[2], vec!["z", "w"]);
    }

    #[test]
    fn csv_quoted_field_can_span_lines() {
        let rows = parse_csv("\"multi\nline\",b", b',');
        assert_eq!(rows[0][0], "multi\nline");
        assert_eq!(rows[0][1], "b");
    }

    #[test]
    fn csv_quote_balance_distinguishes_closed_multiline_from_unclosed() {
        assert!(csv_quotes_balanced("\"multi\nline\",b", b','));
        assert!(csv_quotes_balanced("a,\"say \"\"hi\"\"\"", b','));
        assert!(!csv_quotes_balanced("a,\"unterminated\nnext", b','));
        // A quote in the middle of an unquoted field is literal, as in the parser.
        assert!(csv_quotes_balanced("a b\"c", b','));
    }
}
