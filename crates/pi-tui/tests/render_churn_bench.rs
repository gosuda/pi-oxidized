//! PERF-T3: render-churn benchmark parameter parity test (issue #89).
//!
//! Verifies that the Rust render-churn benchmark binary
//! (`pi_tui_render_churn_bench`) uses parameters matching the upstream
//! TypeScript benchmark (`.references/pi/packages/tui/test/render-churn-bench.ts`).
//!
//! The test reads the `__BENCH_JSON__` output from the benchmark binary and
//! asserts every parameter matches the upstream constants:
//! - viewport: 100×30
//! - transcript: 150 lines
//! - warmup frames: 20
//! - measured frames: 300
//! - two scenarios: static and editor
//!
//! It also asserts the benchmark produces non-zero results (wall time and
//! allocation) for both scenarios, proving the workload actually executes.

use std::path::PathBuf;
use std::process::Command;

use serde_json::Value;

/// Build the benchmark binary in release mode and return its path.
///
/// Follows the repo convention from `transcript_ext_gauntlet.rs`: build the
/// fixture binary before spawning it, so the test works on a clean checkout
/// and never validates a stale binary.
fn ensure_bench_binary() -> PathBuf {
    // If CARGO_BIN_EXE is set (running via cargo test), use that.
    if let Ok(path) = std::env::var("CARGO_BIN_EXE_pi_tui_render_churn_bench") {
        let path = PathBuf::from(path);
        if path.exists() {
            return path;
        }
    }

    // Build the binary in release mode.
    let status = Command::new("cargo")
        .args([
            "build",
            "-p",
            "pi-tui",
            "--bin",
            "pi_tui_render_churn_bench",
            "--release",
        ])
        .status()
        .unwrap_or_else(|e| panic!("failed to spawn cargo build: {e}"));
    assert!(
        status.success(),
        "cargo build for benchmark binary failed"
    );

    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/release/pi_tui_render_churn_bench");
    assert!(
        path.exists(),
        "benchmark binary missing after build at {}",
        path.display()
    );
    path
}

/// Run the benchmark binary and parse the `__BENCH_JSON__` output.
fn run_bench() -> Value {
    let binary = ensure_bench_binary();
    let output = Command::new(&binary)
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn benchmark binary: {e}"));

    assert!(
        output.status.success(),
        "benchmark binary exited with status {:?}",
        output.status
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    let json_marker = "__BENCH_JSON__\n";
    let start = stdout
        .find(json_marker)
        .unwrap_or_else(|| panic!("no __BENCH_JSON__ marker in benchmark output:\n{stdout}"));
    let json_str = &stdout[start + json_marker.len()..];
    let end = json_str.rfind('}').unwrap_or(json_str.len());
    let json_str = json_str[..=end].trim();

    serde_json::from_str(json_str)
        .unwrap_or_else(|e| panic!("failed to parse benchmark JSON: {e}\nraw: {json_str}"))
}

/// Extract a floating-point value from a JSON field that may be a number or
/// string.
fn json_f64(v: &Value) -> Option<f64> {
    v.as_f64().or_else(|| v.as_str().and_then(|s| s.parse::<f64>().ok()))
}

#[test]
fn benchmark_parameters_match_upstream() {
    let json = run_bench();

    // Viewport: 100×30 (matches COLUMNS=100, ROWS=30 in render-churn-bench.ts)
    assert_eq!(
        json["viewport"]["columns"].as_u64(),
        Some(100),
        "viewport columns must be 100"
    );
    assert_eq!(
        json["viewport"]["rows"].as_u64(),
        Some(30),
        "viewport rows must be 30"
    );

    // Frames: 300 (matches FRAMES=300)
    assert_eq!(
        json["frames"].as_u64(),
        Some(300),
        "frames must be 300"
    );

    // Warmup frames: 20 (matches WARMUP_FRAMES=20)
    assert_eq!(
        json["warmupFrames"].as_u64(),
        Some(20),
        "warmup frames must be 20"
    );

    // Transcript: 150 lines (matches buildTranscript loop 0..150)
    let transcript_lines = json["transcriptLines"]
        .as_u64()
        .unwrap_or_else(|| panic!("transcriptLines must be present"));
    assert!(
        transcript_lines >= 150,
        "transcript must have at least 150 source lines, got {transcript_lines}"
    );
}

#[test]
fn benchmark_produces_nonzero_results_for_both_scenarios() {
    let json = run_bench();

    for scenario in ["static", "editor"] {
        let s = &json["scenarios"][scenario];
        assert!(
            s["allocatedBytes"].as_u64().is_some_and(|v| v > 0),
            "{scenario} scenario must allocate non-zero bytes"
        );
        assert!(
            json_f64(&s["elapsedMs"]).is_some_and(|ms| ms > 0.0),
            "{scenario} scenario must have positive wall time"
        );
        assert!(
            s["bytesWritten"].as_u64().is_some_and(|v| v > 0),
            "{scenario} scenario must write non-zero bytes"
        );
        assert!(
            json_f64(&s["msPerFrame"]).is_some_and(|ms| ms > 0.0),
            "{scenario} scenario must have positive ms/frame"
        );
        assert!(
            json_f64(&s["kiBPerFrame"]).is_some_and(|kib| kib > 0.0),
            "{scenario} scenario must have positive KiB/frame"
        );
    }
}

#[test]
fn editor_scenario_allocates_more_than_static() {
    let json = run_bench();

    let static_alloc = json["scenarios"]["static"]["allocatedBytes"]
        .as_u64()
        .unwrap_or_else(|| panic!("static allocatedBytes must be present"));
    let editor_alloc = json["scenarios"]["editor"]["allocatedBytes"]
        .as_u64()
        .unwrap_or_else(|| panic!("editor allocatedBytes must be present"));

    // The editor scenario appends one character per frame, invalidating the
    // editor cache and causing additional allocation.  It should allocate
    // more than the static scenario where nothing changes.
    assert!(
        editor_alloc > static_alloc,
        "editor scenario ({editor_alloc} bytes) should allocate more than static ({static_alloc} bytes)"
    );
}
