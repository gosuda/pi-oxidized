//! Automated accessibility-invariant lane (TUI-V6, issue #72).

#![cfg(all(unix, feature = "testkit"))]
//!
//! Drives the `pi_tui_a11y_fixture` binary through the PTY harness to
//! produce validator-clean schema-v1 transcripts, then computes the three
//! automated accessibility invariants over the CANONICAL settled content
//! of every recorded row:
//!
//! - **notice persistence** — the transient notice text is present in at
//!   least one settled frame; the 2s urgency window is asserted only as a
//!   measured field against the timing envelope with a pinned tolerance
//!   (never as canonical content).
//! - **static sufficiency** — every spinner-status frame (a settled frame
//!   carrying the cancel hint) contains the kind label, the elapsed
//!   counter, and the cancel hint.
//! - **anti-chatter** — within one settled stage (the canonical events
//!   between two input boundaries), an identical announcement string
//!   occupies at most one settled frame consecutively; repeats are counted
//!   over logical sequence numbers.
//!
//! Row selection mirrors the fixture corpora: absent `PI_TUI_TIER_ROW` the
//! run records `local` evidence on the host row (never Tier N);
//! `tier-n/<row>@<image>` records the pinned CI runner row and the
//! validator enforces the Tier N driver pairings.
//!
//! Artifacts land under
//! `target/verification/tui-transcripts/<row>/a11y-gauntlet/run-{1,2,3}/`
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
    CanonicalEvent, CapabilityProfile, ClaimClass, DriverKind, Geometry, NormalizationContext,
    OutputCanon, RowId, RowTier, RunnerRow, Scenario, TimingEnvelope, TranscriptArtifact,
    TranscriptMode, TranscriptRecorder, TranscriptSpec,
};
use pi_tui::testkit::validate::validate_artifact;
use pi_tui::testkit::{RecordingError, RecordingSession};

const INITIAL_COLS: u16 = 80;
const INITIAL_ROWS: u16 = 24;
const K: usize = 3;

/// Notice needle mirroring the product `push_notice("export", …)` payload
/// (`CustomMessageView { custom_type: "export", text }`).
const NOTICE_NEEDLE: &str = "Session exported to: verification-export.jsonl";

/// Nominal urgency window (2s) — reference value only; the assertion is a
/// tolerated measured field against the timing envelope, never canonical.
const NOTICE_URGENCY_NOMINAL_MS: u64 = 2_000;

/// Pinned tolerance for the measured notice window: the settle-abort
/// ceiling (10s). Observed values inside the band are recorded as
/// `tolerated`; values outside are hard failures of the measurement
/// channel itself, not of canonical content.
const NOTICE_URGENCY_TOLERANCE_MS: u64 = 10_000;

// ---------------------------------------------------------------------------
// Settled-frame view — the canonical input of the three invariants
// ---------------------------------------------------------------------------

/// One settled frame as the invariants see it: logical sequence number,
/// the settled stage it belongs to (index of the input boundary group),
/// and the canonical snapshot lines.
#[derive(Clone, Debug, Eq, PartialEq)]
struct SettledFrameView {
    seq: u32,
    stage: usize,
    lines: Vec<String>,
}

/// Extract settled frames from an artifact, grouping by input boundaries:
/// stage N is the maximal run of canonical events after the N-th `Input`
/// event (stage 0 runs from `Spawn`). Settled stages are delimited by
/// input boundaries — content changes by construction of the stepped
/// corpora — so anti-chatter never compares across stages.
fn frames_from_artifact(artifact: &TranscriptArtifact) -> Vec<SettledFrameView> {
    let mut stage = 0usize;
    let mut frames = Vec::new();
    for event in &artifact.canonical.events {
        match event {
            CanonicalEvent::Input { .. } => stage += 1,
            CanonicalEvent::Snapshot { seq, lines, .. } => frames.push(SettledFrameView {
                seq: *seq,
                stage,
                lines: lines.clone(),
            }),
            _ => {}
        }
    }
    frames
}

/// Announcement string for one settled frame: the deterministic text a
/// screen reader would voice for the frame — trimmed non-empty snapshot
/// lines joined by newline.
fn announcement(frame: &SettledFrameView) -> String {
    frame
        .lines
        .iter()
        .map(|line| line.trim())
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Whether a settled line is a spinner-status line (product
/// `status_message` shape ends with the cancel hint).
fn is_spinner_status_line(line: &str) -> bool {
    line.trim_end().ends_with(" to cancel")
}

// ---------------------------------------------------------------------------
// Invariant 1 — notice persistence (canonical) + urgency window (measured)
// ---------------------------------------------------------------------------

/// Canonical notice-persistence check: the transient notice text must be
/// present in at least one settled frame. Returns the number of settled
/// frames carrying the notice, or `Err` with the violation.
fn check_notice_persistence(frames: &[SettledFrameView]) -> Result<usize, String> {
    let carriers = frames
        .iter()
        .filter(|frame| frame.lines.iter().any(|line| line.contains(NOTICE_NEEDLE)))
        .count();
    if carriers == 0 {
        return Err(format!(
            "notice-persistence: no settled frame contains the notice text {NOTICE_NEEDLE:?}"
        ));
    }
    Ok(carriers)
}

/// Measured-field verdict for the notice urgency window: quarantined
/// against the timing envelope, judged only against the pinned tolerance
/// band, and never able to alter canonical content or the digest.
#[derive(serde::Serialize)]
struct UrgencyWindowField {
    nominal_ms: u64,
    observed_ms: u64,
    tolerance_ms: u64,
    verdict: &'static str,
}

fn check_urgency_window(observed_ms: u64) -> Result<UrgencyWindowField, String> {
    if observed_ms > NOTICE_URGENCY_TOLERANCE_MS {
        return Err(format!(
            "notice urgency window measured {observed_ms}ms exceeds the pinned tolerance {NOTICE_URGENCY_TOLERANCE_MS}ms — measurement channel failure, not canonical content"
        ));
    }
    Ok(UrgencyWindowField {
        nominal_ms: NOTICE_URGENCY_NOMINAL_MS,
        observed_ms,
        tolerance_ms: NOTICE_URGENCY_TOLERANCE_MS,
        verdict: "tolerated",
    })
}

// ---------------------------------------------------------------------------
// Invariant 2 — static sufficiency
// ---------------------------------------------------------------------------

/// Static-sufficiency check: every spinner-status frame carries the kind
/// label, the elapsed counter, and the cancel hint. A spinner-status frame
/// is any settled frame with a line ending in the cancel hint (the product
/// `status_message` suffix ` · {key} to cancel`). The canonical line shape
/// is `{spinner} {kind} {N}s · {key} to cancel`; the separator is the last
/// `·` in the line. Returns the spinner-frame count or the violation list.
fn check_static_sufficiency(frames: &[SettledFrameView]) -> Result<usize, Vec<String>> {
    let mut violations = Vec::new();
    let mut spinner_frames = 0usize;
    for frame in frames {
        for line in &frame.lines {
            if !is_spinner_status_line(line) {
                continue;
            }
            spinner_frames += 1;
            let text = line.trim();
            let separator = text
                .rfind('·')
                .unwrap_or_else(|| text.len().saturating_sub(" to cancel".len()));
            let head = text[..separator].trim();
            let (elapsed_token, kind) = match head.rsplit_once(' ') {
                Some((kind, elapsed)) => (elapsed, kind.trim()),
                None => (head, ""),
            };
            let digits = elapsed_token.strip_suffix('s').unwrap_or("");
            let elapsed_ok = !digits.is_empty() && digits.chars().all(|ch| ch.is_ascii_digit());
            let kind_ok =
                !kind.is_empty() && kind.chars().any(|ch| ch.is_alphabetic() || ch == '…');
            if !kind_ok {
                violations.push(format!(
                    "static-sufficiency: spinner-status frame seq {} lacks the kind label: {text:?}",
                    frame.seq
                ));
            }
            if !elapsed_ok {
                violations.push(format!(
                    "static-sufficiency: spinner-status frame seq {} lacks the elapsed counter: {text:?}",
                    frame.seq
                ));
            }
            // The cancel hint itself is guaranteed by classification: a line
            // is only a spinner-status line because it ends with the hint.
        }
    }
    if violations.is_empty() && spinner_frames == 0 {
        violations.push(
            "static-sufficiency: corpus records zero spinner-status frames — the invariant is unexercised"
                .to_owned(),
        );
    }
    if violations.is_empty() {
        Ok(spinner_frames)
    } else {
        Err(violations)
    }
}

// ---------------------------------------------------------------------------
// Invariant 3 — anti-chatter
// ---------------------------------------------------------------------------

/// Anti-chatter check: within one settled stage, an identical announcement
/// string occupies at most one settled frame consecutively — consecutive
/// settled frames in the same stage must announce different content,
/// counted over logical sequence numbers. Identical announcements separated
/// by a content change inside the stage are allowed; identical
/// announcements across a stage boundary are never compared (the boundary
/// is itself a content change).
fn check_anti_chatter(frames: &[SettledFrameView]) -> Result<usize, Vec<String>> {
    let mut violations = Vec::new();
    let mut stages = 0usize;
    let mut prior: Option<(usize, String, u32)> = None; // (stage, announcement, seq)
    for frame in frames {
        let text = announcement(frame);
        if let Some((prior_stage, prior_text, prior_seq)) = &prior
            && *prior_stage == frame.stage
            && *prior_text == text
        {
            violations.push(format!(
                "anti-chatter: identical announcement held across settled frames seq {prior_seq} and seq {} inside stage {}: {:?}",
                frame.seq, frame.stage, text
            ));
        }
        if prior
            .as_ref()
            .is_none_or(|(stage, _, _)| *stage != frame.stage)
        {
            stages += 1;
        }
        prior = Some((frame.stage, text, frame.seq));
    }
    if violations.is_empty() {
        Ok(stages)
    } else {
        Err(violations)
    }
}

// ---------------------------------------------------------------------------
// CorpusError / FixtureRun — mirrors transcript_state_matrix.rs
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

type HostSession = <PosixPtyDriver as TerminalDriver>::Session;

struct FixtureRun {
    recording: RecordingSession<HostSession>,
    context: NormalizationContext,
    policy: SettlePolicy,
    raw_acc: Vec<u8>,
    wall_started: Instant,
    settle_windows_ms: Vec<u64>,
    /// Wall offset (ms from recording start) at each completed settle — the
    /// timing source for quarantined measured fields.
    settle_wall_offsets_ms: Vec<u64>,
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
            output_canon: OutputCanon::Bytes,
        });
        let recording = RecordingSession::new(session, recorder, argv, &context)?;
        Ok(Self {
            recording,
            context,
            policy: SettlePolicy::new(Duration::from_millis(120), Duration::from_secs(10))?,
            raw_acc: Vec::new(),
            wall_started: Instant::now(),
            settle_windows_ms: Vec::new(),
            settle_wall_offsets_ms: Vec::new(),
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
        self.settle_wall_offsets_ms
            .push(u64::try_from(self.wall_started.elapsed().as_millis()).unwrap_or(u64::MAX));
        self.raw_acc.extend_from_slice(&frame.batch.bytes);
        Ok(frame)
    }

    /// Advance the stepped fixture one stage (single step key event).
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
    let _ = DriverGeometry::new(1, 1).map_err(|error| {
        CorpusError::Prerequisite(format!("geometry prerequisite failed: {error}"))
    })?;
    Ok(())
}

fn fixture_binary() -> Result<PathBuf, CorpusError> {
    if let Ok(path) = std::env::var("CARGO_BIN_EXE_pi_tui_a11y_fixture") {
        let path = PathBuf::from(path);
        if path.exists() {
            return Ok(path);
        }
        return Err(CorpusError::Prerequisite(format!(
            "CARGO_BIN_EXE_pi_tui_a11y_fixture points at missing binary: {}",
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
            let path = root.join(profile).join("pi_tui_a11y_fixture");
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
            "pi_tui_a11y_fixture",
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
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/debug/pi_tui_a11y_fixture");
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
// A11y gauntlet scenario runner
// ---------------------------------------------------------------------------

/// One scripted frame of the fixture walk: its unique checkpoint marker
/// (one settled frame per step; content changes between frames so
/// consecutive announcements are never identical).
struct StageSpec {
    label: &'static str,
}

const STAGES: [StageSpec; 10] = [
    StageSpec { label: "notice" },
    StageSpec {
        label: "notice-tick-1",
    },
    StageSpec {
        label: "working-4s",
    },
    StageSpec {
        label: "working-5s",
    },
    StageSpec {
        label: "working-6s",
    },
    StageSpec { label: "retry-2s" },
    StageSpec { label: "retry-3s" },
    StageSpec {
        label: "compaction-7s",
    },
    StageSpec {
        label: "compaction-8s",
    },
    StageSpec {
        label: "DONE-MARKER",
    },
];

/// Settle needle for one scripted frame: its unique OSC 999 checkpoint.
fn stage_needle(stage: &StageSpec) -> String {
    format!("PI_TUI_STAGE={}", stage.label)
}

/// Per-run analysis outcome: the three invariant verdicts plus the
/// quarantined measured field, computed over the recorded artifact.
struct A11yVerdict {
    notice_frames: usize,
    spinner_frames: usize,
    chatter_stages: usize,
    urgency_window_ms: u64,
}

/// One recorded a11y-gauntlet run: settles every sub-frame of every stage,
/// asserts the per-run quality bar, then emits a validator-clean artifact
/// and computes the three invariants over its canonical content.
fn run_a11y_gauntlet(
    iteration: usize,
    row_label: &str,
    row: RunnerRow,
) -> Result<(TranscriptArtifact, A11yVerdict), CorpusError> {
    let binary = fixture_binary()?;
    let argv = vec![binary.to_string_lossy().into_owned()];
    let mut run = FixtureRun::open(argv, Scenario::FixtureA11yGauntlet, row, standard_claims())?;

    // Wall offsets of the settled frames carrying the notice text — the
    // timing source for the quarantined urgency-window measured field.
    let mut notice_settle_indices: Vec<usize> = Vec::new();
    for (index, stage) in STAGES.iter().enumerate() {
        let needle = stage_needle(stage);
        let frame = match run.settle_frame(|bytes| contains_bytes(bytes, needle.as_bytes())) {
            Ok(frame) => frame,
            Err(error) => {
                return Err(CorpusError::Assert(format!(
                    "a11y-gauntlet/{}: settle on {needle:?} failed: {error}",
                    stage.label
                )));
            }
        };
        // The first two scripted frames are the notice pair: the railed
        // notice present in both settled snapshots is the timing source
        // for the quarantined urgency-window measured field.
        if index < 2 {
            if !frame
                .snapshot
                .lines
                .iter()
                .any(|line| line.contains(NOTICE_NEEDLE))
            {
                return Err(CorpusError::Assert(format!(
                    "a11y-gauntlet/{}: settled snapshot missing the notice text {NOTICE_NEEDLE:?}",
                    stage.label
                )));
            }
            notice_settle_indices.push(index);
        }
        if stage.label == "DONE-MARKER"
            && !frame
                .snapshot
                .lines
                .iter()
                .any(|line| line.contains("A11Y-GAUNTLET COMPLETE"))
        {
            return Err(CorpusError::Assert(
                "a11y-gauntlet: settled final snapshot missing completion line".to_owned(),
            ));
        }
        if stage.label != "DONE-MARKER" {
            run.write_step()?;
        }
    }
    assert_no_clear_balanced(run.raw_so_far(), "a11y-gauntlet/final")?;

    // Measured urgency window: wall span between the first and last
    // notice-carrying settled frames, from the settle wall offsets.
    let urgency_window_ms = match (notice_settle_indices.first(), notice_settle_indices.last()) {
        (Some(first), Some(last)) => {
            run.settle_wall_offsets_ms[*last].saturating_sub(run.settle_wall_offsets_ms[*first])
        }
        _ => 0,
    };

    let artifact = run.finish()?;
    // Validator-clean gate: the artifact must pass the schema-v1 validator.
    validate_artifact(&artifact)
        .map_err(|error| CorpusError::Assert(format!("a11y-gauntlet: validator: {error}")))?;
    let path = write_artifact(row_label, "a11y-gauntlet", iteration, &artifact)?;
    // The serialized artifact must round-trip validator-clean too.
    let body = fs::read_to_string(&path)?;
    let value: serde_json::Value = serde_json::from_str(&body)
        .map_err(|error| CorpusError::Io(format!("re-parse artifact: {error}")))?;
    pi_tui::testkit::validate::validate_value(&value)
        .map_err(|error| CorpusError::Assert(format!("a11y-gauntlet: file validator: {error}")))?;

    // The three invariants over the canonical settled content.
    let frames = frames_from_artifact(&artifact);
    let notice_frames = check_notice_persistence(&frames).map_err(CorpusError::Assert)?;
    let spinner_frames = check_static_sufficiency(&frames)
        .map_err(|violations| CorpusError::Assert(violations.join("; ")))?;
    let chatter_stages = check_anti_chatter(&frames)
        .map_err(|violations| CorpusError::Assert(violations.join("; ")))?;
    // Quarantined measured field: judged only against the pinned tolerance
    // band; the observed value is recorded in the per-row verdict below.
    check_urgency_window(urgency_window_ms).map_err(CorpusError::Assert)?;

    Ok((
        artifact,
        A11yVerdict {
            notice_frames,
            spinner_frames,
            chatter_stages,
            urgency_window_ms,
        },
    ))
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

/// Per-row verdict record: per-invariant verdicts, k, digest, measured
/// fields (quarantined against the timing envelope), and tier labels.
fn write_verdict(
    row_label: &str,
    row: &RunnerRow,
    digest: &str,
    verdict: &A11yVerdict,
) -> Result<PathBuf, CorpusError> {
    let path = target_root()
        .join("verification/tui-transcripts")
        .join(row_label)
        .join("a11y-gauntlet")
        .join("verdict.json");
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let record = serde_json::json!({
        "stableId": "TUI-V6",
        "corpus": "a11y-gauntlet",
        "row": {
            "label": row_label,
            "tier": format!("{:?}", row.tier).to_lowercase(),
            "id": format!("{:?}", row.id).to_lowercase(),
            "runnerImage": row.runner_image,
        },
        "k": K,
        "digest": digest,
        "invariants": {
            "noticePersistence": {
                "verdict": "pass",
                "noticeFrames": verdict.notice_frames,
            },
            "staticSufficiency": {
                "verdict": "pass",
                "spinnerFrames": verdict.spinner_frames,
            },
            "antiChatter": {
                "verdict": "pass",
                "settledStages": verdict.chatter_stages,
                "corpusShape": "one settled frame per stage; the cross-frame comparison is exercised by the synthetic probes",
            },
        },
        "measuredFields": {
            "noticeUrgencyWindowMs": {
                "nominalMs": NOTICE_URGENCY_NOMINAL_MS,
                "observedMs": verdict.urgency_window_ms,
                "toleranceMs": NOTICE_URGENCY_TOLERANCE_MS,
                "verdict": "tolerated",
            },
        },
        "manualLane": "docs/TUI-V6-a11y-evidence.md (degraded-verdict limitation row)",
    });
    let body = serde_json::to_vec_pretty(&record)
        .map_err(|error| CorpusError::Io(format!("serialize verdict: {error}")))?;
    fs::write(&path, body)?;
    Ok(path)
}

#[test]
fn transcript_a11y_invariants_corpus() -> Result<(), CorpusError> {
    let (row_label, row) = resolve_row()?;
    if row_label == "local" {
        assert_eq!(row.tier, RowTier::Local);
    }

    let binary = fixture_binary()?;
    require_prerequisites(&binary.to_string_lossy())?;

    let row_run = row.clone();
    let label_run = row_label.clone();
    let digest_cell = std::cell::RefCell::new(String::new());
    let verdict_cell = std::cell::RefCell::new(None::<A11yVerdict>);
    run_scenario_k("a11y-gauntlet", |iteration| {
        let (artifact, verdict) = run_a11y_gauntlet(iteration, &label_run, row_run.clone())?;
        *digest_cell.borrow_mut() = artifact.digest.clone();
        *verdict_cell.borrow_mut() = Some(verdict);
        Ok(artifact)
    })?;

    let digest = digest_cell.into_inner();
    let verdict = verdict_cell
        .into_inner()
        .ok_or_else(|| CorpusError::Assert("a11y-gauntlet: no verdict recorded".to_owned()))?;
    write_verdict(&row_label, &row, &digest, &verdict)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Invariant self-checks — synthetic probes proving each invariant has teeth
// ---------------------------------------------------------------------------

fn frame(seq: u32, stage: usize, lines: &[&str]) -> SettledFrameView {
    SettledFrameView {
        seq,
        stage,
        lines: lines.iter().map(|line| (*line).to_owned()).collect(),
    }
}

#[test]
fn notice_persistence_fails_without_a_notice_frame() -> Result<(), String> {
    let frames = vec![frame(3, 1, &["STATUS spinner-working"])];
    assert!(
        check_notice_persistence(&frames).is_err(),
        "a corpus without the notice text must fail notice-persistence"
    );
    let carriers = check_notice_persistence(&frames_with_notice())?;
    assert_eq!(carriers, 2, "both notice-stage frames carry the notice");
    Ok(())
}

#[test]
fn static_sufficiency_fails_on_missing_elapsed_or_kind() -> Result<(), String> {
    // Missing elapsed counter only: the kind part stays alphabetic so the
    // kind branch passes and this probe isolates the elapsed branch.
    let missing_elapsed = vec![frame(5, 1, &[" ⠋ Working… now · escape to cancel"])];
    assert!(
        check_static_sufficiency(&missing_elapsed).is_err(),
        "spinner-status frame without the elapsed counter must fail"
    );
    // Missing kind label.
    let missing_kind = vec![frame(5, 1, &[" ⠋ 5s · escape to cancel"])];
    assert!(
        check_static_sufficiency(&missing_kind).is_err(),
        "spinner-status frame without the kind label must fail"
    );
    // Non-spinner frames are never classified.
    let plain = vec![frame(1, 0, &["STATUS notice", "│ [export] notice"])];
    assert!(
        check_static_sufficiency(&plain).is_err(),
        "a corpus with zero spinner-status frames must fail as unexercised"
    );
    // The canonical corpus shape passes with all seven spinner frames.
    let spinner =
        check_static_sufficiency(&spinner_frames()).map_err(|violations| violations.join("; "))?;
    assert_eq!(spinner, 7);
    Ok(())
}

#[test]
fn anti_chatter_fails_on_repeated_announcement_inside_a_stage() -> Result<(), String> {
    // Identical announcements in consecutive frames of one stage.
    let chatty = vec![
        frame(2, 0, &["STATUS notice", "GEN 1"]),
        frame(4, 0, &["STATUS notice", "GEN 1"]),
    ];
    assert!(
        check_anti_chatter(&chatty).is_err(),
        "an identical announcement held across two settled frames of one stage must fail"
    );
    // The same two frames split by a stage boundary are never compared.
    let staged = vec![
        frame(2, 0, &["STATUS notice", "GEN 1"]),
        frame(4, 1, &["STATUS notice", "GEN 1"]),
    ];
    check_anti_chatter(&staged).map_err(|violations| violations.join("; "))?;
    // A→B→A inside one stage: each repeat is separated by a content change.
    let abba = vec![
        frame(2, 0, &["tick 1"]),
        frame(4, 0, &["tick 2"]),
        frame(6, 0, &["tick 1"]),
    ];
    check_anti_chatter(&abba).map_err(|violations| violations.join("; "))?;
    Ok(())
}

#[test]
fn urgency_window_is_tolerated_inside_the_pinned_band() -> Result<(), String> {
    let inside = check_urgency_window(0)?;
    assert_eq!(inside.verdict, "tolerated");
    check_urgency_window(NOTICE_URGENCY_NOMINAL_MS)?;
    assert!(
        check_urgency_window(NOTICE_URGENCY_TOLERANCE_MS + 1).is_err(),
        "a window beyond the pinned tolerance is a measurement-channel failure"
    );
    Ok(())
}

/// The two notice-stage frames of the canonical corpus shape.
fn frames_with_notice() -> Vec<SettledFrameView> {
    vec![
        frame(
            2,
            0,
            &[
                "STATUS notice",
                "│ [export] Session exported to: verification-export.jsonl",
                "GEN 1",
            ],
        ),
        frame(
            4,
            0,
            &[
                "STATUS notice",
                "│ [export] Session exported to: verification-export.jsonl",
                "NOTICE-TICK 1 (notice persists)",
                "GEN 2",
            ],
        ),
    ]
}

/// The seven spinner-status frames of the canonical corpus shape
/// (working 4/5/6s, retry 2/3s, compaction 7/8s).
fn spinner_frames() -> Vec<SettledFrameView> {
    let mut frames = Vec::new();
    for (stage, lines) in [
        (1usize, &[" ⠋ Working… 4s · escape to cancel", "GEN 3"][..]),
        (1, &[" ⠋ Working… 5s · escape to cancel", "GEN 4"]),
        (1, &[" ⠋ Working… 6s · escape to cancel", "GEN 5"]),
        (2, &[" ⠋ Retrying… 2s · escape to cancel", "GEN 6"]),
        (2, &[" ⠋ Retrying… 3s · escape to cancel", "GEN 7"]),
        (
            3,
            &[" ⠋ Compacting context… 7s · escape to cancel", "GEN 8"],
        ),
        (
            3,
            &[" ⠋ Compacting context… 8s · escape to cancel", "GEN 9"],
        ),
    ] {
        frames.push(frame(
            u32::try_from(frames.len() * 2 + 2).unwrap_or(0),
            stage,
            lines,
        ));
    }
    frames
}
