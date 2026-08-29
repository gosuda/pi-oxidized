//! TUI-P2 Deterministic contrast measurement prototype (issue #58).
//!
//! Resolves settled-frame fg/bg pairs from canonical schema-v1 snapshots
//! (built-in theme JSON, never timing-dependent captures) to RGB via the
//! pinned 256-palette table and reports numeric WCAG ratios and ΔE2000
//! rung deltas on dark+light terminals in truecolor+forced-256, flagging
//! every pair below the pinned thresholds.
//!
//! Run:
//!   cargo run --manifest-path prototype/tui-p2-contrast/Cargo.toml -- [--json]
//! Prints the text report to stdout; `--json` emits machine-readable JSON.

mod color;
mod palette;
mod report;
mod theme;

use std::io::Write;

fn main() -> std::process::ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let json_mode = args.iter().any(|a| a == "--json");

    let dark = theme::dark_theme();
    let light = theme::light_theme();
    let pairs = theme::inspected_pairs();

    let report = report::build_report(&dark, &light, &pairs);

    let output = if json_mode {
        report::render_json(&report)
    } else {
        report::render_text(&report)
    };

    let stdout = std::io::stdout();
    let mut lock = stdout.lock();
    if lock.write_all(output.as_bytes()).is_err() {
        return std::process::ExitCode::from(1);
    }

    std::process::ExitCode::SUCCESS
}
