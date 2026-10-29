//! Encoding detection (BOM -> strict UTF-8 -> chardetng) and decode/encode.

use encoding_rs::*;

/// Encoding menu: `(encoding id in commands, menu display name)`, grouped in
/// display order (separators drawn between groups).
///
/// The id is the value carried by `Cmd::SetEncoding`, and also the line format
/// in macro files (`SetEncoding:UTF-16LE`) and the key of `set_encoding`; it
/// must **not** change with the display name — display names only matter for
/// layout ("UTF-16 LE" has one more space than the id).
///
/// This table is the single source shared by the encoding menu,
/// `set_encoding`, macro parsing and check state (`actions::is_checked`):
/// previously the menu listed its own labels and the setting function matched
/// its own copy, so adding an entry meant one more place to miss — missing
/// the check state left the menu showing "an encoding chosen but nothing
/// checked".
pub const MENU_GROUPS: &[&[(&str, &str)]] = &[
    &[
        ("UTF-8", "UTF-8"),
        ("UTF-8 BOM", "UTF-8 BOM"),
        ("UTF-16LE", "UTF-16 LE"),
        ("UTF-16BE", "UTF-16 BE"),
    ],
    &[
        ("GBK", "GBK"),
        ("Big5", "Big5"),
        ("Shift-JIS", "Shift-JIS"),
        ("Windows-1252", "Windows-1252"),
    ],
    &[("ANSI", "ANSI")],
];

/// Encoding ids in the menu, flattened in display order.
pub fn menu_ids() -> impl Iterator<Item = &'static str> {
    MENU_GROUPS.iter().flat_map(|g| g.iter().map(|(id, _)| *id))
}

/// Encoding id → (target encoding, whether to write BOM). Unknown ids return `None`.
pub fn menu_choice(id: &str) -> Option<(&'static Encoding, bool)> {
    Some(match id {
        "UTF-8" => (UTF_8, false),
        "UTF-8 BOM" => (UTF_8, true),
        "UTF-16LE" => (UTF_16LE, true),
        "UTF-16BE" => (UTF_16BE, true),
        "GBK" => (GBK, false),
        "Big5" => (BIG5, false),
        "Shift-JIS" => (SHIFT_JIS, false),
        "Windows-1252" => (WINDOWS_1252, false),
        "ANSI" => (ansi_encoding(), false),
        _ => return None,
    })
}

/// The system ANSI code page, mapped onto an `encoding_rs` static.
///
/// "ANSI" means the machine's active code page, which on a Chinese
/// Windows is GBK rather than Latin-1. The mapping covers the code pages that
/// actually occur; anything else falls back to UTF-8.
fn ansi_encoding() -> &'static Encoding {
    let cp = ansi_code_page();
    match cp {
        936 => GBK,
        950 => BIG5,
        932 => SHIFT_JIS,
        949 => EUC_KR,
        1252 => WINDOWS_1252,
        65001 => UTF_8,
        _ => UTF_8,
    }
}

/// Platform differences live in the `platform` crate: Windows reports the
/// real `GetACP()` value, other platforms express "no ANSI code page" as
/// 65001 (UTF-8).
fn ansi_code_page() -> u32 {
    platform::legacy_ansi_codepage()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Detected {
    pub encoding: &'static Encoding,
    pub bom: bool,
    /// true when the bytes are plain valid UTF-8 (no decode needed)
    pub utf8_fast: bool,
    /// Some(le) when the file is UTF-32 (encoding_rs cannot decode/encode it;
    /// we handle decoding manually and save as UTF-8 unless converted)
    pub utf32: Option<bool>,
}

impl Detected {
    pub fn new(encoding: &'static Encoding, bom: bool, utf8_fast: bool) -> Self {
        Detected {
            encoding,
            bom,
            utf8_fast,
            utf32: None,
        }
    }
}

fn starts_with(hay: &[u8], needle: &[u8]) -> bool {
    hay.len() >= needle.len() && &hay[..needle.len()] == needle
}

/// Detect encoding from a prefix of the file (pass as much as is convenient;
/// chardetng improves with more data).
pub fn detect(bytes: &[u8]) -> Detected {
    if starts_with(bytes, &[0x00, 0x00, 0xFE, 0xFF]) {
        return Detected {
            utf32: Some(false),
            ..Detected::new(UTF_8, true, false)
        };
    }
    if starts_with(bytes, &[0xFF, 0xFE, 0x00, 0x00]) {
        return Detected {
            utf32: Some(true),
            ..Detected::new(UTF_8, true, false)
        };
    }
    if starts_with(bytes, &[0xEF, 0xBB, 0xBF]) {
        return Detected::new(UTF_8, true, true);
    }
    if starts_with(bytes, &[0xFF, 0xFE]) {
        return Detected::new(UTF_16LE, true, false);
    }
    if starts_with(bytes, &[0xFE, 0xFF]) {
        return Detected::new(UTF_16BE, true, false);
    }
    match std::str::from_utf8(bytes) {
        Ok(_) => return Detected::new(UTF_8, false, true),
        // `error_len() == None` = the sample was cut in the middle of a
        // multi-byte character (UnexpectedEof), not "invalid UTF-8". The
        // probe only takes the first 64 KB of the file, and cutting through a
        // multi-byte character is the **normal case**: Chinese is 3 bytes per
        // char, so ~2/3 of boundaries cut through one. Treating it as invalid
        // input makes chardetng decode a perfectly valid UTF-8 file with its
        // second-best guess (GBK/1252) — mojibake everywhere, and saving may
        // write the mojibake back to disk.
        //
        // Only genuinely invalid bytes (error_len == Some) fall through to
        // encoding detection.
        Err(e) if e.error_len().is_none() => return Detected::new(UTF_8, false, true),
        Err(_) => {}
    }
    let mut det = chardetng::EncodingDetector::new();
    det.feed(bytes, true);
    let enc = det.guess(None, true);
    if enc == UTF_8 {
        // invalid UTF-8 but guessed UTF-8: force non-UTF-8 guess
        let mut det2 = chardetng::EncodingDetector::new();
        det2.feed(bytes, true);
        let enc2 = det2.guess(None, false);
        return Detected::new(enc2, false, false);
    }
    Detected::new(enc, false, false)
}

/// Is a buffer that failed UTF-8 validation *really* UTF-8 with a corrupt
/// tail, rather than a legacy-encoded file?
///
/// The open path has two shapes that look identical at the byte level — a
/// valid ASCII/UTF-8 prefix followed by invalid bytes:
///
/// * a UTF-8 file whose last few bytes are truncated/corrupt (a crash while
///   writing, a truncated download) — the body is a large amount of valid
///   multi-byte UTF-8;
/// * a GBK/Big5/Shift-JIS file whose head happens to be plain ASCII — the body
///   is *not* valid UTF-8 at all, and the bytes only make sense when decoded as
///   the legacy encoding.
///
/// Count the valid multi-byte UTF-8 bytes: when they dominate, the file is
/// UTF-8 and the invalid bytes are a rounding error worth a U+FFFD; when they
/// are absent the file needs its real encoding (decoding it as lossy UTF-8
/// would replace every CJK character and then write those replacements back on
/// the next save).
pub fn mostly_valid_utf8(bytes: &[u8]) -> bool {
    let mut valid_multibyte = 0usize;
    let mut invalid = 0usize;
    let mut rest = bytes;
    loop {
        match std::str::from_utf8(rest) {
            Ok(_) => {
                valid_multibyte += rest.iter().filter(|b| **b >= 0x80).count();
                break;
            }
            Err(e) => {
                let good = e.valid_up_to();
                valid_multibyte += rest[..good].iter().filter(|b| **b >= 0x80).count();
                let bad = match e.error_len() {
                    // A truncated sequence at the very end of the buffer.
                    None => rest.len() - good,
                    Some(n) => n,
                };
                invalid += bad;
                let next = good + bad;
                if next >= rest.len() {
                    break;
                }
                rest = &rest[next..];
            }
        }
    }
    // A handful of corrupt bytes next to a real body of UTF-8 text. The
    // threshold is deliberately permissive in one direction only: wrongly
    // choosing the legacy encoding costs a "Reopen with encoding" click, while
    // wrongly choosing lossy UTF-8 destroys the CJK content on the next save.
    invalid > 0 && valid_multibyte >= 32 && invalid * 100 <= valid_multibyte
}

/// Decode a complete buffer to UTF-8. Handles UTF-32 (not supported by
/// encoding_rs) manually. Returns owned text with any BOM stripped.
pub fn decode_all(bytes: &[u8], detected: Detected) -> String {
    decode_all_checked(bytes, detected).0
}

/// [`decode_all`] plus whether any byte had to be replaced with U+FFFD.
///
/// The open path uses the flag to drive the save warning: an undecodable byte
/// in a GBK/Big5 file silently becomes U+FFFD, and re-encoding U+FFFD writes a
/// `&#65533;` character reference (7 bytes for 1) — the user has to be told
/// before that lands on disk.
pub fn decode_all_checked(bytes: &[u8], detected: Detected) -> (String, bool) {
    let enc = detected.encoding;
    if let Some(le) = detected.utf32 {
        return (decode_utf32(bytes, le), false);
    }
    if detected.utf8_fast {
        let mut b = bytes;
        // Guard: the bom flag may come from a prefix probe while this buffer
        // is shorter (the public API semantically allows any input); slicing
        // [3..] directly with len<3 would panic.
        if detected.bom && b.len() >= 3 {
            b = &b[3..];
        }
        // safe: utf8_fast implies valid UTF-8 (BOM removed keeps validity)
        return match String::from_utf8(b.to_vec()) {
            Ok(s) => (s, false),
            Err(e) => (String::from_utf8_lossy(e.as_bytes()).into_owned(), true),
        };
    }
    let (cow, _, had_errors) = enc.decode(bytes); // strips BOM where applicable
    (cow.into_owned(), had_errors)
}

fn decode_utf32(bytes: &[u8], le: bool) -> String {
    // Strip the 4-byte BOM for both endiannesses: UTF-32LE is FF FE 00 00,
    // UTF-32BE is 00 00 FE FF (its first byte is 0x00, so a 2-byte LE/BE-BOM
    // check would never match and the BOM leaked into the text as U+FEFF).
    let b = if bytes.len() >= 4
        && (bytes[0..4] == [0xFF, 0xFE, 0x00, 0x00] || bytes[0..4] == [0x00, 0x00, 0xFE, 0xFF])
    {
        &bytes[4..]
    } else {
        bytes
    };
    let mut out = String::with_capacity(b.len() / 2 + 4);
    let mut i = 0;
    while i + 4 <= b.len() {
        let u = if le {
            u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]])
        } else {
            u32::from_be_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]])
        };
        out.push(char::from_u32(u).unwrap_or('\u{FFFD}'));
        i += 4;
    }
    out
}

pub fn bom_for(enc: &'static Encoding, want_bom: bool) -> &'static [u8] {
    if !want_bom {
        return b"";
    }
    if enc == UTF_8 {
        b"\xEF\xBB\xBF"
    } else if enc == UTF_16LE {
        b"\xFF\xFE"
    } else if enc == UTF_16BE {
        b"\xFE\xFF"
    } else {
        b""
    }
}

/// Encode UTF-8 text to the target encoding. Returns (bytes_without_bom, had_errors).
pub fn encode(text: &str, enc: &'static Encoding) -> (Vec<u8>, bool) {
    if enc == UTF_8 {
        return (text.as_bytes().to_vec(), false);
    }
    // encoding_rs treats UTF-16 as decode-only; encode manually
    if enc == UTF_16LE {
        let mut v = Vec::with_capacity(text.len() * 2);
        for u in text.encode_utf16() {
            v.extend_from_slice(&u.to_le_bytes());
        }
        return (v, false);
    }
    if enc == UTF_16BE {
        let mut v = Vec::with_capacity(text.len() * 2);
        for u in text.encode_utf16() {
            v.extend_from_slice(&u.to_be_bytes());
        }
        return (v, false);
    }
    let (cow, _, had_errors) = enc.encode(text);
    (cow.into_owned(), had_errors)
}

pub fn display_name(enc: &'static Encoding) -> &'static str {
    enc.name()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_boms() {
        assert_eq!(detect(&[0xEF, 0xBB, 0xBF, b'a']).encoding, UTF_8);
        let d = detect(&[0xFF, 0xFE, 0x61, 0x00]);
        assert_eq!(d.encoding, UTF_16LE);
        assert!(d.bom);
        let d = detect(&[0xFE, 0xFF, 0x00, 0x61]);
        assert_eq!(d.encoding, UTF_16BE);
    }

    #[test]
    fn detect_utf8_and_ascii() {
        let d = detect("hello 世界\n".as_bytes());
        assert_eq!(d.encoding, UTF_8);
        assert!(d.utf8_fast);
        assert!(!d.bom);
    }

    #[test]
    fn detect_gbk() {
        // GBK bytes for "你好世界" (invalid as UTF-8)
        let bytes = vec![0xC4, 0xE3, 0xBA, 0xC3, 0xCA, 0xC0, 0xBD, 0xE7];
        let d = detect(&bytes);
        assert_ne!(d.encoding, UTF_8);
        let text = decode_all(&bytes, d);
        assert_eq!(text, "你好世界");
    }

    /// A sample cut in the middle of a multi-byte character is **not**
    /// invalid UTF-8.
    ///
    /// Regression: `detect` used to treat `UnexpectedEof` as bad input, so a
    /// file whose first 64 KB were all valid UTF-8 but whose boundary cut
    /// open one Chinese character was decoded with chardetng's
    /// windows-1252/GBK guess — the whole file became mojibake (measured: an
    /// 80 KB file read back as 100 KB of `ä¸­æ–‡`).
    #[test]
    fn a_sample_cut_inside_a_character_is_still_utf8() {
        let mut full = "a".repeat(64 * 1024 - 1);
        full.push_str("中文测试行，继续写下去。\n");
        let bytes = full.as_bytes();
        // 0..=65535 cuts exactly after the first byte of the 3-byte character
        let cut = 64 * 1024;
        assert!(
            !full.is_char_boundary(cut),
            "sample must be cut mid-character"
        );
        let d = detect(&bytes[..cut]);
        assert_eq!(
            d.encoding, UTF_8,
            "truncated sample should still detect as UTF-8"
        );
        assert!(d.utf8_fast);
        assert!(!d.bom);
        // Decoding the full (valid) file with this verdict must reproduce it
        // byte for byte
        let full_detected = detect(bytes);
        assert!(full_detected.utf8_fast);
        assert_eq!(decode_all(bytes, full_detected), full);
    }

    /// Genuinely invalid bytes (not "cut mid-character") still fall through
    /// to encoding detection.
    #[test]
    fn genuinely_invalid_utf8_still_falls_back_to_detection() {
        let mut bytes = "hello ".repeat(100).into_bytes();
        bytes.extend_from_slice(&[0xFF, 0xFE, 0x41]); // tail clearly not UTF-8
        let d = detect(&bytes);
        assert!(!d.utf8_fast);
        assert_ne!(d.encoding, UTF_8);
    }

    #[test]
    fn utf16_roundtrip() {
        let text = "hello 世界\r\nsecond";
        let (mut bytes, err) = encode(text, UTF_16LE);
        assert!(!err);
        let mut with_bom = vec![0xFF, 0xFE];
        with_bom.append(&mut bytes);
        let d = detect(&with_bom);
        assert_eq!(d.encoding, UTF_16LE);
        let back = decode_all(&with_bom, d);
        assert_eq!(back, text);
    }

    #[test]
    fn utf8_bom_stripped() {
        let mut v = vec![0xEF, 0xBB, 0xBF];
        v.extend_from_slice("abc".as_bytes());
        let d = detect(&v);
        let s = decode_all(&v, d);
        assert_eq!(s, "abc");
    }

    #[test]
    fn utf32_le_decode() {
        let mut v = vec![0xFF, 0xFE, 0x00, 0x00];
        for cp in ['a' as u32, '中' as u32] {
            v.extend_from_slice(&cp.to_le_bytes());
        }
        let d = detect(&v);
        assert_eq!(d.utf32, Some(true));
        assert_eq!(decode_all(&v, d), "a中");
    }
}
