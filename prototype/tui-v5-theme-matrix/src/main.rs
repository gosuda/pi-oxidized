//! TUI-V5 Theme and contrast matrix with numeric oracle (issue #79).
//!
//! Measures every polarity switch, ramp rung, escalation, and rail-hue
//! verdict as a number against its pinned threshold over canonical
//! snapshots (built-in theme JSON, never timing-dependent captures).
//!
//! Run:
//!   cargo run --manifest-path prototype/tui-v5-theme-matrix/Cargo.toml -- [--json]
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

    let report = report::build_report(&dark, &light);

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
