//! wys hint autocompletion.
//!
//! Two trigger surfaces (both in wys mode; text mode never triggers — this module hangs only on
//! `MdWys`):
//!
//! - **emoji**: typing `:` pops candidates (`:smi` filters down to smile…), and further typing
//!   filters live; confirming replaces the `:prefix` with the emoji character (the same shape as
//!   the toolbar's Emoji insert — the 72-item subset of `mdws::EMOJIS`, names in [`EMOJI_NAMES`]).
//! - **code-block language**: with focus at the start of a fenced code block (right after ```),
//!   type a language name (`ru` → rust/ruby…); confirming deletes the typed prefix and writes the
//!   block's lang.
//!
//! The layering matches the existing overlay (`mdws::WysPreedit`): **all interaction logic is
//! pure functions / a pure state machine** ([`detect`] / [`emoji_candidates`] / [`step`] /
//! [`confirm_for`] …, see the unit tests at the end of the file); [`WysHint`] only renders the
//! candidate panel as an overlay hugging the caret; model mutations and event-phase geometry
//! queries live in `mdws` ([`hint_confirm`](super::mdws::hint_confirm) / `mdws::hint_probe`). The
//! position reuses the caret overlay's coordinates (`mdws::OVERLAYS` signal, a read-only
//! snapshot during render).
//!
//! Known trade-off: writing the language on the fence line (` ```ru⏎ `) is not covered — the
//! kernel's autoformat only recognizes a bare ` ``` ` (`edit.rs` must not change), so languages
//! always go through the front door: typing inside the block.

use dioxus_native::prelude::*;

use crate::ui::state::EditorState;

/// Code language candidates (the common set).
pub const LANGS: [&str; 30] = [
    "rust",
    "python",
    "javascript",
    "typescript",
    "java",
    "c",
    "cpp",
    "csharp",
    "go",
    "ruby",
    "php",
    "swift",
    "kotlin",
    "scala",
    "html",
    "css",
    "sql",
    "shell",
    "bash",
    "powershell",
    "json",
    "yaml",
    "toml",
    "xml",
    "markdown",
    "lua",
    "perl",
    "dart",
    "haskell",
    "diff",
];

/// Emoji names (**index-for-index** with `mdws::EMOJIS`; the unit tests check length and
/// uniqueness).
/// Candidates render as `😀 grinning` and filter by name prefix (case-insensitive).
pub const EMOJI_NAMES: [&str; 72] = [
    "grinning",
    "smiley",
    "smile",
    "grin",
    "laughing",
    "sweat_smile",
    "rofl",
    "joy",
    "slightly_smiling",
    "upside_down",
    "wink",
    "blush",
    "innocent",
    "heart_smile",
    "heart_eyes",
    "star_struck",
    "kissing_heart",
    "kissing",
    "kissing_closed_eyes",
    "kissing_smiling_eyes",
    "yum",
    "stuck_out_tongue",
    "tongue_wink",
    "zany",
    "raised_eyebrow",
    "monocle",
    "nerd",
    "sunglasses",
    "partying",
    "smirk",
    "unamused",
    "disappointed",
    "pensive",
    "worried",
    "confused",
    "frowning",
    "persevere",
    "confounded",
    "tired",
    "weary",
    "pleading",
    "cry",
    "sob",
    "triumph",
    "angry",
    "rage",
    "cursing",
    "exploding_head",
    "flushed",
    "hot_face",
    "cold_face",
    "scream",
    "fearful",
    "cold_sweat",
    "sad_relieved",
    "sweat",
    "hugging",
    "thinking",
    "hand_over_mouth",
    "shushing",
    "lying",
    "mute",
    "neutral",
    "expressionless",
    "grimacing",
    "rolling_eyes",
    "hushed",
    "frowning_open",
    "anguished",
    "open_mouth",
    "astonished",
    "yawn",
];

/// Max rows the candidate panel shows (typing narrows the range; the selected index clamps to
/// this window too).
pub const MAX_ROWS: usize = 8;

/// Max trigger-string length (any longer is not treated as completion intent — close the panel).
const MAX_PREFIX: usize = 24;

/// The kind of candidate.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HintKind {
    Emoji,
    Lang,
}

/// Panel state (pure data; transitions in [`step`]).
#[derive(Clone, PartialEq, Debug)]
pub struct HintState {
    pub kind: HintKind,
    /// The filter prefix typed so far (emoji excludes the `:`; lang = the text typed inside the
    /// block).
    pub prefix: String,
    /// The currently highlighted row (0..window).
    pub sel: usize,
    /// The caret's viewport anchor `(x, y, row height)` (relative to the wys-root border box,
    /// same as `mdws::caret_view` — an event-phase snapshot; the panel renders hugging the caret).
    pub pos: Option<(f32, f32, f32)>,
    /// Scroll offset at show time (any scroll closes the panel, see [`step`]).
    pub scroll: (f32, f32),
}

/// Description of the model mutation that confirms one item (pure data; executed in
/// `mdws::hint_confirm`).
#[derive(Clone, PartialEq, Debug)]
pub struct Confirm {
    /// How many trigger-string characters to delete (for emoji, the `:` itself counts).
    pub delete: usize,
    /// Text to insert (the emoji character; empty for language candidates).
    pub insert: String,
    /// Language candidate: written into the code block's lang (the kernel `Cmd` has no language
    /// variant).
    pub lang: Option<&'static str>,
}

/// Trigger-string character set (shared by emoji prefixes and language names: identifier-like).
fn trigger_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '+' | '#' | '-' | '.')
}

/// Emoji trigger detection (pure function): `before` = the text before the caret.
///
/// Take the **last** `:`: only a suffix of trigger chars (≤ `MAX_PREFIX`) forms a trigger, and
/// the `:` itself must sit at the start or right after a non-word character (`http://…` and `a:b`
/// do not trigger — `p`/`a` are word characters). An empty prefix (just typed `:`) also triggers
/// (showing the first batch of candidates).
pub fn emoji_prefix(before: &str) -> Option<String> {
    let colon = before.rfind(':')?;
    if colon > 0 {
        let prev = before[..colon].chars().next_back()?;
        if prev.is_ascii_alphanumeric() || prev == '_' {
            return None;
        }
    }
    let suffix = &before[colon + 1..];
    if suffix.len() > MAX_PREFIX || !suffix.chars().all(trigger_char) {
        return None;
    }
    Some(suffix.to_string())
}

/// Code-block language trigger detection (pure function): `code` = the whole block's text,
/// `off` = the caret's byte offset.
///
/// It forms only when typing "from the block's start all the way to the caret": the caret is at
/// the end and the whole text is trigger chars (an empty string or an interior space disqualifies
/// it — that is ordinary code, not a language name).
pub fn lang_prefix(code: &str, off: usize) -> Option<String> {
    if code.is_empty() || off != code.len() || code.len() > MAX_PREFIX {
        return None;
    }
    if !code.chars().all(trigger_char) {
        return None;
    }
    Some(code.to_string())
}

/// Post-keystroke trigger detection (pure function): code blocks route through [`lang_prefix`],
/// everything else through [`emoji_prefix`]. `None` = there should be no panel.
pub fn detect(is_code: bool, before: &str, code: &str, off: usize) -> Option<(HintKind, String)> {
    if is_code {
        lang_prefix(code, off).map(|p| (HintKind::Lang, p))
    } else {
        emoji_prefix(before).map(|p| (HintKind::Emoji, p))
    }
}

/// Emoji candidates (pure function): name **prefix** match (case-insensitive); an empty prefix =
/// the full set. Returns `mdws::EMOJIS` indices (full-set order preserved).
pub fn emoji_candidates(prefix: &str) -> Vec<usize> {
    let p = prefix.to_lowercase();
    EMOJI_NAMES
        .iter()
        .enumerate()
        .filter(|(_, name)| p.is_empty() || name.to_lowercase().starts_with(&p))
        .map(|(i, _)| i)
        .collect()
}

/// Language candidates (pure function): same rules as [`emoji_candidates`]; indices into
/// [`LANGS`].
pub fn lang_candidates(prefix: &str) -> Vec<usize> {
    let p = prefix.to_lowercase();
    LANGS
        .iter()
        .enumerate()
        .filter(|(_, name)| p.is_empty() || name.to_lowercase().starts_with(&p))
        .map(|(i, _)| i)
        .collect()
}

impl HintState {
    /// The current filter results (the full list; rendering/selection takes the first
    /// [`MAX_ROWS`]).
    pub fn candidates(&self) -> Vec<usize> {
        match self.kind {
            HintKind::Emoji => emoji_candidates(&self.prefix),
            HintKind::Lang => lang_candidates(&self.prefix),
        }
    }

    /// How many rows are selectable/visible (the smaller of full list and window).
    pub fn rows(&self) -> usize {
        self.candidates().len().min(MAX_ROWS)
    }

    /// Move the selection with ↑/↓ (pure state machine): clamped inside the window, no
    /// wraparound.
    pub fn on_nav(&mut self, up: bool) {
        let rows = self.rows();
        if rows == 0 {
            self.sel = 0;
            return;
        }
        let last = rows - 1;
        self.sel = if up {
            self.sel.saturating_sub(1)
        } else {
            (self.sel + 1).min(last)
        };
    }

    /// The model mutation for confirming the highlighted item from the keyboard.
    pub fn confirm(&self) -> Option<Confirm> {
        let idx = *self.candidates().get(self.sel)?;
        confirm_for(self.kind, &self.prefix, idx)
    }
}

/// The model mutation for confirming item `idx` (full-set index; pure function — mouse and
/// keyboard share the same source).
///
/// emoji: delete the `:prefix` (prefix + colon = n+1 chars) and insert the emoji character;
/// language: delete the typed prefix and set `lang = Some(…)` (inserted text is empty).
/// `idx` out of range / not in the current filter results → `None` (no action).
pub fn confirm_for(kind: HintKind, prefix: &str, idx: usize) -> Option<Confirm> {
    let list = match kind {
        HintKind::Emoji => emoji_candidates(prefix),
        HintKind::Lang => lang_candidates(prefix),
    };
    if !list.contains(&idx) {
        return None;
    }
    let typed = prefix.chars().count();
    Some(match kind {
        HintKind::Emoji => Confirm {
            delete: typed + 1,
            insert: super::mdws::EMOJIS[idx].to_string(),
            lang: None,
        },
        HintKind::Lang => Confirm {
            delete: typed,
            insert: String::new(),
            lang: Some(LANGS[idx]),
        },
    })
}

/// State transition (pure function): every probe round folds "trigger-detection result + caret
/// anchor" into a new state.
///
/// - detection fails / empty filter results → close the panel (`None`);
/// - kind and prefix both unchanged → keep the selection (typing only narrows; ↑↓ is not lost);
/// - any change → selection resets to 0.
pub fn step(
    prev: Option<&HintState>,
    det: Option<(HintKind, String)>,
    pos: Option<(f32, f32, f32)>,
    scroll: (f32, f32),
) -> Option<HintState> {
    let (kind, prefix) = det?;
    let mut next = HintState {
        kind,
        prefix,
        sel: 0,
        pos,
        scroll,
    };
    if next.rows() == 0 {
        return None;
    }
    if let Some(p) = prev {
        if p.kind == next.kind && p.prefix == next.prefix {
            next.sel = p.sel.min(next.rows().saturating_sub(1));
        }
    }
    Some(next)
}

// ---- global panel state (written in the host's event phase, read-only during render) --------

static HINT: GlobalSignal<Option<HintState>> = Signal::global(|| None);

/// Whether the panel is open (keyboard routing decides which key belongs to whom).
pub fn open() -> bool {
    HINT.read().is_some()
}

/// Read the current state (for rendering/confirming).
pub fn state() -> Option<HintState> {
    HINT.read().clone()
}

/// Conditional write (the iron rule: write only on change, never a self-perpetuating redraw).
pub fn set_state(next: Option<HintState>) {
    if *HINT.read() != next {
        *HINT.signal().write() = next;
    }
}

/// Close the panel (zero writes when already closed).
pub fn close() {
    set_state(None);
}

/// ↑/↓ navigation (event phase).
pub fn nav(up: bool) {
    let mut g = HINT.signal();
    let mut w = g.write();
    if let Some(s) = w.as_mut() {
        s.on_nav(up);
    }
}

/// The keyboard-confirm target: the full-set index of the highlighted row (outside the window /
/// empty panel → `None`).
pub fn selected_index() -> Option<usize> {
    let g = HINT.read();
    let s = g.as_ref()?;
    s.candidates().get(s.sel).copied()
}

// ---- view (not covered by headless render tests — manual verification in the component's comment) ----

/// Row text: emoji shows `😀 grinning`, language shows `rust`.
fn row_label(kind: HintKind, idx: usize) -> String {
    match kind {
        HintKind::Emoji => format!("{} {}", super::mdws::EMOJIS[idx], EMOJI_NAMES[idx]),
        HintKind::Lang => LANGS[idx].to_string(),
    }
}

/// The hint candidate panel: rendered just below the caret overlay ([`HintState::pos`]);
/// ↑↓/Enter/Tab/Esc and further typing are routed by `mdws::wys_key`, and click-to-confirm runs
/// `mdws::hint_confirm`.
///
/// Beyond what headless render tests cover — verify manually: type `:` on an empty line in wys
/// mode and the panel pops hugging the caret; typing `smi` narrows it live, ↑↓ moves the
/// highlight, Enter confirms to 😄; in a code block type `ru` and rust/ruby appear, and after
/// confirming the block's language becomes rust (the emitted fence line carries `rust`).
#[component]
pub fn WysHint() -> Element {
    let mut state = use_context::<Signal<EditorState>>();
    let snap = {
        let g = HINT.read();
        g.as_ref().map(|s| (s.kind, s.sel, s.pos, s.candidates()))
    };
    let Some((kind, sel, pos, cands)) = snap else {
        return rsx! {};
    };
    let Some((x, y, h)) = pos else {
        return rsx! {};
    };
    if cands.is_empty() {
        return rsx! {};
    }
    // Theme color snapshot (read-only during render)
    let (bg, fg, border, hl) = {
        let st = state.read();
        let t = st.theme();
        (t.menu_bg, t.menu_fg, t.menu_border, t.menu_open)
    };
    let panel = format!(
        "position:absolute; left:{x:.1}px; top:{:.1}px; min-width:150px; \
         background:{bg}; border:1px solid {border}; border-radius:6px; \
         box-shadow:0 2px 8px rgba(0,0,0,0.35); padding:2px; z-index:6; \
         font-size:12px; color:{fg}; overflow:hidden; touch-action:none;",
        y + h
    );
    // Rows = (full-set index, text, style): the highlighted row gets the selection background
    let rows: Vec<(usize, String, String)> = cands
        .iter()
        .take(MAX_ROWS)
        .enumerate()
        .map(|(i, &idx)| {
            let row_bg = if i == sel { hl } else { "transparent" };
            (
                idx,
                row_label(kind, idx),
                format!(
                    "height:22px; line-height:22px; padding:0 8px; border-radius:4px; \
                     background:{row_bg}"
                ),
            )
        })
        .collect();
    rsx! {
        div { style: "{panel}",
            for (idx, label, row_style) in rows {
                div {
                    key: "{idx}",
                    style: "{row_style}",
                    onclick: move |_| {
                        super::mdws::hint_confirm(&mut state.write(), idx);
                    },
                    "{label}"
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- emoji prefix matching (trigger + filter) -----------------------------

    #[test]
    fn emoji_prefix_detects_trigger() {
        // just typing `:` (empty prefix) triggers immediately
        assert_eq!(emoji_prefix("hello :").as_deref(), Some(""));
        assert_eq!(emoji_prefix(":").as_deref(), Some(""));
        // the filter string
        assert_eq!(emoji_prefix("hello :smi").as_deref(), Some("smi"));
        assert_eq!(emoji_prefix(":a-b_c+d.e").as_deref(), Some("a-b_c+d.e"));
        // a word char right against `:` does not trigger (http:// and a:b are not completion intent)
        assert_eq!(emoji_prefix("http://x.y"), None);
        assert_eq!(emoji_prefix("a:b"), None);
        // a non-trigger character breaks the run (CJK / space)
        assert_eq!(emoji_prefix(":smi 中"), None);
        assert_eq!(emoji_prefix(":中"), None);
        // overly long counts as plain text
        assert_eq!(
            emoji_prefix(&format!(":{}", "a".repeat(MAX_PREFIX + 1))),
            None
        );
        // take the last colon
        assert_eq!(emoji_prefix("a: x :ok").as_deref(), Some("ok"));
    }

    #[test]
    fn emoji_candidates_filter_by_name_prefix() {
        // `:smi` → smile (index 2) etc.; case-insensitive
        let c = emoji_candidates("smi");
        assert!(c.contains(&2), "smile missing from :smi candidates: {c:?}");
        assert!(c.contains(&1), "smiley: {c:?}");
        assert_eq!(c, emoji_candidates("SMI"));
        // empty prefix = the full set
        assert_eq!(emoji_candidates("").len(), 72);
        // no match = empty (step closes the panel after a trigger)
        assert!(emoji_candidates("zzz").is_empty());
    }

    #[test]
    fn emoji_names_pair_with_emojis() {
        // The name table pairs one-to-one with `mdws::EMOJIS` (change one side, change the other)
        assert_eq!(EMOJI_NAMES.len(), super::super::mdws::EMOJIS.len());
        let mut names = EMOJI_NAMES.to_vec();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), EMOJI_NAMES.len(), "duplicate names");
    }

    // ---- code language candidate filtering -----------------------------------

    #[test]
    fn lang_prefix_only_at_code_head() {
        // typing from a fresh block's start all the way to the caret = a trigger string
        assert_eq!(lang_prefix("ru", 2).as_deref(), Some("ru"));
        assert_eq!(lang_prefix("rust", 4).as_deref(), Some("rust"));
        // an empty block (nothing typed yet) does not pop
        assert_eq!(lang_prefix("", 0), None);
        // caret not at the end (editing existing code) does not pop
        assert_eq!(lang_prefix("rust", 2), None);
        // the whole text not being all trigger chars (ordinary code) does not pop
        assert_eq!(lang_prefix("let x", 5), None);
        assert_eq!(lang_prefix("a b", 3), None);
        // too long does not pop
        assert_eq!(
            lang_prefix(&"a".repeat(MAX_PREFIX + 1), MAX_PREFIX + 1),
            None
        );
    }

    #[test]
    fn lang_candidates_filter() {
        assert_eq!(lang_candidates("py"), vec![1]); // python
        assert!(lang_candidates("ru").contains(&0), "rust");
        assert!(lang_candidates("ru").contains(&9), "ruby");
        assert_eq!(lang_candidates("java"), vec![2, 4]); // javascript, java
        assert_eq!(lang_candidates("").len(), LANGS.len());
        assert!(lang_candidates("zzz").is_empty());
    }

    #[test]
    fn detect_routes_by_block_kind() {
        // code block → language; ordinary block → emoji
        assert_eq!(
            detect(true, "ru", "ru", 2),
            Some((HintKind::Lang, "ru".into()))
        );
        assert_eq!(
            detect(false, "hi :smi", "", 0),
            Some((HintKind::Emoji, "smi".into()))
        );
        assert_eq!(detect(false, "plain text", "", 0), None);
    }

    // ---- selection state machine (navigation / transition / confirmation) ----------------

    #[test]
    fn nav_clamps_within_visible_window() {
        let mut s = HintState {
            kind: HintKind::Emoji,
            prefix: String::new(), // full set 72 → window 8
            sel: 0,
            pos: None,
            scroll: (0.0, 0.0),
        };
        s.on_nav(true);
        assert_eq!(s.sel, 0, "↑ does not wrap at the top");
        for _ in 0..10 {
            s.on_nav(false);
        }
        assert_eq!(s.sel, MAX_ROWS - 1, "↓ clamps to the last visible row");
        s.on_nav(true);
        assert_eq!(s.sel, MAX_ROWS - 2);
        // no candidates: selection resets to 0
        let mut e = HintState {
            kind: HintKind::Lang,
            prefix: "zzz".into(),
            sel: 3,
            pos: None,
            scroll: (0.0, 0.0),
        };
        e.on_nav(false);
        assert_eq!(e.sel, 0);
    }

    #[test]
    fn step_keeps_sel_only_when_same_kind_and_prefix() {
        let det = Some((HintKind::Emoji, "smi".to_string()));
        let s1 = step(None, det.clone(), Some((1.0, 2.0, 12.0)), (0.0, 0.0)).unwrap();
        assert_eq!(s1.sel, 0);
        // prefix unchanged (repeat probe in the same round) → keep the selection
        let mut moved = s1.clone();
        moved.on_nav(false);
        let s2 = step(
            Some(&moved),
            det.clone(),
            Some((1.0, 2.0, 12.0)),
            (0.0, 0.0),
        )
        .unwrap();
        assert_eq!(s2.sel, 1);
        // prefix changed → reset (smil is still a valid prefix of smile, so the panel stays open)
        let s3 = step(
            Some(&s2),
            Some((HintKind::Emoji, "smil".to_string())),
            None,
            (0.0, 0.0),
        )
        .unwrap();
        assert_eq!(s3.sel, 0);
        // a kind change resets too (lang's ru has candidates)
        let s4 = step(
            Some(&s3),
            Some((HintKind::Lang, "ru".to_string())),
            None,
            (0.0, 0.0),
        )
        .unwrap();
        assert_eq!(s4.sel, 0);
        assert_eq!(s4.kind, HintKind::Lang);
        // detection failed / empty candidates → close
        assert_eq!(step(Some(&s1), None, None, (0.0, 0.0)), None);
        assert_eq!(
            step(
                Some(&s1),
                Some((HintKind::Emoji, "zzz".into())),
                None,
                (0.0, 0.0)
            ),
            None
        );
    }

    #[test]
    fn confirm_replaces_trigger_with_emoji_or_lang() {
        use super::super::mdws::EMOJIS;
        // emoji: confirming smile for `:smi` → delete 3+1 chars, insert 😄 (same shape as the toolbar's Emoji)
        let c = confirm_for(HintKind::Emoji, "smi", 2).unwrap();
        assert_eq!(c.delete, 4);
        assert_eq!(c.insert, EMOJIS[2]);
        assert_eq!(c.lang, None);
        // just typed `:` (empty prefix) → delete only the colon
        assert_eq!(confirm_for(HintKind::Emoji, "", 0).unwrap().delete, 1);
        // language: delete the typed prefix, inserted text empty, lang lands as Some
        let c = confirm_for(HintKind::Lang, "ru", 0).unwrap();
        assert_eq!(c.delete, 2);
        assert!(c.insert.is_empty());
        assert_eq!(c.lang, Some("rust"));
        // an index outside the current filter results does nothing
        assert_eq!(
            confirm_for(HintKind::Emoji, "smi", 47),
            None,
            "exploding_head does not match smi"
        );
        assert_eq!(confirm_for(HintKind::Lang, "ru", 1), None);
        // the state machine's keyboard confirm shares this same source
        let s = HintState {
            kind: HintKind::Lang,
            prefix: "py".into(),
            sel: 0,
            pos: None,
            scroll: (0.0, 0.0),
        };
        assert_eq!(s.confirm(), confirm_for(HintKind::Lang, "py", 1));
    }
}
