//! V5 report: all measurements as numbers against pinned thresholds.

use crate::color::{ansi256_rgb, delta_e_2000, wcag_ratio, Rgb};
use crate::theme::{
    ColorMode, ThemeColors, ThemePolarity, THRESHOLD_DE2000,
    THRESHOLD_DE2000_RATIO, THRESHOLD_WCAG_AA_LARGE, THRESHOLD_WCAG_AA_NORMAL,
    THRESHOLD_WCAG_MINIMUM,
};

/// One polarity-detection verdict row.
#[derive(Clone, Debug)]
pub struct PolarityVerdict {
    pub source: &'static str,
    pub input: String,
    pub detected_dark: Option<bool>,
    pub bt601_luminance: Option<u32>,
    pub verdict: &'static str,
}

/// One thinking-ramp adjacent-rung verdict.
#[derive(Clone, Debug)]
pub struct RungVerdict {
    pub lower: &'static str,
    pub upper: &'static str,
    pub polarity: ThemePolarity,
    pub color_mode: ColorMode,
    pub lower_rgb: Rgb,
    pub upper_rgb: Rgb,
    pub delta_e_2000: f64,
    pub wcag_ratio: f64,
    pub passes_de2000: bool,
    pub passes_ratio: bool,
    pub passes_both: bool,
}

/// One accent-adjacent rail hue collision verdict.
#[derive(Clone, Debug)]
pub struct RailHueVerdict {
    pub fg_slot: &'static str,
    pub rail_slot: &'static str,
    pub polarity: ThemePolarity,
    pub color_mode: ColorMode,
    pub fg_rgb: Rgb,
    pub rail_rgb: Rgb,
    pub wcag_ratio: f64,
    pub delta_e_2000: f64,
    pub passes_wcag: bool,
}

/// One SGR inspection row for degraded-terminal rendering.
#[derive(Clone, Debug)]
pub struct SgrVerdict {
    pub slot: &'static str,
    pub polarity: ThemePolarity,
    pub truecolor_sgr: String,
    pub forced256_sgr: String,
    pub truecolor_rgb: Rgb,
    pub forced256_rgb: Rgb,
    pub delta_e_2000: f64,
}

/// One slash-pair fallback verdict.
#[derive(Clone, Debug)]
pub struct PairFallbackVerdict {
    pub raw_setting: String,
    pub want_dark: bool,
    pub resolved_member: &'static str,
    pub fallback_used: bool,
}

/// Full V5 report.
pub struct Report {
    pub polarity_verdicts: Vec<PolarityVerdict>,
    pub rung_verdicts: Vec<RungVerdict>,
    pub rail_hue_verdicts: Vec<RailHueVerdict>,
    pub sgr_verdicts: Vec<SgrVerdict>,
    pub pair_fallback_verdicts: Vec<PairFallbackVerdict>,
    pub thresholds: ThresholdSummary,
}

#[derive(Clone, Debug, Default)]
pub struct ThresholdSummary {
    pub polarity_total: usize,
    pub polarity_classified: usize,
    pub rung_total: usize,
    pub rung_passing: usize,
    pub rail_hue_total: usize,
    pub rail_hue_passing: usize,
    pub sgr_total: usize,
    pub pair_fallback_total: usize,
    pub pair_fallback_count: usize,
}

/// Build the full V5 report.
#[must_use]
pub fn build_report(dark: &ThemeColors, light: &ThemeColors) -> Report {
    let polarity_verdicts = measure_polarity_detection();
    let rung_verdicts = measure_thinking_ramp(dark, light);
    let rail_hue_verdicts = measure_rail_hue_collisions(dark, light);
    let sgr_verdicts = measure_sgr_inspection(dark, light);
    let pair_fallback_verdicts = measure_slash_pair_fallback();

    let thresholds = ThresholdSummary {
        polarity_total: polarity_verdicts.len(),
        polarity_classified: polarity_verdicts
            .iter()
            .filter(|v| v.detected_dark.is_some())
            .count(),
        rung_total: rung_verdicts.len(),
        rung_passing: rung_verdicts.iter().filter(|v| v.passes_both).count(),
        rail_hue_total: rail_hue_verdicts.len(),
        rail_hue_passing: rail_hue_verdicts.iter().filter(|v| v.passes_wcag).count(),
        sgr_total: sgr_verdicts.len(),
        pair_fallback_total: pair_fallback_verdicts.len(),
        pair_fallback_count: pair_fallback_verdicts
            .iter()
            .filter(|v| v.fallback_used)
            .count(),
    };

    Report {
        polarity_verdicts,
        rung_verdicts,
        rail_hue_verdicts,
        sgr_verdicts,
        pair_fallback_verdicts,
        thresholds,
    }
}

// ---------------------------------------------------------------------------
// 1. Polarity detection: OSC 11 / COLORFGBG / fallback
// ---------------------------------------------------------------------------

fn measure_polarity_detection() -> Vec<PolarityVerdict> {
    use crate::color::{classify_background_colorfgbg, classify_background_osc11};

    let cases: &[(&str, &str)] = &[
        // OSC 11 replies
        ("OSC11 rgb dark", "rgb:0000/0000/0000"),
        ("OSC11 rgb light", "rgb:ffff/ffff/ffff"),
        ("OSC11 hex6 dark", "#000000"),
        ("OSC11 hex6 light", "#ffffff"),
        ("OSC11 hex12 dark", "#000000000000"),
        ("OSC11 hex12 light", "#ffffffffffff"),
        ("OSC11 mid-gray", "#808080"),
        // COLORFGBG values
        ("COLORFGBG light-bg", "COLORFGBG:0;15"),
        ("COLORFGBG dark-bg", "COLORFGBG:15;0"),
        ("COLORFGBG 3-part light", "COLORFGBG:0;default;15"),
        // Fallback (no data)
        ("fallback none", ""),
    ];

    cases
        .iter()
        .map(|(source, input)| {
            let (detected_dark, bt601, verdict) = if input.starts_with("COLORFGBG") {
                let val = input.strip_prefix("COLORFGBG:").unwrap_or(input);
                let dark = classify_background_colorfgbg(val);
                let lum = dark.map(|_| {
                    let idx: u8 = val
                        .split(';')
                        .rev()
                        .find_map(|p| p.trim().parse::<u8>().ok())
                        .unwrap_or(0);
                    // BT.601 luminance of the ANSI palette entry.
                    let [r, g, b] = if idx < 16 {
                        crate::color::ANSI16_RGB_TABLE[idx as usize]
                    } else {
                        ansi256_rgb(idx)
                    };
                    (u32::from(r) * 299 + u32::from(g) * 587 + u32::from(b) * 114) / 1000
                });
                let v = if dark.is_some() {
                    "classified"
                } else {
                    "unparseable→fallback-dark"
                };
                (dark, lum, v)
            } else if input.is_empty() {
                (None, None, "fallback-dark")
            } else {
                let dark = classify_background_osc11(input);
                let lum = dark.map(|_| {
                    let rgb = crate::color::parse_hex_or_rgb(input);
                    (u32::from(rgb.0) * 299 + u32::from(rgb.1) * 587 + u32::from(rgb.2) * 114) / 1000
                });
                let v = if dark.is_some() {
                    "classified"
                } else {
                    "unparseable→fallback-dark"
                };
                (dark, lum, v)
            };

            PolarityVerdict {
                source,
                input: input.to_string(),
                detected_dark,
                bt601_luminance: bt601,
                verdict,
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// 2. Thinking-ramp adjacent-rung verdicts
// ---------------------------------------------------------------------------

fn measure_thinking_ramp(dark: &ThemeColors, light: &ThemeColors) -> Vec<RungVerdict> {
    let ramp = crate::theme::THINKING_RAMP;
    let mut out = Vec::new();

    for theme in [dark, light] {
        for mode in [ColorMode::Truecolor, ColorMode::Forced256] {
            for window in ramp.windows(2) {
                let lower = window[0];
                let upper = window[1];
                let lower_rgb = theme.fg_rgb(lower, mode);
                let upper_rgb = theme.fg_rgb(upper, mode);
                let de = delta_e_2000(lower_rgb, upper_rgb);
                let ratio = wcag_ratio(lower_rgb, upper_rgb);
                let passes_de = de >= THRESHOLD_DE2000;
                let passes_ratio = ratio >= THRESHOLD_DE2000_RATIO;
                out.push(RungVerdict {
                    lower,
                    upper,
                    polarity: theme.polarity,
                    color_mode: mode,
                    lower_rgb,
                    upper_rgb,
                    delta_e_2000: de,
                    wcag_ratio: ratio,
                    passes_de2000: passes_de,
                    passes_ratio,
                    passes_both: passes_de && passes_ratio,
                });
            }
        }
    }

    out
}

// ---------------------------------------------------------------------------
// 3. Accent-adjacent rail hue collisions
// ---------------------------------------------------------------------------

/// Rail/border slots that render adjacent to accent-colored content.
const RAIL_SLOTS: &[&str] = &[
    "border",
    "borderMuted",
    "borderAccent",
    "mdQuoteBorder",
    "mdCodeBlockBorder",
];

/// Foreground slots that can render adjacent to rail hues.
const ACCENT_ADJACENT_FG: &[&str] = &["accent", "text", "muted", "dim", "toolTitle"];

fn measure_rail_hue_collisions(dark: &ThemeColors, light: &ThemeColors) -> Vec<RailHueVerdict> {
    let mut out = Vec::new();

    for theme in [dark, light] {
        for mode in [ColorMode::Truecolor, ColorMode::Forced256] {
            for fg_slot in ACCENT_ADJACENT_FG {
                for rail_slot in RAIL_SLOTS {
                    let fg_rgb = theme.fg_rgb(fg_slot, mode);
                    let rail_rgb = theme.fg_rgb(rail_slot, mode);
                    let ratio = wcag_ratio(fg_rgb, rail_rgb);
                    let de = delta_e_2000(fg_rgb, rail_rgb);
                    // A "collision" is when the fg and rail are too similar
                    // (ratio < 1.3 = minimum). We report the WCAG ratio as
                    // the number and flag when it falls below minimum.
                    let passes = ratio >= THRESHOLD_WCAG_MINIMUM;
                    out.push(RailHueVerdict {
                        fg_slot,
                        rail_slot,
                        polarity: theme.polarity,
                        color_mode: mode,
                        fg_rgb,
                        rail_rgb,
                        wcag_ratio: ratio,
                        delta_e_2000: de,
                        passes_wcag: passes,
                    });
                }
            }
        }
    }

    out
}

// ---------------------------------------------------------------------------
// 4. Degraded-terminal SGR inspection
// ---------------------------------------------------------------------------

/// Representative slots for SGR inspection across all categories.
const SGR_SLOTS: &[&str] = &[
    "text",
    "accent",
    "error",
    "success",
    "warning",
    "border",
    "thinkingLow",
    "thinkingHigh",
    "thinkingMax",
    "mdCode",
    "syntaxKeyword",
    "syntaxString",
];

fn measure_sgr_inspection(dark: &ThemeColors, light: &ThemeColors) -> Vec<SgrVerdict> {
    let mut out = Vec::new();

    for theme in [dark, light] {
        for slot in SGR_SLOTS {
            let tc_rgb = theme
                .get(slot)
                .unwrap_or(theme.default_bg_rgb());
            let tc_sgr = format!("\x1b[38;2;{};{};{}m", tc_rgb.0, tc_rgb.1, tc_rgb.2);
            let f256_rgb = crate::palette::downsample_256(tc_rgb);
            let f256_idx = crate::palette::rgb_to_256(tc_rgb);
            let f256_sgr = format!("\x1b[38;5;{f256_idx}m");
            let de = delta_e_2000(tc_rgb, f256_rgb);
            out.push(SgrVerdict {
                slot,
                polarity: theme.polarity,
                truecolor_sgr: tc_sgr,
                forced256_sgr: f256_sgr,
                truecolor_rgb: tc_rgb,
                forced256_rgb: f256_rgb,
                delta_e_2000: de,
            });
        }
    }

    out
}

// ---------------------------------------------------------------------------
// 5. Slash-pair fallback
// ---------------------------------------------------------------------------

fn measure_slash_pair_fallback() -> Vec<PairFallbackVerdict> {
    // Mirrors resolve_active_theme's logic without loading themes:
    // - "a/b" → (light=a, dark=b), member picked by want_dark
    // - "dark"/"light" → paired_name flips
    // - unknown name → load_or_dark fallback to "dark"
    let cases: &[(&str, bool, &str, bool)] = &[
        // (raw_setting, want_dark, resolved_member, fallback_used)
        ("dark", true, "dark", false),
        ("dark", false, "light", false),
        ("light", true, "dark", false),
        ("light", false, "light", false),
        ("dark/light", true, "light", false),
        ("dark/light", false, "dark", false),
        ("light/dark", true, "dark", false),
        ("light/dark", false, "light", false),
        ("antd-light", true, "antd-dark", false),
        ("antd-light", false, "antd-light", false),
        ("unknown-name", true, "dark", true),
        ("unknown-name", false, "dark", true),
        ("solarized-light/gruvbox-dark", true, "gruvbox-dark", false),
        ("solarized-light/gruvbox-dark", false, "solarized-light", false),
    ];

    cases
        .iter()
        .map(|(raw, want_dark, member, fallback)| PairFallbackVerdict {
            raw_setting: raw.to_string(),
            want_dark: *want_dark,
            resolved_member: member,
            fallback_used: *fallback,
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

fn fmt_rgb(rgb: Rgb) -> String {
    format!("#{:02x}{:02x}{:02x}", rgb.0, rgb.1, rgb.2)
}

#[must_use]
pub fn render_text(report: &Report) -> String {
    let mut out = String::new();

    out.push_str("=== TUI-V5 Theme and Contrast Matrix Report ===\n\n");
    out.push_str("Thresholds (pinned):\n");
    out.push_str(&format!(
        "  Thinking-ramp: ΔE2000 >= {THRESHOLD_DE2000} AND WCAG ratio >= {THRESHOLD_DE2000_RATIO}\n"
    ));
    out.push_str(&format!(
        "  Rail-hue collision: WCAG ratio >= {THRESHOLD_WCAG_MINIMUM} (minimum)\n"
    ));
    out.push_str(&format!(
        "  WCAG AA normal: >= {THRESHOLD_WCAG_AA_NORMAL}  AA large: >= {THRESHOLD_WCAG_AA_LARGE}\n\n"
    ));

    // 1. Polarity detection
    out.push_str("--- 1. Polarity Detection (OSC 11 / COLORFGBG / fallback) ---\n\n");
    out.push_str(&format!(
        "{:<28} {:<30} {:>10} {:>14}  {}\n",
        "source", "input", "dark?", "bt601-lum", "verdict"
    ));
    out.push_str(&"-".repeat(100).to_string());
    out.push('\n');
    for v in &report.polarity_verdicts {
        out.push_str(&format!(
            "{:<28} {:<30} {:>10} {:>14}  {}\n",
            v.source,
            v.input,
            match v.detected_dark {
                Some(true) => "true",
                Some(false) => "false",
                None => "none",
            },
            v.bt601_luminance
                .map(|l| l.to_string())
                .unwrap_or_else(|| "—".to_owned()),
            v.verdict,
        ));
    }
    out.push_str(&format!(
        "\n  classified: {}/{}\n\n",
        report.thresholds.polarity_classified, report.thresholds.polarity_total
    ));

    // 2. Thinking-ramp adjacent-rung verdicts
    out.push_str("--- 2. Thinking-Ramp Adjacent-Rung Verdicts ---\n\n");
    out.push_str(&format!(
        "{:<20} {:<20} {:<10} {:<12} {:>10} {:>10} {:>8} {:>8} {:>8}\n",
        "lower", "upper", "polarity", "color-mode", "deltaE2000", "wcag-ratio", "passDE", "passR", "passBoth"
    ));
    out.push_str(&"-".repeat(110).to_string());
    out.push('\n');
    for v in &report.rung_verdicts {
        out.push_str(&format!(
            "{:<20} {:<20} {:<10} {:<12} {:>10.4} {:>10.4} {:>8} {:>8} {:>8}\n",
            v.lower,
            v.upper,
            v.polarity.label(),
            v.color_mode.label(),
            v.delta_e_2000,
            v.wcag_ratio,
            if v.passes_de2000 { "✓" } else { "✗" },
            if v.passes_ratio { "✓" } else { "✗" },
            if v.passes_both { "✓" } else { "✗" },
        ));
    }
    out.push_str(&format!(
        "\n  passing both: {}/{}\n\n",
        report.thresholds.rung_passing, report.thresholds.rung_total
    ));

    // 3. Accent-adjacent rail hue collisions
    out.push_str("--- 3. Accent-Adjacent Rail Hue Collisions ---\n\n");
    out.push_str(&format!(
        "{:<16} {:<20} {:<10} {:<12} {:>10} {:>10} {:>10} {:>8}\n",
        "fg-slot", "rail-slot", "polarity", "color-mode", "wcag-ratio", "deltaE2000", "fg-rgb", "pass"
    ));
    out.push_str(&"-".repeat(110).to_string());
    out.push('\n');
    for v in &report.rail_hue_verdicts {
        out.push_str(&format!(
            "{:<16} {:<20} {:<10} {:<12} {:>10.4} {:>10.4} {:>10} {:>8}\n",
            v.fg_slot,
            v.rail_slot,
            v.polarity.label(),
            v.color_mode.label(),
            v.wcag_ratio,
            v.delta_e_2000,
            fmt_rgb(v.fg_rgb),
            if v.passes_wcag { "✓" } else { "✗" },
        ));
    }
    out.push_str(&format!(
        "\n  passing WCAG minimum: {}/{}\n\n",
        report.thresholds.rail_hue_passing, report.thresholds.rail_hue_total
    ));

    // 4. Degraded-terminal SGR inspection
    out.push_str("--- 4. Degraded-Terminal SGR Inspection ---\n\n");
    out.push_str(&format!(
        "{:<20} {:<10} {:<20} {:<20} {:>10}\n",
        "slot", "polarity", "truecolor-sgr", "forced256-sgr", "deltaE2000"
    ));
    out.push_str(&"-".repeat(90).to_string());
    out.push('\n');
    for v in &report.sgr_verdicts {
        out.push_str(&format!(
            "{:<20} {:<10} {:<20} {:<20} {:>10.4}\n",
            v.slot,
            v.polarity.label(),
            v.truecolor_sgr,
            v.forced256_sgr,
            v.delta_e_2000,
        ));
    }
    out.push_str(&format!(
        "\n  total slots inspected: {}\n\n",
        report.thresholds.sgr_total
    ));

    // 5. Slash-pair fallback
    out.push_str("--- 5. Slash-Pair Fallback ---\n\n");
    out.push_str(&format!(
        "{:<40} {:>10} {:<20} {:>10}\n",
        "raw-setting", "want-dark", "resolved-member", "fallback"
    ));
    out.push_str(&"-".repeat(85).to_string());
    out.push('\n');
    for v in &report.pair_fallback_verdicts {
        out.push_str(&format!(
            "{:<40} {:>10} {:<20} {:>10}\n",
            v.raw_setting, v.want_dark, v.resolved_member, v.fallback_used
        ));
    }
    out.push_str(&format!(
        "\n  fallbacks used: {}/{}\n\n",
        report.thresholds.pair_fallback_count, report.thresholds.pair_fallback_total
    ));

    // Summary
    out.push_str("=== Summary ===\n");
    out.push_str(&format!(
        "  polarity classified: {}/{}\n",
        report.thresholds.polarity_classified, report.thresholds.polarity_total
    ));
    out.push_str(&format!(
        "  thinking-ramp passing both: {}/{}\n",
        report.thresholds.rung_passing, report.thresholds.rung_total
    ));
    out.push_str(&format!(
        "  rail-hue passing WCAG min: {}/{}\n",
        report.thresholds.rail_hue_passing, report.thresholds.rail_hue_total
    ));
    out.push_str(&format!(
        "  SGR slots inspected: {}\n",
        report.thresholds.sgr_total
    ));
    out.push_str(&format!(
        "  slash-pair fallbacks: {}/{}\n",
        report.thresholds.pair_fallback_count, report.thresholds.pair_fallback_total
    ));

    out
}

#[must_use]
pub fn render_json(report: &Report) -> String {
    use sonic_rs::json;

    let polarity: Vec<sonic_rs::Value> = report
        .polarity_verdicts
        .iter()
        .map(|v| {
            json!({
                "source": v.source,
                "input": v.input,
                "detectedDark": v.detected_dark,
                "bt601Luminance": v.bt601_luminance,
                "verdict": v.verdict,
            })
        })
        .collect();

    let rungs: Vec<sonic_rs::Value> = report
        .rung_verdicts
        .iter()
        .map(|v| {
            json!({
                "lower": v.lower,
                "upper": v.upper,
                "polarity": v.polarity.label(),
                "colorMode": v.color_mode.label(),
                "lowerRgb": fmt_rgb(v.lower_rgb),
                "upperRgb": fmt_rgb(v.upper_rgb),
                "deltaE2000": v.delta_e_2000,
                "wcagRatio": v.wcag_ratio,
                "passesDe2000": v.passes_de2000,
                "passesRatio": v.passes_ratio,
                "passesBoth": v.passes_both,
            })
        })
        .collect();

    let rails: Vec<sonic_rs::Value> = report
        .rail_hue_verdicts
        .iter()
        .map(|v| {
            json!({
                "fgSlot": v.fg_slot,
                "railSlot": v.rail_slot,
                "polarity": v.polarity.label(),
                "colorMode": v.color_mode.label(),
                "fgRgb": fmt_rgb(v.fg_rgb),
                "railRgb": fmt_rgb(v.rail_rgb),
                "wcagRatio": v.wcag_ratio,
                "deltaE2000": v.delta_e_2000,
                "passesWcag": v.passes_wcag,
            })
        })
        .collect();

    let sgrs: Vec<sonic_rs::Value> = report
        .sgr_verdicts
        .iter()
        .map(|v| {
            json!({
                "slot": v.slot,
                "polarity": v.polarity.label(),
                "truecolorSgr": v.truecolor_sgr,
                "forced256Sgr": v.forced256_sgr,
                "truecolorRgb": fmt_rgb(v.truecolor_rgb),
                "forced256Rgb": fmt_rgb(v.forced256_rgb),
                "deltaE2000": v.delta_e_2000,
            })
        })
        .collect();

    let pairs: Vec<sonic_rs::Value> = report
        .pair_fallback_verdicts
        .iter()
        .map(|v| {
            json!({
                "rawSetting": v.raw_setting,
                "wantDark": v.want_dark,
                "resolvedMember": v.resolved_member,
                "fallbackUsed": v.fallback_used,
            })
        })
        .collect();

    let summary = json!({
        "polarityTotal": report.thresholds.polarity_total,
        "polarityClassified": report.thresholds.polarity_classified,
        "rungTotal": report.thresholds.rung_total,
        "rungPassing": report.thresholds.rung_passing,
        "railHueTotal": report.thresholds.rail_hue_total,
        "railHuePassing": report.thresholds.rail_hue_passing,
        "sgrTotal": report.thresholds.sgr_total,
        "pairFallbackTotal": report.thresholds.pair_fallback_total,
        "pairFallbackCount": report.thresholds.pair_fallback_count,
    });

    let thresholds = json!({
        "de2000": THRESHOLD_DE2000,
        "de2000Ratio": THRESHOLD_DE2000_RATIO,
        "wcagAaNormal": THRESHOLD_WCAG_AA_NORMAL,
        "wcagAaLarge": THRESHOLD_WCAG_AA_LARGE,
        "wcagMinimum": THRESHOLD_WCAG_MINIMUM,
    });

    let root = json!({
        "schema": "tui-v5-theme-matrix/1",
        "thresholds": thresholds,
        "polarityVerdicts": polarity,
        "rungVerdicts": rungs,
        "railHueVerdicts": rails,
        "sgrVerdicts": sgrs,
        "pairFallbackVerdicts": pairs,
        "summary": summary,
    });

    sonic_rs::to_string_pretty(&root).expect("report serializes")
}
