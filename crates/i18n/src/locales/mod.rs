//! One file per language.
//!
//! Each locale file is an exhaustive `match` over [`Msg`] / [`Fmt`]: the
//! compiler rejects a row missing from any language, so "translated" is a
//! build-time property, not a review claim. The two dispatchers below are
//! exhaustive over [`Locale`]: adding a variant forces a file to be wired in.

pub(crate) mod ar;
pub(crate) mod de;
pub(crate) mod en_us;
pub(crate) mod es;
pub(crate) mod fr;
pub(crate) mod it;
pub(crate) mod ja;
pub(crate) mod ko;
pub(crate) mod pt;
pub(crate) mod zh_cn;

use std::borrow::Cow;

use crate::{Fmt, Locale, Msg};

/// Dispatch a fixed message to the language's file.
pub(crate) fn msg(loc: Locale, m: Msg) -> &'static str {
    match loc {
        Locale::ZhCn => zh_cn::msg(m),
        Locale::EnUs => en_us::msg(m),
        Locale::Ja => ja::msg(m),
        Locale::Ko => ko::msg(m),
        Locale::Fr => fr::msg(m),
        Locale::De => de::msg(m),
        Locale::Es => es::msg(m),
        Locale::Pt => pt::msg(m),
        Locale::It => it::msg(m),
        Locale::Ar => ar::msg(m),
    }
}

/// Dispatch an argument-bearing message to the language's file.
pub(crate) fn fmt(loc: Locale, m: Fmt) -> Cow<'static, str> {
    match loc {
        Locale::ZhCn => zh_cn::fmt(m),
        Locale::EnUs => en_us::fmt(m),
        Locale::Ja => ja::fmt(m),
        Locale::Ko => ko::fmt(m),
        Locale::Fr => fr::fmt(m),
        Locale::De => de::fmt(m),
        Locale::Es => es::fmt(m),
        Locale::Pt => pt::fmt(m),
        Locale::It => it::fmt(m),
        Locale::Ar => ar::fmt(m),
    }
}
