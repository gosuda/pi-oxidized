//! Theme loading, fg/bg pair definitions, and canonical snapshot parsing.
//!
//! The "canonical schema-v1 snapshots" are the built-in theme JSON files
//! (`dark.json`, `light.json`) — the deterministic source of color values,
//! never timing-dependent PTY captures.
use sonic_rs::{JsonContainerTrait, JsonValueTrait};
use crate::color::Rgb;
use crate::palette;

/// Pinned WCAG and perceptual thresholds from issue #58.
pub const THRESHOLD_WCAG_AA_NORMAL: f64 = 4.5;
pub const THRESHOLD_WCAG_AA_LARGE: f64 = 3.0;
pub const THRESHOLD_WCAG_MINIMUM: f64 = 1.3;
pub const THRESHOLD_DE2000: f64 = 2.3;
pub const THRESHOLD_DE2000_RATIO: f64 = 1.25;

/// Color mode: truecolor (24-bit) or forced-256 (palette downsampled).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ColorMode {
    Truecolor,
    Forced256,
}

impl ColorMode {
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Truecolor => "truecolor",
            Self::Forced256 => "forced-256",
        }
    }
}

/// Theme polarity.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ThemePolarity {
    Dark,
    Light,
}

impl ThemePolarity {
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Dark => "dark",
            Self::Light => "light",
        }
    }

    /// Default terminal background RGB for this polarity.
    /// Dark terminals default to black; light terminals to white.
    /// Matches the OSC-11 probe reply in the testkit profile (rgb:0000/0000/0000
    /// for dark) and the standard light-terminal convention.
    #[must_use]
    pub fn default_bg(self) -> Rgb {
        match self {
            Self::Dark => Rgb(0, 0, 0),
            Self::Light => Rgb(255, 255, 255),
        }
    }
}

/// A foreground color slot name.
pub type FgSlot = &'static str;

/// A background color slot name, or "default" for the terminal default.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BgSlot {
    Default,
    Named(&'static str),
}

impl BgSlot {
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Named(name) => name,
        }
    }
}

/// One fg/bg pair to inspect.
#[derive(Clone, Copy, Debug)]
pub struct ColorPair {
    pub fg: FgSlot,
    pub bg: BgSlot,
    /// Human-readable category for grouping in the report.
    pub category: &'static str,
}

/// The inspected fg/bg pairs, derived from the issue's enumeration:
/// text, dim, muted, thinkingText, toolOutput, quote, link suffix,
/// diff add/remove, syntax, border/rail hues — plus contextual pairs
/// where foregrounds render against theme-defined backgrounds.
#[must_use]
pub fn inspected_pairs() -> Vec<ColorPair> {
    let default = BgSlot::Default;
    // (fg, bg, category)
    let pairs: &[(&str, BgSlot, &str)] = &[
        // --- core text on default background ---
        ("text", default, "text"),
        ("dim", default, "text"),
        ("muted", default, "text"),
        ("thinkingText", default, "text"),
        ("toolOutput", default, "text"),
        // --- markdown ---
        ("mdQuote", default, "quote"),
        ("mdLinkUrl", default, "link-suffix"),
        ("mdLink", default, "link"),
        ("mdHeading", default, "markdown"),
        ("mdCode", default, "markdown"),
        ("mdCodeBlock", default, "markdown"),
        ("mdListBullet", default, "markdown"),
        ("mdHr", default, "markdown"),
        // --- diff ---
        ("toolDiffAdded", default, "diff-add"),
        ("toolDiffRemoved", default, "diff-remove"),
        ("toolDiffContext", default, "diff-context"),
        // --- syntax ---
        ("syntaxComment", default, "syntax"),
        ("syntaxKeyword", default, "syntax"),
        ("syntaxFunction", default, "syntax"),
        ("syntaxVariable", default, "syntax"),
        ("syntaxString", default, "syntax"),
        ("syntaxNumber", default, "syntax"),
        ("syntaxType", default, "syntax"),
        ("syntaxOperator", default, "syntax"),
        ("syntaxPunctuation", default, "syntax"),
        // --- border / rail hues ---
        ("border", default, "border"),
        ("borderMuted", default, "border"),
        ("borderAccent", default, "border"),
        ("mdQuoteBorder", default, "border"),
        ("mdCodeBlockBorder", default, "border"),
        ("thinkingOff", default, "border"),
        ("thinkingMinimal", default, "border"),
        ("thinkingLow", default, "border"),
        ("thinkingMedium", default, "border"),
        ("thinkingHigh", default, "border"),
        ("thinkingXhigh", default, "border"),
        ("thinkingMax", default, "border"),
        ("bashMode", default, "border"),
        // --- accent / status ---
        ("accent", default, "accent"),
        ("success", default, "status"),
        ("error", default, "status"),
        ("warning", default, "status"),
        ("toolTitle", default, "tool"),
        // --- contextual: fg on theme-defined backgrounds ---
        ("userMessageText", BgSlot::Named("userMessageBg"), "user-message"),
        ("customMessageText", BgSlot::Named("customMessageBg"), "custom-message"),
        ("customMessageLabel", BgSlot::Named("customMessageBg"), "custom-message"),
        ("toolOutput", BgSlot::Named("toolPendingBg"), "tool-pending"),
        ("toolOutput", BgSlot::Named("toolSuccessBg"), "tool-success"),
        ("toolOutput", BgSlot::Named("toolErrorBg"), "tool-error"),
        ("toolDiffAdded", BgSlot::Named("toolSuccessBg"), "diff-add-on-success-bg"),
        ("toolDiffRemoved", BgSlot::Named("toolErrorBg"), "diff-remove-on-error-bg"),
        ("text", BgSlot::Named("selectedBg"), "selected"),
        ("accent", BgSlot::Named("selectedBg"), "selected"),
        ("toolTitle", BgSlot::Named("toolPendingBg"), "tool-pending"),
        ("toolTitle", BgSlot::Named("toolSuccessBg"), "tool-success"),
        ("toolTitle", BgSlot::Named("toolErrorBg"), "tool-error"),
    ];

    pairs
        .iter()
        .map(|&(fg, bg, category)| ColorPair { fg, bg, category })
        .collect()
}

/// Parse a `#rrggbb` hex string into an [`Rgb`].
fn parse_hex(hex: &str) -> Option<Rgb> {
    let h = hex.strip_prefix('#')?;
    if h.len() != 6 {
        return None;
    }
    let r = u8::from_str_radix(&h[0..2], 16).ok()?;
    let g = u8::from_str_radix(&h[2..4], 16).ok()?;
    let b = u8::from_str_radix(&h[4..6], 16).ok()?;
    Some(Rgb(r, g, b))
}

/// A parsed theme: slot name → RGB.
#[derive(Debug)]
pub struct ThemeColors {
    pub polarity: ThemePolarity,
    pub colors: Vec<(&'static str, Rgb)>,
}

impl ThemeColors {
    /// Look up the RGB for a slot name.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<Rgb> {
        self.colors
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, rgb)| *rgb)
    }

    /// Resolve a background slot to RGB.
    #[must_use]
    pub fn bg_rgb(&self, slot: BgSlot) -> Rgb {
        match slot {
            BgSlot::Default => self.polarity.default_bg(),
            BgSlot::Named(name) => self.get(name).unwrap_or(self.polarity.default_bg()),
        }
    }

    /// Resolve a foreground slot to RGB in the given color mode.
    #[must_use]
    pub fn fg_rgb(&self, name: &str, mode: ColorMode) -> Option<Rgb> {
        let raw = self.get(name)?;
        Some(match mode {
            ColorMode::Truecolor => raw,
            ColorMode::Forced256 => palette::downsample_256(raw),
        })
    }

    /// Resolve a background slot to RGB in the given color mode.
    #[must_use]
    pub fn bg_rgb_mode(&self, slot: BgSlot, mode: ColorMode) -> Rgb {
        let raw = self.bg_rgb(slot);
        match mode {
            ColorMode::Truecolor => raw,
            ColorMode::Forced256 => palette::downsample_256(raw),
        }
    }

    /// Raw (truecolor) RGB for a slot — used for rung-delta computation.
    #[must_use]
    pub fn raw_rgb(&self, name: &str) -> Option<Rgb> {
        self.get(name)
    }
}

/// Parse the dark theme JSON (embedded at compile time).
#[must_use]
pub fn dark_theme() -> ThemeColors {
    parse_theme(ThemePolarity::Dark, include_str!("../../../crates/pi/assets/theme/dark.json"))
}

/// Parse the light theme JSON (embedded at compile time).
#[must_use]
pub fn light_theme() -> ThemeColors {
    parse_theme(ThemePolarity::Light, include_str!("../../../crates/pi/assets/theme/light.json"))
}

fn parse_theme(polarity: ThemePolarity, json: &str) -> ThemeColors {
    let val: sonic_rs::Value = sonic_rs::from_str(json).expect("theme JSON is valid");
    let colors_obj = val
        .get("colors")
        .expect("theme has colors object")
        .as_object()
        .expect("colors is an object");

    let mut colors = Vec::new();
    for (key, value) in colors_obj.iter() {
        if let Some(hex) = value.as_str() {
            if let Some(rgb) = parse_hex(hex) {
                // Leak the key to get a &'static str — this is a short-lived prototype.
                let key_static: &'static str = Box::leak(key.to_string().into_boxed_str());
                colors.push((key_static, rgb));
            }
        }
    }

    ThemeColors { polarity, colors }
}

/// Threshold flags for a single pair measurement.
#[derive(Clone, Debug)]
pub struct ThresholdFlags {
    pub below_aa_normal: bool,
    pub below_aa_large: bool,
    pub below_minimum: bool,
    pub below_de2000_and_ratio: bool,
}

impl ThresholdFlags {
    #[must_use]
    pub fn any(&self) -> bool {
        self.below_aa_normal || self.below_aa_large || self.below_minimum || self.below_de2000_and_ratio
    }

    /// Comma-separated list of failed threshold names.
    #[must_use]
    pub fn labels(&self) -> Vec<&'static str> {
        let mut out = Vec::new();
        if self.below_aa_normal {
            out.push("wcag-aa-normal<4.5");
        }
        if self.below_aa_large {
            out.push("wcag-aa-large<3.0");
        }
        if self.below_minimum {
            out.push("wcag-minimum<1.3");
        }
        if self.below_de2000_and_ratio {
            out.push("de2000<2.3+ratio<1.25");
        }
        out
    }
}

/// Evaluate thresholds for a WCAG ratio and ΔE2000.
#[must_use]
pub fn evaluate_thresholds(wcag: f64, de2000: f64) -> ThresholdFlags {
    ThresholdFlags {
        below_aa_normal: wcag < THRESHOLD_WCAG_AA_NORMAL,
        below_aa_large: wcag < THRESHOLD_WCAG_AA_LARGE,
        below_minimum: wcag < THRESHOLD_WCAG_MINIMUM,
        below_de2000_and_ratio: de2000 < THRESHOLD_DE2000 && wcag < THRESHOLD_DE2000_RATIO,
    }
}
