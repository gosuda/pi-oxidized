#![cfg(all(unix, feature = "testkit"))]

//! Two-tier state-matrix conformance corpus (TUI-V1, issue #76).
//!
//! Drives the `pi_tui_state_matrix_fixture` binary through the PTY harness
//! to produce validator-clean schema-v1 transcripts proving the full state
//! matrix renders per the quality bar:
//! - empty, loading, retry, queue, streaming, error, focus-marked, ext-ui
//! - per-state content checkpoints (content predicates, never timer-only)
//! - no full-screen clears and balanced synchronized-output markers per state
//! - k=3 byte-identical canonical bytes and digest per row
//!
//! Row selection mirrors the fixture corpus: absent `PI_TUI_TIER_ROW` the
//! run records `local` evidence on the host row (never Tier N);
//! `tier-n/<row>@<image>` records the pinned CI runner row and the validator
//! enforces the Tier N driver pairings.
//!
//! Artifacts land under
//! `target/verification/tui-transcripts/<row>/state-matrix/run-{1,2,3}/`
//! with a `verdict.json` per-row verdict record.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use pi_tui::terminal::audit_bytes;
use pi_tui::testkit::driver::{
    Geometry as DriverGeometry, LaunchSpec, SettlePolicy, TerminalDriver,
};
use pi_tui::testkit::posix::PosixPtyDriver;
use pi_tui::testkit::repeat::{RepeatError, run_k};
use pi_tui::testkit::transcript::{
    CapabilityProfile, ClaimClass, DriverKind, Geometry, NormalizationContext, RowId, RowTier,
    RunnerRow, Scenario, TimingEnvelope, TranscriptArtifact, TranscriptMode, TranscriptRecorder,
    TranscriptSpec,
};
use pi_tui::testkit::validate::validate_artifact;
use pi_tui::testkit::{RecordingError, RecordingSession};

const INITIAL_COLS: u16 = 80;
const INITIAL_ROWS: u16 = 24;
const K: usize = 3;

/// The closed state matrix: (state label, settle marker, snapshot needle).
/// Settle markers are contiguous paint runs (the Tui paints per-cell diffs,
/// so `STATUS <label>` is never contiguous on the wire — the label is).
const STATE_MATRIX: [(&str, &[u8], &str); 8] = [
    ("empty", b"PI_TUI_STATE=empty", "EMPTY no messages"),
    ("loading", b"PI_TUI_STATE=loading", "working"),
    ("retry", b"PI_TUI_STATE=retry", "Retrying (1/3) in 5s"),
    ("queue", b"PI_TUI_STATE=queue", "queued follow-up"),
    ("streaming", b"PI_TUI_STATE=streaming", "verification-stream-0001"),
    ("error", b"PI_TUI_STATE=error", "Error: request failed"),
    ("focus-marked", b"PI_TUI_STATE=focus-marked", "verification focus probe"),
    ("ext-ui", b"PI_TUI_STATE=ext-ui", "verification-ext-widget"),
];

#[derive(Debug)]
enum CorpusError {
    Prerequisite(String),
    Driver(String),
    Transcript(String),
    Io(String),
    Assert(String),
}

impl std::fmt::Display for CorpusError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Prerequisite(message)
            | Self::Driver(message)
            | Self::Transcript(message)
            | Self::Io(message)
            | Self::Assert(message) => write!(formatter, "{message}"),
        }
    }
}

impl From<RecordingError> for CorpusError {
    fn from(error: RecordingError) -> Self {
        match error {
            RecordingError::Driver(error) => Self::Driver(error.to_string()),
            RecordingError::Transcript(error) => Self::Transcript(error.to_string()),
            RecordingError::FinishBeforeClose => {
                Self::Transcript("recording cannot finish before close".to_owned())
            }
        }
    }
}

impl From<pi_tui::testkit::driver::DriverError> for CorpusError {
    fn from(error: pi_tui::testkit::driver::DriverError) -> Self {
        Self::Driver(error.to_string())
    }
}

impl From<std::io::Error> for CorpusError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error.to_string())
    }
}

impl From<pi_tui::testkit::transcript::TranscriptError> for CorpusError {
    fn from(error: pi_tui::testkit::transcript::TranscriptError) -> Self {
        Self::Transcript(error.to_string())
    }
}

// ---------------------------------------------------------------------------
// FixtureRun wrapper — mirrors transcript_ext_gauntlet.rs
// ---------------------------------------------------------------------------

type HostSession = <PosixPtyDriver as TerminalDriver>::Session;

struct FixtureRun {
    recording: RecordingSession<HostSession>,
    context: NormalizationContext,
    policy: SettlePolicy,
    raw_acc: Vec<u8>,
    wall_started: Instant,
    settle_windows_ms: Vec<u64>,
}

impl FixtureRun {
    fn open(
        argv: Vec<String>,
        scenario: Scenario,
        row: RunnerRow,
        claims: Vec<ClaimClass>,
    ) -> Result<Self, CorpusError> {
        require_prerequisites(&argv[0])?;
        let geometry = Geometry {
            cols: INITIAL_COLS,
            rows: INITIAL_ROWS,
        };
        let profile = CapabilityProfile::Xterm256ColorTruecolor;
        let cwd = std::env::current_dir().map_err(|error| {
            CorpusError::Prerequisite(format!("current_dir unavailable: {error}"))
        })?;
        let context = NormalizationContext {
            home: std::env::var_os("HOME").map(|value| value.as_encoded_bytes().to_vec()),
            cwd: Some(cwd.as_os_str().as_encoded_bytes().to_vec()),
        };
        let mut env = BTreeMap::new();
        env.insert("PI_TUI_AUDIT".to_owned(), "1".to_owned());
        env.insert("TERM".to_owned(), "xterm-256color".to_owned());
        env.insert("COLORTERM".to_owned(), "truecolor".to_owned());

        let spec = LaunchSpec {
            argv: argv.clone(),
            cwd,
            env,
            geometry,
            profile,
        };
        let session = PosixPtyDriver.open(&spec)?;
        let recorder = TranscriptRecorder::new(TranscriptSpec {
            scenario,
            row,
            geometry,
            capability_profile: profile,
            driver_kind: DriverKind::PosixPty,
            mode: TranscriptMode::Standard,
            claims,
            timing: TimingEnvelope::default(),
        });
        let recording = RecordingSession::new(session, recorder, argv, &context)?;
        Ok(Self {
            recording,
            context,
            policy: SettlePolicy::new(Duration::from_millis(120), Duration::from_secs(10))?,
            raw_acc: Vec::new(),
            wall_started: Instant::now(),
            settle_windows_ms: Vec::new(),
        })
    }

    fn settle_frame<F>(
        &mut self,
        mut predicate: F,
    ) -> Result<pi_tui::testkit::driver::SettledFrame, CorpusError>
    where
        F: FnMut(&[u8]) -> bool,
    {
        let started = Instant::now();
        let prior = self.raw_acc.clone();
        let frame = self.recording.read_settled_frame(
            &self.policy,
            |bytes| predicate(bytes) || predicate(&merge_acc(&prior, bytes)),
            &self.context,
        )?;
        self.settle_windows_ms.push(
            u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        );
        self.raw_acc.extend_from_slice(&frame.batch.bytes);
        Ok(frame)
    }

    /// Advance the stepped fixture one state (single step key event).
    fn write_step(&mut self) -> Result<(), CorpusError> {
        self.recording.write(b" ")?;
        Ok(())
    }

    fn finish(mut self) -> Result<TranscriptArtifact, CorpusError> {
        let _status = self.recording.close()?;
        let mut artifact = self.recording.finish()?;
        artifact.timing.wall_ms =
            u64::try_from(self.wall_started.elapsed().as_millis()).unwrap_or(u64::MAX);
        artifact.timing.settle_windows_ms = self.settle_windows_ms;
        Ok(artifact)
    }

    fn raw_so_far(&self) -> &[u8] {
        &self.raw_acc
    }
}

fn merge_acc(prefix: &[u8], pending: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(prefix.len() + pending.len());
    out.extend_from_slice(prefix);
    out.extend_from_slice(pending);
    out
}

fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

// ---------------------------------------------------------------------------
// Prerequisites, row resolution, artifact IO — mirrors the corpus tests
// ---------------------------------------------------------------------------

fn require_prerequisites(fixture: &str) -> Result<(), CorpusError> {
    if !cfg!(unix) {
        return Err(CorpusError::Prerequisite(
            "PosixPtyDriver transcript corpus requires a unix host".to_owned(),
        ));
    }
    if !Path::new(fixture).exists() {
        return Err(CorpusError::Prerequisite(format!(
            "fixture binary missing: {fixture}"
        )));
    }
    let _ = DriverGeometry::new(1, 1)
        .map_err(|error| CorpusError::Prerequisite(format!("geometry prerequisite failed: {error}")))?;
    Ok(())
}

fn fixture_binary() -> Result<PathBuf, CorpusError> {
    if let Ok(path) = std::env::var("CARGO_BIN_EXE_pi_tui_state_matrix_fixture") {
        let path = PathBuf::from(path);
        if path.exists() {
            return Ok(path);
        }
        return Err(CorpusError::Prerequisite(format!(
            "CARGO_BIN_EXE_pi_tui_state_matrix_fixture points at missing binary: {}",
            path.display()
        )));
    }
    let mut candidates = Vec::new();
    if let Ok(target) = std::env::var("CARGO_TARGET_DIR") {
        candidates.push(PathBuf::from(target));
    }
    candidates.push(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target"));
    candidates.push(PathBuf::from("target"));
    for root in candidates {
        for profile in ["debug", "release"] {
            let path = root.join(profile).join("pi_tui_state_matrix_fixture");
            if path.exists() {
                return Ok(path);
            }
        }
    }
    let status = Command::new("cargo")
        .args([
            "build",
            "-p",
            "pi-tui",
            "--bin",
            "pi_tui_state_matrix_fixture",
            "--quiet",
        ])
        .status()
        .map_err(|error| {
            CorpusError::Prerequisite(format!("fixture build spawn failed: {error}"))
        })?;
    if !status.success() {
        return Err(CorpusError::Prerequisite(
            "fixture build failed; hard-failing transcript corpus prerequisites".to_owned(),
        ));
    }
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/debug/pi_tui_state_matrix_fixture");
    if path.exists() {
        Ok(path)
    } else {
        Err(CorpusError::Prerequisite(format!(
            "fixture binary missing after build at {}",
            path.display()
        )))
    }
}

fn host_row_id() -> RowId {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "aarch64") => RowId::GnuArm64,
        ("macos", "x86_64") => RowId::DarwinX64,
        ("macos", "aarch64") => RowId::DarwinArm64,
        ("windows", _) => RowId::WindowsX64,
        _ => RowId::GnuX64,
    }
}

/// Row directory label + `RunnerRow`. Absent `PI_TUI_TIER_ROW` ⇒ `local`.
fn resolve_row() -> Result<(String, RunnerRow), CorpusError> {
    match std::env::var("PI_TUI_TIER_ROW") {
        Ok(value) if !value.trim().is_empty() => {
            let value = value.trim().to_owned();
            let (tier, id, runner_image) = parse_tier_row(&value)?;
            Ok((
                value,
                RunnerRow {
                    tier,
                    id,
                    runner_image,
                },
            ))
        }
        _ => Ok((
            "local".to_owned(),
            RunnerRow {
                tier: RowTier::Local,
                id: host_row_id(),
                runner_image: None,
            },
        )),
    }
}

fn parse_tier_row(raw: &str) -> Result<(RowTier, RowId, Option<String>), CorpusError> {
    if raw == "local" {
        return Ok((RowTier::Local, host_row_id(), None));
    }
    let (tier_prefix, rest) = if let Some(rest) = raw.strip_prefix("tier-n/") {
        (RowTier::TierN, rest)
    } else if let Some(rest) = raw.strip_prefix("tier-n:") {
        (RowTier::TierN, rest)
    } else {
        (RowTier::Local, raw)
    };
    let (id_raw, image) = match rest.split_once('@') {
        Some((id, image)) => (id, Some(image.to_owned())),
        None => (rest, None),
    };
    let id = match id_raw {
        "gnu-x64" | "GnuX64" => RowId::GnuX64,
        "gnu-arm64" | "GnuArm64" => RowId::GnuArm64,
        "darwin-x64" | "DarwinX64" => RowId::DarwinX64,
        "darwin-arm64" | "DarwinArm64" => RowId::DarwinArm64,
        "windows-x64" | "WindowsX64" => RowId::WindowsX64,
        other => {
            return Err(CorpusError::Prerequisite(format!(
                "unknown PI_TUI_TIER_ROW id {other}"
            )));
        }
    };
    if tier_prefix == RowTier::TierN && image.as_ref().is_none_or(String::is_empty) {
        return Err(CorpusError::Prerequisite(
            "Tier-n PI_TUI_TIER_ROW requires @runner-image".to_owned(),
        ));
    }
    Ok((tier_prefix, id, image))
}

fn target_root() -> PathBuf {
    if let Ok(target) = std::env::var("CARGO_TARGET_DIR") {
        return PathBuf::from(target);
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target")
}

fn artifact_path(row_label: &str, scenario_dir: &str, iteration: usize) -> PathBuf {
    target_root()
        .join("verification/tui-transcripts")
        .join(row_label)
        .join(scenario_dir)
        .join(format!("run-{}", iteration + 1))
        .join("transcript.artifact.json")
}

fn write_artifact(
    row_label: &str,
    scenario_dir: &str,
    iteration: usize,
    artifact: &TranscriptArtifact,
) -> Result<PathBuf, CorpusError> {
    if artifact.row.tier == RowTier::TierN && row_label == "local" {
        return Err(CorpusError::Assert(
            "local runs must never claim Tier N".to_owned(),
        ));
    }
    let path = artifact_path(row_label, scenario_dir, iteration);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let body = serde_json::to_vec_pretty(artifact)
        .map_err(|error| CorpusError::Io(format!("serialize artifact: {error}")))?;
    let mut file = fs::File::create(&path)?;
    file.write_all(&body)?;
    file.write_all(b"\n")?;
    Ok(path)
}

/// Per-row verdict record: per-state pass/fail, k, digest, and tier labels.
fn write_verdict(
    row_label: &str,
    row: &RunnerRow,
    digest: &str,
    states: &[(&'static str, &'static str)],
) -> Result<PathBuf, CorpusError> {
    let path = target_root()
        .join("verification/tui-transcripts")
        .join(row_label)
        .join("state-matrix")
        .join("verdict.json");
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let verdict = serde_json::json!({
        "stableId": "TUI-V1",
        "corpus": "state-matrix",
        "row": {
            "label": row_label,
            "tier": format!("{:?}", row.tier).to_lowercase(),
            "id": format!("{:?}", row.id).to_lowercase(),
            "runnerImage": row.runner_image,
        },
        "k": K,
        "digest": digest,
        "states": states
            .iter()
            .map(|(state, verdict)| { (state.to_owned(), verdict.to_owned()) })
            .collect::<BTreeMap<_, _>>(),
    });
    let body = serde_json::to_vec_pretty(&verdict)
        .map_err(|error| CorpusError::Io(format!("serialize verdict: {error}")))?;
    fs::write(&path, body)?;
    Ok(path)
}

fn standard_claims() -> Vec<ClaimClass> {
    vec![
        ClaimClass::Execution,
        ClaimClass::Protocol,
        ClaimClass::Pty,
        ClaimClass::Render,
        ClaimClass::SynchronizedOutput,
        ClaimClass::NoClear,
        ClaimClass::Snapshot,
    ]
}

fn assert_no_clear_balanced(raw: &[u8], label: &str) -> Result<(), CorpusError> {
    let audit = audit_bytes(raw);
    if audit.clear_2j != 0 || audit.clear_3j != 0 {
        return Err(CorpusError::Assert(format!(
            "{label}: clear sequences forbidden (2J={}, 3J={})",
            audit.clear_2j, audit.clear_3j
        )));
    }
    if audit.sync_begin != audit.sync_end {
        return Err(CorpusError::Assert(format!(
            "{label}: unbalanced sync markers begin={} end={}",
            audit.sync_begin, audit.sync_end
        )));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// State-matrix scenario runner
// ---------------------------------------------------------------------------

/// One recorded state-matrix run: settles every state checkpoint, asserts
/// per-state snapshot content and the per-state quality bar, then emits a
/// validator-clean artifact.
fn run_state_matrix(
    iteration: usize,
    row_label: &str,
    row: &RunnerRow,
) -> Result<TranscriptArtifact, CorpusError> {
    let binary = fixture_binary()?;
    let argv = vec![binary.to_string_lossy().into_owned()];
    let mut run = FixtureRun::open(
        argv,
        Scenario::FixtureStateMatrix,
        row.clone(),
        standard_claims(),
    )?;

    for (state, marker, snapshot_needle) in STATE_MATRIX {
        // Settle on the state checkpoint (content predicate, never a timer).
        // Focus-marked renders two commits (focused with the hardware cursor
        // annotation, then unfocused); both land under the same STATUS
        // checkpoint and are captured in the settled snapshot.
        let frame = match run.settle_frame(|bytes| contains_bytes(bytes, marker)) {
            Ok(frame) => frame,
            Err(error) => {
                return Err(CorpusError::Assert(format!(
                    "state-matrix/{state}: settle failed: {error}"
                )));
            }
        };
        if !frame
            .snapshot
            .lines
            .iter()
            .any(|line| line.contains(snapshot_needle))
            && !contains_bytes(run.raw_so_far(), snapshot_needle.as_bytes())
        {
            return Err(CorpusError::Assert(format!(
                "state-matrix/{state}: settled snapshot missing {snapshot_needle:?}"
            )));
        }
        // Per-state quality bar: no clears, balanced sync at this checkpoint.
        assert_no_clear_balanced(run.raw_so_far(), &format!("state-matrix/{state}"))?;
        // Step the fixture into the next state.
        run.write_step()?;
    }

    // Final settle: DONE marker.
    let frame = run.settle_frame(|bytes| contains_bytes(bytes, b"DONE-MARKER"))?;
    if !frame
        .snapshot
        .lines
        .iter()
        .any(|line| line.contains("STATE-MATRIX COMPLETE"))
    {
        return Err(CorpusError::Assert(
            "state-matrix: settled final snapshot missing completion line".to_owned(),
        ));
    }
    assert_no_clear_balanced(run.raw_so_far(), "state-matrix/final")?;

    let artifact = run.finish()?;
    // Validator-clean gate: the artifact must pass the schema-v1 validator.
    validate_artifact(&artifact)
        .map_err(|error| CorpusError::Assert(format!("state-matrix: validator: {error}")))?;
    let path = write_artifact(row_label, "state-matrix", iteration, &artifact)?;
    // The serialized artifact must round-trip validator-clean too.
    let body = fs::read_to_string(&path)?;
    let value: serde_json::Value = serde_json::from_str(&body)
        .map_err(|error| CorpusError::Io(format!("re-parse artifact: {error}")))?;
    pi_tui::testkit::validate::validate_value(&value)
        .map_err(|error| CorpusError::Assert(format!("state-matrix: file validator: {error}")))?;
    Ok(artifact)
}

fn run_scenario_k(
    scenario_dir: &str,
    producer: impl FnMut(usize) -> Result<TranscriptArtifact, CorpusError>,
) -> Result<(), CorpusError> {
    match run_k(K, producer) {
        Ok(report) => {
            assert_eq!(report.k, K);
            assert!(
                report.digest.starts_with("sha256:"),
                "{scenario_dir}: digest must be sha256-prefixed"
            );
            assert!(
                !report.canonical_bytes.is_empty(),
                "{scenario_dir}: canonical bytes must be non-empty"
            );
            Ok(())
        }
        Err(RepeatError::Divergence {
            first_divergent_seq,
            left_digest,
            right_digest,
        }) => Err(CorpusError::Assert(format!(
            "{scenario_dir}: run-to-run divergence at seq {first_divergent_seq}: {left_digest} != {right_digest}"
        ))),
        Err(error) => Err(CorpusError::Assert(format!("{scenario_dir}: {error}"))),
    }
}

#[test]
fn transcript_state_matrix_corpus() -> Result<(), CorpusError> {
    let (row_label, row) = resolve_row()?;
    if row_label == "local" {
        assert_eq!(row.tier, RowTier::Local);
    }

    let binary = fixture_binary()?;
    require_prerequisites(&binary.to_string_lossy())?;

    let row_run = row.clone();
    let label_run = row_label.clone();
    let digest_cell = std::cell::RefCell::new(String::new());
    run_scenario_k("state-matrix", |iteration| {
        let artifact = run_state_matrix(iteration, &label_run, &row_run)?;
        *digest_cell.borrow_mut() = artifact.digest.clone();
        Ok(artifact)
    })?;

    // Per-row verdict record: all eight states settled pass with a k>=3
    // identical canonical digest.
    let states: Vec<(&'static str, &'static str)> =
        STATE_MATRIX.iter().map(|(s, _, _)| (*s, "pass")).collect();
    let digest = digest_cell.into_inner();
    write_verdict(&row_label, &row, &digest, &states)?;
    Ok(())
}
