//! The two catalog macros: `msg_keys!` (no arguments) and `fmt_keys!` (with
//! arguments).
//!
//! One invocation generates the enum from the **key list only** — the wording
//! lives in `crate::locales`, one file per language. The split keeps the three
//! original guarantees, all from the compiler:
//!
//! * every locale file is an exhaustive `match` over the enum — a half-
//!   translated language cannot be written;
//! * the per-locale dispatch in `locales` is an exhaustive `match` over
//!   [`crate::Locale`] — a new locale cannot skip its file;
//! * a `Fmt` field name is an **identifier** bound by destructuring in the
//!   locale file, so a typo is a compile error instead of a panic or literal
//!   `{lien}` on screen.

/// Generates the argument-less message enum `Msg`.
///
/// ```ignore
/// msg_keys! {
///     /// The File menu
///     MenuFile;
///     MenuEdit;
/// }
/// ```
///
/// Rendering borrows a `&'static str` (`Cow::Borrowed`): rebuilding two hundred
/// menu labels per frame allocates nothing.
macro_rules! msg_keys {
    ($(
        $(#[$meta:meta])*
        $key:ident;
    )*) => {
        /// An argument-less UI message. The wording lives in `crate::locales`.
        ///
        /// The variant name is the stable key used by code; the text is data. They
        /// are separate namespaces, so editing wording never touches code and
        /// editing code never touches wording.
        #[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
        pub enum Msg {
            $(
                $(#[$meta])*
                $key,
            )*
        }

        impl $crate::Text for Msg {
            fn render(&self, loc: $crate::Locale) -> ::std::borrow::Cow<'static, str> {
                ::std::borrow::Cow::Borrowed(self.get(loc))
            }
        }

        impl Msg {
            /// The literal for `loc`, borrowed for the whole program.
            ///
            /// For APIs that only take `&'static str`: Win32 window titles
            /// (`winshell::Popup::title`), registry strings, static tables.
            /// Argument-bearing messages have no such method — they must allocate,
            /// use [`crate::Fmt`].
            pub fn get(self, loc: $crate::Locale) -> &'static str {
                $crate::locales::msg(loc, self)
            }

            /// The variant name (`Msg::MenuFile` → `"MenuFile"`).
            ///
            /// For **tests and logs** only: a test should assert which key a command
            /// carries, not the wording of one language (wording changes should not
            /// turn tests red).
            pub fn key(&self) -> &'static str {
                match self {
                    $( Msg::$key => stringify!($key), )*
                }
            }
        }
    };
}

/// Generates the argument-bearing message enum `Fmt`.
///
/// ```ignore
/// fmt_keys! {
///     LineCol { line: usize, col: usize };
/// }
/// ```
///
/// Field names are the `format!` placeholders in each locale file. Word order
/// may differ between locales — which is exactly why the arguments stay in the
/// struct instead of a rendered string ending up in state.
macro_rules! fmt_keys {
    ($(
        $(#[$meta:meta])*
        $key:ident { $($arg:ident : $ty:ty),* $(,)? };
    )*) => {
        /// A UI message **with arguments** (status bar, dialog body, question).
        ///
        /// Separate from [`Msg`] because the arguments live in the value, so
        /// rendering can be deferred: an open dialog or a status message then
        /// follows a language switch instead of keeping a snapshot.
        #[derive(Clone, PartialEq, Eq, Debug)]
        pub enum Fmt {
            $(
                $(#[$meta])*
                $key { $($arg: $ty),* },
            )*
        }

        impl $crate::Text for Fmt {
            // One message need not use every argument in every language: Chinese
            // says "共 3 行" where English needs a noun ("3 lines"). That is a
            // property of a translation table, not a typo, so the lint is off at
            // the locale-file level; a wrong argument *name* is still caught by
            // `format!`.
            fn render(&self, loc: $crate::Locale) -> ::std::borrow::Cow<'static, str> {
                $crate::locales::fmt(loc, ::std::clone::Clone::clone(self))
            }
        }

        impl Fmt {
            /// The variant name (for tests and logs, see [`Msg::key`]).
            pub fn key(&self) -> &'static str {
                match self {
                    $( Fmt::$key { .. } => stringify!($key), )*
                }
            }
        }
    };
}

/// English plural: `n == 1` takes the singular form.
///
/// Only used by an English-side `Fmt` (or by callers pre-computing a noun).
/// Both forms are spelled out instead of appending `-s`, because the rules are
/// not uniform ("1 entry" / "2 entries", "1 match" / "2 matches") and a guessed
/// rule is wrong sooner or later.
pub fn plural<'a>(n: usize, one: &'a str, many: &'a str) -> &'a str {
    if n == 1 {
        one
    } else {
        many
    }
}
