//! PERF-T5: dispatch-only tool benchmark protocol test (issue #93).
//!
//! Drives the `pi_tool_dispatch_bench` binary on the production
//! `pi_agent::execute_tool_calls` path and asserts the shared dispatch
//! protocol that the TypeScript worker (upstream `runAgentLoop`) also
//! satisfies: one start/update/end event triple per call, one session append
//! of the trigger assistant message plus one of the tool-result message per
//! call, zero error results for the valid payload, and a full
//! validation-rejection (no update, error result) for the shared invalid
//! payload — proving identical argument-validation outcomes on both
//! implementations.

use std::process::Command;

use serde_json::Value;

fn run_bench(label: &str, extra: &[&str]) -> Value {
	let binary = env!("CARGO_BIN_EXE_pi_tool_dispatch_bench");
	// Unique per test: cargo runs tests in parallel threads that share a pid.
	let session_dir = std::env::temp_dir().join(format!(
		"pi-tool-dispatch-test-{}-{label}",
		std::process::id()
	));
	let mut command = Command::new(binary);
	command
		.arg("--session-dir")
		.arg(&session_dir)
		.arg("--calls")
		.arg("8")
		.arg("--warmup")
		.arg("4")
		.arg("--blocks")
		.arg("1");
	for flag in extra {
		command.arg(flag);
	}
	let output = command
		.output()
		.unwrap_or_else(|error| panic!("failed to spawn bench binary: {error}"));
	assert!(
		output.status.success(),
		"bench exited with {:?}: {}",
		output.status,
		String::from_utf8_lossy(&output.stderr)
	);
	let stdout = String::from_utf8_lossy(&output.stdout);
	let report: Value = serde_json::from_str(stdout.trim())
		.unwrap_or_else(|error| panic!("failed to parse bench JSON: {error}\nraw: {stdout}"));
	let _ = std::fs::remove_dir_all(&session_dir);
	report
}

fn events(report: &Value, key: &str) -> i64 {
	report["events"][key]
		.as_i64()
		.unwrap_or_else(|| panic!("events.{key} missing in {report}"))
}

#[test]
fn valid_dispatch_satisfies_the_shared_protocol() {
	let report = run_bench("valid", &[]);
	assert_eq!(report["implementation"].as_str(), Some("rust"));
	assert_eq!(report["argumentsMode"].as_str(), Some("valid"));
	assert_eq!(events(&report, "start"), 8);
	assert_eq!(events(&report, "update"), 8);
	assert_eq!(events(&report, "end"), 8);
	assert_eq!(events(&report, "errorResults"), 0);
	assert_eq!(report["appends"].as_i64(), Some(16));

	let block = &report["blocks"][0];
	assert_eq!(block["calls"].as_i64(), Some(8));
	let wall = block["wallMsPerCall"]
		.as_f64()
		.expect("wallMsPerCall must be a number");
	assert!(wall > 0.0, "wallMsPerCall must be positive, got {wall}");
	assert!(
		block["session"].is_object() || report["session"]["bytesDelta"].as_i64().is_some(),
		"session delta must be reported"
	);
	let bytes = report["session"]["bytesDelta"].as_i64().expect("bytesDelta");
	assert!(bytes > 0, "session must grow with appends, delta {bytes}");
}

#[test]
fn invalid_payload_is_rejected_by_argument_validation() {
	let report = run_bench("invalid", &["--arguments", "invalid"]);
	assert_eq!(report["argumentsMode"].as_str(), Some("invalid"));
	// Validation fails before execution: no updates, every call an error result.
	assert_eq!(events(&report, "start"), 8);
	assert_eq!(events(&report, "update"), 0);
	assert_eq!(events(&report, "end"), 8);
	assert_eq!(events(&report, "errorResults"), 8);
	assert_eq!(report["appends"].as_i64(), Some(16));
}
