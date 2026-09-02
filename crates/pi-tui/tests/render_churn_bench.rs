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

use std::path::Path;
use std::process::Command;

use serde_json::Value;

/// Path to the benchmark binary (built in release mode).
fn bench_binary() -> String {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    format!("{manifest_dir}/../../target/release/pi_tui_render_churn_bench")
}

/// True when the benchmark binary has been built (requires
/// `cargo build -p pi-tui --release --features bench`). Tests skip
/// gracefully when this is false so a default `cargo test` run without
/// the `bench` feature does not fail.
fn bench_binary_exists() -> bool {
    Path::new(&bench_binary()).exists()
}

/// Skip the calling test when the benchmark binary is absent, printing a
/// clear message to stderr.
macro_rules! require_bench_binary {
    () => {
        if !bench_binary_exists() {
            eprintln!(
                "skipped: benchmark binary not found — build with \
                 `cargo build -p pi-tui --release --features bench`"
            );
            return;
        }
    };
}

/// Run the benchmark binary and parse the `__BENCH_JSON__` output.
#[expect(
    clippy::panic,
    reason = "test-only code: benchmark spawn and parse failures are irrecoverable"
)]
fn run_bench() -> Value {
    let binary = bench_binary();
    let output = Command::new(&binary)
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn benchmark binary {binary}: {e}"));

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
    v.as_f64()
        .or_else(|| v.as_str().and_then(|s| s.parse::<f64>().ok()))
}

#[test]
#[expect(clippy::expect_used, reason = "test-only assertions")]
fn benchmark_parameters_match_upstream() {
    require_bench_binary!();
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
    assert_eq!(json["frames"].as_u64(), Some(300), "frames must be 300");

    // Warmup frames: 20 (matches WARMUP_FRAMES=20)
    assert_eq!(
        json["warmupFrames"].as_u64(),
        Some(20),
        "warmup frames must be 20"
    );

    // Transcript: 150 lines (matches buildTranscript loop 0..150)
    let transcript_lines = json["transcriptLines"]
        .as_u64()
        .expect("transcriptLines must be present");
    assert!(
        transcript_lines >= 150,
        "transcript must have at least 150 source lines, got {transcript_lines}"
    );
}

#[test]
fn benchmark_produces_nonzero_results_for_both_scenarios() {
    require_bench_binary!();
    let json = run_bench();

    for scenario in ["static", "editor"] {
        let s = &json["scenarios"][scenario];
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
    }
    // PERF-T11 terminal-paint Design A pooled the paint transaction, so the
    // static scenario's steady-state allocation is now zero bytes (it was
    // 100 B/frame after Design F). Only the editor scenario — whose
    // workload-side rebuild still allocates — pins a positive allocation.
    let editor = &json["scenarios"]["editor"];
    assert!(
        editor["allocatedBytes"].as_u64().is_some_and(|v| v > 0),
        "editor scenario must allocate non-zero bytes"
    );
    assert!(
        json_f64(&editor["kiBPerFrame"]).is_some_and(|kib| kib > 0.0),
        "editor scenario must have positive KiB/frame"
    );
}

#[test]
#[expect(clippy::expect_used, reason = "test-only assertions")]
fn editor_scenario_allocates_more_than_static() {
    require_bench_binary!();
    let json = run_bench();

    let static_alloc = json["scenarios"]["static"]["allocatedBytes"]
        .as_u64()
        .expect("static allocatedBytes must be present");
    let editor_alloc = json["scenarios"]["editor"]["allocatedBytes"]
        .as_u64()
        .expect("editor allocatedBytes must be present");

    // The editor scenario appends one character per frame, invalidating the
    // editor cache and causing additional allocation.  It should allocate
    // more than the static scenario where nothing changes.
    assert!(
        editor_alloc > static_alloc,
        "editor scenario ({editor_alloc} bytes) should allocate more than static ({static_alloc} bytes)"
    );
}

/// Run the benchmark binary in `--probe` mode and parse `__PROBE_JSON__`
/// (PERF-T11 iteration 7 floor-probe contract).
#[expect(
    clippy::panic,
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "test-only code: probe spawn and parse failures are irrecoverable"
)]
fn run_probe() -> Value {
    let output = Command::new(bench_binary())
        .arg("--probe")
        .output()
        .expect("benchmark binary must run with --probe");
    assert!(output.status.success(), "--probe run failed");
    let stdout = String::from_utf8(output.stdout).unwrap();
    let marker = "__PROBE_JSON__\n";
    let start = stdout
        .find(marker)
        .unwrap_or_else(|| panic!("no __PROBE_JSON__ marker in probe output:\n{stdout}"));
    let body = stdout[start + marker.len()..].trim_end();
    serde_json::from_str(body).expect("probe JSON must parse")
}

#[test]
#[expect(clippy::expect_used, reason = "test-only assertions")]
fn floor_probe_reports_sane_constants() {
    require_bench_binary!();
    let probe = run_probe();
    let measured = &probe["measured"];
    let derived = &probe["derived"];

    let term = |v: &Value| -> f64 {
        let value = json_f64(v).expect("probe term must be a number");
        assert!(
            value.is_finite() && value > 0.0,
            "probe term must be finite and positive, got {value}"
        );
        value
    };

    // Every measured/derived term present, finite, positive.
    let static30 = term(&measured["frameStatic30Us"]);
    let poke = term(&measured["framePokeUs"]);
    let steady = term(&measured["frameEditorSteadyUs"]);
    term(&measured["frameStatic50Us"]);
    term(&measured["frameStatic60Us"]);
    term(&measured["wrapKeyUsPerLine"]);
    // The identity slope subtracts two ~2 µs static-frame medians and
    // divides by ~20 lines — a noise-scale quantity on a bursty box, where
    // it can wobble slightly negative without any broken invariant. Gate
    // it on finiteness and the same far-below-the-derive-term bound as the
    // slope assertion below; real measured times keep strict positivity.
    let slope = json_f64(&derived["identitySlopeUsPerLine"]).expect("slope must be a number");
    assert!(
        slope.is_finite() && slope.abs() < 1.0,
        "identity slope must be finite and far below the pre-campaign 1.3 µs/line derive term, got {slope}"
    );
    term(&derived["changedLineCommitUs"]);
    // PERF-T11 terminal-paint probe terms (paint-only instrument). The
    // relations compare paint terms with each other — cross-loop
    // comparisons against the separately timed whole-frame loops flake
    // under the box's bursty contention.
    let paint_static = term(&measured["paintStatic30Us"]);
    let paint_poke = term(&measured["paintPokeUs"]);
    let paint_poke_diff = term(&measured["paintPokeDiffUs"]);
    let paint_steady = term(&measured["paintEditorSteadyUs"]);
    let paint_steady_diff = term(&measured["paintEditorSteadyDiffUs"]);
    assert!(
        paint_poke > paint_static,
        "a changed-line frame's paint share must exceed the static paint share"
    );
    assert!(
        paint_poke_diff <= paint_poke && paint_steady_diff <= paint_steady,
        "the diff phase cannot exceed the paint transaction it sits inside"
    );

    // Sanity relations pinning the exhaustion-record arithmetic.
    assert!(
        poke > static30,
        "a changed-line frame must cost more than static"
    );
    assert!(
        steady > static30,
        "editor steady frame must cost more than static"
    );
    assert!(
        slope < 1.0,
        "identity slope must be far below the pre-campaign 1.3 µs/line derive term"
    );
    assert_eq!(
        derived["visibleLines30"].as_u64(),
        Some(25),
        "30-row viewport shows 25 transcript lines over the 5-row dock"
    );

    // Pin the exhaustion-record arithmetic itself: every derived term must
    // recompute from the emitted measured fields (within the JSON's
    // four-decimal rounding tolerance) — a wrong operand or sign in the
    // probe's formulas must fail here, not just look plausible.
    let close = |a: f64, b: f64| (a - b).abs() <= 1e-3;
    let wrap_key = term(&measured["wrapKeyUsPerLine"]);
    let rebuild = term(&measured["editorRebuildUs"]);
    let commit = term(&derived["changedLineCommitUs"]);
    let row_commit = term(&derived["editorRowCommitUs"]);
    assert!(
        close(commit, poke - static30 - wrap_key),
        "changedLineCommit {commit} must equal poke {poke} - static {static30} - wrapKey {wrap_key}"
    );
    assert!(
        close(row_commit, steady - static30 - rebuild),
        "editorRowCommit {row_commit} must equal steady {steady} - static {static30} - rebuild {rebuild}"
    );
    let v30 = json_f64(&derived["visibleLines30"]).expect("visible30");
    let v50 = json_f64(&derived["visibleLines50"]).expect("visible50");
    let static50 = term(&measured["frameStatic50Us"]);
    assert!(
        close(slope, (static50 - static30) / (v50 - v30)),
        "identitySlope {slope} must equal (static50 {static50} - static30 {static30}) / (visible {v50} - {v30})"
    );
}
