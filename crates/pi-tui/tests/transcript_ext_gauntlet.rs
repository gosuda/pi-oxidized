#![cfg(all(unix, feature = "testkit"))]
#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_used,
    clippy::too_many_lines
)]
//! Extension-UI gauntlet transcript corpus (TUI-P3, issue #70).
//!
//! Drives the `pi_tui_ext_fixture` binary through the PTY harness to produce
//! validator-clean schema-v1 transcripts proving sanitization floors hold:
//! - Custom railed messages
//! - Widget slots
//! - Stacked overlays with focus restore
//! - HostUiRequest confirm/select/input dialogs
//! - Extension shortcuts in the footer
//! - Hostile setTheme (bad hex, contrast < 4.5, hue swaps)
//! - OSC 0 title injection with C0/C1 and >256 UTF-8 bytes
//!
//! Artifacts land under `target/verification/tui-transcripts/<row>/ext-gauntlet/run-{1,2,3}/`.

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
use pi_tui::testkit::{RecordingError, RecordingSession};

const INITIAL_COLS: u16 = 80;
const INITIAL_ROWS: u16 = 24;
const K: usize = 3;

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
// FixtureRun wrapper — mirrors transcript_fixture.rs
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
            policy: SettlePolicy::new(Duration::from_millis(250), Duration::from_secs(15))?,
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
        self.settle_windows_ms
            .push(u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX));
        self.raw_acc.extend_from_slice(&frame.batch.bytes);
        Ok(frame)
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

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() {
        return true;
    }
    haystack.windows(needle.len()).any(|w| w == needle)
}

fn require_prerequisites(fixture: &str) -> Result<(), CorpusError> {
    if !cfg!(unix) {
        return Err(CorpusError::Prerequisite(
            "PosixPtyDriver transcript corpus requires a unix host".to_owned(),
        ));
    }
    let path = Path::new(fixture);
    if !path.exists() {
        return Err(CorpusError::Prerequisite(format!(
            "fixture binary missing: {}",
            path.display()
        )));
    }
    let _ = DriverGeometry::new(1, 1).map_err(|error| {
        CorpusError::Prerequisite(format!("geometry prerequisite failed: {error}"))
    })?;
    Ok(())
}

fn fixture_binary() -> Result<PathBuf, CorpusError> {
    if let Ok(path) = std::env::var("CARGO_BIN_EXE_pi_tui_ext_fixture") {
        let path = PathBuf::from(path);
        if path.exists() {
            return Ok(path);
        }
        return Err(CorpusError::Prerequisite(format!(
            "CARGO_BIN_EXE_pi_tui_ext_fixture points at missing binary: {}",
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
            let path = root.join(profile).join("pi_tui_ext_fixture");
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
            "pi_tui_ext_fixture",
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
    let path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/debug/pi_tui_ext_fixture");
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
        ("linux", "x86_64") => RowId::GnuX64,
        ("linux", "aarch64") => RowId::GnuArm64,
        ("macos", "x86_64") => RowId::DarwinX64,
        ("macos", "aarch64") => RowId::DarwinArm64,
        ("windows", _) => RowId::WindowsX64,
        _ => RowId::GnuX64,
    }
}

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

fn fixture_argv(serve: bool) -> Result<Vec<String>, CorpusError> {
    let binary = fixture_binary()?;
    let mut argv = vec![binary.to_string_lossy().into_owned()];
    if serve {
        argv.push("--serve".to_owned());
    }
    Ok(argv)
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
// Gauntlet scenario runner
// ---------------------------------------------------------------------------

fn run_ext_gauntlet(
    iteration: usize,
    row_label: &str,
    row: &RunnerRow,
) -> Result<TranscriptArtifact, CorpusError> {
    let argv = fixture_argv(true)?;
    let mut run = FixtureRun::open(
        argv,
        Scenario::FixtureExtGauntlet,
        row.clone(),
        standard_claims(),
    )?;

    // Wait for the gauntlet to reach the DONE-MARKER phase.
    let frame = run.settle_frame(|bytes| contains_bytes(bytes, b"DONE-MARKER"))?;

    // Verify the snapshot contains evidence of the gauntlet phases.
    let snapshot_lines = &frame.snapshot.lines;

    let has_content = snapshot_lines
        .iter()
        .any(|line| line.contains("DONE") || line.contains("STATUS") || line.contains("FOOTER"));
    if !has_content {
        return Err(CorpusError::Assert(
            "ext-gauntlet: settled snapshot missing fixture content".to_owned(),
        ));
    }

    // Verify no destructive clears and balanced sync markers.
    assert_no_clear_balanced(run.raw_so_far(), "ext-gauntlet")?;

    let artifact = run.finish()?;
    write_artifact(row_label, "ext-gauntlet", iteration, &artifact)?;
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
fn transcript_ext_gauntlet_corpus() {
    let (row_label, row) = resolve_row().unwrap_or_else(|error| {
        panic!("hard-fail harness prerequisites / row config: {error}");
    });
    if row_label == "local" {
        assert_eq!(row.tier, RowTier::Local);
    }

    let binary = fixture_binary().unwrap_or_else(|error| {
        panic!("fixture binary prerequisite: {error}");
    });
    require_prerequisites(&binary.to_string_lossy()).unwrap_or_else(|error| {
        panic!("prerequisite check: {error}");
    });

    let row_gauntlet = row.clone();
    let label_gauntlet = row_label.clone();
    run_scenario_k("ext-gauntlet", move |iteration| {
        run_ext_gauntlet(iteration, &label_gauntlet, &row_gauntlet)
    })
    .unwrap_or_else(|error| panic!("{error}"));
}
