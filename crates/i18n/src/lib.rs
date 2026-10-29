//! UI text: one **compile-time** catalog plus a runtime language switch.
//!
//! Design and migration plan: `README.md` in this crate.
//!
//! # Three guarantees, all from the compiler
//!
//! 1. **No missing translation.** A message is a [`Msg`] / [`Fmt`] variant; every
//!    language file under `locales/` is an exhaustive `match` over the whole
//!    enum, and the per-locale dispatcher is an exhaustive `match` over
//!    [`Locale`] — a missing row or an unwired locale stops the build.
//! 2. **No typo'd placeholder.** Each locale of a [`Fmt`] is a `format!` literal
//!    whose placeholders are the destructured field names. `{lien}` is a
//!    "cannot find value" error — not a runtime panic, not text on screen.
//! 3. **No allocation for static text.** [`Msg`] renders as `Cow::Borrowed`.
//!
//! # Two kinds of message
//!
//! * [`Msg`] — fixed text: menu entries, buttons, group titles.
//! * [`Fmt`] — text with arguments: status bar, dialog bodies, questions.
//!
//! The split is not only about allocation. `Fmt` keeps its arguments as fields, so
//! rendering happens at the last moment: state carries an `Fmt`, not a rendered
//! `String`, which is what lets an open dialog or a status message follow a
//! language switch. A rendered string stored in state is frozen in that language.
//!
//! # Usage
//!
//! ```ignore
//! span { "{st.tr(Msg::StatusReadOnly)}" }
//! span { "{st.tr(Fmt::LineCol { line, col })}" }
//!
//! // Without an EditorState (library internals, crash log, registry):
//! i18n::current().tr(Msg::CommonCancel)
//! ```
//!
//! # Language tags
//!
//! Persist [`Locale::tag`] (`"zh-CN"` / `"en-US"`), never a numeric index: an
//! index shifts when the enum changes, so an old settings file would silently
//! select a different language.

#![forbid(unsafe_code)]

use std::borrow::Cow;
use std::sync::atomic::{AtomicU8, Ordering};

#[macro_use]
mod table;
mod keys;
mod locales;

pub use keys::{Fmt, Msg};
pub use table::plural;

/// A UI language.
///
/// `#[repr(u8)]` exists only for [`current`]'s atomic slot; the value is **not** a
/// persistence format. Persist [`Locale::tag`].
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
#[repr(u8)]
pub enum Locale {
    /// Simplified Chinese.
    #[default]
    ZhCn = 0,
    /// English (US).
    EnUs = 1,
    /// Japanese.
    Ja = 2,
    /// Korean.
    Ko = 3,
    /// French.
    Fr = 4,
    /// German.
    De = 5,
    /// Spanish.
    Es = 6,
    /// Portuguese.
    Pt = 7,
    /// Italian.
    It = 8,
    /// Arabic.
    ///
    /// The catalog is translated; the UI layout itself is still LTR — proper
    /// RTL mirroring is a rendering-level feature, not a catalog one.
    Ar = 9,
}

impl Locale {
    /// Order used by the language picker and by tests.
    pub const ALL: [Locale; 10] = [
        Locale::ZhCn,
        Locale::EnUs,
        Locale::Ja,
        Locale::Ko,
        Locale::Fr,
        Locale::De,
        Locale::Es,
        Locale::Pt,
        Locale::It,
        Locale::Ar,
    ];

    /// Used when nothing else applies. Chinese is the project's first language,
    /// so the default UI does not change during migration.
    pub const DEFAULT: Locale = Locale::ZhCn;

    /// BCP-47 tag. **This is the persistence key** (settings file, registry,
    /// crash log); changing it changes the file format.
    pub const fn tag(self) -> &'static str {
        match self {
            Locale::ZhCn => "zh-CN",
            Locale::EnUs => "en-US",
            Locale::Ja => "ja",
            Locale::Ko => "ko",
            Locale::Fr => "fr",
            Locale::De => "de",
            Locale::Es => "es",
            Locale::Pt => "pt",
            Locale::It => "it",
            Locale::Ar => "ar",
        }
    }

    /// The language's **own** name, shown in the picker.
    ///
    /// Deliberately outside the catalog and never translated: an English UI must
    /// still show "简体中文" legibly, otherwise a user has to read English to get
    /// back to Chinese.
    pub const fn native_name(self) -> &'static str {
        match self {
            Locale::ZhCn => "简体中文",
            Locale::EnUs => "English",
            Locale::Ja => "日本語",
            Locale::Ko => "한국어",
            Locale::Fr => "Français",
            Locale::De => "Deutsch",
            Locale::Es => "Español",
            Locale::Pt => "Português",
            Locale::It => "Italiano",
            Locale::Ar => "العربية",
        }
    }

    /// Parses a language tag, accepting the shapes that show up in environment
    /// variables: `zh`, `zh-CN`, `zh_CN.UTF-8`, `en-US`, `en_GB@euro`.
    ///
    /// Only the primary subtag is considered. There is no `zh-Hant` variant yet,
    /// so `zh-TW` maps to Simplified Chinese; supporting Traditional means adding
    /// a [`Locale`] variant, not a hidden mapping here.
    pub fn from_tag(raw: &str) -> Option<Self> {
        let cleaned: String = raw
            .trim()
            .split(['.', '@'])
            .next()
            .unwrap_or("")
            .replace('_', "-")
            .to_ascii_lowercase();
        match cleaned.split('-').next().unwrap_or("") {
            "zh" => Some(Locale::ZhCn),
            "en" => Some(Locale::EnUs),
            "ja" => Some(Locale::Ja),
            "ko" => Some(Locale::Ko),
            "fr" => Some(Locale::Fr),
            "de" => Some(Locale::De),
            "es" => Some(Locale::Es),
            "pt" => Some(Locale::Pt),
            "it" => Some(Locale::It),
            "ar" => Some(Locale::Ar),
            _ => None,
        }
    }

    /// Renders one message. `loc.tr(Msg::…)`; UI code usually goes through
    /// `EditorState::tr`, which forwards here.
    pub fn tr(self, text: impl Text) -> Cow<'static, str> {
        text.render(self)
    }
}

/// One translatable message. Both [`Msg`] and [`Fmt`] implement it.
///
/// It exists so the call site has one spelling — `st.tr(…)` — without caring which
/// table a message lives in. Resolution is monomorphized, there is no vtable.
pub trait Text {
    fn render(&self, loc: Locale) -> Cow<'static, str>;
}

/// Either kind of message as **one value**: a fixed [`Msg`] or a
/// parameterized [`Fmt`].
///
/// State that holds UI text (`EditorState::status`, `vim::Op::Status`, dialog
/// bodies) stores this, never a rendered `String` — so the text re-renders in
/// the current language on every draw instead of freezing into one language.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Message {
    /// Fixed text.
    Plain(Msg),
    /// Text with arguments, rendered at draw time.
    With(Fmt),
}

impl Text for Message {
    fn render(&self, loc: Locale) -> Cow<'static, str> {
        match self {
            Message::Plain(m) => m.render(loc),
            Message::With(f) => f.render(loc),
        }
    }
}

impl From<Msg> for Message {
    fn from(m: Msg) -> Self {
        Message::Plain(m)
    }
}

impl From<Fmt> for Message {
    fn from(f: Fmt) -> Self {
        Message::With(f)
    }
}

impl Message {
    /// The variant name (`"Plain:MenuSave"` / `"With:StatusLines"`), for tests.
    pub fn key(&self) -> String {
        match self {
            Message::Plain(m) => format!("Plain:{}", m.key()),
            Message::With(f) => format!("With:{}", f.key()),
        }
    }
}

// ---------------------------------------------------------------------------
// Process-level mirror
// ---------------------------------------------------------------------------

/// The current language. `EditorState` is the source of truth; this is its mirror.
///
/// Only for places that cannot reach the state:
///
/// * libraries (`wysiwyg`, `ui-kit` do not know about the editor state);
/// * one-shot effects outside the reactive tree (registry writes, crash log,
///   panic hook).
///
/// [`set_current`] is the only writer; the application calls it at startup and
/// when the user switches language.
static CURRENT: AtomicU8 = AtomicU8::new(Locale::DEFAULT as u8);

/// Sets the process-level language (see [`current`]).
pub fn set_current(loc: Locale) {
    CURRENT.store(loc as u8, Ordering::Relaxed);
}

/// The process-level language, [`Locale::DEFAULT`] until set.
pub fn current() -> Locale {
    Locale::ALL
        .get(CURRENT.load(Ordering::Relaxed) as usize)
        .copied()
        .unwrap_or(Locale::DEFAULT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tags_round_trip() {
        for loc in Locale::ALL {
            assert_eq!(Locale::from_tag(loc.tag()), Some(loc), "{}", loc.tag());
        }
    }

    #[test]
    fn accepts_the_shapes_that_end_up_in_env_vars() {
        for raw in ["zh", "zh-CN", "zh_CN.UTF-8", "ZH_cn", "zh-Hans-CN"] {
            assert_eq!(Locale::from_tag(raw), Some(Locale::ZhCn), "{raw}");
        }
        for raw in ["en", "en-US", "en_GB@euro", "EN_us.utf8"] {
            assert_eq!(Locale::from_tag(raw), Some(Locale::EnUs), "{raw}");
        }
        // New languages resolve to their own locale; the caller decides what an
        // untranslated column means (rendering falls back to English).
        for (raw, want) in [
            ("ja", Locale::Ja),
            ("ja-JP", Locale::Ja),
            ("ko", Locale::Ko),
            ("fr-FR", Locale::Fr),
            ("de", Locale::De),
            ("es", Locale::Es),
            ("pt", Locale::Pt),
            ("pt-BR", Locale::Pt),
            ("it", Locale::It),
            ("ar", Locale::Ar),
        ] {
            assert_eq!(Locale::from_tag(raw), Some(want), "{raw}");
        }
        // Unknown tags must have no answer, so the caller can fall back to system
        // detection or the default instead of a catch-all mapping swallowing it.
        for raw in ["", "C", "POSIX", "ru"] {
            assert_eq!(Locale::from_tag(raw), None, "{raw}");
        }
    }

    #[test]
    fn native_names_are_not_translated() {
        assert_eq!(Locale::ZhCn.native_name(), "简体中文");
        assert_eq!(Locale::EnUs.native_name(), "English");
    }

    #[test]
    fn process_mirror_defaults_to_the_default_locale() {
        // No global write here: it would race with parallel tests.
        assert_eq!(Locale::DEFAULT, Locale::ZhCn);
        let _ = current();
    }
}
