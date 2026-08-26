//! Report generation: per-pair measurement and JSON/text output.

use crate::color::{delta_e_2000, wcag_ratio, Rgb};
use crate::theme::{
    evaluate_thresholds, ColorMode, ThemeColors, ThemePolarity, THRESHOLD_DE2000,
    THRESHOLD_DE2000_RATIO, THRESHOLD_WCAG_AA_LARGE, THRESHOLD_WCAG_AA_NORMAL,
    THRESHOLD_WCAG_MINIMUM,
};

/// One measured fg/bg pair result.
#[derive(Clone, Debug)]
pub struct PairResult {
    pub fg_slot: &'static str,
    pub bg_slot: &'static str,
    pub category: &'static str,
    pub polarity: ThemePolarity,
    pub color_mode: ColorMode,
    pub fg_rgb: Rgb,
    pub bg_rgb: Rgb,
    pub wcag_ratio: f64,
    pub delta_e_2000: f64,
    pub flags: crate::theme::ThresholdFlags,
}

/// Rung delta: the perceptual distance between truecolor and forced-256
/// rendering of the same fg/bg pair. This measures how much information
/// is lost when downsampling to the 256-color palette.
#[derive(Clone, Debug)]
pub struct RungDelta {
    pub fg_slot: &'static str,
    pub bg_slot: &'static str,
    pub polarity: ThemePolarity,
    pub fg_truecolor: Rgb,
    pub fg_256: Rgb,
    pub bg_truecolor: Rgb,
    pub bg_256: Rgb,
    pub fg_delta_e: f64,
    pub bg_delta_e: f64,
}

/// Measure all pairs for one theme × color-mode combination.
#[must_use]
pub fn measure_pairs(
    theme: &ThemeColors,
    mode: ColorMode,
    pairs: &[crate::theme::ColorPair],
) -> Vec<PairResult> {
    pairs
        .iter()
        .map(|pair| {
            let fg_rgb = theme
                .fg_rgb(pair.fg, mode)
                .unwrap_or(theme.polarity.default_bg());
            let bg_rgb = theme.bg_rgb_mode(pair.bg, mode);

            let wcag = wcag_ratio(fg_rgb, bg_rgb);
            let de = delta_e_2000(fg_rgb, bg_rgb);
            let flags = evaluate_thresholds(wcag, de);

            PairResult {
                fg_slot: pair.fg,
                bg_slot: pair.bg.label(),
                category: pair.category,
                polarity: theme.polarity,
                color_mode: mode,
                fg_rgb,
                bg_rgb,
                wcag_ratio: wcag,
                delta_e_2000: de,
                flags,
            }
        })
        .collect()
}

/// Compute rung deltas (truecolor vs forced-256) for all pairs in one theme.
#[must_use]
pub fn compute_rung_deltas(
    theme: &ThemeColors,
    pairs: &[crate::theme::ColorPair],
) -> Vec<RungDelta> {
    pairs
        .iter()
        .map(|pair| {
            let fg_tc = theme
                .raw_rgb(pair.fg)
                .unwrap_or(theme.polarity.default_bg());
            let fg_256 = crate::palette::downsample_256(fg_tc);
            let bg_tc = theme.bg_rgb(pair.bg);
            let bg_256 = crate::palette::downsample_256(bg_tc);

            RungDelta {
                fg_slot: pair.fg,
                bg_slot: pair.bg.label(),
                polarity: theme.polarity,
                fg_truecolor: fg_tc,
                fg_256,
                bg_truecolor: bg_tc,
                bg_256,
                fg_delta_e: delta_e_2000(fg_tc, fg_256),
                bg_delta_e: delta_e_2000(bg_tc, bg_256),
            }
        })
        .collect()
}

/// Full report covering dark+light × truecolor+forced-256.
pub struct Report {
    pub results: Vec<PairResult>,
    pub rung_deltas: Vec<RungDelta>,
    pub thresholds: ThresholdSummary,
}

/// Summary of threshold violations across all measurements.
#[derive(Clone, Debug, Default)]
pub struct ThresholdSummary {
    pub total_pairs: usize,
    pub below_aa_normal: usize,
    pub below_aa_large: usize,
    pub below_minimum: usize,
    pub below_de2000_and_ratio: usize,
    pub total_flagged: usize,
}

/// Build the full report across all four combinations.
#[must_use]
pub fn build_report(
    dark: &ThemeColors,
    light: &ThemeColors,
    pairs: &[crate::theme::ColorPair],
) -> Report {
    let mut results = Vec::new();
    let mut rung_deltas = Vec::new();

    for theme in [dark, light] {
        for mode in [ColorMode::Truecolor, ColorMode::Forced256] {
            results.extend(measure_pairs(theme, mode, pairs));
        }
        rung_deltas.extend(compute_rung_deltas(theme, pairs));
    }

    let total_pairs = results.len();
    let below_aa_normal = results.iter().filter(|r| r.flags.below_aa_normal).count();
    let below_aa_large = results.iter().filter(|r| r.flags.below_aa_large).count();
    let below_minimum = results.iter().filter(|r| r.flags.below_minimum).count();
    let below_de2000_and_ratio = results
        .iter()
        .filter(|r| r.flags.below_de2000_and_ratio)
        .count();
    let total_flagged = results.iter().filter(|r| r.flags.any()).count();

    Report {
        results,
        rung_deltas,
        thresholds: ThresholdSummary {
            total_pairs,
            below_aa_normal,
            below_aa_large,
            below_minimum,
            below_de2000_and_ratio,
            total_flagged,
        },
    }
}

/// Format an RGB as `#rrggbb`.
fn fmt_rgb(rgb: Rgb) -> String {
    format!("#{:02x}{:02x}{:02x}", rgb.0, rgb.1, rgb.2)
}

/// Render the report as human-readable text.
#[must_use]
pub fn render_text(report: &Report) -> String {
    let mut out = String::new();

    out.push_str("=== TUI-P2 Deterministic Contrast Measurement Report ===\n\n");
    out.push_str("Thresholds (pinned):\n");
    out.push_str(&format!(
        "  WCAG AA normal text:  >= {THRESHOLD_WCAG_AA_NORMAL}\n"
    ));
    out.push_str(&format!(
        "  WCAG AA large text:   >= {THRESHOLD_WCAG_AA_LARGE}\n"
    ));
    out.push_str(&format!(
        "  WCAG minimum:        >= {THRESHOLD_WCAG_MINIMUM}\n"
    ));
    out.push_str(&format!(
        "  CIEDE2000:           >= {THRESHOLD_DE2000} (and WCAG ratio >= {THRESHOLD_DE2000_RATIO})\n"
    ));
    out.push_str("\n");

    // Group by polarity × color_mode
    for polarity in [ThemePolarity::Dark, ThemePolarity::Light] {
        for mode in [ColorMode::Truecolor, ColorMode::Forced256] {
            out.push_str(&format!(
                "--- {} / {} ---\n",
                polarity.label(),
                mode.label()
            ));
            out.push_str(&format!(
                "{:<24} {:<16} {:<10} {:>10} {:>10}  {}\n",
                "fg-slot", "bg-slot", "category", "wcag-ratio", "deltaE2000", "flags"
            ));
            out.push_str(&"-".repeat(90).to_string());
            out.push('\n');

            let subset: Vec<&PairResult> = report
                .results
                .iter()
                .filter(|r| r.polarity == polarity && r.color_mode == mode)
                .collect();

            for r in &subset {
                let flag_str = if r.flags.any() {
                    r.flags.labels().join(", ")
                } else {
                    "OK".to_string()
                };
                out.push_str(&format!(
                    "{:<24} {:<16} {:<10} {:>10.4} {:>10.4}  {}\n",
                    r.fg_slot,
                    r.bg_slot,
                    r.category,
                    r.wcag_ratio,
                    r.delta_e_2000,
                    flag_str
                ));
            }

            let flagged = subset.iter().filter(|r| r.flags.any()).count();
            out.push_str(&format!(
                "\n  flagged: {flagged} / {} pairs\n\n",
                subset.len()
            ));
        }
    }

    // Rung deltas
    out.push_str("=== Rung Deltas (truecolor → forced-256) ===\n\n");
    out.push_str(&format!(
        "{:<24} {:<16} {:<10} {:>10} {:>10} {:>10} {:>10}\n",
        "fg-slot",
        "bg-slot",
        "polarity",
        "fg-tc",
        "fg-256",
        "bg-tc",
        "bg-256"
    ));
    out.push_str(&"-".repeat(90).to_string());
    out.push('\n');

    for d in &report.rung_deltas {
        out.push_str(&format!(
            "{:<24} {:<16} {:<10} {:>10} {:>10} {:>10} {:>10}\n",
            d.fg_slot,
            d.bg_slot,
            d.polarity.label(),
            fmt_rgb(d.fg_truecolor),
            fmt_rgb(d.fg_256),
            fmt_rgb(d.bg_truecolor),
            fmt_rgb(d.bg_256)
        ));
        out.push_str(&format!(
            "{:>58} fg ΔE={:>8.4}  bg ΔE={:>8.4}\n",
            "", d.fg_delta_e, d.bg_delta_e
        ));
    }

    // Summary
    out.push_str("\n=== Summary ===\n");
    out.push_str(&format!(
        "  total pairs measured: {}\n",
        report.thresholds.total_pairs
    ));
    out.push_str(&format!(
        "  below WCAG AA normal (4.5): {}\n",
        report.thresholds.below_aa_normal
    ));
    out.push_str(&format!(
        "  below WCAG AA large (3.0):  {}\n",
        report.thresholds.below_aa_large
    ));
    out.push_str(&format!(
        "  below WCAG minimum (1.3):   {}\n",
        report.thresholds.below_minimum
    ));
    out.push_str(&format!(
        "  below ΔE2000+ratio (2.3+1.25): {}\n",
        report.thresholds.below_de2000_and_ratio
    ));
    out.push_str(&format!(
        "  total flagged:              {}\n",
        report.thresholds.total_flagged
    ));

    out
}

/// Render the report as JSON (sonic-rs Value).
#[must_use]
pub fn render_json(report: &Report) -> String {
    use sonic_rs::json;

    let results: Vec<sonic_rs::Value> = report
        .results
        .iter()
        .map(|r| {
            json!({
                "fgSlot": r.fg_slot,
                "bgSlot": r.bg_slot,
                "category": r.category,
                "polarity": r.polarity.label(),
                "colorMode": r.color_mode.label(),
                "fgRgb": fmt_rgb(r.fg_rgb),
                "bgRgb": fmt_rgb(r.bg_rgb),
                "wcagRatio": r.wcag_ratio,
                "deltaE2000": r.delta_e_2000,
                "flags": r.flags.labels(),
            })
        })
        .collect();

    let rung_deltas: Vec<sonic_rs::Value> = report
        .rung_deltas
        .iter()
        .map(|d| {
            json!({
                "fgSlot": d.fg_slot,
                "bgSlot": d.bg_slot,
                "polarity": d.polarity.label(),
                "fgTruecolor": fmt_rgb(d.fg_truecolor),
                "fg256": fmt_rgb(d.fg_256),
                "bgTruecolor": fmt_rgb(d.bg_truecolor),
                "bg256": fmt_rgb(d.bg_256),
                "fgDeltaE": d.fg_delta_e,
                "bgDeltaE": d.bg_delta_e,
            })
        })
        .collect();

    let summary = json!({
        "totalPairs": report.thresholds.total_pairs,
        "belowAaNormal": report.thresholds.below_aa_normal,
        "belowAaLarge": report.thresholds.below_aa_large,
        "belowMinimum": report.thresholds.below_minimum,
        "belowDe2000AndRatio": report.thresholds.below_de2000_and_ratio,
        "totalFlagged": report.thresholds.total_flagged,
    });

    let thresholds = json!({
        "wcagAaNormal": THRESHOLD_WCAG_AA_NORMAL,
        "wcagAaLarge": THRESHOLD_WCAG_AA_LARGE,
        "wcagMinimum": THRESHOLD_WCAG_MINIMUM,
        "de2000": THRESHOLD_DE2000,
        "de2000Ratio": THRESHOLD_DE2000_RATIO,
    });

    let root = json!({
        "schema": "tui-p2-contrast/1",
        "thresholds": thresholds,
        "results": results,
        "rungDeltas": rung_deltas,
        "summary": summary,
    });

    sonic_rs::to_string_pretty(&root).expect("report serializes")
}
