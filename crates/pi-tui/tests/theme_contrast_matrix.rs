#![allow(
    clippy::panic,
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::too_many_lines,
    clippy::cast_possible_truncation,
    dead_code
)]
//! TUI-V5 Theme and contrast matrix evidence (issue #79).
//!
//! Asserts the numeric oracle verdicts from the standalone V5 prototype
//! against pinned thresholds over canonical theme snapshots. Every verdict
//! is a number — never inferred.
//!
//! The test reads the generated JSON report (`prototype/tui-v5-theme-matrix/
//! report.json`) and asserts:
//!
//! 1. **Polarity detection** — every OSC 11 and COLORFGBG input is classified
//!    or falls back to dark, with BT.601 luminance reported as a number.
//! 2. **Thinking-ramp adjacent-rung verdicts** — ΔE2000 and WCAG ratio are
//!    reported as numbers against the pinned thresholds (ΔE2000 ≥ 2.3 AND
//!    ratio ≥ 1.25).
//! 3. **Accent-adjacent rail hue collisions** — WCAG ratio reported as a
//!    number against the minimum threshold (≥ 1.3).
//! 4. **Degraded-terminal SGR inspection** — truecolor and forced-256 SGR
//!    sequences are reported with ΔE2000 between them.
//! 5. **Slash-pair fallback** — every raw setting resolves to a named member
//!    with fallback flagged.

use std::path::PathBuf;

/// Locate the generated V5 report JSON.
fn report_path() -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop(); // crates/pi-tui → crates
    p.pop(); // crates → repo root
    p.push("prototype/tui-v5-theme-matrix/report.json");
    p
}

#[derive(Debug)]
struct ReportData {
    json: serde_json::Value,
}

impl ReportData {
    fn load() -> Self {
        let path = report_path();
        let content = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("failed to read {}: {e}", path.display()));
        let json: serde_json::Value =
            serde_json::from_str(&content).expect("report.json is valid JSON");
        Self { json }
    }

    fn summary(&self) -> &serde_json::Value {
        self.json.get("summary").expect("summary present")
    }

    fn polarity_verdicts(&self) -> &[serde_json::Value] {
        self.json
            .get("polarityVerdicts")
            .and_then(|v| v.as_array())
            .expect("polarityVerdicts array")
    }

    fn rung_verdicts(&self) -> &[serde_json::Value] {
        self.json
            .get("rungVerdicts")
            .and_then(|v| v.as_array())
            .expect("rungVerdicts array")
    }

    fn rail_hue_verdicts(&self) -> &[serde_json::Value] {
        self.json
            .get("railHueVerdicts")
            .and_then(|v| v.as_array())
            .expect("railHueVerdicts array")
    }

    fn sgr_verdicts(&self) -> &[serde_json::Value] {
        self.json
            .get("sgrVerdicts")
            .and_then(|v| v.as_array())
            .expect("sgrVerdicts array")
    }

    fn pair_fallback_verdicts(&self) -> &[serde_json::Value] {
        self.json
            .get("pairFallbackVerdicts")
            .and_then(|v| v.as_array())
            .expect("pairFallbackVerdicts array")
    }

    fn thresholds(&self) -> &serde_json::Value {
        self.json.get("thresholds").expect("thresholds present")
    }
}

#[test]
fn polarity_detection_reports_numbers_not_inferences() {
    let report = ReportData::load();
    let verdicts = report.polarity_verdicts();

    // Every verdict must have a numeric BT.601 luminance or be the fallback case.
    for v in verdicts {
        let source = v
            .get("source")
            .and_then(|s| s.as_str())
            .unwrap_or("unknown");
        let verdict = v
            .get("verdict")
            .and_then(|s| s.as_str())
            .unwrap_or("unknown");

        if verdict == "fallback-dark" {
            // Fallback case: no luminance, no classification — by design.
            assert!(
                v.get("detectedDark")
                    .and_then(serde_json::Value::as_bool)
                    .is_none(),
                "fallback case {source} must not classify"
            );
        } else {
            // Classified cases must report detectedDark as a bool and
            // bt601Luminance as a number.
            assert!(
                v.get("detectedDark")
                    .and_then(serde_json::Value::as_bool)
                    .is_some(),
                "classified case {source} must report detectedDark as bool"
            );
            assert!(
                v.get("bt601Luminance")
                    .and_then(serde_json::Value::as_u64)
                    .is_some(),
                "classified case {source} must report bt601Luminance as number"
            );
        }
    }

    let classified = report
        .summary()
        .get("polarityClassified")
        .and_then(serde_json::Value::as_u64)
        .expect("polarityClassified");
    let total = report
        .summary()
        .get("polarityTotal")
        .and_then(serde_json::Value::as_u64)
        .expect("polarityTotal");
    assert_eq!(
        classified,
        u64::try_from(
            verdicts
                .iter()
                .filter(|v| v
                    .get("detectedDark")
                    .and_then(serde_json::Value::as_bool)
                    .is_some())
                .count()
        )
        .unwrap(),
        "polarityClassified must match actual classified count"
    );
    // 10 of 11 classified (only the fallback case is unclassified).
    assert_eq!(classified, 10, "expected 10 classified, got {classified}");
    assert_eq!(total, 11, "expected 11 total, got {total}");
}

#[test]
fn thinking_ramp_verdicts_report_numbers_against_thresholds() {
    let report = ReportData::load();
    let verdicts = report.rung_verdicts();
    let thresholds = report.thresholds();

    let de_threshold = thresholds
        .get("de2000")
        .and_then(serde_json::Value::as_f64)
        .expect("de2000 threshold");
    let ratio_threshold = thresholds
        .get("de2000Ratio")
        .and_then(serde_json::Value::as_f64)
        .expect("de2000Ratio threshold");

    assert!(
        (de_threshold - 2.3).abs() < 1e-9,
        "ΔE2000 threshold pinned at 2.3"
    );
    assert!(
        (ratio_threshold - 1.25).abs() < 1e-9,
        "ratio threshold pinned at 1.25"
    );

    for v in verdicts {
        let lower = v.get("lower").and_then(|s| s.as_str()).unwrap_or("unknown");
        let upper = v.get("upper").and_then(|s| s.as_str()).unwrap_or("unknown");
        let polarity = v
            .get("polarity")
            .and_then(|s| s.as_str())
            .unwrap_or("unknown");
        let color_mode = v
            .get("colorMode")
            .and_then(|s| s.as_str())
            .unwrap_or("unknown");

        let de = v
            .get("deltaE2000")
            .and_then(serde_json::Value::as_f64)
            .unwrap_or_else(|| {
                panic!("{lower}→{upper} {polarity}/{color_mode}: deltaE2000 must be a number")
            });
        let ratio = v
            .get("wcagRatio")
            .and_then(serde_json::Value::as_f64)
            .unwrap_or_else(|| {
                panic!("{lower}→{upper} {polarity}/{color_mode}: wcagRatio must be a number")
            });

        // Verify the pass/fail flags match the numeric thresholds.
        let passes_de = de >= de_threshold;
        let passes_ratio = ratio >= ratio_threshold;
        let passes_both = passes_de && passes_ratio;

        let actual_passes_de = v
            .get("passesDe2000")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        let actual_passes_ratio = v
            .get("passesRatio")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        let actual_passes_both = v
            .get("passesBoth")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);

        assert_eq!(
            actual_passes_de, passes_de,
            "{lower}→{upper} {polarity}/{color_mode}: passesDe2000 flag mismatch (de={de:.4}, threshold={de_threshold})"
        );
        assert_eq!(
            actual_passes_ratio, passes_ratio,
            "{lower}→{upper} {polarity}/{color_mode}: passesRatio flag mismatch (ratio={ratio:.4}, threshold={ratio_threshold})"
        );
        assert_eq!(
            actual_passes_both, passes_both,
            "{lower}→{upper} {polarity}/{color_mode}: passesBoth flag mismatch"
        );
    }

    // 2 themes × 2 color modes × 6 adjacent rungs = 24 verdicts.
    assert_eq!(verdicts.len(), 24, "expected 24 rung verdicts");
}

#[test]
fn rail_hue_collisions_report_wcag_ratios_against_minimum() {
    let report = ReportData::load();
    let verdicts = report.rail_hue_verdicts();
    let thresholds = report.thresholds();

    let min_threshold = thresholds
        .get("wcagMinimum")
        .and_then(serde_json::Value::as_f64)
        .expect("wcagMinimum threshold");
    assert!(
        (min_threshold - 1.3).abs() < 1e-9,
        "WCAG minimum threshold pinned at 1.3"
    );

    for v in verdicts {
        let fg = v
            .get("fgSlot")
            .and_then(|s| s.as_str())
            .unwrap_or("unknown");
        let rail = v
            .get("railSlot")
            .and_then(|s| s.as_str())
            .unwrap_or("unknown");
        let polarity = v
            .get("polarity")
            .and_then(|s| s.as_str())
            .unwrap_or("unknown");
        let color_mode = v
            .get("colorMode")
            .and_then(|s| s.as_str())
            .unwrap_or("unknown");

        let ratio = v
            .get("wcagRatio")
            .and_then(serde_json::Value::as_f64)
            .unwrap_or_else(|| {
                panic!("{fg} vs {rail} {polarity}/{color_mode}: wcagRatio must be a number")
            });

        let passes = ratio >= min_threshold;
        let actual_passes = v
            .get("passesWcag")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);

        assert_eq!(
            actual_passes, passes,
            "{fg} vs {rail} {polarity}/{color_mode}: passesWcag flag mismatch (ratio={ratio:.4}, threshold={min_threshold})"
        );
    }

    // 2 themes × 2 color modes × 5 fg slots × 5 rail slots = 100 verdicts.
    assert_eq!(verdicts.len(), 100, "expected 100 rail-hue verdicts");
}

#[test]
fn sgr_inspection_reports_both_modes_with_delta_e() {
    let report = ReportData::load();
    let verdicts = report.sgr_verdicts();

    for v in verdicts {
        let slot = v.get("slot").and_then(|s| s.as_str()).unwrap_or("unknown");
        let polarity = v
            .get("polarity")
            .and_then(|s| s.as_str())
            .unwrap_or("unknown");

        let tc_sgr = v
            .get("truecolorSgr")
            .and_then(|s| s.as_str())
            .unwrap_or_else(|| panic!("{slot} {polarity}: truecolorSgr must be present"));
        let f256_sgr = v
            .get("forced256Sgr")
            .and_then(|s| s.as_str())
            .unwrap_or_else(|| panic!("{slot} {polarity}: forced256Sgr must be present"));
        let de = v
            .get("deltaE2000")
            .and_then(serde_json::Value::as_f64)
            .unwrap_or_else(|| panic!("{slot} {polarity}: deltaE2000 must be a number"));

        // Truecolor SGR must use 24-bit form.
        assert!(
            tc_sgr.starts_with("\u{1b}[38;2;"),
            "{slot} {polarity}: truecolor SGR must be 24-bit, got {tc_sgr:?}"
        );
        // Forced-256 SGR must use palette form.
        assert!(
            f256_sgr.starts_with("\u{1b}[38;5;"),
            "{slot} {polarity}: forced-256 SGR must be palette, got {f256_sgr:?}"
        );
        // ΔE2000 must be non-negative.
        assert!(
            de >= 0.0,
            "{slot} {polarity}: deltaE2000 must be non-negative, got {de}"
        );
    }

    // 2 themes × 12 slots = 24 verdicts.
    assert_eq!(verdicts.len(), 24, "expected 24 SGR verdicts");
}

#[test]
fn slash_pair_fallback_resolves_every_case() {
    let report = ReportData::load();
    let verdicts = report.pair_fallback_verdicts();

    for v in verdicts {
        let raw = v
            .get("rawSetting")
            .and_then(|s| s.as_str())
            .unwrap_or("unknown");
        let member = v
            .get("resolvedMember")
            .and_then(|s| s.as_str())
            .unwrap_or("unknown");
        let fallback = v
            .get("fallbackUsed")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);

        // Every case must resolve to a named member.
        assert!(
            !member.is_empty(),
            "raw setting {raw:?} must resolve to a non-empty member"
        );

        // Unknown names must trigger fallback to "dark".
        if raw == "unknown-name" {
            assert!(
                fallback,
                "unknown-name must trigger fallback, got member={member:?}"
            );
            assert_eq!(
                member, "dark",
                "fallback must resolve to dark, got {member:?}"
            );
        } else {
            // Known names must not trigger fallback.
            assert!(!fallback, "known setting {raw:?} must not trigger fallback");
        }
    }

    // 14 cases total, 2 fallbacks.
    assert_eq!(verdicts.len(), 14, "expected 14 pair-fallback verdicts");
    let fallback_count = report
        .summary()
        .get("pairFallbackCount")
        .and_then(serde_json::Value::as_u64)
        .expect("pairFallbackCount");
    assert_eq!(
        fallback_count, 2,
        "expected 2 fallbacks, got {fallback_count}"
    );
}
