//! Theme loading, thinking-ramp ordering, and canonical snapshot parsing.

use sonic_rs::{JsonContainerTrait, JsonValueTrait};
use crate::color::{parse_hex, Rgb};
use crate::palette;

/// Pinned thresholds from issue #79.
pub const THRESHOLD_DE2000: f64 = 2.3;
pub const THRESHOLD_DE2000_RATIO: f64 = 1.25;
pub const THRESHOLD_WCAG_AA_NORMAL: f64 = 4.5;
pub const THRESHOLD_WCAG_AA_LARGE: f64 = 3.0;
pub const THRESHOLD_WCAG_MINIMUM: f64 = 1.3;

/// Color mode.
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

    #[must_use]
    pub fn default_bg(self) -> Rgb {
        match self {
            Self::Dark => Rgb(0, 0, 0),
            Self::Light => Rgb(255, 255, 255),
        }
    }
}

/// Thinking-ramp rungs in ascending order (off → max).
pub const THINKING_RAMP: &[&str] = &[
    "thinkingOff",
    "thinkingMinimal",
    "thinkingLow",
    "thinkingMedium",
    "thinkingHigh",
    "thinkingXhigh",
    "thinkingMax",
];

/// A parsed theme: slot name → RGB.
#[derive(Debug)]
pub struct ThemeColors {
    pub polarity: ThemePolarity,
    pub colors: Vec<(&'static str, Rgb)>,
}

impl ThemeColors {
    #[must_use]
    pub fn get(&self, name: &str) -> Option<Rgb> {
        self.colors
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, rgb)| *rgb)
    }

    #[must_use]
    pub fn bg_rgb(&self, name: &str) -> Rgb {
        self.get(name).unwrap_or(self.polarity.default_bg())
    }

    #[must_use]
    pub fn fg_rgb(&self, name: &str, mode: ColorMode) -> Rgb {
        let raw = self.get(name).unwrap_or(self.polarity.default_bg());
        match mode {
            ColorMode::Truecolor => raw,
            ColorMode::Forced256 => palette::downsample_256(raw),
        }
    }

    /// Default terminal background RGB.
    #[must_use]
    pub fn default_bg_rgb(&self) -> Rgb {
        self.polarity.default_bg()
    }
}

/// Parse the dark theme JSON (embedded at compile time).
#[must_use]
pub fn dark_theme() -> ThemeColors {
    parse_theme(
        ThemePolarity::Dark,
        include_str!("../../../crates/pi/assets/theme/dark.json"),
    )
}

/// Parse the light theme JSON (embedded at compile time).
#[must_use]
pub fn light_theme() -> ThemeColors {
    parse_theme(
        ThemePolarity::Light,
        include_str!("../../../crates/pi/assets/theme/light.json"),
    )
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
        self.below_aa_normal
            || self.below_aa_large
            || self.below_minimum
            || self.below_de2000_and_ratio
    }

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

#[must_use]
pub fn evaluate_thresholds(wcag: f64, de2000: f64) -> ThresholdFlags {
    ThresholdFlags {
        below_aa_normal: wcag < THRESHOLD_WCAG_AA_NORMAL,
        below_aa_large: wcag < THRESHOLD_WCAG_AA_LARGE,
        below_minimum: wcag < THRESHOLD_WCAG_MINIMUM,
        below_de2000_and_ratio: de2000 < THRESHOLD_DE2000 && wcag < THRESHOLD_DE2000_RATIO,
    }
}
