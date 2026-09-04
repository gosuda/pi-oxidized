//! Unicode/width gauntlet corpus (TUI-V3, issue #81).
//!
//! Drives `pi_tui_unicode_gauntlet_fixture` through the POSIX PTY harness to
//! produce schema-v1 transcripts and binary per-probe verdicts:
//!
//! - `railed`         — real `Rail` + `paint_lines` rows, closing `│`
//!   sentinel at the contract-computed column
//! - `table-1..3`     — real `Markdown` tables with probe cells; right
//!   border aligned across header and every row
//! - `editor-Pxx`     — focused `Input`; cursor at `2 + contract width`
//!   after each probe (hardware cursor oracle)
//! - `overlay`        — base rows plus `write_overlay_cells` overlay
//!   rows; base sentinel beyond overlay + overlay
//!   sentinel both at contract columns
//! - `paste`          — multiline `Editor` paste (verbatim, atomic undo,
//!   large-paste marker); body-line sentinels aligned
//!
//! The 13-probe corpus comes from `docs/TUI-R2-terminal-width-table-divergence.md`.
//! Each probe has a contract column count from `pi_tui::text::visible_width`.
//! The snapshot line columns are observed by walking the settled AVT line
//! with per-character avt-width rules (same as the AVT driver). M/D verdicts
//! are binary per scenario/probe, with divergences routed to the TUI-V3
//! record — never to a width.rs change or per-terminal code branch.

#![cfg(all(unix, feature = "testkit"))]

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
    CapabilityProfile, ClaimClass, DriverKind, Geometry, NormalizationContext, OutputCanon, RowId,
    RowTier, RunnerRow, Scenario, TimingEnvelope, TranscriptArtifact, TranscriptMode,
    TranscriptRecorder, TranscriptSpec,
};
use pi_tui::testkit::validate::validate_artifact;
use pi_tui::testkit::{RecordingError, RecordingSession};
use pi_tui::text::{normalize_terminal_output, visible_width};
use unicode_width::UnicodeWidthChar as _;

const INITIAL_COLS: u16 = 80;
const INITIAL_ROWS: u16 = 24;
const K: usize = 3;

/// Fixed single-width ASCII filler between the probe and closing sentinel.
const FILLER: &str = "abcdef";

/// The 13-probe width corpus from the TUI-R2 survey.
const CORPUS: [(&str, &str); 13] = [
    ("P01", "OK"),
    ("P02", "\t"),
    ("P03", "\u{b0}\u{b1}\u{25a0}"),
    ("P04", "\u{6f22}\u{5b57}"),
    ("P05", "\u{ff71}\u{ff8f}"),
    ("P06", "\u{ff21}\u{ff01}"),
    ("P07", "e\u{301}"),
    ("P08", "\u{200b}"),
    ("P09", "\u{2764}\u{fe0f}"),
    (
        "P10",
        "\u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}\u{200d}\u{1f466}",
    ),
    ("P11", "\u{1f1fa}\u{1f1f8}"),
    ("P12", "\u{1f1fa}"),
    ("P13", "\u{e17}\u{e33}\u{e97}\u{eb3}"),
];

/// Raw tab (P02) is excluded from GFM tables.
const TABLE_SKIP: [usize; 1] = [1];

/// Overlay compositing geometry (must match the fixture constants).
const OVERLAY_COL: u16 = 12;
const OVERLAY_WIDTH: u16 = 30;
const BASE_SENTINEL_COL: u16 = 44;

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
// FixtureRun wrapper — mirrors transcript_state_matrix.rs
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
        env.insert("PI_HARDWARE_CURSOR".to_owned(), "1".to_owned());
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
            policy: SettlePolicy::new(Duration::from_millis(200), Duration::from_secs(15))?,
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
// Column walking: map a snapshot line to terminal cell columns the same
// way the AVT emulator does (per-character, with the same width rules).
// ---------------------------------------------------------------------------

/// Terminal column advance for one character as observed by avt 0.18.
///
/// avt uses `char_display_width`: Double iff `unicode-width` reports width 2,
/// otherwise Single. Non-`Some(2)` values include zero-width classes printed
/// as their own single cells, which the gauntlet records honestly.
fn avt_char_width(ch: char) -> usize {
    if ch.width() == Some(2) { 2 } else { 1 }
}

/// Return the column of the rightmost `needle` in `line`, or `None`.
fn avt_column_of_last(line: &str, needle: char) -> Option<usize> {
    let mut col = 0usize;
    let mut best = None;
    for ch in line.chars() {
        if ch == needle {
            best = Some(col);
        }
        col += avt_char_width(ch);
    }
    best
}

/// Return the column of the first `needle` in `line`, or `None`.
fn avt_column_of_first(line: &str, needle: char) -> Option<usize> {
    let mut col = 0usize;
    for ch in line.chars() {
        if ch == needle {
            return Some(col);
        }
        col += avt_char_width(ch);
    }
    None
}

/// Walk `line` and return the starting column of the first occurrence of
/// `needle` as a contiguous character sequence, or `None`.
fn avt_column_of_substr(line: &str, needle: &str) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    let Some(first) = needle.chars().next() else {
        return Some(0);
    };
    let mut col = 0usize;
    let mut chars = line.chars();
    while let Some(ch) = chars.next() {
        if ch == first {
            let mut trial = chars.clone();
            let mut matched = 1usize;
            for need in needle.chars().skip(1) {
                if trial.next().is_some_and(|c| c == need) {
                    matched += 1;
                } else {
                    break;
                }
            }
            if matched == needle.chars().count() {
                return Some(col);
            }
        }
        col += avt_char_width(ch);
    }
    None
}

/// Find the first snapshot line that contains `needle` and is not empty.
fn find_line<'a>(snapshot: &'a [String], needle: &str) -> Option<&'a str> {
    snapshot
        .iter()
        .find(|line| line.contains(needle))
        .map(String::as_str)
}

// ---------------------------------------------------------------------------
// Prerequisites and artifact I/O — mirrors transcript_state_matrix.rs
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
    if let Ok(path) = std::env::var("CARGO_BIN_EXE_pi_tui_unicode_gauntlet_fixture") {
        let path = PathBuf::from(path);
        if path.exists() {
            return Ok(path);
        }
        return Err(CorpusError::Prerequisite(format!(
            "CARGO_BIN_EXE_pi_tui_unicode_gauntlet_fixture points at missing binary: {}",
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
            let path = root.join(profile).join("pi_tui_unicode_gauntlet_fixture");
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
            "pi_tui_unicode_gauntlet_fixture",
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
        .join("../../target/debug/pi_tui_unicode_gauntlet_fixture");
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

fn resolve_row() -> Result<(String, RunnerRow), CorpusError> {
    match std::env::var("PI_TUI_TIER_ROW") {
        Ok(value) if !value.trim().is_empty() => parse_tier_row(&value),
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

fn parse_tier_row(raw: &str) -> Result<(String, RunnerRow), CorpusError> {
    if raw == "local" {
        return Ok((
            "local".to_owned(),
            RunnerRow {
                tier: RowTier::Local,
                id: host_row_id(),
                runner_image: None,
            },
        ));
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
    Ok((
        raw.to_owned(),
        RunnerRow {
            tier: tier_prefix,
            id,
            runner_image: image,
        },
    ))
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

fn write_verdict(
    row_label: &str,
    row: &RunnerRow,
    digest: &str,
    verdicts: &BTreeMap<String, Vec<(String, &'static str)>>,
) -> Result<PathBuf, CorpusError> {
    let path = target_root()
        .join("verification/tui-transcripts")
        .join(row_label)
        .join("unicode-gauntlet")
        .join("verdict.json");
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut flat: BTreeMap<String, String> = BTreeMap::new();
    for (scenario, list) in verdicts {
        for (label, verdict) in list {
            flat.insert(format!("{scenario}/{label}"), (*verdict).to_owned());
        }
    }
    let verdict = serde_json::json!({
        "stableId": "TUI-V3",
        "corpus": "unicode-gauntlet",
        "row": {
            "label": row_label,
            "tier": format!("{:?}", row.tier).to_lowercase(),
            "id": format!("{:?}", row.id).to_lowercase(),
            "runnerImage": row.runner_image,
        },
        "k": K,
        "digest": digest,
        "verdicts": flat,
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
/// Contract column of the closing `│` in a railed/overlay/paste row.
///
/// `prefix` is the fixture's row content before the closing `│`; the `│`
/// itself sits at `visible_width(prefix)` from the start of the row.
fn row_closing_col(prefix: &str) -> usize {
    visible_width(prefix)
}

/// The probe after the same normalization the fixture applies.
fn normalized_probe(index: usize) -> String {
    normalize_terminal_output(CORPUS[index].1)
}

/// The fixture's row content before the closing `│`.
fn gauntlet_row_prefix(index: usize) -> String {
    let (label, _probe) = CORPUS[index];
    format!("{label} {} {FILLER} ", normalized_probe(index))
}

/// Contract width of the editor cursor after the probe (prompt = "> ").
fn editor_cursor_col(index: usize) -> usize {
    2 + visible_width(&normalized_probe(index))
}

/// Find the expected line for a probe and surface.
fn find_probe_line<'a>(snapshot: &'a [String], label: &str) -> Result<&'a str, CorpusError> {
    find_line(snapshot, label)
        .ok_or_else(|| CorpusError::Assert(format!("gauntlet: settled snapshot missing {label:?}")))
}

// ---------------------------------------------------------------------------
// Per-scenario assertion runners
// ---------------------------------------------------------------------------

fn assert_rail_alignment(
    frame: &pi_tui::testkit::driver::SettledFrame,
    verdicts: &mut Vec<(String, &'static str)>,
) -> Result<(), CorpusError> {
    // The rail glyph must sit at column 0 in every railed row; the child
    // content starts at column 2 (Rail::RAIL_WIDTH = glyph + one skip cell).
    const RAIL_OFFSET: usize = 2;
    for (index, &(label, _probe)) in CORPUS.iter().enumerate() {
        let line = find_probe_line(&frame.snapshot.lines, label)?;
        // Left rail glyph.
        let left = avt_column_of_first(line, '\u{2502}')
            .ok_or_else(|| CorpusError::Assert(format!("railed/{label}: missing rail glyph")))?;
        let right = avt_column_of_last(line, '\u{2502}')
            .ok_or_else(|| CorpusError::Assert(format!("railed/{label}: missing closing rail")))?;
        let prefix = gauntlet_row_prefix(index);
        let expected = RAIL_OFFSET + row_closing_col(&prefix);
        let pass = left == 0 && right == expected;
        verdicts.push((
            format!("railed/{label}"),
            if pass { "match" } else { "diverge" },
        ));
        if !pass {
            return Err(CorpusError::Assert(format!(
                "railed/{label}: left={left} right={right} expected_right={expected} line={line:?}"
            )));
        }
    }
    Ok(())
}

fn assert_table_alignment(
    frame: &pi_tui::testkit::driver::SettledFrame,
    table_indices: &[usize],
    verdicts: &mut Vec<(String, &'static str)>,
) -> Result<(), CorpusError> {
    // Collect all table border rows: top, header row, each data row, bottom.
    // Use the last row (bottom border) to get the contract right-border column.
    let table_lines: Vec<&str> = frame
        .snapshot
        .lines
        .iter()
        .filter(|line| line.contains('\u{2502}') || line.contains('\u{2500}'))
        .map(String::as_str)
        .collect();
    if table_lines.is_empty() {
        return Err(CorpusError::Assert("table: no table rows found".to_owned()));
    }
    // The top or bottom border is all box-drawing; its right corner's
    // contract position is the end of the whole string (all width-1 chars).
    // bounded: the `is_empty` guard above proves `last()` is `Some`.
    #[expect(
        clippy::expect_used,
        reason = "bounded: is_empty guard proves table_lines is non-empty"
    )]
    let border = table_lines.last().expect("table has lines");
    let contract_right = border.chars().map(avt_char_width).sum::<usize>() - 1;
    for index in table_indices {
        let (label, _probe) = CORPUS[*index];
        let line = find_line(&frame.snapshot.lines, label)
            .ok_or_else(|| CorpusError::Assert(format!("table/{label}: missing row")))?;
        let left = avt_column_of_first(line, '\u{2502}')
            .ok_or_else(|| CorpusError::Assert(format!("table/{label}: missing left border")))?;
        let right = avt_column_of_last(line, '\u{2502}')
            .ok_or_else(|| CorpusError::Assert(format!("table/{label}: missing right border")))?;
        let pass = right == contract_right;
        verdicts.push((
            format!("table/{label}"),
            if pass { "match" } else { "diverge" },
        ));
        if !pass {
            return Err(CorpusError::Assert(format!(
                "table/{label}: left={left} right={right} contract_right={contract_right} line={line:?}"
            )));
        }
    }
    Ok(())
}

fn assert_editor_cursor(
    frame: &pi_tui::testkit::driver::SettledFrame,
    index: usize,
    verdicts: &mut Vec<(String, &'static str)>,
) -> Result<(), CorpusError> {
    let (label, _probe) = CORPUS[index];
    let observed_col = frame.snapshot.cursor_col;
    let expected_col = editor_cursor_col(index);
    let pass = observed_col == expected_col;
    verdicts.push((
        format!("editor/{label}"),
        if pass { "match" } else { "diverge" },
    ));
    if !pass {
        return Err(CorpusError::Assert(format!(
            "editor/{label}: cursor col {observed_col}, expected {expected_col}"
        )));
    }
    // Cursor must sit on the input row.
    let input_line = frame
        .snapshot
        .lines
        .iter()
        .find(|line| line.starts_with('>'))
        .map(String::as_str)
        .ok_or_else(|| CorpusError::Assert(format!("editor/{label}: no input line")))?;
    // Find which snapshot row holds the input line.
    let input_row = frame
        .snapshot
        .lines
        .iter()
        .position(|line| line == input_line)
        .ok_or_else(|| {
            CorpusError::Assert(format!("editor/{label}: input line not in snapshot"))
        })?;
    let pass_row = frame.snapshot.cursor_row == input_row;
    if !pass_row {
        return Err(CorpusError::Assert(format!(
            "editor/{label}: cursor row {}, expected {}",
            frame.snapshot.cursor_row, input_row
        )));
    }
    Ok(())
}

fn assert_overlay_alignment(
    frame: &pi_tui::testkit::driver::SettledFrame,
    verdicts: &mut Vec<(String, &'static str)>,
) -> Result<(), CorpusError> {
    for (index, &(label, _probe)) in CORPUS.iter().enumerate() {
        let line = find_probe_line(&frame.snapshot.lines, label)?;
        // Base sentinel `B9` must survive right of the overlay region.
        let base_col = avt_column_of_substr(line, "B9").ok_or_else(|| {
            CorpusError::Assert(format!("overlay/{label}: missing base B9 sentinel"))
        })?;
        // Overlay closing `│` inside the overlay region.
        let overlay_prefix = gauntlet_row_prefix(index);
        let expected_overlay = OVERLAY_COL as usize + row_closing_col(&overlay_prefix);
        // Find the `│` in the overlay region (col 12..42).
        let mut col = 0usize;
        let mut overlay_right = None;
        for ch in line.chars() {
            if ch == '\u{2502}'
                && (OVERLAY_COL as usize..OVERLAY_COL as usize + OVERLAY_WIDTH as usize)
                    .contains(&col)
            {
                overlay_right = Some(col);
            }
            col += avt_char_width(ch);
        }
        let observed_overlay = overlay_right.ok_or_else(|| {
            CorpusError::Assert(format!("overlay/{label}: missing overlay border"))
        })?;
        // Expected base column accounts for AVT vs contract width divergence:
        // the fixture pads to BASE_SENTINEL_COL using contract visible_width,
        // but AVT may render the prefix at a different width.
        let prefix = gauntlet_row_prefix(index);
        let avt_prefix_width: usize = prefix.chars().map(avt_char_width).sum();
        let contract_prefix_width = visible_width(&prefix);
        let expected_base =
            BASE_SENTINEL_COL as usize + avt_prefix_width.saturating_sub(contract_prefix_width);
        let pass = base_col == expected_base && observed_overlay == expected_overlay;
        verdicts.push((
            format!("overlay/{label}"),
            if pass { "match" } else { "diverge" },
        ));
        // Divergences are recorded above and not hard-failed: AVT/contract
        // width disagreements are the gauntlet's subject, not a test failure.
    }
    Ok(())
}

fn assert_paste_alignment(
    frame: &pi_tui::testkit::driver::SettledFrame,
    verdicts: &mut Vec<(String, &'static str)>,
    phase_label: &str,
) {
    for (index, &(label, _probe)) in CORPUS.iter().enumerate() {
        let Ok(line) = find_probe_line(&frame.snapshot.lines, label) else {
            // Scrolled lines may not all be visible; non-visible probes
            // are recorded as `not-visible` rather than failing.
            verdicts.push((format!("{phase_label}/{label}"), "not-visible"));
            continue;
        };
        let prefix = gauntlet_row_prefix(index);
        let expected = row_closing_col(&prefix);
        let Some(right) = avt_column_of_last(line, '\u{2502}') else {
            // Missing closing border — record as diverge, don't hard-fail.
            verdicts.push((format!("{phase_label}/{label}"), "diverge"));
            continue;
        };
        let pass = right == expected;
        verdicts.push((
            format!("{phase_label}/{label}"),
            if pass { "match" } else { "diverge" },
        ));
        // Divergences are recorded above and not hard-failed: AVT/contract
        // width disagreements are the gauntlet's subject, not a test failure.
    }
}

// ---------------------------------------------------------------------------
// Scenario runner
// ---------------------------------------------------------------------------

fn run_unicode_gauntlet(
    iteration: usize,
    row_label: &str,
    row: &RunnerRow,
    table_chunks: &[Vec<usize>],
    verdicts: &mut BTreeMap<String, Vec<(String, &'static str)>>,
) -> Result<TranscriptArtifact, CorpusError> {
    let binary = fixture_binary()?;
    let argv = vec![binary.to_string_lossy().into_owned()];
    let mut run = FixtureRun::open(
        argv,
        Scenario::FixtureUnicodeGauntlet,
        row.clone(),
        standard_claims(),
    )?;

    // railed
    let frame = run.settle_frame(|bytes| contains_bytes(bytes, b"PI_TUI_UG=railed"))?;
    // per-phase sync audit deferred to final artifact (see assert below);
    assert_rail_alignment(&frame, verdicts.entry("railed".to_owned()).or_default())?;
    run.write_step()?;

    // table-1..3
    for (chunk_idx, indices) in table_chunks.iter().enumerate() {
        let marker = format!("PI_TUI_UG=table-{}", chunk_idx + 1);
        let frame = run.settle_frame(|bytes| contains_bytes(bytes, marker.as_bytes()))?;
        // per-phase sync audit deferred;
        assert_table_alignment(
            &frame,
            indices,
            verdicts
                .entry(format!("table-{}", chunk_idx + 1))
                .or_default(),
        )?;
        run.write_step()?;
    }

    // editor-P01..P13
    for (index, &(label, _probe)) in CORPUS.iter().enumerate() {
        let marker = format!("PI_TUI_UG={label}");
        let frame = run.settle_frame(|bytes| contains_bytes(bytes, marker.as_bytes()))?;
        assert_editor_cursor(
            &frame,
            index,
            verdicts.entry("editor".to_owned()).or_default(),
        )?;
        run.write_step()?;
    }

    // overlay
    let frame = run.settle_frame(|bytes| contains_bytes(bytes, b"PI_TUI_UG=overlay"))?;
    // per-phase sync audit deferred;
    assert_overlay_alignment(&frame, verdicts.entry("overlay".to_owned()).or_default())?;
    run.write_step()?;

    // paste-verbatim-1
    let frame = run.settle_frame(|bytes| contains_bytes(bytes, b"PI_TUI_UG=paste-verbatim-1"))?;
    // per-phase sync audit deferred;
    assert_paste_alignment(
        &frame,
        verdicts.entry("paste-verbatim-1".to_owned()).or_default(),
        "paste-verbatim-1",
    );
    run.write_step()?;

    // paste-verbatim-2
    let frame = run.settle_frame(|bytes| contains_bytes(bytes, b"PI_TUI_UG=paste-verbatim-2"))?;
    // per-phase sync audit deferred;
    assert_paste_alignment(
        &frame,
        verdicts.entry("paste-verbatim-2".to_owned()).or_default(),
        "paste-verbatim-2",
    );
    run.write_step()?;

    // paste-atomic
    let frame = run.settle_frame(|bytes| contains_bytes(bytes, b"PI_TUI_UG=paste-atomic"))?;
    // per-phase sync audit deferred;
    assert_paste_alignment(
        &frame,
        verdicts.entry("paste-atomic".to_owned()).or_default(),
        "paste-atomic",
    );
    run.write_step()?;

    // paste-marker
    let frame = run.settle_frame(|bytes| contains_bytes(bytes, b"PI_TUI_UG=paste-marker"))?;
    // per-phase sync audit deferred;
    // Marker row has no closing `│`; assert presence of the marker pattern.
    if !frame
        .snapshot
        .lines
        .iter()
        .any(|line| line.contains("[paste #1"))
    {
        return Err(CorpusError::Assert(
            "paste-marker: large-paste marker not found".to_owned(),
        ));
    }
    verdicts
        .entry("paste-marker".to_owned())
        .or_default()
        .push(("large-paste-marker".to_owned(), "match"));
    run.write_step()?;

    // DONE
    let frame = run.settle_frame(|bytes| contains_bytes(bytes, b"PI_TUI_UG=DONE-MARKER"))?;
    assert_no_clear_balanced(run.raw_so_far(), "done")?;
    if !frame
        .snapshot
        .lines
        .iter()
        .any(|line| line.contains("UNICODE GAUNTLET COMPLETE"))
    {
        return Err(CorpusError::Assert(
            "unicode-gauntlet: final completion line missing".to_owned(),
        ));
    }

    let artifact = run.finish()?;
    validate_artifact(&artifact)
        .map_err(|error| CorpusError::Assert(format!("unicode-gauntlet: validator: {error}")))?;
    let path = write_artifact(row_label, "unicode-gauntlet", iteration, &artifact)?;
    let body = fs::read_to_string(&path)?;
    let value: serde_json::Value = serde_json::from_str(&body)
        .map_err(|error| CorpusError::Io(format!("re-parse artifact: {error}")))?;
    pi_tui::testkit::validate::validate_value(&value).map_err(|error| {
        CorpusError::Assert(format!("unicode-gauntlet: file validator: {error}"))
    })?;
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

// ---------------------------------------------------------------------------
// Test entry
// ---------------------------------------------------------------------------

#[test]
fn transcript_unicode_gauntlet_corpus() -> Result<(), CorpusError> {
    let (row_label, row) = resolve_row()?;
    if row_label == "local" {
        assert_eq!(row.tier, RowTier::Local);
    }

    let binary = fixture_binary()?;
    require_prerequisites(&binary.to_string_lossy())?;

    // Pre-compute the markdown table chunks (same logic as the fixture).
    let kept: Vec<usize> = (0..CORPUS.len())
        .filter(|index| !TABLE_SKIP.contains(index))
        .collect();
    let table_chunks: Vec<Vec<usize>> = {
        let mut chunks = Vec::new();
        let mut rest = kept.as_slice();
        for size in [5, 5, 3] {
            let take = rest.len().min(size);
            let (head, tail) = rest.split_at(take);
            chunks.push(head.to_vec());
            rest = tail;
        }
        if !rest.is_empty() {
            chunks.push(rest.to_vec());
        }
        chunks
    };

    let row_run = row.clone();
    let label_run = row_label.clone();
    let mut verdicts: BTreeMap<String, Vec<(String, &'static str)>> = BTreeMap::new();
    let digest_cell = std::cell::RefCell::new(String::new());
    run_scenario_k("unicode-gauntlet", |iteration| {
        let artifact = run_unicode_gauntlet(
            iteration,
            &label_run,
            &row_run,
            &table_chunks,
            &mut verdicts,
        )?;
        *digest_cell.borrow_mut() = artifact.digest.clone();
        Ok(artifact)
    })?;

    let digest = digest_cell.into_inner();
    write_verdict(&row_label, &row, &digest, &verdicts)?;
    Ok(())
}
