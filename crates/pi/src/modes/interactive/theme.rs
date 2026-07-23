//! Product theme: resolved colors, built-in defaults, JSON loading, and
//! conversion to pi-tui component themes.
//!
//! Ports `.references/pi/packages/coding-agent/src/modes/interactive/theme/`
//! (`theme.ts`, `dark.json`, `light.json`). The reference uses a global
//! singleton `theme`; pi-tui component themes take `fn(&str) -> String` hooks
//! that cannot capture state, so this module mirrors the singleton with a
//! thread-local *current* [`Arc<ResolvedTheme>`] that the `fn`-pointer color
//! helpers read. Built-in themes are [`LazyLock`] interns; loaded themes are
//! returned as owned [`Arc`] handles (no leaks).
//!
//! # Fallibility
//!
//! [`ThemeJson`] validates structure. [`ThemeJson::resolve`] fails on missing
//! colors, bad hex, unknown variable references, or circular variable chains.
//! The view-model falls back to the built-in dark theme on any error so a bad
//! `theme.json` never breaks the terminal — see [`dark`] and the
//! `theme_invalid_falls_back_to_dark` test.

use std::borrow::Cow;
use std::cell::RefCell;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::{Arc, LazyLock};

pub use pi_tui::components::{DefaultTextStyle, MarkdownOptions, MarkdownTheme};
use pi_tui::components::{SelectListTheme, SettingsListTheme};
use pi_tui::terminal::probe::TerminalTheme;
use pi_tui::text::{truncate_to_width, visible_width};
use syntect::highlighting::ScopeSelector;
use syntect::parsing::{
    ParseState, ScopeStack, ScopeStackOp, SyntaxDefinition, SyntaxSet, SyntaxSetBuilder,
};
use syntect::util::LinesWithEndings;

use crate::core::config;
use crate::core::settings::ThemeMode;

/// Terminal color depth selected at startup.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ColorMode {
    /// 24-bit `CSI 38;2;r;g;b` sequences.
    Truecolor,
    /// Downsampled `CSI 38;5;n` 256-color palette.
    Palette256,
}

impl ColorMode {
    /// Map a terminal true-color capability flag to the emitted color depth.
    #[must_use]
    pub const fn from_true_color(tc: bool) -> Self {
        if tc {
            Self::Truecolor
        } else {
            Self::Palette256
        }
    }
}

/// Product foreground/text color slots (see reference `ThemeColor`).
///
/// Order matches the JSON schema and the reference type alias.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum ThemeColor {
    /// Core UI.
    Accent,
    /// Border default.
    Border,
    /// Accent border.
    BorderAccent,
    /// Muted border.
    BorderMuted,
    /// Success foreground.
    Success,
    /// Error foreground.
    Error,
    /// Warning foreground.
    Warning,
    /// Muted foreground.
    Muted,
    /// Dim foreground.
    Dim,
    /// Default text foreground.
    Text,
    /// Reasoning text foreground.
    ThinkingText,
    /// User message foreground.
    UserMessageText,
    /// Custom message foreground.
    CustomMessageText,
    /// Custom message label.
    CustomMessageLabel,
    /// Tool title.
    ToolTitle,
    /// Tool output.
    ToolOutput,
    /// Markdown heading.
    MdHeading,
    /// Markdown link.
    MdLink,
    /// Markdown link URL suffix.
    MdLinkUrl,
    /// Markdown inline code.
    MdCode,
    /// Markdown code block body.
    MdCodeBlock,
    /// Markdown code block border.
    MdCodeBlockBorder,
    /// Markdown quote body.
    MdQuote,
    /// Markdown quote border.
    MdQuoteBorder,
    /// Markdown horizontal rule.
    MdHr,
    /// Markdown list bullet.
    MdListBullet,
    /// Diff added line.
    ToolDiffAdded,
    /// Diff removed line.
    ToolDiffRemoved,
    /// Diff context line.
    ToolDiffContext,
    /// Syntax comment.
    SyntaxComment,
    /// Syntax keyword.
    SyntaxKeyword,
    /// Syntax function.
    SyntaxFunction,
    /// Syntax variable.
    SyntaxVariable,
    /// Syntax string.
    SyntaxString,
    /// Syntax number.
    SyntaxNumber,
    /// Syntax type.
    SyntaxType,
    /// Syntax operator.
    SyntaxOperator,
    /// Syntax punctuation.
    SyntaxPunctuation,
    /// Thinking-off border.
    ThinkingOff,
    /// Thinking-minimal border.
    ThinkingMinimal,
    /// Thinking-low border.
    ThinkingLow,
    /// Thinking-medium border.
    ThinkingMedium,
    /// Thinking-high border.
    ThinkingHigh,
    /// Thinking-xhigh border.
    ThinkingXhigh,
    /// Thinking-max border (falls back to xhigh).
    ThinkingMax,
    /// Bash-mode border.
    BashMode,
}

/// Product background color slots (see reference `ThemeBg`).
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum ThemeBg {
    /// Selected row background.
    SelectedBg,
    /// User message background.
    UserMessageBg,
    /// Custom message background.
    CustomMessageBg,
    /// Pending tool background.
    ToolPendingBg,
    /// Successful tool background.
    ToolSuccessBg,
    /// Errored tool background.
    ToolErrorBg,
}

/// All foreground slots in schema order.
pub const ALL_FG: [ThemeColor; 46] = [
    ThemeColor::Accent,
    ThemeColor::Border,
    ThemeColor::BorderAccent,
    ThemeColor::BorderMuted,
    ThemeColor::Success,
    ThemeColor::Error,
    ThemeColor::Warning,
    ThemeColor::Muted,
    ThemeColor::Dim,
    ThemeColor::Text,
    ThemeColor::ThinkingText,
    ThemeColor::UserMessageText,
    ThemeColor::CustomMessageText,
    ThemeColor::CustomMessageLabel,
    ThemeColor::ToolTitle,
    ThemeColor::ToolOutput,
    ThemeColor::MdHeading,
    ThemeColor::MdLink,
    ThemeColor::MdLinkUrl,
    ThemeColor::MdCode,
    ThemeColor::MdCodeBlock,
    ThemeColor::MdCodeBlockBorder,
    ThemeColor::MdQuote,
    ThemeColor::MdQuoteBorder,
    ThemeColor::MdHr,
    ThemeColor::MdListBullet,
    ThemeColor::ToolDiffAdded,
    ThemeColor::ToolDiffRemoved,
    ThemeColor::ToolDiffContext,
    ThemeColor::SyntaxComment,
    ThemeColor::SyntaxKeyword,
    ThemeColor::SyntaxFunction,
    ThemeColor::SyntaxVariable,
    ThemeColor::SyntaxString,
    ThemeColor::SyntaxNumber,
    ThemeColor::SyntaxType,
    ThemeColor::SyntaxOperator,
    ThemeColor::SyntaxPunctuation,
    ThemeColor::ThinkingOff,
    ThemeColor::ThinkingMinimal,
    ThemeColor::ThinkingLow,
    ThemeColor::ThinkingMedium,
    ThemeColor::ThinkingHigh,
    ThemeColor::ThinkingXhigh,
    ThemeColor::ThinkingMax,
    ThemeColor::BashMode,
];

/// All background slots in schema order.
pub const ALL_BG: [ThemeBg; 6] = [
    ThemeBg::SelectedBg,
    ThemeBg::UserMessageBg,
    ThemeBg::CustomMessageBg,
    ThemeBg::ToolPendingBg,
    ThemeBg::ToolSuccessBg,
    ThemeBg::ToolErrorBg,
];

/// One resolved color: an 8-bit-per-channel RGB triple.
///
/// [`Self::none`] is retained as the reset sentinel for [`ResolvedTheme::from_slots`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Rgb(pub u8, pub u8, pub u8);

impl Rgb {
    /// The reset sentinel used by the compatibility [`ResolvedTheme::from_slots`] constructor.
    #[must_use]
    pub const fn none() -> Self {
        Self(0, 0, 0)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ResolvedColor {
    Default,
    Indexed(u8),
    Rgb(Rgb),
}

impl ResolvedColor {
    const fn rgb(self) -> Rgb {
        match self {
            Self::Default => Rgb::none(),
            Self::Indexed(index) => Rgb(index, index, index),
            Self::Rgb(rgb) => rgb,
        }
    }
}

/// A fully resolved product theme: ANSI emitters for every slot plus mode.
///
/// Cheap to clone via [`Arc`]; the view-model reads it through [`current()`]
/// inside the `fn`-pointer color hooks.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedTheme {
    fg: [ResolvedColor; ALL_FG.len()],
    bg: [ResolvedColor; ALL_BG.len()],
    mode: ColorMode,
    /// Theme display name.
    pub name: Cow<'static, str>,
}

impl ResolvedTheme {
    fn fg_index(color: ThemeColor) -> usize {
        ALL_FG.iter().position(|c| *c == color).unwrap_or(0)
    }

    fn bg_index(bg: ThemeBg) -> usize {
        ALL_BG.iter().position(|b| *b == bg).unwrap_or(0)
    }

    /// Build from resolved per-slot colors.
    #[must_use]
    pub fn from_slots(
        fg: impl IntoIterator<Item = (ThemeColor, Rgb)>,
        bg: impl IntoIterator<Item = (ThemeBg, Rgb)>,
        mode: ColorMode,
        name: impl Into<Cow<'static, str>>,
    ) -> Self {
        Self::from_resolved_slots(
            fg.into_iter().map(|(slot, rgb)| {
                let color = if rgb == Rgb::none() {
                    ResolvedColor::Default
                } else {
                    ResolvedColor::Rgb(rgb)
                };
                (slot, color)
            }),
            bg.into_iter().map(|(slot, rgb)| {
                let color = if rgb == Rgb::none() {
                    ResolvedColor::Default
                } else {
                    ResolvedColor::Rgb(rgb)
                };
                (slot, color)
            }),
            mode,
            name,
        )
    }

    fn from_resolved_slots(
        fg: impl IntoIterator<Item = (ThemeColor, ResolvedColor)>,
        bg: impl IntoIterator<Item = (ThemeBg, ResolvedColor)>,
        mode: ColorMode,
        name: impl Into<Cow<'static, str>>,
    ) -> Self {
        let mut fg_arr = [ResolvedColor::Default; ALL_FG.len()];
        for (color, resolved) in fg {
            fg_arr[Self::fg_index(color)] = resolved;
        }
        let mut bg_arr = [ResolvedColor::Default; ALL_BG.len()];
        for (bg, resolved) in bg {
            bg_arr[Self::bg_index(bg)] = resolved;
        }
        Self {
            fg: fg_arr,
            bg: bg_arr,
            mode,
            name: name.into(),
        }
    }

    /// Active color mode.
    #[must_use]
    pub const fn mode(&self) -> ColorMode {
        self.mode
    }

    /// Clone this theme with a different color [`mode`](Self::mode).
    ///
    /// Slot arrays are `Copy`; only an owned `name` allocates. Byte-equivalent
    /// to re-resolving the source JSON at `mode` (slot values downsample at
    /// emit time), so built-in interns become 256-color without re-parsing.
    fn with_mode(&self, mode: ColorMode) -> Self {
        Self {
            fg: self.fg,
            bg: self.bg,
            mode,
            name: self.name.clone(),
        }
    }

    /// Raw RGB for a foreground slot (empty slots return black; check [`Self::is_fg_empty`]).
    #[must_use]
    pub fn fg_rgb(&self, color: ThemeColor) -> Rgb {
        self.fg[Self::fg_index(color)].rgb()
    }

    /// Raw RGB for a background slot.
    #[must_use]
    pub fn bg_rgb(&self, bg: ThemeBg) -> Rgb {
        self.bg[Self::bg_index(bg)].rgb()
    }

    /// Whether a foreground slot is empty (resets color).
    #[must_use]
    pub fn is_fg_empty(&self, color: ThemeColor) -> bool {
        self.fg[Self::fg_index(color)] == ResolvedColor::Default
    }

    /// Whether a background slot is empty.
    #[must_use]
    pub fn is_bg_empty(&self, bg: ThemeBg) -> bool {
        self.bg[Self::bg_index(bg)] == ResolvedColor::Default
    }

    /// Style `text` with a foreground color, resetting foreground after.
    ///
    /// Mirrors reference `Theme.fg`: `\x1b[38..m{text}\x1b[39m`.
    #[must_use]
    pub fn fg(&self, color: ThemeColor, text: &str) -> String {
        let mut out = String::with_capacity(text.len() + 24);
        self.push_fg(&mut out, self.fg[Self::fg_index(color)]);
        out.push_str(text);
        out.push_str("\x1b[39m");
        out
    }

    /// Style `text` with a background color, resetting background after.
    #[must_use]
    pub fn bg(&self, bg: ThemeBg, text: &str) -> String {
        let mut out = String::with_capacity(text.len() + 24);
        self.push_bg(&mut out, self.bg[Self::bg_index(bg)]);
        out.push_str(text);
        out.push_str("\x1b[49m");
        out
    }

    fn push_fg(&self, out: &mut String, color: ResolvedColor) {
        match color {
            ResolvedColor::Default => out.push_str("\x1b[39m"),
            ResolvedColor::Indexed(index) => {
                out.push_str("\x1b[38;5;");
                out.push_str(index.to_string().as_str());
                out.push('m');
            }
            ResolvedColor::Rgb(rgb) => match self.mode {
                ColorMode::Truecolor => {
                    out.push_str("\x1b[38;2;");
                    push_rgb(out, rgb);
                    out.push('m');
                }
                ColorMode::Palette256 => {
                    out.push_str("\x1b[38;5;");
                    out.push_str(rgb_to_256(rgb).to_string().as_str());
                    out.push('m');
                }
            },
        }
    }

    fn push_bg(&self, out: &mut String, color: ResolvedColor) {
        match color {
            ResolvedColor::Default => out.push_str("\x1b[49m"),
            ResolvedColor::Indexed(index) => {
                out.push_str("\x1b[48;5;");
                out.push_str(index.to_string().as_str());
                out.push('m');
            }
            ResolvedColor::Rgb(rgb) => match self.mode {
                ColorMode::Truecolor => {
                    out.push_str("\x1b[48;2;");
                    push_rgb(out, rgb);
                    out.push('m');
                }
                ColorMode::Palette256 => {
                    out.push_str("\x1b[48;5;");
                    out.push_str(rgb_to_256(rgb).to_string().as_str());
                    out.push('m');
                }
            },
        }
    }

    /// Return the background ANSI prefix string for `bg` (no trailing reset).
    ///
    /// Used to build the `Fn(&str) -> String` background applicators that
    /// pi-tui's `Padded`/`Text` containers accept.
    #[must_use]
    pub fn bg_ansi(&self, bg: ThemeBg) -> String {
        let mut out = String::new();
        self.push_bg(&mut out, self.bg[Self::bg_index(bg)]);
        out
    }

    /// Return the foreground ANSI prefix string for `color` (no trailing reset).
    #[must_use]
    pub fn fg_ansi(&self, color: ThemeColor) -> String {
        let mut out = String::new();
        self.push_fg(&mut out, self.fg[Self::fg_index(color)]);
        out
    }
    /// Resolved value of a foreground slot for wire serialization.
    #[must_use]
    pub fn fg_value(&self, color: ThemeColor) -> ThemeSlotValue {
        ThemeSlotValue::from(self.fg[Self::fg_index(color)])
    }

    /// Resolved value of a background slot for wire serialization.
    #[must_use]
    pub fn bg_value(&self, bg: ThemeBg) -> ThemeSlotValue {
        ThemeSlotValue::from(self.bg[Self::bg_index(bg)])
    }

    /// Build a theme from per-slot wire values (extension `setTheme` object
    /// form). Missing slots stay empty (reset).
    #[must_use]
    pub fn from_value_slots(
        fg: impl IntoIterator<Item = (ThemeColor, ThemeSlotValue)>,
        bg: impl IntoIterator<Item = (ThemeBg, ThemeSlotValue)>,
        mode: ColorMode,
        name: impl Into<Cow<'static, str>>,
    ) -> Self {
        Self::from_resolved_slots(
            fg.into_iter().map(|(slot, value)| (slot, value.into())),
            bg.into_iter().map(|(slot, value)| (slot, value.into())),
            mode,
            name,
        )
    }
}

/// One theme slot value in the JSON / extension wire vocabulary:
/// empty (reset), a 256-color index, or an RGB triple.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ThemeSlotValue {
    /// No color: emits the reset sequence.
    Empty,
    /// 256-color palette index.
    Indexed(u8),
    /// 24-bit color.
    Rgb(Rgb),
}

impl From<ResolvedColor> for ThemeSlotValue {
    fn from(color: ResolvedColor) -> Self {
        match color {
            ResolvedColor::Default => Self::Empty,
            ResolvedColor::Indexed(index) => Self::Indexed(index),
            ResolvedColor::Rgb(rgb) => Self::Rgb(rgb),
        }
    }
}

impl From<ThemeSlotValue> for ResolvedColor {
    fn from(value: ThemeSlotValue) -> Self {
        match value {
            ThemeSlotValue::Empty => Self::Default,
            ThemeSlotValue::Indexed(index) => Self::Indexed(index),
            ThemeSlotValue::Rgb(rgb) => Self::Rgb(rgb),
        }
    }
}

/// Foreground slot enum ↔ JSON slot name table (schema order).
#[must_use]
pub const fn fg_slot_names() -> &'static [(ThemeColor, &'static str)] {
    ALL_FG_SLOTS
}

/// Background slot enum ↔ JSON slot name table (schema order).
#[must_use]
pub const fn bg_slot_names() -> &'static [(ThemeBg, &'static str)] {
    ALL_BG_SLOTS
}

fn push_rgb(out: &mut String, Rgb(r, g, b): Rgb) {
    out.push_str(r.to_string().as_str());
    out.push(';');
    out.push_str(g.to_string().as_str());
    out.push(';');
    out.push_str(b.to_string().as_str());
}

thread_local! {
    /// Current theme read by the `fn`-pointer color hooks. `None` ⇒ dark.
    static CURRENT: RefCell<Option<Arc<ResolvedTheme>>> = const { RefCell::new(None) };
}

/// Install `theme` as the thread-local current theme for the duration of `f`.
///
/// Re-entrant; restores the prior theme on drop. Use this around any render
/// that builds pi-tui themed components.
pub fn with_theme<R>(theme: Arc<ResolvedTheme>, f: impl FnOnce() -> R) -> R {
    let prior = CURRENT.with(|c| c.borrow().clone());
    CURRENT.with(|c| *c.borrow_mut() = Some(theme));
    let r = f();
    CURRENT.with(|c| *c.borrow_mut() = prior);
    r
}

/// Permanently install `theme` as this thread's current theme.
///
/// Used by the runtime at startup and on `/reload`. Tests prefer
/// [`with_theme`] for scoped swaps.
pub fn set_current(theme: Arc<ResolvedTheme>) {
    CURRENT.with(|c| *c.borrow_mut() = Some(theme));
}

/// Clone the current thread-local theme (`None` installed ⇒ built-in dark).
///
/// Returns an [`Arc`] clone (one atomic op). Called by the `fn`-pointer hooks.
#[must_use]
pub fn current() -> Arc<ResolvedTheme> {
    CURRENT.with(|c| c.borrow().clone()).unwrap_or_else(dark)
}

/// Build a foreground `fn(&str) -> String` for `color` that resolves against
/// [`current()`] at call time.
///
/// pi-tui theme fields are plain `fn` pointers that cannot capture a runtime
/// color, so each variant dispatches through the thread-local.
#[must_use]
pub fn make_fg(color: ThemeColor) -> fn(&str) -> String {
    match color {
        ThemeColor::Accent => |s| current().fg(ThemeColor::Accent, s),
        ThemeColor::Border => |s| current().fg(ThemeColor::Border, s),
        ThemeColor::BorderAccent => |s| current().fg(ThemeColor::BorderAccent, s),
        ThemeColor::BorderMuted => |s| current().fg(ThemeColor::BorderMuted, s),
        ThemeColor::Success => |s| current().fg(ThemeColor::Success, s),
        ThemeColor::Error => |s| current().fg(ThemeColor::Error, s),
        ThemeColor::Warning => |s| current().fg(ThemeColor::Warning, s),
        ThemeColor::Muted => |s| current().fg(ThemeColor::Muted, s),
        ThemeColor::Dim => |s| current().fg(ThemeColor::Dim, s),
        ThemeColor::Text => |s| current().fg(ThemeColor::Text, s),
        ThemeColor::ThinkingText => |s| current().fg(ThemeColor::ThinkingText, s),
        ThemeColor::UserMessageText => |s| current().fg(ThemeColor::UserMessageText, s),
        ThemeColor::CustomMessageText => |s| current().fg(ThemeColor::CustomMessageText, s),
        ThemeColor::CustomMessageLabel => |s| current().fg(ThemeColor::CustomMessageLabel, s),
        ThemeColor::ToolTitle => |s| current().fg(ThemeColor::ToolTitle, s),
        ThemeColor::ToolOutput => |s| current().fg(ThemeColor::ToolOutput, s),
        ThemeColor::MdHeading => |s| current().fg(ThemeColor::MdHeading, s),
        ThemeColor::MdLink => |s| current().fg(ThemeColor::MdLink, s),
        ThemeColor::MdLinkUrl => |s| current().fg(ThemeColor::MdLinkUrl, s),
        ThemeColor::MdCode => |s| current().fg(ThemeColor::MdCode, s),
        ThemeColor::MdCodeBlock => |s| current().fg(ThemeColor::MdCodeBlock, s),
        ThemeColor::MdCodeBlockBorder => |s| current().fg(ThemeColor::MdCodeBlockBorder, s),
        ThemeColor::MdQuote => |s| current().fg(ThemeColor::MdQuote, s),
        ThemeColor::MdQuoteBorder => |s| current().fg(ThemeColor::MdQuoteBorder, s),
        ThemeColor::MdHr => |s| current().fg(ThemeColor::MdHr, s),
        ThemeColor::MdListBullet => |s| current().fg(ThemeColor::MdListBullet, s),
        ThemeColor::ToolDiffAdded => |s| current().fg(ThemeColor::ToolDiffAdded, s),
        ThemeColor::ToolDiffRemoved => |s| current().fg(ThemeColor::ToolDiffRemoved, s),
        ThemeColor::ToolDiffContext => |s| current().fg(ThemeColor::ToolDiffContext, s),
        ThemeColor::SyntaxComment => |s| current().fg(ThemeColor::SyntaxComment, s),
        ThemeColor::SyntaxKeyword => |s| current().fg(ThemeColor::SyntaxKeyword, s),
        ThemeColor::SyntaxFunction => |s| current().fg(ThemeColor::SyntaxFunction, s),
        ThemeColor::SyntaxVariable => |s| current().fg(ThemeColor::SyntaxVariable, s),
        ThemeColor::SyntaxString => |s| current().fg(ThemeColor::SyntaxString, s),
        ThemeColor::SyntaxNumber => |s| current().fg(ThemeColor::SyntaxNumber, s),
        ThemeColor::SyntaxType => |s| current().fg(ThemeColor::SyntaxType, s),
        ThemeColor::SyntaxOperator => |s| current().fg(ThemeColor::SyntaxOperator, s),
        ThemeColor::SyntaxPunctuation => |s| current().fg(ThemeColor::SyntaxPunctuation, s),
        ThemeColor::ThinkingOff => |s| current().fg(ThemeColor::ThinkingOff, s),
        ThemeColor::ThinkingMinimal => |s| current().fg(ThemeColor::ThinkingMinimal, s),
        ThemeColor::ThinkingLow => |s| current().fg(ThemeColor::ThinkingLow, s),
        ThemeColor::ThinkingMedium => |s| current().fg(ThemeColor::ThinkingMedium, s),
        ThemeColor::ThinkingHigh => |s| current().fg(ThemeColor::ThinkingHigh, s),
        ThemeColor::ThinkingXhigh => |s| current().fg(ThemeColor::ThinkingXhigh, s),
        ThemeColor::ThinkingMax => |s| current().fg(ThemeColor::ThinkingMax, s),
        ThemeColor::BashMode => |s| current().fg(ThemeColor::BashMode, s),
    }
}

/// Shared highlighter handle: `markdown_theme()` runs once per painted frame,
/// so the `Arc<dyn Fn>` wrapper is built once and refcount-bumped per frame.
static HIGHLIGHT_CODE: LazyLock<pi_tui::components::HighlightCodeFn> =
    LazyLock::new(|| Arc::new(highlight_code));

/// Build a markdown theme from [`current()`]. Mirrors `getMarkdownTheme()`.
#[must_use]
pub fn markdown_theme() -> MarkdownTheme {
    MarkdownTheme {
        heading: make_fg(ThemeColor::MdHeading),
        link: make_fg(ThemeColor::MdLink),
        link_url: make_fg(ThemeColor::MdLinkUrl),
        code: make_fg(ThemeColor::MdCode),
        code_block: make_fg(ThemeColor::MdCodeBlock),
        code_block_border: make_fg(ThemeColor::MdCodeBlockBorder),
        quote: make_fg(ThemeColor::MdQuote),
        quote_border: make_fg(ThemeColor::MdQuoteBorder),
        hr: make_fg(ThemeColor::MdHr),
        list_bullet: make_fg(ThemeColor::MdListBullet),
        bold,
        italic,
        underline,
        strikethrough,
        highlight_code: Some(HIGHLIGHT_CODE.clone()),
        code_block_indent: "  ".to_owned(),
    }
}

/// Curated `.sublime-syntax` grammars embedded at compile time. Provenance,
/// upstream pins, and the hand-authored stubs are documented in
/// `assets/syntax/NOTICE`.
const SYNTAX_SOURCES: &[(&str, &str)] = &[
    (
        "Plain-Text",
        include_str!("../../../assets/syntax/Plain-Text.sublime-syntax"),
    ),
    (
        "Bash",
        include_str!("../../../assets/syntax/Bash.sublime-syntax"),
    ),
    ("C", include_str!("../../../assets/syntax/C.sublime-syntax")),
    (
        "C++",
        include_str!("../../../assets/syntax/C++.sublime-syntax"),
    ),
    (
        "CSS",
        include_str!("../../../assets/syntax/CSS.sublime-syntax"),
    ),
    (
        "Diff",
        include_str!("../../../assets/syntax/Diff.sublime-syntax"),
    ),
    (
        "Go",
        include_str!("../../../assets/syntax/Go.sublime-syntax"),
    ),
    (
        "HTML",
        include_str!("../../../assets/syntax/HTML.sublime-syntax"),
    ),
    (
        "JSON",
        include_str!("../../../assets/syntax/JSON.sublime-syntax"),
    ),
    (
        "Java",
        include_str!("../../../assets/syntax/Java.sublime-syntax"),
    ),
    (
        "JavaScript",
        include_str!("../../../assets/syntax/JavaScript.sublime-syntax"),
    ),
    (
        "JavaScript-RegExp",
        include_str!("../../../assets/syntax/JavaScript-RegExp.sublime-syntax"),
    ),
    (
        "Python",
        include_str!("../../../assets/syntax/Python.sublime-syntax"),
    ),
    (
        "Python-RegExp",
        include_str!("../../../assets/syntax/Python-RegExp.sublime-syntax"),
    ),
    (
        "Python-RegExp-RawFString",
        include_str!("../../../assets/syntax/Python-RegExp-RawFString.sublime-syntax"),
    ),
    (
        "Rust",
        include_str!("../../../assets/syntax/Rust.sublime-syntax"),
    ),
    (
        "SQL",
        include_str!("../../../assets/syntax/SQL.sublime-syntax"),
    ),
    (
        "Shell-Unix-Generic",
        include_str!("../../../assets/syntax/Shell-Unix-Generic.sublime-syntax"),
    ),
    (
        "TOML",
        include_str!("../../../assets/syntax/TOML.sublime-syntax"),
    ),
    (
        "YAML",
        include_str!("../../../assets/syntax/YAML.sublime-syntax"),
    ),
];

/// Build a syntax set from the named vendored grammars. `Plain-Text` should
/// always be included: it is syntect's fallback target for unresolved `embed`
/// references.
// Panic rationale: vendored grammars are compile-time constants; a parse
// failure is a build-time bug the syntax tests catch, never a runtime state.
#[expect(clippy::panic)]
fn build_syntax_set(names: &[&str]) -> SyntaxSet {
    let mut builder = SyntaxSetBuilder::new();
    for (name, source) in SYNTAX_SOURCES {
        if names.contains(name) {
            let definition = SyntaxDefinition::load_from_str(source, true, Some(name))
                .unwrap_or_else(|error| panic!("vendored syntax {name} should parse: {error}"));
            builder.add(definition);
        }
    }
    builder.build()
}

/// Per-language syntax-set shards, each parsed lazily on first use. Sharding
/// keeps the first highlighted code block cheap (a `rust` fence never pays
/// for CSS or C++) and startup paths (including `--version`) never touch any
/// shard. Each shard carries the grammars reachable from its root language:
/// cross-grammar `include`s must be present, while `embed`s degrade to the
/// `Plain-Text` fallback when their target is not.
macro_rules! shard {
    ($name:ident, $($source:literal),+ $(,)?) => {
        static $name: LazyLock<SyntaxSet> = LazyLock::new(|| build_syntax_set(&[$($source),+]));
    };
}

shard!(SHARD_BASH, "Plain-Text", "Bash", "Shell-Unix-Generic");
shard!(SHARD_C, "Plain-Text", "C");
shard!(SHARD_CPP, "Plain-Text", "C", "C++");
shard!(SHARD_CSS, "Plain-Text", "CSS");
shard!(SHARD_DIFF, "Plain-Text", "Diff");
shard!(SHARD_GO, "Plain-Text", "Go");
shard!(
    SHARD_HTML,
    "Plain-Text",
    "HTML",
    "CSS",
    "JavaScript",
    "JavaScript-RegExp",
);
shard!(SHARD_JSON, "Plain-Text", "JSON");
shard!(SHARD_JAVA, "Plain-Text", "Java");
shard!(SHARD_JS, "Plain-Text", "JavaScript", "JavaScript-RegExp",);
shard!(
    SHARD_PYTHON,
    "Plain-Text",
    "Python",
    "Python-RegExp",
    "Python-RegExp-RawFString",
    "SQL",
);
shard!(SHARD_RUST, "Plain-Text", "Rust", "TOML");
shard!(SHARD_SQL, "Plain-Text", "SQL");
shard!(SHARD_TOML, "Plain-Text", "TOML");
shard!(SHARD_YAML, "Plain-Text", "YAML");

/// Every shard, for tests that validate the whole vendored set.
#[cfg(test)]
const ALL_SHARDS: &[&LazyLock<SyntaxSet>] = &[
    &SHARD_BASH,
    &SHARD_C,
    &SHARD_CPP,
    &SHARD_CSS,
    &SHARD_DIFF,
    &SHARD_GO,
    &SHARD_HTML,
    &SHARD_JSON,
    &SHARD_JAVA,
    &SHARD_JS,
    &SHARD_PYTHON,
    &SHARD_RUST,
    &SHARD_SQL,
    &SHARD_TOML,
    &SHARD_YAML,
];

/// Scope selectors mapped onto the product syntax slots, checked against the
/// scope stack in order; first match wins. Ports the hljs-class-to-syntax-slot
/// table in upstream `buildCliHighlightTheme` to `TextMate` scopes.
// Panic rationale: selector strings are compile-time constants, validated by
// the syntax tests on first use.
#[expect(clippy::panic)]
static SLOT_SELECTORS: LazyLock<Vec<(ScopeSelector, ThemeColor)>> = LazyLock::new(|| {
    const TABLE: &[(&str, ThemeColor)] = &[
        ("comment", ThemeColor::SyntaxComment),
        ("string", ThemeColor::SyntaxString),
        ("constant.character", ThemeColor::SyntaxString),
        ("constant.numeric", ThemeColor::SyntaxNumber),
        ("constant.language", ThemeColor::SyntaxNumber),
        ("keyword.operator", ThemeColor::SyntaxOperator),
        ("keyword", ThemeColor::SyntaxKeyword),
        ("storage", ThemeColor::SyntaxKeyword),
        ("entity.name.tag", ThemeColor::SyntaxKeyword),
        ("entity.name.function", ThemeColor::SyntaxFunction),
        ("support.function", ThemeColor::SyntaxFunction),
        ("support.macro", ThemeColor::SyntaxFunction),
        ("variable.function", ThemeColor::SyntaxFunction),
        ("entity.name.type", ThemeColor::SyntaxType),
        ("entity.name.class", ThemeColor::SyntaxType),
        ("entity.name.struct", ThemeColor::SyntaxType),
        ("entity.name.enum", ThemeColor::SyntaxType),
        ("entity.name.trait", ThemeColor::SyntaxType),
        ("entity.name.interface", ThemeColor::SyntaxType),
        ("entity.name.impl", ThemeColor::SyntaxType),
        ("support.type", ThemeColor::SyntaxType),
        ("support.class", ThemeColor::SyntaxType),
        ("markup.inserted", ThemeColor::ToolDiffAdded),
        ("markup.deleted", ThemeColor::ToolDiffRemoved),
        ("variable", ThemeColor::SyntaxVariable),
        ("entity.name.variable", ThemeColor::SyntaxVariable),
        ("entity.other.attribute-name", ThemeColor::SyntaxVariable),
        ("support.constant", ThemeColor::SyntaxVariable),
        ("punctuation", ThemeColor::SyntaxPunctuation),
    ];
    TABLE
        .iter()
        .map(|(selector, slot)| {
            (
                ScopeSelector::from_str(selector).unwrap_or_else(|error| {
                    panic!("slot selector {selector} should parse: {error}")
                }),
                *slot,
            )
        })
        .collect()
});

/// Map a code-fence language token onto its vendored syntax token and shard.
///
/// Mirrors the language names upstream's `highlightCode` accepts via hljs,
/// reduced to the vendored grammar set. `ts`/`tsx` map to JavaScript: the
/// upstream TypeScript grammar cannot be loaded by syntect (see
/// `assets/syntax/NOTICE`). Unmapped languages return `None` (plain text —
/// auto-detection stays disabled, matching upstream).
fn syntax_for(lang: &str) -> Option<(&'static str, &'static LazyLock<SyntaxSet>)> {
    Some(match lang.to_ascii_lowercase().as_str() {
        "js" | "jsx" | "mjs" | "cjs" | "javascript" | "node" | "ts" | "tsx" | "mts" | "cts"
        | "typescript" => ("js", &SHARD_JS),
        "py" | "python" | "python3" => ("py", &SHARD_PYTHON),
        "rs" | "rust" => ("rs", &SHARD_RUST),
        "go" | "golang" => ("go", &SHARD_GO),
        "c" | "h" => ("c", &SHARD_C),
        "cpp" | "cc" | "cxx" | "c++" | "hpp" | "hxx" | "hh" => ("cpp", &SHARD_CPP),
        "java" => ("java", &SHARD_JAVA),
        "sh" | "bash" | "zsh" | "shell" | "shellscript" => ("sh", &SHARD_BASH),
        "json" | "jsonc" => ("json", &SHARD_JSON),
        "yaml" | "yml" => ("yaml", &SHARD_YAML),
        "toml" => ("toml", &SHARD_TOML),
        "html" | "htm" => ("html", &SHARD_HTML),
        "css" => ("css", &SHARD_CSS),
        "sql" => ("sql", &SHARD_SQL),
        "diff" | "patch" => ("diff", &SHARD_DIFF),
        _ => return None,
    })
}

/// Language token for a file path's extension (ports `getLanguageFromPath`).
///
/// Returns the token the markdown highlighter understands, or `None` when no
/// vendored grammar covers the extension. Extension aliases match upstream's
/// table within the vendored set (`ts`/`tsx` yield the JavaScript grammar).
#[must_use]
pub fn language_from_path(path: &str) -> Option<&'static str> {
    path.rsplit('.')
        .next()
        .and_then(|ext| syntax_for(ext).map(|(token, _)| token))
}

/// Highlight `code` as `lang`, one styled [`String`] per line.
///
/// Ports upstream `highlightCode`: only known languages are highlighted
/// (auto-detection stays disabled); unknown languages and parse failures
/// degrade to `mdCodeBlock`-colored plain lines. Colors resolve through
/// [`current()`] at call time, so a theme switch needs no cache invalidation.
fn highlight_code(code: &str, lang: Option<&str>) -> Vec<String> {
    let theme = current();
    let Some((token, shard)) = lang.and_then(|lang| syntax_for(lang.trim())) else {
        return plain_lines(&theme, code);
    };
    let syntax_set = &**shard;
    let Some(syntax) = syntax_set.find_syntax_by_token(token) else {
        return plain_lines(&theme, code);
    };
    let mut state = ParseState::new(syntax);
    let mut stack = ScopeStack::new();
    let mut lines = Vec::new();
    for line in LinesWithEndings::from(code) {
        match state.parse_line(line, syntax_set) {
            Ok(ops) => lines.push(highlight_line(&theme, line, &ops, &mut stack)),
            Err(_) => return plain_lines(&theme, code),
        }
    }
    lines
}

/// `mdCodeBlock`-colored plain lines for the no-highlight path, matching the
/// renderer's trailing-newline handling (no synthetic empty final line).
fn plain_lines(theme: &ResolvedTheme, code: &str) -> Vec<String> {
    if code.is_empty() {
        return Vec::new();
    }
    code.strip_suffix('\n')
        .unwrap_or(code)
        .split('\n')
        .map(|line| theme.fg(ThemeColor::MdCodeBlock, line))
        .collect()
}

/// Emit one line with tokens wrapped in the active theme's syntax colors.
/// `line` may end with `\n`; the newline is neither colored nor emitted.
fn highlight_line(
    theme: &ResolvedTheme,
    line: &str,
    ops: &[(usize, ScopeStackOp)],
    stack: &mut ScopeStack,
) -> String {
    let body = line.strip_suffix('\n').unwrap_or(line);
    let mut out = String::with_capacity(body.len() + 32);
    let mut offset = 0;
    for &(end, ref op) in ops {
        push_segment(&mut out, theme, stack, &line[offset..end.min(body.len())]);
        // Parser-produced ops are well-formed for this stack.
        let _ = stack.apply(op);
        offset = end;
    }
    push_segment(
        &mut out,
        theme,
        stack,
        &line[offset..body.len().max(offset)],
    );
    out
}

/// Append `segment` colored by its scope-stack slot (raw when unstyled).
fn push_segment(out: &mut String, theme: &ResolvedTheme, stack: &ScopeStack, segment: &str) {
    if segment.is_empty() {
        return;
    }
    match slot_for_stack(stack) {
        Some(slot) if !theme.is_fg_empty(slot) => out.push_str(&theme.fg(slot, segment)),
        _ => out.push_str(segment),
    }
}

/// First syntax slot whose selector matches the scope stack, if any.
fn slot_for_stack(stack: &ScopeStack) -> Option<ThemeColor> {
    SLOT_SELECTORS
        .iter()
        .find_map(|(selector, slot)| selector.does_match(stack.as_slice()).map(|_| *slot))
}

/// Markdown options matching the user-message renderer.
#[must_use]
pub fn user_markdown_options() -> MarkdownOptions {
    MarkdownOptions {
        preserve_ordered_list_markers: true,
        preserve_backslash_escapes: true,
        hyperlinks: false,
    }
}

/// Select-list theme from [`current()`]. Mirrors `getSelectListTheme()`.
#[must_use]
pub fn select_list_theme() -> SelectListTheme {
    SelectListTheme {
        selected_prefix: make_fg(ThemeColor::Accent),
        selected_text: make_fg(ThemeColor::Accent),
        description: make_fg(ThemeColor::Muted),
        scroll_info: make_fg(ThemeColor::Muted),
        no_match: make_fg(ThemeColor::Muted),
    }
}

/// Settings-list theme from [`current()`]. Mirrors `getSettingsListTheme()`.
///
/// `cursor` is resolved once (it is a `String`, not a hook).
#[must_use]
pub fn settings_list_theme() -> SettingsListTheme {
    SettingsListTheme {
        label: label_selected,
        value: value_selected,
        description: make_fg(ThemeColor::Dim),
        cursor: current().fg(ThemeColor::Accent, "→ "),
        hint: make_fg(ThemeColor::Dim),
    }
}

fn label_selected(s: &str, selected: bool) -> String {
    if selected {
        current().fg(ThemeColor::Accent, s)
    } else {
        s.to_owned()
    }
}

fn value_selected(s: &str, selected: bool) -> String {
    if selected {
        current().fg(ThemeColor::Accent, s)
    } else {
        current().fg(ThemeColor::Muted, s)
    }
}

/// Bold text (`\x1b[1m…\x1b[22m`).
#[must_use]
pub fn bold(s: &str) -> String {
    format!("\x1b[1m{s}\x1b[22m")
}

/// Italic text.
#[must_use]
pub fn italic(s: &str) -> String {
    format!("\x1b[3m{s}\x1b[23m")
}

/// Underline text.
#[must_use]
pub fn underline(s: &str) -> String {
    format!("\x1b[4m{s}\x1b[24m")
}

/// Strikethrough text.
#[must_use]
pub fn strikethrough(s: &str) -> String {
    format!("\x1b[9m{s}\x1b[29m")
}

/// Inverse video.
#[must_use]
pub fn inverse(s: &str) -> String {
    format!("\x1b[7m{s}\x1b[27m")
}

/// Default text style for assistant body markdown (no decoration).
#[must_use]
pub fn default_text_style() -> DefaultTextStyle {
    DefaultTextStyle::default()
}

/// Helper to truncate a single line to `width` with an ellipsis.
#[must_use]
pub fn truncate_line(text: &str, width: usize, ellipsis: &str) -> String {
    if width == 0 {
        return String::new();
    }
    if visible_width(text) <= width {
        return text.to_owned();
    }
    truncate_to_width(text, width, ellipsis, false)
}

const CUBE_VALUES: [u8; 6] = [0, 95, 135, 175, 215, 255];

fn closest_cube(value: u8) -> usize {
    let mut best = 0usize;
    let mut best_dist = u32::MAX;
    for (i, c) in CUBE_VALUES.iter().enumerate() {
        let d = (i32::from(value) - i32::from(*c)).unsigned_abs();
        if d < best_dist {
            best_dist = d;
            best = i;
        }
    }
    best
}

fn color_distance(r1: u8, g1: u8, b1: u8, r2: u8, g2: u8, b2: u8) -> u32 {
    let dr = i32::from(r1) - i32::from(r2);
    let dg = i32::from(g1) - i32::from(g2);
    let db = i32::from(b1) - i32::from(b2);
    let weighted = (dr * dr * 299 + dg * dg * 587 + db * db * 114) / 1000;
    weighted.try_into().unwrap_or(u32::MAX)
}

/// Map an RGB triple to the nearest 256-color palette index.
#[must_use]
pub fn rgb_to_256(rgb: Rgb) -> u8 {
    let Rgb(r, g, b) = rgb;
    let r_idx = closest_cube(r);
    let g_idx = closest_cube(g);
    let b_idx = closest_cube(b);
    let cube_r = CUBE_VALUES[r_idx];
    let cube_g = CUBE_VALUES[g_idx];
    let cube_b = CUBE_VALUES[b_idx];
    let cube_index = 16 + 36 * r_idx + 6 * g_idx + b_idx;
    let cube_dist = color_distance(r, g, b, cube_r, cube_g, cube_b);

    let gray = u8::try_from((u32::from(r) * 299 + u32::from(g) * 587 + u32::from(b) * 114) / 1000)
        .unwrap_or(255);
    let gray_idx = closest_gray(gray);
    let gray_value = GRAY_VALUES[gray_idx];
    let gray_index = 232 + gray_idx;
    let gray_dist = color_distance(r, g, b, gray_value, gray_value, gray_value);

    let max_c = r.max(g).max(b);
    let min_c = r.min(g).min(b);
    let spread = u32::from(max_c) - u32::from(min_c);

    if spread < 10 && gray_dist < cube_dist {
        u8::try_from(gray_index).unwrap_or(255)
    } else {
        u8::try_from(cube_index).unwrap_or(255)
    }
}

const GRAY_VALUES: [u8; 24] = gray_values();
const fn gray_values() -> [u8; 24] {
    let mut out = [0u8; 24];
    let mut i = 0u8;
    while i < 24 {
        // i ∈ 0..24 ⇒ 8 + i*10 ∈ 8..=238, always fits in u8.
        out[i as usize] = 8 + i * 10;
        i += 1;
    }
    out
}

fn closest_gray(gray: u8) -> usize {
    let mut best = 0usize;
    let mut best_dist = u32::MAX;
    for (i, v) in GRAY_VALUES.iter().enumerate() {
        let d = (i32::from(gray) - i32::from(*v)).unsigned_abs();
        if d < best_dist {
            best_dist = d;
            best = i;
        }
    }
    best
}

/// Built-in dark theme (interned).
#[must_use]
pub fn dark() -> Arc<ResolvedTheme> {
    DARK_INTERN.clone()
}

/// Built-in light theme (interned).
#[must_use]
pub fn light() -> Arc<ResolvedTheme> {
    LIGHT_INTERN.clone()
}

macro_rules! built_in_theme {
    ($static_name:ident, $file:literal) => {
        static $static_name: LazyLock<Arc<ResolvedTheme>> = LazyLock::new(|| {
            ThemeJson::parse(include_str!(concat!(
                "../../../assets/theme/",
                $file,
                ".json"
            )))
            .and_then(|theme| theme.resolve(ColorMode::Truecolor))
            .expect("built-in theme JSON is valid")
        });
    };
}

built_in_theme!(DARK_INTERN, "dark");
built_in_theme!(LIGHT_INTERN, "light");
built_in_theme!(CLASSIC_DARK_INTERN, "classic-dark");
built_in_theme!(CLASSIC_LIGHT_INTERN, "classic-light");
built_in_theme!(MOTION_DARK_INTERN, "motion-dark");
built_in_theme!(MOTION_LIGHT_INTERN, "motion-light");
built_in_theme!(M3_DARK_INTERN, "m3-dark");
built_in_theme!(M3_LIGHT_INTERN, "m3-light");
built_in_theme!(ANTD_DARK_INTERN, "antd-dark");
built_in_theme!(ANTD_LIGHT_INTERN, "antd-light");

/// Errors produced while loading or validating a theme JSON document.
#[derive(Debug, thiserror::Error)]
pub enum ThemeError {
    /// A required color slot was missing.
    #[error("missing required color slot: {0}")]
    MissingColor(String),
    /// A color value was neither a hex string, a variable name, nor empty.
    #[error("invalid color value for `{slot}`: {value}")]
    InvalidColor {
        /// Slot name.
        slot: String,
        /// Raw value that failed to parse.
        value: String,
    },
    /// A variable referenced an undefined name.
    #[error("unknown theme variable reference: {0}")]
    UnknownVar(String),
    /// A variable chain was cyclic.
    #[error("circular theme variable reference: {0}")]
    CircularVar(String),
    /// `vars`/`colors` map held a non-string/non-number color.
    #[error("theme `{field}` must be a string or integer")]
    InvalidFieldType {
        /// `vars` or `colors`.
        field: &'static str,
    },
    /// The top-level document was not a JSON object.
    #[error("theme json must be an object")]
    NotAnObject,
    /// The theme `name` was missing.
    #[error("theme json missing required field: name")]
    MissingName,
    /// The theme name contained a `/`.
    #[error("invalid theme name \"{0}\": names cannot contain '/'")]
    InvalidName(String),
    /// The named theme file was not found in any theme directory.
    #[error("theme not found: {0}")]
    NotFound(String),
    /// I/O error reading the file.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    /// JSON parse error.
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
}

/// Parsed (not yet resolved) theme document.
#[derive(Clone, Debug)]
pub struct ThemeJson {
    name: String,
    vars: Vec<(String, ColorValue)>,
    colors: Vec<(String, ColorValue)>,
}

#[derive(Clone, Debug)]
enum ColorValue {
    Hex(String),
    Var(String),
    Empty,
    Indexed(u8),
}

impl ThemeJson {
    /// Parse a theme document from JSON text.
    ///
    /// # Errors
    ///
    /// Returns [`ThemeError`] when the document is malformed.
    pub fn parse(json: &str) -> Result<Self, ThemeError> {
        let value: serde_json::Value = serde_json::from_str(json)?;
        Self::from_value(&value)
    }

    /// Parse from an already-deserialized JSON value.
    ///
    /// # Errors
    ///
    /// Returns [`ThemeError`] when the value is malformed.
    pub fn from_value(value: &serde_json::Value) -> Result<Self, ThemeError> {
        let obj = value.as_object().ok_or(ThemeError::NotAnObject)?;
        let name = obj
            .get("name")
            .and_then(serde_json::Value::as_str)
            .ok_or(ThemeError::MissingName)?
            .to_owned();
        if name.contains('/') {
            return Err(ThemeError::InvalidName(name));
        }
        let vars = parse_color_map(obj.get("vars"), "vars")?;
        let colors_obj = obj
            .get("colors")
            .and_then(|v| v.as_object())
            .ok_or_else(|| ThemeError::MissingColor("colors".to_owned()))?;
        let mut colors = Vec::new();
        for slot in REQUIRED_COLORS {
            let val = colors_obj
                .get(*slot)
                .ok_or_else(|| ThemeError::MissingColor((*slot).to_owned()))?;
            let cv = parse_color_value(val).ok_or_else(|| ThemeError::InvalidColor {
                slot: (*slot).to_owned(),
                value: val.to_string(),
            })?;
            colors.push(((*slot).to_owned(), cv));
        }
        // thinkingMax is optional → falls back to thinkingXhigh.
        if let Some(value) = colors_obj.get("thinkingMax") {
            let color = parse_color_value(value).ok_or_else(|| ThemeError::InvalidColor {
                slot: "thinkingMax".to_owned(),
                value: value.to_string(),
            })?;
            colors.push(("thinkingMax".to_owned(), color));
        } else if let Some((_, x)) = colors.iter().find(|(k, _)| k == "thinkingXhigh") {
            colors.push(("thinkingMax".to_owned(), x.clone()));
        }
        Ok(Self { name, vars, colors })
    }

    /// Theme display name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Resolve into an interned [`Arc<ResolvedTheme>`].
    ///
    /// # Errors
    ///
    /// Returns [`ThemeError`] on unknown/circular variable references or bad hex.
    pub fn resolve(&self, mode: ColorMode) -> Result<Arc<ResolvedTheme>, ThemeError> {
        Ok(Arc::new(self.resolve_owned(mode)?))
    }

    /// Resolve into an owned [`ResolvedTheme`].
    ///
    /// # Errors
    ///
    /// Returns [`ThemeError`] on resolution failure.
    pub fn resolve_owned(&self, mode: ColorMode) -> Result<ResolvedTheme, ThemeError> {
        let mut fg = [(ThemeColor::Accent, ResolvedColor::Default); ALL_FG.len()];
        for (i, (slot_enum, slot_name)) in ALL_FG_SLOTS.iter().enumerate() {
            let value = self
                .colors
                .iter()
                .find(|(k, _)| k == *slot_name)
                .map(|(_, v)| v.clone())
                .ok_or_else(|| ThemeError::MissingColor((*slot_name).to_owned()))?;
            let resolved = resolve_value(&value, &self.vars, slot_name)?;
            fg[i] = (*slot_enum, resolved);
        }
        let mut bg = [(ThemeBg::SelectedBg, ResolvedColor::Default); ALL_BG.len()];
        for (i, (slot_enum, slot_name)) in ALL_BG_SLOTS.iter().enumerate() {
            let value = self
                .colors
                .iter()
                .find(|(k, _)| k == *slot_name)
                .map(|(_, v)| v.clone())
                .ok_or_else(|| ThemeError::MissingColor((*slot_name).to_owned()))?;
            let resolved = resolve_value(&value, &self.vars, slot_name)?;
            bg[i] = (*slot_enum, resolved);
        }
        Ok(ResolvedTheme::from_resolved_slots(
            fg,
            bg,
            mode,
            self.name.clone(),
        ))
    }
}

fn parse_color_map(
    value: Option<&serde_json::Value>,
    field: &'static str,
) -> Result<Vec<(String, ColorValue)>, ThemeError> {
    let Some(obj) = value.and_then(|v| v.as_object()) else {
        return Ok(Vec::new());
    };
    let mut out = Vec::with_capacity(obj.len());
    for (k, v) in obj {
        let cv = parse_color_value(v).ok_or(ThemeError::InvalidFieldType { field })?;
        out.push((k.clone(), cv));
    }
    Ok(out)
}

fn parse_color_value(v: &serde_json::Value) -> Option<ColorValue> {
    if let Some(s) = v.as_str() {
        if s.is_empty() {
            return Some(ColorValue::Empty);
        }
        if let Some(rest) = s.strip_prefix('#') {
            if rest.len() == 6 && rest.chars().all(|c| c.is_ascii_hexdigit()) {
                return Some(ColorValue::Hex(s.to_owned()));
            }
            return None;
        }
        return Some(ColorValue::Var(s.to_owned()));
    }
    if let Some(n) = v.as_i64().filter(|n| (0..=255).contains(n)) {
        return Some(ColorValue::Indexed(u8::try_from(n).unwrap_or(0)));
    }
    None
}

fn resolve_value(
    value: &ColorValue,
    vars: &[(String, ColorValue)],
    slot: &str,
) -> Result<ResolvedColor, ThemeError> {
    let mut visited: Vec<String> = Vec::new();
    let mut current = value.clone();
    loop {
        match current {
            ColorValue::Empty => return Ok(ResolvedColor::Default),
            ColorValue::Hex(s) => {
                return hex_to_rgb(&s).map(ResolvedColor::Rgb).ok_or_else(|| {
                    ThemeError::InvalidColor {
                        slot: slot.to_owned(),
                        value: s.clone(),
                    }
                });
            }
            ColorValue::Indexed(index) => return Ok(ResolvedColor::Indexed(index)),
            ColorValue::Var(name) => {
                if visited.iter().any(|v| v == &name) {
                    return Err(ThemeError::CircularVar(name));
                }
                visited.push(name.clone());
                let Some((_, next)) = vars.iter().find(|(k, _)| k == &name) else {
                    return Err(ThemeError::UnknownVar(name));
                };
                current = next.clone();
            }
        }
    }
}

fn built_in_theme(name: &str, mode: ColorMode) -> Option<Arc<ResolvedTheme>> {
    let intern = match name {
        "dark" => dark(),
        "light" => light(),
        "classic-dark" => CLASSIC_DARK_INTERN.clone(),
        "classic-light" => CLASSIC_LIGHT_INTERN.clone(),
        "motion-dark" => MOTION_DARK_INTERN.clone(),
        "motion-light" => MOTION_LIGHT_INTERN.clone(),
        "m3-dark" => M3_DARK_INTERN.clone(),
        "m3-light" => M3_LIGHT_INTERN.clone(),
        "antd-dark" => ANTD_DARK_INTERN.clone(),
        "antd-light" => ANTD_LIGHT_INTERN.clone(),
        _ => return None,
    };
    // Interns are the canonical Truecolor authority; hand back a downsampled
    // 256-color clone only when the terminal lacks 24-bit support.
    Some(match mode {
        ColorMode::Truecolor => intern,
        ColorMode::Palette256 => Arc::new(intern.with_mode(ColorMode::Palette256)),
    })
}

/// Other-variant theme name for light/dark switching; `None` when the name has no convention pair.
#[must_use]
pub fn paired_name(name: &str, want_dark: bool) -> Option<String> {
    match name {
        "dark" | "light" => Some(if want_dark { "dark" } else { "light" }.to_owned()),
        _ => {
            let stem = name
                .strip_suffix("-dark")
                .or_else(|| name.strip_suffix("-light"))?;
            Some(format!(
                "{stem}-{}",
                if want_dark { "dark" } else { "light" }
            ))
        }
    }
}

/// Parse `#rrggbb` into an [`Rgb`] (wire deserialization helper).
#[must_use]
pub fn parse_hex_color(hex: &str) -> Option<Rgb> {
    hex_to_rgb(hex)
}

fn hex_to_rgb(hex: &str) -> Option<Rgb> {
    let rest = hex.strip_prefix('#')?;
    if rest.len() != 6 {
        return None;
    }
    let r = u8::from_str_radix(&rest[0..2], 16).ok()?;
    let g = u8::from_str_radix(&rest[2..4], 16).ok()?;
    let b = u8::from_str_radix(&rest[4..6], 16).ok()?;
    Some(Rgb(r, g, b))
}

/// Load a theme by name from the configured theme directories.
///
/// Searches built-in (`get_themes_dir`) then custom (`get_custom_themes_dir`).
/// Built-in names resolve to interned themes without disk access.
///
/// # Errors
///
/// See [`ThemeError`] variants.
pub fn load_by_name(name: &str, mode: ColorMode) -> Result<Arc<ResolvedTheme>, ThemeError> {
    if let Some(theme) = built_in_theme(name, mode) {
        return Ok(theme);
    }
    let builtin_path = config::get_themes_dir().join(format!("{name}.json"));
    let custom_path = config::get_custom_themes_dir().join(format!("{name}.json"));
    let path = if builtin_path.exists() {
        builtin_path
    } else if custom_path.exists() {
        custom_path
    } else {
        return Err(ThemeError::NotFound(name.to_owned()));
    };
    let text = std::fs::read_to_string(&path)?;
    ThemeJson::parse(&text)?.resolve(mode)
}

/// Load a theme, falling back to the built-in dark theme on any error.
///
/// This is the safe entry point used by the view-model so a corrupt theme
/// never breaks rendering.
#[must_use]
pub fn load_or_dark(name: &str, mode: ColorMode) -> Arc<ResolvedTheme> {
    load_by_name(name, mode).unwrap_or_else(|_| built_in_theme("dark", mode).unwrap_or_else(dark))
}

/// The ten built-in theme names in stable order (dark member first per family).
pub const BUILT_IN_THEME_NAMES: [&str; 10] = [
    "dark",
    "light",
    "classic-dark",
    "classic-light",
    "motion-dark",
    "motion-light",
    "m3-dark",
    "m3-light",
    "antd-dark",
    "antd-light",
];

/// Built-in theme families shown in `/settings` and `/theme` (storage uses dark members).
pub const BUILT_IN_THEME_FAMILIES: [&str; 5] = ["default", "classic", "motion", "m3", "antd"];

/// Dark-member storage name for a built-in family label, or `None` when custom/unknown.
#[must_use]
pub fn family_to_storage_name(family: &str) -> Option<&'static str> {
    match family {
        "default" => Some("dark"),
        "classic" => Some("classic-dark"),
        "motion" => Some("motion-dark"),
        "m3" => Some("m3-dark"),
        "antd" => Some("antd-dark"),
        _ => None,
    }
}

/// Display family (or custom/pair raw) for a stored `theme` setting value.
///
/// Built-in `dark`/`light` and `*-dark`/`*-light` map to their family label.
/// Slash pairs and unpaired customs display as the raw stored string.
#[must_use]
pub fn storage_name_to_display(raw: Option<&str>) -> String {
    let Some(raw) = raw.filter(|s| !s.is_empty()) else {
        return "default".to_owned();
    };
    if parse_theme_pair(raw).is_some() {
        return raw.to_owned();
    }
    match raw {
        "dark" | "light" => "default".to_owned(),
        name if name.ends_with("-dark") => name.strip_suffix("-dark").unwrap_or(name).to_owned(),
        name if name.ends_with("-light") => name.strip_suffix("-light").unwrap_or(name).to_owned(),
        other => other.to_owned(),
    }
}

/// Values for the Theme settings row: five built-in families plus custom theme names.
#[must_use]
pub fn theme_selector_values() -> Vec<String> {
    let mut values: Vec<String> = BUILT_IN_THEME_FAMILIES
        .iter()
        .map(|family| (*family).to_owned())
        .collect();
    let mut customs: Vec<String> = available_themes(ColorMode::Truecolor)
        .into_iter()
        .filter_map(|(info, _)| {
            let name = info.name;
            if BUILT_IN_THEME_NAMES.contains(&name.as_str()) {
                None
            } else {
                Some(name)
            }
        })
        .collect();
    customs.sort();
    customs.dedup();
    values.extend(customs);
    values
}

/// Map a Theme-row selection value to the persisted `theme` storage string.
///
/// Families store their dark member; customs store the literal name.
#[must_use]
pub fn theme_selection_to_storage(selection: &str) -> String {
    family_to_storage_name(selection).map_or_else(|| selection.to_owned(), str::to_owned)
}

/// Parse a `light/dark` pair setting (upstream `parseAutoThemeSetting`).
///
/// Exactly one `/`, both members non-empty after trimming; member names are
/// arbitrary (no `-light`/`-dark` suffix assumption). Returns
/// `(light, dark)`.
#[must_use]
pub fn parse_theme_pair(raw: &str) -> Option<(String, String)> {
    let (light, dark) = raw.split_once('/')?;
    if dark.contains('/') {
        return None;
    }
    let light = light.trim();
    let dark = dark.trim();
    if light.is_empty() || dark.is_empty() {
        return None;
    }
    Some((light.to_owned(), dark.to_owned()))
}

/// Resolve the active theme from the raw `theme` setting, the `themeMode`
/// polarity, and the detected terminal background.
///
/// Ports the upstream `parseAutoThemeSetting` / `resolveThemeSetting`
/// semantics with the `themeMode` extension:
///
/// - unset ⇒ base `"dark"`;
/// - `"a/b"` ⇒ `{light: a, dark: b}`, member picked by `want_dark`, each
///   member falling back independently via [`load_or_dark`];
/// - plain name ⇒ [`paired_name`] flips polarity when the mode asks for the
///   other variant; `None` or a failed pair load keeps the base name.
///
/// `want_dark = mode == Dark || (mode == Auto && terminal == Dark)`.
///
/// `color` selects the emitted color depth (24-bit truecolor vs downsampled
/// 256-color), sourced from the terminal's detected capability at the callsite.
#[must_use]
pub fn resolve_active_theme(
    raw: Option<&str>,
    mode: ThemeMode,
    terminal: TerminalTheme,
    color: ColorMode,
) -> Arc<ResolvedTheme> {
    let want_dark = match mode {
        ThemeMode::Dark => true,
        ThemeMode::Light => false,
        ThemeMode::Auto => terminal == TerminalTheme::Dark,
    };
    let base = raw.unwrap_or("dark");
    if let Some((light, dark)) = parse_theme_pair(base) {
        let member = if want_dark { dark } else { light };
        return load_or_dark(&member, color);
    }
    if let Some(paired) = paired_name(base, want_dark)
        && let Ok(theme) = load_by_name(&paired, color)
    {
        return theme;
    }
    load_or_dark(base, color)
}

/// A discoverable theme: display name plus its JSON path when file-backed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ThemeInfo {
    /// Display name (built-in name, or the custom theme's JSON `name` field).
    pub name: String,
    /// JSON file path (built-ins report the shipped path; customs their file).
    pub path: Option<PathBuf>,
    /// File stem for customs whose JSON `name` differs from the filename
    /// (upstream `getTheme` loads customs by filename).
    pub file_stem: Option<String>,
}

/// Enumerate every available theme with its resolved colors: the ten
/// built-ins followed by discovered custom themes, deduplicated by name and
/// sorted by name (upstream `getAvailableThemesWithPaths`). Invalid custom
/// JSONs are skipped.
#[must_use]
pub fn available_themes(mode: ColorMode) -> Vec<(ThemeInfo, Arc<ResolvedTheme>)> {
    let mut seen = std::collections::HashSet::new();
    let mut result = Vec::new();
    let builtin_dir = config::get_themes_dir();
    for name in BUILT_IN_THEME_NAMES {
        let Some(theme) = built_in_theme(name, mode) else {
            continue;
        };
        seen.insert(name.to_owned());
        result.push((
            ThemeInfo {
                name: name.to_owned(),
                path: Some(builtin_dir.join(format!("{name}.json"))),
                file_stem: None,
            },
            theme,
        ));
    }
    let custom_dir = config::get_custom_themes_dir();
    if let Ok(entries) = std::fs::read_dir(&custom_dir) {
        let mut files: Vec<PathBuf> = entries
            .filter_map(|entry| entry.ok().map(|e| e.path()))
            .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
            .collect();
        files.sort();
        for path in files {
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            let Ok(theme) = ThemeJson::parse(&text).and_then(|json| json.resolve(mode)) else {
                continue;
            };
            let name = theme.name.to_string();
            if name.is_empty() || !seen.insert(name.clone()) {
                continue;
            }
            let file_stem = path
                .file_stem()
                .map(|stem| stem.to_string_lossy().into_owned());
            result.push((
                ThemeInfo {
                    name,
                    path: Some(path),
                    file_stem,
                },
                theme,
            ));
        }
    }
    result.sort_by(|a, b| a.0.name.cmp(&b.0.name));
    result
}

// --- slot name tables ------------------------------------------------------

const REQUIRED_COLORS: &[&str] = &[
    "accent",
    "border",
    "borderAccent",
    "borderMuted",
    "success",
    "error",
    "warning",
    "muted",
    "dim",
    "text",
    "thinkingText",
    "selectedBg",
    "userMessageBg",
    "userMessageText",
    "customMessageBg",
    "customMessageText",
    "customMessageLabel",
    "toolPendingBg",
    "toolSuccessBg",
    "toolErrorBg",
    "toolTitle",
    "toolOutput",
    "mdHeading",
    "mdLink",
    "mdLinkUrl",
    "mdCode",
    "mdCodeBlock",
    "mdCodeBlockBorder",
    "mdQuote",
    "mdQuoteBorder",
    "mdHr",
    "mdListBullet",
    "toolDiffAdded",
    "toolDiffRemoved",
    "toolDiffContext",
    "syntaxComment",
    "syntaxKeyword",
    "syntaxFunction",
    "syntaxVariable",
    "syntaxString",
    "syntaxNumber",
    "syntaxType",
    "syntaxOperator",
    "syntaxPunctuation",
    "thinkingOff",
    "thinkingMinimal",
    "thinkingLow",
    "thinkingMedium",
    "thinkingHigh",
    "thinkingXhigh",
    "bashMode",
];

const ALL_FG_SLOTS: &[(ThemeColor, &str)] = &[
    (ThemeColor::Accent, "accent"),
    (ThemeColor::Border, "border"),
    (ThemeColor::BorderAccent, "borderAccent"),
    (ThemeColor::BorderMuted, "borderMuted"),
    (ThemeColor::Success, "success"),
    (ThemeColor::Error, "error"),
    (ThemeColor::Warning, "warning"),
    (ThemeColor::Muted, "muted"),
    (ThemeColor::Dim, "dim"),
    (ThemeColor::Text, "text"),
    (ThemeColor::ThinkingText, "thinkingText"),
    (ThemeColor::UserMessageText, "userMessageText"),
    (ThemeColor::CustomMessageText, "customMessageText"),
    (ThemeColor::CustomMessageLabel, "customMessageLabel"),
    (ThemeColor::ToolTitle, "toolTitle"),
    (ThemeColor::ToolOutput, "toolOutput"),
    (ThemeColor::MdHeading, "mdHeading"),
    (ThemeColor::MdLink, "mdLink"),
    (ThemeColor::MdLinkUrl, "mdLinkUrl"),
    (ThemeColor::MdCode, "mdCode"),
    (ThemeColor::MdCodeBlock, "mdCodeBlock"),
    (ThemeColor::MdCodeBlockBorder, "mdCodeBlockBorder"),
    (ThemeColor::MdQuote, "mdQuote"),
    (ThemeColor::MdQuoteBorder, "mdQuoteBorder"),
    (ThemeColor::MdHr, "mdHr"),
    (ThemeColor::MdListBullet, "mdListBullet"),
    (ThemeColor::ToolDiffAdded, "toolDiffAdded"),
    (ThemeColor::ToolDiffRemoved, "toolDiffRemoved"),
    (ThemeColor::ToolDiffContext, "toolDiffContext"),
    (ThemeColor::SyntaxComment, "syntaxComment"),
    (ThemeColor::SyntaxKeyword, "syntaxKeyword"),
    (ThemeColor::SyntaxFunction, "syntaxFunction"),
    (ThemeColor::SyntaxVariable, "syntaxVariable"),
    (ThemeColor::SyntaxString, "syntaxString"),
    (ThemeColor::SyntaxNumber, "syntaxNumber"),
    (ThemeColor::SyntaxType, "syntaxType"),
    (ThemeColor::SyntaxOperator, "syntaxOperator"),
    (ThemeColor::SyntaxPunctuation, "syntaxPunctuation"),
    (ThemeColor::ThinkingOff, "thinkingOff"),
    (ThemeColor::ThinkingMinimal, "thinkingMinimal"),
    (ThemeColor::ThinkingLow, "thinkingLow"),
    (ThemeColor::ThinkingMedium, "thinkingMedium"),
    (ThemeColor::ThinkingHigh, "thinkingHigh"),
    (ThemeColor::ThinkingXhigh, "thinkingXhigh"),
    (ThemeColor::ThinkingMax, "thinkingMax"),
    (ThemeColor::BashMode, "bashMode"),
];

const ALL_BG_SLOTS: &[(ThemeBg, &str)] = &[
    (ThemeBg::SelectedBg, "selectedBg"),
    (ThemeBg::UserMessageBg, "userMessageBg"),
    (ThemeBg::CustomMessageBg, "customMessageBg"),
    (ThemeBg::ToolPendingBg, "toolPendingBg"),
    (ThemeBg::ToolSuccessBg, "toolSuccessBg"),
    (ThemeBg::ToolErrorBg, "toolErrorBg"),
];

#[cfg(test)]
mod tests {
    use super::*;

    type TestResult = Result<(), String>;

    fn parsed_theme(overrides: &[(&str, serde_json::Value)]) -> Result<ThemeJson, String> {
        let mut colors = serde_json::Map::new();
        for slot in REQUIRED_COLORS {
            colors.insert((*slot).to_owned(), serde_json::json!("#010203"));
        }
        for (slot, value) in overrides {
            colors.insert((*slot).to_owned(), value.clone());
        }
        ThemeJson::from_value(&serde_json::json!({
            "name": "test",
            "colors": colors,
        }))
        .map_err(|error| format!("test theme should parse: {error}"))
    }

    const BUILTIN_JSONS: &[(&str, &str)] = &[
        ("dark", include_str!("../../../assets/theme/dark.json")),
        ("light", include_str!("../../../assets/theme/light.json")),
        (
            "classic-dark",
            include_str!("../../../assets/theme/classic-dark.json"),
        ),
        (
            "classic-light",
            include_str!("../../../assets/theme/classic-light.json"),
        ),
        (
            "motion-dark",
            include_str!("../../../assets/theme/motion-dark.json"),
        ),
        (
            "motion-light",
            include_str!("../../../assets/theme/motion-light.json"),
        ),
        (
            "m3-dark",
            include_str!("../../../assets/theme/m3-dark.json"),
        ),
        (
            "m3-light",
            include_str!("../../../assets/theme/m3-light.json"),
        ),
        (
            "antd-dark",
            include_str!("../../../assets/theme/antd-dark.json"),
        ),
        (
            "antd-light",
            include_str!("../../../assets/theme/antd-light.json"),
        ),
    ];

    #[test]
    fn built_in_jsons_parse_and_resolve() -> TestResult {
        for (name, json) in BUILTIN_JSONS {
            let theme = ThemeJson::parse(json)
                .map_err(|error| format!("{name} should parse: {error}"))?
                .resolve(ColorMode::Truecolor)
                .map_err(|error| format!("{name} should resolve: {error}"))?;
            assert_eq!(theme.name, *name);
        }
        Ok(())
    }

    #[test]
    fn built_in_jsons_are_the_intern_authority() -> TestResult {
        for (name, json) in BUILTIN_JSONS {
            let resolved = ThemeJson::parse(json)
                .map_err(|error| format!("{name} should parse: {error}"))?
                .resolve(ColorMode::Truecolor)
                .map_err(|error| format!("{name} should resolve: {error}"))?;
            let intern = built_in_theme(name, ColorMode::Truecolor)
                .ok_or_else(|| format!("{name} intern should exist"))?;
            assert_eq!(resolved, intern);
        }
        Ok(())
    }

    #[test]
    fn paired_names_follow_dark_light_convention() {
        assert_eq!(paired_name("dark", true).as_deref(), Some("dark"));
        assert_eq!(paired_name("dark", false).as_deref(), Some("light"));
        assert_eq!(paired_name("m3-light", true).as_deref(), Some("m3-dark"));
        assert_eq!(paired_name("mytheme", true), None);
        assert_eq!(paired_name("mytheme", false), None);
    }

    #[test]
    fn family_storage_round_trip() {
        assert_eq!(family_to_storage_name("default"), Some("dark"));
        assert_eq!(family_to_storage_name("classic"), Some("classic-dark"));
        assert_eq!(family_to_storage_name("motion"), Some("motion-dark"));
        assert_eq!(family_to_storage_name("m3"), Some("m3-dark"));
        assert_eq!(family_to_storage_name("antd"), Some("antd-dark"));
        assert_eq!(family_to_storage_name("mytheme"), None);

        assert_eq!(storage_name_to_display(None), "default");
        assert_eq!(storage_name_to_display(Some("dark")), "default");
        assert_eq!(storage_name_to_display(Some("light")), "default");
        assert_eq!(storage_name_to_display(Some("classic-light")), "classic");
        assert_eq!(storage_name_to_display(Some("motion-dark")), "motion");
        assert_eq!(storage_name_to_display(Some("mytheme")), "mytheme");
        assert_eq!(
            storage_name_to_display(Some("solarized-light/gruvbox-dark")),
            "solarized-light/gruvbox-dark"
        );
        assert_eq!(theme_selection_to_storage("default"), "dark");
        assert_eq!(theme_selection_to_storage("m3"), "m3-dark");
        assert_eq!(theme_selection_to_storage("custom-x"), "custom-x");
        let values = theme_selector_values();
        assert_eq!(
            &values[..5],
            &["default", "classic", "motion", "m3", "antd"]
        );
        assert!(!values.iter().any(|v| v == "dark" || v == "classic-dark"));
    }

    #[test]
    fn built_in_palette_spot_checks() -> TestResult {
        let lookup = |name: &str| {
            built_in_theme(name, ColorMode::Truecolor)
                .ok_or_else(|| format!("{name} intern should exist"))
        };
        assert_eq!(dark().fg_rgb(ThemeColor::Text), Rgb(237, 237, 237));
        assert_eq!(
            lookup("m3-dark")?.fg_rgb(ThemeColor::SyntaxKeyword),
            Rgb(158, 202, 255)
        );
        assert_eq!(
            lookup("antd-light")?.fg_rgb(ThemeColor::Accent),
            Rgb(22, 119, 255)
        );
        assert_eq!(
            lookup("classic-light")?.fg_rgb(ThemeColor::ThinkingMax),
            Rgb(175, 0, 95)
        );
        Ok(())
    }

    /// Golden: every semantic slot in the built-in dark (and light) palette.
    ///
    /// Values are the authoritative RGB triples from `assets/theme/{dark,light}.json`.
    /// Changing any single color in those files must fail these tests.
    fn assert_palette(theme: &ResolvedTheme, fg: &[(ThemeColor, Rgb)], bg: &[(ThemeBg, Rgb)]) {
        assert_eq!(
            fg.len(),
            ALL_FG.len(),
            "{}: golden fg must cover every ThemeColor",
            theme.name
        );
        assert_eq!(
            bg.len(),
            ALL_BG.len(),
            "{}: golden bg must cover every ThemeBg",
            theme.name
        );
        for (i, &(slot, rgb)) in fg.iter().enumerate() {
            assert_eq!(slot, ALL_FG[i], "{}: golden fg order", theme.name);
            assert_eq!(theme.fg_rgb(slot), rgb, "{} fg {slot:?}", theme.name);
        }
        for (i, &(slot, rgb)) in bg.iter().enumerate() {
            assert_eq!(slot, ALL_BG[i], "{}: golden bg order", theme.name);
            assert_eq!(theme.bg_rgb(slot), rgb, "{} bg {slot:?}", theme.name);
        }
    }

    #[test]
    fn dark_palette_pins_every_semantic_color() {
        assert_palette(
            &dark(),
            &[
                (ThemeColor::Accent, Rgb(82, 168, 255)),
                (ThemeColor::Border, Rgb(69, 69, 69)),
                (ThemeColor::BorderAccent, Rgb(0, 114, 245)),
                (ThemeColor::BorderMuted, Rgb(46, 46, 46)),
                (ThemeColor::Success, Rgb(98, 192, 115)),
                (ThemeColor::Error, Rgb(255, 97, 102)),
                (ThemeColor::Warning, Rgb(255, 178, 36)),
                (ThemeColor::Muted, Rgb(237, 237, 237)),
                (ThemeColor::Dim, Rgb(161, 161, 161)),
                (ThemeColor::Text, Rgb(237, 237, 237)),
                (ThemeColor::ThinkingText, Rgb(237, 237, 237)),
                (ThemeColor::UserMessageText, Rgb(237, 237, 237)),
                (ThemeColor::CustomMessageText, Rgb(237, 237, 237)),
                (ThemeColor::CustomMessageLabel, Rgb(10, 199, 180)),
                (ThemeColor::ToolTitle, Rgb(82, 168, 255)),
                (ThemeColor::ToolOutput, Rgb(237, 237, 237)),
                (ThemeColor::MdHeading, Rgb(82, 168, 255)),
                (ThemeColor::MdLink, Rgb(82, 168, 255)),
                (ThemeColor::MdLinkUrl, Rgb(161, 161, 161)),
                (ThemeColor::MdCode, Rgb(10, 199, 180)),
                (ThemeColor::MdCodeBlock, Rgb(237, 237, 237)),
                (ThemeColor::MdCodeBlockBorder, Rgb(46, 46, 46)),
                (ThemeColor::MdQuote, Rgb(237, 237, 237)),
                (ThemeColor::MdQuoteBorder, Rgb(69, 69, 69)),
                (ThemeColor::MdHr, Rgb(46, 46, 46)),
                (ThemeColor::MdListBullet, Rgb(82, 168, 255)),
                (ThemeColor::ToolDiffAdded, Rgb(98, 192, 115)),
                (ThemeColor::ToolDiffRemoved, Rgb(255, 97, 102)),
                (ThemeColor::ToolDiffContext, Rgb(237, 237, 237)),
                (ThemeColor::SyntaxComment, Rgb(161, 161, 161)),
                (ThemeColor::SyntaxKeyword, Rgb(82, 168, 255)),
                (ThemeColor::SyntaxFunction, Rgb(255, 178, 36)),
                (ThemeColor::SyntaxVariable, Rgb(237, 237, 237)),
                (ThemeColor::SyntaxString, Rgb(98, 192, 115)),
                (ThemeColor::SyntaxNumber, Rgb(242, 162, 13)),
                (ThemeColor::SyntaxType, Rgb(10, 199, 180)),
                (ThemeColor::SyntaxOperator, Rgb(161, 161, 161)),
                (ThemeColor::SyntaxPunctuation, Rgb(161, 161, 161)),
                (ThemeColor::ThinkingOff, Rgb(161, 161, 161)),
                (ThemeColor::ThinkingMinimal, Rgb(161, 161, 161)),
                (ThemeColor::ThinkingLow, Rgb(82, 168, 255)),
                (ThemeColor::ThinkingMedium, Rgb(10, 199, 180)),
                (ThemeColor::ThinkingHigh, Rgb(242, 162, 13)),
                (ThemeColor::ThinkingXhigh, Rgb(255, 97, 102)),
                (ThemeColor::ThinkingMax, Rgb(255, 97, 102)),
                (ThemeColor::BashMode, Rgb(255, 178, 36)),
            ],
            &[
                (ThemeBg::SelectedBg, Rgb(41, 41, 41)),
                (ThemeBg::UserMessageBg, Rgb(26, 26, 26)),
                (ThemeBg::CustomMessageBg, Rgb(15, 28, 46)),
                (ThemeBg::ToolPendingBg, Rgb(26, 26, 26)),
                (ThemeBg::ToolSuccessBg, Rgb(11, 34, 18)),
                (ThemeBg::ToolErrorBg, Rgb(42, 19, 20)),
            ],
        );
    }

    #[test]
    fn light_palette_pins_every_semantic_color() {
        assert_palette(
            &light(),
            &[
                (ThemeColor::Accent, Rgb(0, 114, 245)),
                (ThemeColor::Border, Rgb(201, 201, 201)),
                (ThemeColor::BorderAccent, Rgb(0, 114, 245)),
                (ThemeColor::BorderMuted, Rgb(235, 235, 235)),
                (ThemeColor::Success, Rgb(69, 165, 87)),
                (ThemeColor::Error, Rgb(229, 72, 77)),
                (ThemeColor::Warning, Rgb(163, 82, 0)),
                (ThemeColor::Muted, Rgb(77, 77, 77)),
                (ThemeColor::Dim, Rgb(143, 143, 143)),
                (ThemeColor::Text, Rgb(23, 23, 23)),
                (ThemeColor::ThinkingText, Rgb(77, 77, 77)),
                (ThemeColor::UserMessageText, Rgb(23, 23, 23)),
                (ThemeColor::CustomMessageText, Rgb(23, 23, 23)),
                (ThemeColor::CustomMessageLabel, Rgb(6, 122, 110)),
                (ThemeColor::ToolTitle, Rgb(0, 104, 214)),
                (ThemeColor::ToolOutput, Rgb(77, 77, 77)),
                (ThemeColor::MdHeading, Rgb(0, 104, 214)),
                (ThemeColor::MdLink, Rgb(0, 104, 214)),
                (ThemeColor::MdLinkUrl, Rgb(143, 143, 143)),
                (ThemeColor::MdCode, Rgb(189, 40, 100)),
                (ThemeColor::MdCodeBlock, Rgb(77, 77, 77)),
                (ThemeColor::MdCodeBlockBorder, Rgb(235, 235, 235)),
                (ThemeColor::MdQuote, Rgb(77, 77, 77)),
                (ThemeColor::MdQuoteBorder, Rgb(201, 201, 201)),
                (ThemeColor::MdHr, Rgb(235, 235, 235)),
                (ThemeColor::MdListBullet, Rgb(0, 104, 214)),
                (ThemeColor::ToolDiffAdded, Rgb(41, 122, 58)),
                (ThemeColor::ToolDiffRemoved, Rgb(203, 42, 47)),
                (ThemeColor::ToolDiffContext, Rgb(77, 77, 77)),
                (ThemeColor::SyntaxComment, Rgb(143, 143, 143)),
                (ThemeColor::SyntaxKeyword, Rgb(0, 104, 214)),
                (ThemeColor::SyntaxFunction, Rgb(120, 32, 188)),
                (ThemeColor::SyntaxVariable, Rgb(23, 23, 23)),
                (ThemeColor::SyntaxString, Rgb(41, 122, 58)),
                (ThemeColor::SyntaxNumber, Rgb(163, 82, 0)),
                (ThemeColor::SyntaxType, Rgb(6, 122, 110)),
                (ThemeColor::SyntaxOperator, Rgb(77, 77, 77)),
                (ThemeColor::SyntaxPunctuation, Rgb(77, 77, 77)),
                (ThemeColor::ThinkingOff, Rgb(168, 168, 168)),
                (ThemeColor::ThinkingMinimal, Rgb(77, 77, 77)),
                (ThemeColor::ThinkingLow, Rgb(0, 104, 214)),
                (ThemeColor::ThinkingMedium, Rgb(6, 122, 110)),
                (ThemeColor::ThinkingHigh, Rgb(163, 82, 0)),
                (ThemeColor::ThinkingXhigh, Rgb(203, 42, 47)),
                (ThemeColor::ThinkingMax, Rgb(203, 42, 47)),
                (ThemeColor::BashMode, Rgb(163, 82, 0)),
            ],
            &[
                (ThemeBg::SelectedBg, Rgb(230, 230, 230)),
                (ThemeBg::UserMessageBg, Rgb(242, 242, 242)),
                (ThemeBg::CustomMessageBg, Rgb(240, 247, 255)),
                (ThemeBg::ToolPendingBg, Rgb(242, 242, 242)),
                (ThemeBg::ToolSuccessBg, Rgb(239, 251, 239)),
                (ThemeBg::ToolErrorBg, Rgb(255, 240, 240)),
            ],
        );
    }

    #[test]
    fn theme_pair_parsing_follows_upstream() {
        assert_eq!(
            parse_theme_pair("solarized-light/gruvbox-dark"),
            Some(("solarized-light".to_owned(), "gruvbox-dark".to_owned()))
        );
        assert_eq!(
            parse_theme_pair(" a / b "),
            Some(("a".to_owned(), "b".to_owned()))
        );
        assert_eq!(parse_theme_pair("plain"), None);
        assert_eq!(parse_theme_pair("a/b/c"), None, "two slashes are invalid");
        assert_eq!(parse_theme_pair("/dark"), None, "empty light member");
        assert_eq!(parse_theme_pair("light/"), None, "empty dark member");
    }

    /// Resolution matrix over (mode, terminal) ∈ {auto,dark,light}×{dark,light}.
    #[test]
    fn resolve_active_theme_matrix() {
        let cells = [
            (ThemeMode::Auto, TerminalTheme::Dark, true),
            (ThemeMode::Auto, TerminalTheme::Light, false),
            (ThemeMode::Dark, TerminalTheme::Dark, true),
            (ThemeMode::Dark, TerminalTheme::Light, true),
            (ThemeMode::Light, TerminalTheme::Dark, false),
            (ThemeMode::Light, TerminalTheme::Light, false),
        ];
        for (mode, terminal, want_dark) in cells {
            // Unset raw setting: base "dark", flipped by polarity.
            let expected = if want_dark { "dark" } else { "light" };
            assert_eq!(
                resolve_active_theme(None, mode, terminal, ColorMode::Truecolor).name,
                expected,
                "unset raw, {mode:?}/{terminal:?}"
            );
            assert_eq!(
                resolve_active_theme(Some("dark"), mode, terminal, ColorMode::Truecolor).name,
                expected,
                "base dark, {mode:?}/{terminal:?}"
            );
            // Plain suffixed name: paired_name flips within the family.
            let expected = if want_dark { "antd-dark" } else { "antd-light" };
            assert_eq!(
                resolve_active_theme(Some("antd-light"), mode, terminal, ColorMode::Truecolor).name,
                expected,
                "base antd-light, {mode:?}/{terminal:?}"
            );
            // Pair members are positional, not suffix-derived: the reversed
            // pair "dark/light" means {light: dark-theme, dark: light-theme}.
            let expected = if want_dark { "light" } else { "dark" };
            assert_eq!(
                resolve_active_theme(Some("dark/light"), mode, terminal, ColorMode::Truecolor).name,
                expected,
                "reversed pair, {mode:?}/{terminal:?}"
            );
            // Unknown pair members fall back per-member to built-in dark.
            assert_eq!(
                resolve_active_theme(
                    Some("solarized-light/gruvbox-dark"),
                    mode,
                    terminal,
                    ColorMode::Truecolor
                )
                .name,
                "dark",
                "unknown pair members, {mode:?}/{terminal:?}"
            );
        }
    }

    #[test]
    fn resolve_active_theme_member_failure_falls_back_per_member() {
        // Dark member loads; light member is unknown and falls back to dark.
        assert_eq!(
            resolve_active_theme(
                Some("nope/m3-dark"),
                ThemeMode::Dark,
                TerminalTheme::Dark,
                ColorMode::Truecolor
            )
            .name,
            "m3-dark"
        );
        assert_eq!(
            resolve_active_theme(
                Some("nope/m3-dark"),
                ThemeMode::Light,
                TerminalTheme::Dark,
                ColorMode::Truecolor
            )
            .name,
            "dark"
        );
        // Unpaired custom name that does not exist: load_or_dark fallback.
        assert_eq!(
            resolve_active_theme(
                Some("mytheme"),
                ThemeMode::Auto,
                TerminalTheme::Light,
                ColorMode::Truecolor
            )
            .name,
            "dark"
        );
        // Paired name whose counterpart does not exist keeps the base.
        // ("classic-light" pairs to "classic-dark", both exist; use a fake
        // family to hit the fallback.)
        assert_eq!(
            resolve_active_theme(
                Some("ghost-light"),
                ThemeMode::Dark,
                TerminalTheme::Dark,
                ColorMode::Truecolor
            )
            .name,
            "dark",
            "ghost-dark fails to load, ghost-light fails to load, dark fallback"
        );
    }

    #[test]
    fn color_mode_from_true_color_maps_capability() {
        assert_eq!(ColorMode::from_true_color(true), ColorMode::Truecolor);
        assert_eq!(ColorMode::from_true_color(false), ColorMode::Palette256);
    }

    #[test]
    fn built_in_palette256_downsamples_at_emit() -> TestResult {
        let truecolor = built_in_theme("dark", ColorMode::Truecolor)
            .ok_or_else(|| "dark truecolor intern should exist".to_owned())?;
        let palette = built_in_theme("dark", ColorMode::Palette256)
            .ok_or_else(|| "dark 256-color built-in should exist".to_owned())?;
        assert_eq!(truecolor.mode(), ColorMode::Truecolor);
        assert_eq!(palette.mode(), ColorMode::Palette256);
        // Same resolved slot values; only the emitted depth differs.
        assert_eq!(
            truecolor.fg_rgb(ThemeColor::Text),
            palette.fg_rgb(ThemeColor::Text)
        );
        let tc = truecolor.fg_ansi(ThemeColor::Text);
        let p256 = palette.fg_ansi(ThemeColor::Text);
        assert!(
            tc.contains("\x1b[38;2;"),
            "truecolor emits 24-bit SGR: {tc:?}"
        );
        assert!(
            p256.contains("\x1b[38;5;"),
            "256-color built-in emits indexed SGR: {p256:?}"
        );
        assert!(
            !p256.contains("\x1b[38;2;"),
            "256-color built-in must not emit 24-bit SGR: {p256:?}"
        );
        // Clone-with-mode is byte-equivalent to re-resolving the JSON at 256.
        let reresolved = ThemeJson::parse(BUILTIN_JSONS[0].1)
            .map_err(|error| format!("dark should parse: {error}"))?
            .resolve_owned(ColorMode::Palette256)
            .map_err(|error| format!("dark should resolve: {error}"))?;
        assert_eq!(*palette, reresolved, "clone-with-mode equals re-resolve");
        Ok(())
    }

    #[test]
    fn fallback_theme_matches_terminal_color_mode() {
        // A missing theme on a 256-color terminal degrades to 256-color dark,
        // not truecolor dark.
        let p256 = load_or_dark("nonexistent-theme-xyz", ColorMode::Palette256);
        assert_eq!(p256.name, "dark");
        assert_eq!(p256.mode(), ColorMode::Palette256);
        assert!(p256.fg_ansi(ThemeColor::Text).contains("\x1b[38;5;"));
        // Truecolor terminals still fall back to truecolor dark.
        let tc = load_or_dark("nonexistent-theme-xyz", ColorMode::Truecolor);
        assert_eq!(tc.name, "dark");
        assert_eq!(tc.mode(), ColorMode::Truecolor);
    }

    #[test]
    fn resolve_active_theme_honors_color_mode() {
        let dark256 = resolve_active_theme(
            Some("dark"),
            ThemeMode::Dark,
            TerminalTheme::Dark,
            ColorMode::Palette256,
        );
        assert_eq!(dark256.mode(), ColorMode::Palette256);
        assert!(
            dark256.fg_ansi(ThemeColor::Text).contains("\x1b[38;5;"),
            "256-color capability selects indexed emission"
        );
        assert!(!dark256.fg_ansi(ThemeColor::Text).contains("\x1b[38;2;"));
        let dark24 = resolve_active_theme(
            Some("dark"),
            ThemeMode::Dark,
            TerminalTheme::Dark,
            ColorMode::Truecolor,
        );
        assert_eq!(dark24.mode(), ColorMode::Truecolor);
        assert!(dark24.fg_ansi(ThemeColor::Text).contains("\x1b[38;2;"));
    }

    #[test]
    fn slot_values_roundtrip_through_wire_vocabulary() {
        let theme = dark();
        let fg: Vec<_> = fg_slot_names()
            .iter()
            .map(|(slot, _)| (*slot, theme.fg_value(*slot)))
            .collect();
        let bg: Vec<_> = bg_slot_names()
            .iter()
            .map(|(slot, _)| (*slot, theme.bg_value(*slot)))
            .collect();
        let rebuilt =
            ResolvedTheme::from_value_slots(fg, bg, theme.mode(), theme.name.clone().into_owned());
        assert_eq!(rebuilt, *theme);
    }

    #[test]
    fn dark_accent_resolves() {
        let th = dark();
        assert_eq!(th.fg_rgb(ThemeColor::Accent), Rgb(82, 168, 255));
    }

    #[test]
    fn classic_light_literal_black_remains_a_color() -> TestResult {
        let theme = built_in_theme("classic-light", ColorMode::Truecolor)
            .ok_or_else(|| "classic-light intern should exist".to_owned())?;
        assert!(!theme.is_fg_empty(ThemeColor::SyntaxOperator));
        assert_eq!(
            theme.fg_ansi(ThemeColor::SyntaxOperator),
            "\x1b[38;2;0;0;0m"
        );
        Ok(())
    }

    #[test]
    fn json_black_is_distinct_from_default() -> TestResult {
        let theme = parsed_theme(&[
            ("accent", serde_json::json!("#000000")),
            ("muted", serde_json::json!("")),
            ("selectedBg", serde_json::json!("#000000")),
            ("toolErrorBg", serde_json::json!("")),
        ])?
        .resolve_owned(ColorMode::Truecolor)
        .map_err(|error| format!("test theme should resolve: {error}"))?;

        assert_eq!(theme.fg_rgb(ThemeColor::Accent), Rgb(0, 0, 0));
        assert!(!theme.is_fg_empty(ThemeColor::Accent));
        assert_eq!(theme.fg_ansi(ThemeColor::Accent), "\x1b[38;2;0;0;0m");
        assert!(theme.is_fg_empty(ThemeColor::Muted));
        assert_eq!(theme.fg_ansi(ThemeColor::Muted), "\x1b[39m");

        assert_eq!(theme.bg_rgb(ThemeBg::SelectedBg), Rgb(0, 0, 0));
        assert!(!theme.is_bg_empty(ThemeBg::SelectedBg));
        assert_eq!(theme.bg_ansi(ThemeBg::SelectedBg), "\x1b[48;2;0;0;0m");
        assert!(theme.is_bg_empty(ThemeBg::ToolErrorBg));
        assert_eq!(theme.bg_ansi(ThemeBg::ToolErrorBg), "\x1b[49m");
        Ok(())
    }

    #[test]
    fn json_indexed_colors_emit_exact_sequences_in_every_mode() -> TestResult {
        let parsed = parsed_theme(&[
            ("accent", serde_json::json!(17)),
            ("selectedBg", serde_json::json!(231)),
        ])?;

        for mode in [ColorMode::Truecolor, ColorMode::Palette256] {
            let theme = parsed
                .resolve_owned(mode)
                .map_err(|error| format!("test theme should resolve: {error}"))?;
            assert_eq!(theme.fg_ansi(ThemeColor::Accent), "\x1b[38;5;17m");
            assert_eq!(theme.bg_ansi(ThemeBg::SelectedBg), "\x1b[48;5;231m");
            assert_eq!(theme.fg_rgb(ThemeColor::Accent), Rgb(17, 17, 17));
            assert_eq!(theme.bg_rgb(ThemeBg::SelectedBg), Rgb(231, 231, 231));
        }
        Ok(())
    }

    // ---- syntax highlighting ----

    #[test]
    fn markdown_theme_wires_highlight_code() {
        assert!(markdown_theme().highlight_code.is_some());
    }

    #[test]
    fn vendored_syntaxes_link_except_known_go_injections() {
        for shard in ALL_SHARDS {
            for context in shard.find_unlinked_contexts() {
                assert!(
                    context.starts_with("Syntax 'Go'"),
                    "unexpected unresolved context: {context}"
                );
            }
        }
    }

    #[test]
    fn highlight_rust_uses_active_theme_syntax_slots() {
        let theme = dark();
        let lines = with_theme(Arc::clone(&theme), || {
            highlight_code("fn main() {\n    let s = \"hi\"; // c\n}\n", Some("rust"))
        });
        let keyword = theme.fg_ansi(ThemeColor::SyntaxKeyword);
        let string = theme.fg_ansi(ThemeColor::SyntaxString);
        let comment = theme.fg_ansi(ThemeColor::SyntaxComment);
        assert_eq!(lines.len(), 3);
        assert!(
            lines[0].contains(&format!("{keyword}fn\x1b[39m")),
            "`fn` should carry the keyword color: {:?}",
            lines[0]
        );
        assert!(
            lines[1].contains(&format!("{string}\"\x1b[39m{string}hi\x1b[39m")),
            "string literal should carry the string color: {:?}",
            lines[1]
        );
        assert!(
            lines[1].contains(&format!("{comment}//\x1b[39m")),
            "comment should carry the comment color: {:?}",
            lines[1]
        );
    }

    #[test]
    fn highlight_js_and_ts_alias_to_javascript() {
        let theme = dark();
        let keyword = theme.fg_ansi(ThemeColor::SyntaxKeyword);
        for lang in ["javascript", "js", "jsx", "typescript", "ts", "tsx"] {
            let lines = with_theme(Arc::clone(&theme), || {
                highlight_code("const f = (a) => a * 2;\n", Some(lang))
            });
            assert!(
                lines[0].contains(&format!("{keyword}const\x1b[39m")),
                "{lang}: `const` should carry the keyword color: {:?}",
                lines[0]
            );
        }
    }

    #[test]
    fn unknown_language_stays_plain() {
        let theme = dark();
        let expected = vec![theme.fg(ThemeColor::MdCodeBlock, "hello world")];
        let unknown = with_theme(Arc::clone(&theme), || {
            highlight_code("hello world\n", Some("cobol"))
        });
        assert_eq!(unknown, expected);
        let bare = with_theme(Arc::clone(&theme), || highlight_code("hello world\n", None));
        assert_eq!(bare, expected);
    }

    #[test]
    fn empty_code_block_yields_no_lines() {
        let theme = dark();
        let highlighted = with_theme(Arc::clone(&theme), || highlight_code("", Some("rust")));
        assert!(highlighted.is_empty());
        let plain = with_theme(Arc::clone(&theme), || highlight_code("", None));
        assert!(plain.is_empty());
    }

    #[test]
    fn theme_switch_changes_highlight_colors() {
        let code = "fn f() {}\n";
        let dark_lines = with_theme(dark(), || highlight_code(code, Some("rust")));
        let light_lines = with_theme(light(), || highlight_code(code, Some("rust")));
        assert_ne!(dark_lines, light_lines);
    }

    #[test]
    fn palette256_mode_downsamples_highlight_colors() -> TestResult {
        let theme = ThemeJson::parse(BUILTIN_JSONS[0].1)
            .map_err(|error| format!("dark should parse: {error}"))?
            .resolve_owned(ColorMode::Palette256)
            .map_err(|error| format!("dark should resolve: {error}"))?;
        let lines = with_theme(Arc::new(theme), || {
            highlight_code("fn f() {}\n", Some("rust"))
        });
        assert!(
            lines[0].contains("\x1b[38;5;"),
            "expected 256-color sequences: {:?}",
            lines[0]
        );
        assert!(
            !lines[0].contains("\x1b[38;2;"),
            "truecolor sequences should be downsampled: {:?}",
            lines[0]
        );
        Ok(())
    }

    #[test]
    fn python_regexp_raw_strings_still_highlight() {
        let theme = dark();
        let lines = with_theme(Arc::clone(&theme), || {
            highlight_code("import re\nm = re.compile(r\"\\d+\")\n", Some("python"))
        });
        assert_eq!(lines.len(), 2);
        let keyword = theme.fg_ansi(ThemeColor::SyntaxKeyword);
        assert!(
            lines[0].contains(&format!("{keyword}import\x1b[39m")),
            "`import` should carry the keyword color: {:?}",
            lines[0]
        );
    }

    #[test]
    fn language_from_path_maps_vendored_extensions() {
        assert_eq!(language_from_path("src/main.rs"), Some("rs"));
        assert_eq!(language_from_path("a/b/Component.TSX"), Some("js"));
        assert_eq!(language_from_path("notes.yaml"), Some("yaml"));
        assert_eq!(language_from_path("Makefile"), None);
        assert_eq!(language_from_path("x.kt"), None);
    }
}
