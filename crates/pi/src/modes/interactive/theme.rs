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
use std::sync::{Arc, LazyLock};

pub use pi_tui::components::{DefaultTextStyle, MarkdownOptions, MarkdownTheme};
use pi_tui::components::{SelectListTheme, SettingsListTheme};
use pi_tui::text::{truncate_to_width, visible_width};

use crate::core::config;

/// Terminal color depth selected at startup.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ColorMode {
    /// 24-bit `CSI 38;2;r;g;b` sequences.
    Truecolor,
    /// Downsampled `CSI 38;5;n` 256-color palette.
    Palette256,
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
#[derive(Clone, Debug)]
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

    fn from_rgb_arrays(
        fg: [Rgb; ALL_FG.len()],
        bg: [Rgb; ALL_BG.len()],
        mode: ColorMode,
        name: impl Into<Cow<'static, str>>,
    ) -> Self {
        Self {
            fg: fg.map(ResolvedColor::Rgb),
            bg: bg.map(ResolvedColor::Rgb),
            mode,
            name: name.into(),
        }
    }

    /// Active color mode.
    #[must_use]
    pub const fn mode(&self) -> ColorMode {
        self.mode
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

// ---------------------------------------------------------------------------
// fn-pointer color hooks (read the thread-local current theme)
// ---------------------------------------------------------------------------

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
        highlight_code: None,
        code_block_indent: "  ".to_owned(),
    }
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

// ---------------------------------------------------------------------------
// 256-color downsampling (ports rgbTo256 from theme.ts)
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// Built-in dark / light themes (resolved interns)
// ---------------------------------------------------------------------------

const fn rgb(r: u8, g: u8, b: u8) -> Rgb {
    Rgb(r, g, b)
}

/// Built-in dark theme (interned). Mirrors `dark.json` with vars resolved.
#[must_use]
pub fn dark() -> Arc<ResolvedTheme> {
    DARK_CLONE.clone()
}

/// Built-in light theme (interned). Mirrors `light.json` with vars resolved.
#[must_use]
pub fn light() -> Arc<ResolvedTheme> {
    LIGHT_INTERN.clone()
}

static DARK_CLONE: LazyLock<Arc<ResolvedTheme>> = LazyLock::new(|| {
    Arc::new(ResolvedTheme::from_rgb_arrays(
        dark_fg(),
        dark_bg(),
        ColorMode::Truecolor,
        Cow::Borrowed("dark"),
    ))
});

static LIGHT_INTERN: LazyLock<Arc<ResolvedTheme>> = LazyLock::new(|| {
    Arc::new(ResolvedTheme::from_rgb_arrays(
        light_fg(),
        light_bg(),
        ColorMode::Truecolor,
        Cow::Borrowed("light"),
    ))
});

// ---------------------------------------------------------------------------
// JSON loading + validation
// ---------------------------------------------------------------------------

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
        if !colors.iter().any(|(k, _)| k == "thinkingMax")
            && let Some((_, x)) = colors.iter().find(|(k, _)| k == "thinkingXhigh")
        {
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
/// `"dark"` and `"light"` resolve to the built-in interns without disk access.
///
/// # Errors
///
/// See [`ThemeError`] variants.
pub fn load_by_name(name: &str, mode: ColorMode) -> Result<Arc<ResolvedTheme>, ThemeError> {
    if name == "dark" {
        return Ok(dark());
    }
    if name == "light" {
        return Ok(light());
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
    load_by_name(name, mode).unwrap_or_else(|_| dark())
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

// --- built-in resolved color tables ----------------------------------------

const fn dark_fg() -> [Rgb; 46] {
    [
        rgb(138, 190, 183), // accent
        rgb(95, 135, 255),  // border
        rgb(0, 215, 255),   // borderAccent
        rgb(80, 80, 80),    // borderMuted
        rgb(181, 189, 104), // success
        rgb(204, 102, 102), // error
        rgb(255, 255, 0),   // warning
        rgb(128, 128, 128), // muted
        rgb(102, 102, 102), // dim
        rgb(212, 212, 212), // text
        rgb(128, 128, 128), // thinkingText
        rgb(212, 212, 212), // userMessageText
        rgb(212, 212, 212), // customMessageText
        rgb(149, 117, 205), // customMessageLabel
        rgb(212, 212, 212), // toolTitle
        rgb(128, 128, 128), // toolOutput
        rgb(240, 198, 116), // mdHeading
        rgb(129, 162, 190), // mdLink
        rgb(102, 102, 102), // mdLinkUrl
        rgb(138, 190, 183), // mdCode
        rgb(181, 189, 104), // mdCodeBlock
        rgb(128, 128, 128), // mdCodeBlockBorder
        rgb(128, 128, 128), // mdQuote
        rgb(128, 128, 128), // mdQuoteBorder
        rgb(128, 128, 128), // mdHr
        rgb(138, 190, 183), // mdListBullet
        rgb(181, 189, 104), // toolDiffAdded
        rgb(204, 102, 102), // toolDiffRemoved
        rgb(128, 128, 128), // toolDiffContext
        rgb(106, 153, 85),  // syntaxComment
        rgb(86, 156, 214),  // syntaxKeyword
        rgb(220, 220, 170), // syntaxFunction
        rgb(156, 220, 254), // syntaxVariable
        rgb(206, 145, 120), // syntaxString
        rgb(181, 206, 168), // syntaxNumber
        rgb(78, 201, 176),  // syntaxType
        rgb(212, 212, 212), // syntaxOperator
        rgb(212, 212, 212), // syntaxPunctuation
        rgb(80, 80, 80),    // thinkingOff
        rgb(110, 110, 110), // thinkingMinimal
        rgb(95, 135, 175),  // thinkingLow
        rgb(129, 162, 190), // thinkingMedium
        rgb(178, 148, 187), // thinkingHigh
        rgb(209, 131, 232), // thinkingXhigh
        rgb(255, 95, 255),  // thinkingMax
        rgb(181, 189, 104), // bashMode
    ]
}

const fn dark_bg() -> [Rgb; 6] {
    [
        rgb(58, 58, 74), // selectedBg
        rgb(52, 53, 65), // userMessageBg
        rgb(45, 40, 56), // customMessageBg
        rgb(40, 40, 50), // toolPendingBg
        rgb(40, 50, 40), // toolSuccessBg
        rgb(60, 40, 40), // toolErrorBg
    ]
}

const fn light_fg() -> [Rgb; 46] {
    [
        rgb(90, 128, 128),  // accent
        rgb(84, 125, 167),  // border
        rgb(90, 128, 128),  // borderAccent
        rgb(176, 176, 176), // borderMuted
        rgb(88, 132, 88),   // success
        rgb(170, 85, 85),   // error
        rgb(154, 115, 38),  // warning
        rgb(108, 108, 108), // muted
        rgb(118, 118, 118), // dim
        rgb(31, 35, 40),    // text
        rgb(108, 108, 108), // thinkingText
        rgb(31, 35, 40),    // userMessageText
        rgb(31, 35, 40),    // customMessageText
        rgb(126, 87, 194),  // customMessageLabel
        rgb(31, 35, 40),    // toolTitle
        rgb(108, 108, 108), // toolOutput
        rgb(154, 115, 38),  // mdHeading
        rgb(84, 125, 167),  // mdLink
        rgb(118, 118, 118), // mdLinkUrl
        rgb(90, 128, 128),  // mdCode
        rgb(88, 132, 88),   // mdCodeBlock
        rgb(108, 108, 108), // mdCodeBlockBorder
        rgb(108, 108, 108), // mdQuote
        rgb(108, 108, 108), // mdQuoteBorder
        rgb(108, 108, 108), // mdHr
        rgb(88, 132, 88),   // mdListBullet
        rgb(88, 132, 88),   // toolDiffAdded
        rgb(170, 85, 85),   // toolDiffRemoved
        rgb(108, 108, 108), // toolDiffContext
        rgb(0, 128, 0),     // syntaxComment
        rgb(0, 0, 255),     // syntaxKeyword
        rgb(121, 94, 38),   // syntaxFunction
        rgb(0, 16, 128),    // syntaxVariable
        rgb(163, 21, 21),   // syntaxString
        rgb(9, 134, 88),    // syntaxNumber
        rgb(38, 127, 153),  // syntaxType
        rgb(0, 0, 0),       // syntaxOperator
        rgb(0, 0, 0),       // syntaxPunctuation
        rgb(176, 176, 176), // thinkingOff
        rgb(118, 118, 118), // thinkingMinimal
        rgb(84, 125, 167),  // thinkingLow
        rgb(90, 128, 128),  // thinkingMedium
        rgb(135, 95, 135),  // thinkingHigh
        rgb(139, 0, 139),   // thinkingXhigh
        rgb(175, 0, 95),    // thinkingMax
        rgb(88, 132, 88),   // bashMode
    ]
}

const fn light_bg() -> [Rgb; 6] {
    [
        rgb(208, 208, 224), // selectedBg
        rgb(232, 232, 232), // userMessageBg
        rgb(237, 231, 246), // customMessageBg
        rgb(232, 232, 240), // toolPendingBg
        rgb(232, 240, 232), // toolSuccessBg
        rgb(240, 232, 232), // toolErrorBg
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed_theme(overrides: &[(&str, serde_json::Value)]) -> ThemeJson {
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
        .expect("test theme should parse")
    }

    #[test]
    fn dark_accent_resolves() {
        let th = dark();
        assert_eq!(th.fg_rgb(ThemeColor::Accent), Rgb(138, 190, 183));
    }

    #[test]
    fn builtin_literal_black_remains_a_color() {
        let theme = light();
        assert!(!theme.is_fg_empty(ThemeColor::SyntaxOperator));
        assert_eq!(
            theme.fg_ansi(ThemeColor::SyntaxOperator),
            "\x1b[38;2;0;0;0m"
        );
    }

    #[test]
    fn json_black_is_distinct_from_default() {
        let theme = parsed_theme(&[
            ("accent", serde_json::json!("#000000")),
            ("muted", serde_json::json!("")),
            ("selectedBg", serde_json::json!("#000000")),
            ("toolErrorBg", serde_json::json!("")),
        ])
        .resolve_owned(ColorMode::Truecolor)
        .expect("test theme should resolve");

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
    }

    #[test]
    fn json_indexed_colors_emit_exact_sequences_in_every_mode() {
        let parsed = parsed_theme(&[
            ("accent", serde_json::json!(17)),
            ("selectedBg", serde_json::json!(231)),
        ]);

        for mode in [ColorMode::Truecolor, ColorMode::Palette256] {
            let theme = parsed
                .resolve_owned(mode)
                .expect("test theme should resolve");
            assert_eq!(theme.fg_ansi(ThemeColor::Accent), "\x1b[38;5;17m");
            assert_eq!(theme.bg_ansi(ThemeBg::SelectedBg), "\x1b[48;5;231m");
            assert_eq!(theme.fg_rgb(ThemeColor::Accent), Rgb(17, 17, 17));
            assert_eq!(theme.bg_rgb(ThemeBg::SelectedBg), Rgb(231, 231, 231));
        }
    }
}
