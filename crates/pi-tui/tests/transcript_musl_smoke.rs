#![cfg(all(unix, feature = "testkit"))]
//! Two-tier musl release-row lane (TUI-V1, issue #76).
//!
//! The two mandatory musl release rows (`x86_64-unknown-linux-musl`,
//! `aarch64-unknown-linux-musl`) assert ONLY their four packaging/protocol
//! axes — never an interaction claim:
//!
//! 1. host-native artifact execution (`pi --version` under the smoke driver)
//! 2. static-link/unpack/integrity (ELF `PT_INTERP` absence + archive
//!    member/integrity check when an archive is supplied)
//! 3. compiled-host JSONL `hello` protocol smoke (`pi-extension-host`)
//! 4. bundled-Bun-fallback JSONL `hello` protocol smoke
//!    (`bun pi-extension-host.js`)
//!
//! Verbatim absence line carried by every verdict record:
//! `no PTY/render/synchronized-output/no-clear claims`.
//!
//! Transcripts record through `QemuUserSmokeDriver` (piped stdio, never a
//! render session) in contingency mode with claims limited to
//! `Execution` + `Protocol`; the schema-v1 validator rejects any stronger
//! claim for this driver kind.
//!
//! Environment:
//! - `PI_TUI_MUSL_ROW=musl-x64|musl-arm64` — selects the row (required to
//!   run the lane; absent runs only the self-checks below).
//! - `PI_TUI_MUSL_ROOT=<dir>` — artifact root laid out like the release
//!   archive: `pi`, `pi-extension-host`, `bun`, `pi-extension-host.js`.
//! - `PI_TUI_MUSL_QEMU=<argv prefix>` — QEMU user-mode prefix for
//!   cross-architecture rows (recorded as the QEMU contingency label).
//!   Same-architecture rows execute host-natively and leave it unset.
//! - `PI_TUI_MUSL_ARCHIVE=<path>` — optional release archive for the
//!   unpack/integrity axis; absent records the named limitation.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use pi_tui::testkit::driver::{DriverError, LaunchSpec, SettlePolicy, TerminalDriver};
use pi_tui::testkit::qemu::QemuUserSmokeDriver;
use pi_tui::testkit::repeat::{RepeatError, run_k};
use pi_tui::testkit::transcript::{
    CapabilityProfile, ClaimClass, DriverKind, Geometry, NormalizationContext, RowId, RowTier,
    RunnerRow, Scenario, TimingEnvelope, TranscriptArtifact, TranscriptMode, TranscriptSpec,
};
use pi_tui::testkit::validate::validate_artifact;
use pi_tui::testkit::{RecordingError, RecordingSession};
use tempfile::TempDir;

const K: usize = 3;

/// Verbatim absence line from issue #76 — musl rows never carry interaction
/// claims.
const ABSENCE_LINE: &str = "no PTY/render/synchronized-output/no-clear claims";
/// Wire protocol version negotiated in `hello` (mirrors pi-tui-protocol).
const HOST_PROTOCOL_VERSION: u32 = 1;

/// Compatibility target version (mirrors host `COMPATIBILITY_VERSION`).
const HOST_COMPATIBILITY_VERSION: &str = "0.80.10";

/// Canonical JSONL hello request (mirrors `scripts/release/host.ts`).
fn hello_request_line() -> Vec<u8> {
    format!(
        "{{\"id\":1,\"kind\":\"req\",\"method\":\"hello\",\"payload\":{{\"protocolVersion\":{HOST_PROTOCOL_VERSION},\"compatibilityVersion\":\"{HOST_COMPATIBILITY_VERSION}\"}}}}\n"
    )
    .into_bytes()
}

/// Validate a hello acknowledgment line: `kind:"res"`, `method:"hello"`,
/// `id:1`, and both versions matching.
fn is_hello_ack(line: &str) -> bool {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
        return false;
    };
    value.get("kind").and_then(serde_json::Value::as_str) == Some("res")
        && value.get("method").and_then(serde_json::Value::as_str) == Some("hello")
        && value.get("id").and_then(serde_json::Value::as_i64) == Some(1)
        && value
            .get("payload")
            .and_then(|p| p.get("protocolVersion"))
            .and_then(serde_json::Value::as_u64)
            == Some(u64::from(HOST_PROTOCOL_VERSION))
        && value
            .get("payload")
            .and_then(|p| p.get("compatibilityVersion"))
            .and_then(serde_json::Value::as_str)
            == Some(HOST_COMPATIBILITY_VERSION)
}

/// ELF64 header fields needed to walk program headers (`None` when malformed:
/// wrong magic/class, or a `phentsize` too short for the fields this lane
/// reads — the ELF64 spec fixes it at 56).
fn elf64_phdrs(bytes: &[u8]) -> Option<(u64, usize, usize)> {
    if bytes.len() < 64 || &bytes[..4] != b"\x7fELF" || bytes[4] != 2 {
        return None;
    }
    let phoff = u64::from_le_bytes(bytes[32..40].try_into().ok()?);
    let phentsize = usize::from(u16::from_le_bytes(bytes[54..56].try_into().ok()?));
    let phnum = usize::from(u16::from_le_bytes(bytes[56..58].try_into().ok()?));
    if phentsize < 56 {
        return None;
    }
    Some((phoff, phentsize, phnum))
}

/// One program-header entry, bounds- and overflow-checked (`None` when the
/// table runs off the file or the offset arithmetic wraps).
fn elf64_ph(bytes: &[u8], table: (u64, usize, usize), index: usize) -> Option<&[u8]> {
    let (phoff, phentsize, phnum) = table;
    if index >= phnum {
        return None;
    }
    let offset = usize::try_from(phoff)
        .ok()?
        .checked_add(index.checked_mul(phentsize)?)?;
    let end = offset.checked_add(phentsize)?;
    bytes.get(offset..end)
}

/// Minimal ELF static-link check: an ELF64 executable without a `PT_INTERP`
/// program header is statically linked (no dynamic loader path).
fn elf64_is_static(bytes: &[u8]) -> Option<bool> {
    let table = elf64_phdrs(bytes)?;
    for index in 0..table.2 {
        let entry = elf64_ph(bytes, table, index)?;
        let p_type = u32::from_le_bytes(entry[0..4].try_into().ok()?);
        if p_type == 3 {
            // PT_INTERP
            return Some(false);
        }
    }
    Some(true)
}

/// Parse the `PT_INTERP` loader path of an ELF64 binary (`None` when absent
/// or when a malformed header makes the range unreadable).
fn elf64_interp(bytes: &[u8]) -> Option<String> {
    let table = elf64_phdrs(bytes)?;
    for index in 0..table.2 {
        let entry = elf64_ph(bytes, table, index)?;
        let p_type = u32::from_le_bytes(entry[0..4].try_into().ok()?);
        if p_type == 3 {
            let p_offset =
                usize::try_from(u64::from_le_bytes(entry[8..16].try_into().ok()?)).ok()?;
            let p_filesz =
                usize::try_from(u64::from_le_bytes(entry[32..40].try_into().ok()?)).ok()?;
            let end = p_offset.checked_add(p_filesz)?;
            let path = bytes.get(p_offset..end)?;
            let path = path.split(|b| *b == 0).next()?;
            return Some(String::from_utf8_lossy(path).into_owned());
        }
    }
    None
}

#[derive(Debug)]
enum LaneError {
    Prerequisite(String),
    Driver(String),
    Transcript(String),
    Io(String),
    Assert(String),
}

impl std::fmt::Display for LaneError {
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

impl From<RecordingError> for LaneError {
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

impl From<DriverError> for LaneError {
    fn from(error: DriverError) -> Self {
        Self::Driver(error.to_string())
    }
}

impl From<std::io::Error> for LaneError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error.to_string())
    }
}

/// One smoke run: spawn `argv` under the driver, optionally write the hello
/// request, settle on the expected output, close, and record the transcript.
struct SmokeRun {
    recording: RecordingSession<<QemuUserSmokeDriver as TerminalDriver>::Session>,
    policy: SettlePolicy,
    context: NormalizationContext,
    wall_started: Instant,
    output: Vec<u8>,
}

impl SmokeRun {
    fn open(
        argv: Vec<String>,
        scenario: Scenario,
        row: RunnerRow,
        claims: Vec<ClaimClass>,
        mode: TranscriptMode,
    ) -> Result<Self, LaneError> {
        let cwd = std::env::current_dir().map_err(|error| {
            LaneError::Prerequisite(format!("current_dir unavailable: {error}"))
        })?;
        let context = NormalizationContext {
            home: std::env::var_os("HOME").map(|v| v.as_encoded_bytes().to_vec()),
            cwd: Some(cwd.as_os_str().as_encoded_bytes().to_vec()),
        };
        // Native execution wraps argv with `env` (the driver requires a
        // non-empty prefix); QEMU rows prepend the emulator prefix instead.
        let prefix = std::env::var("PI_TUI_MUSL_QEMU")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .map_or_else(
                || vec!["env".to_owned()],
                |v| v.split_whitespace().map(str::to_owned).collect(),
            );
        let driver = QemuUserSmokeDriver::new(prefix)?;
        let spec = LaunchSpec {
            argv,
            cwd,
            env: BTreeMap::new(),
            geometry: Geometry { cols: 80, rows: 24 },
            profile: CapabilityProfile::Dumb,
        };
        let session = driver.open(&spec)?;
        let recording = RecordingSession::new_qemu(
            session,
            TranscriptSpec {
                scenario,
                row,
                geometry: Geometry { cols: 80, rows: 24 },
                capability_profile: CapabilityProfile::Dumb,
                driver_kind: DriverKind::QemuUserSmoke,
                mode,
                claims,
                timing: TimingEnvelope::default(),
            },
            spec.argv,
            &context,
        )?;
        Ok(Self {
            recording,
            policy: SettlePolicy::new(Duration::from_millis(150), Duration::from_secs(30))?,
            context,
            wall_started: Instant::now(),
            output: Vec::new(),
        })
    }

    fn write(&mut self, bytes: &[u8]) -> Result<(), LaneError> {
        self.recording.write(bytes)?;
        Ok(())
    }

    fn settle_output(&mut self, predicate: impl FnMut(&[u8]) -> bool) -> Result<(), LaneError> {
        let batch = self
            .recording
            .read_output(&self.policy, predicate, &self.context)?;
        self.output.extend_from_slice(&batch.bytes);
        Ok(())
    }

    fn output(&self) -> &[u8] {
        &self.output
    }

    fn finish(mut self) -> Result<TranscriptArtifact, LaneError> {
        let _status = self.recording.close()?;
        let mut artifact = self.recording.finish()?;
        artifact.timing.wall_ms =
            u64::try_from(self.wall_started.elapsed().as_millis()).unwrap_or(u64::MAX);
        validate_artifact(&artifact).map_err(|error| {
            LaneError::Assert(format!("validator rejected smoke artifact: {error}"))
        })?;
        Ok(artifact)
    }
}

fn musl_claims() -> Vec<ClaimClass> {
    vec![ClaimClass::Execution, ClaimClass::Protocol]
}

fn target_root() -> PathBuf {
    if let Ok(target) = std::env::var("CARGO_TARGET_DIR") {
        return PathBuf::from(target);
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target")
}

fn write_artifact(
    row_label: &str,
    axis: &str,
    iteration: usize,
    artifact: &TranscriptArtifact,
) -> Result<PathBuf, LaneError> {
    let path = target_root()
        .join("verification/tui-transcripts")
        .join(row_label)
        .join("musl-smoke")
        .join(axis)
        .join(format!("run-{}", iteration + 1))
        .join("transcript.artifact.json");
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let body = serde_json::to_vec_pretty(artifact)
        .map_err(|error| LaneError::Io(format!("serialize artifact: {error}")))?;
    let mut file = fs::File::create(&path)?;
    file.write_all(&body)?;
    file.write_all(b"\n")?;
    Ok(path)
}

/// One axis: deterministic argv + optional stdin + settle needle, run k
/// times with byte-identical canonical transcripts.
struct AxisSpec {
    axis: &'static str,
    argv: Vec<String>,
    stdin: Option<Vec<u8>>,
    settle_needle: &'static [u8],
    /// Optional first-line acknowledgment validator (hello protocol axes).
    ack_check: Option<fn(&str) -> bool>,
}

fn run_axis_k(row_label: &str, row: &RunnerRow, spec: &AxisSpec) -> Result<String, LaneError> {
    match run_k(K, |iteration| {
        let mut run = SmokeRun::open(
            spec.argv.clone(),
            Scenario::MuslPackagingSmoke,
            row.clone(),
            musl_claims(),
            TranscriptMode::Contingency,
        )?;
        if let Some(bytes) = spec.stdin.as_ref() {
            run.write(bytes)?;
        }
        run.settle_output(|bytes| {
            bytes
                .windows(spec.settle_needle.len())
                .any(|w| w == spec.settle_needle)
        })?;
        if let Some(check) = spec.ack_check {
            let text = String::from_utf8_lossy(run.output()).into_owned();
            let first_line = text.lines().next().unwrap_or("");
            if !check(first_line) {
                return Err(LaneError::Assert(format!(
                    "{}: hello acknowledgment failed validation: {first_line:?}",
                    spec.axis
                )));
            }
        }
        let artifact = run.finish()?;
        write_artifact(row_label, spec.axis, iteration, &artifact)?;
        Ok(artifact)
    }) {
        Ok(report) => Ok(report.digest),
        Err(RepeatError::Divergence {
            first_divergent_seq,
            left_digest,
            right_digest,
        }) => Err(LaneError::Assert(format!(
            "{}: divergence at seq {first_divergent_seq}: {left_digest} != {right_digest}",
            spec.axis
        ))),
        Err(error) => Err(LaneError::Assert(format!("{}: {error}", spec.axis))),
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

/// Unpack/integrity axis: when an archive is supplied, unpack it and assert
/// the executed `pi` binary is byte-identical to the archive member.
fn unpack_integrity_axis(root: &Path, archive: Option<&str>) -> Result<String, LaneError> {
    let Some(archive) = archive else {
        return Ok("limitation:archive-not-supplied".to_owned());
    };
    let archive = PathBuf::from(archive);
    if !archive.exists() {
        return Err(LaneError::Prerequisite(format!(
            "PI_TUI_MUSL_ARCHIVE missing: {}",
            archive.display()
        )));
    }
    let tmp = TempDir::new().map_err(|e| LaneError::Io(e.to_string()))?;
    let status = Command::new("tar")
        .arg("-xzf")
        .arg(&archive)
        .arg("-C")
        .arg(tmp.path())
        .status()
        .map_err(|e| LaneError::Io(format!("tar spawn: {e}")))?;
    if !status.success() {
        return Err(LaneError::Assert("archive unpack failed".to_owned()));
    }
    // Locate the unpacked `pi` member and compare bytes with the executed one.
    let mut stack = vec![tmp.path().to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = fs::read_dir(&dir).map_err(|e| LaneError::Io(e.to_string()))?;
        for entry in entries {
            let entry = entry.map_err(|e| LaneError::Io(e.to_string()))?;
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.file_name().is_some_and(|n| n == "pi") {
                let unpacked = fs::read(&path).map_err(|e| LaneError::Io(e.to_string()))?;
                let executed =
                    fs::read(root.join("pi")).map_err(|e| LaneError::Io(e.to_string()))?;
                if unpacked != executed {
                    return Err(LaneError::Assert(
                        "integrity mismatch: unpacked pi differs from executed pi".to_owned(),
                    ));
                }
                return Ok("pass".to_owned());
            }
        }
    }
    Err(LaneError::Assert(
        "archive contains no pi member".to_owned(),
    ))
}

#[expect(
    clippy::too_many_lines,
    reason = "the four release-row axes are one sequential verdict; splitting would hide the absence-line contract"
)]
#[test]
fn musl_packaging_protocol_lane() -> Result<(), LaneError> {
    let row_env = std::env::var("PI_TUI_MUSL_ROW")
        .ok()
        .filter(|v| !v.trim().is_empty());
    let Some(row_label) = row_env else {
        // Self-check mode (no lane inputs on this host): the lane still
        // proves its own fixtures — the ELF detector flags a dynamically
        // linked host binary as non-static, and the hello ack shape check
        // rejects malformed acknowledgments.
        let host =
            std::env::current_exe().map_err(|e| LaneError::Io(format!("current_exe: {e}")))?;
        let bytes = fs::read(&host)?;
        assert_eq!(
            elf64_is_static(&bytes),
            Some(false),
            "host cargo test binary must be dynamically linked"
        );
        assert!(!is_hello_ack("{\"id\":1,\"kind\":\"req\"}"));
        assert!(is_hello_ack(
            "{\"id\":1,\"kind\":\"res\",\"method\":\"hello\",\"payload\":{\"protocolVersion\":1,\"compatibilityVersion\":\"0.80.10\"}}"
        ));
        // Malformed program headers must return None, never panic.
        let mut truncated = bytes[..64].to_vec();
        truncated[54..56].copy_from_slice(&4u16.to_le_bytes());
        assert_eq!(
            elf64_is_static(&truncated),
            None,
            "short phentsize rejected"
        );
        assert_eq!(elf64_interp(&truncated), None, "short phentsize rejected");
        let mut huge_offset = bytes[..64].to_vec();
        huge_offset[32..40].copy_from_slice(&u64::MAX.to_le_bytes());
        assert_eq!(
            elf64_is_static(&huge_offset),
            None,
            "overflowing phoff rejected"
        );
        return Ok(());
    };

    let row_label = row_label.trim().to_owned();
    let root = PathBuf::from(std::env::var("PI_TUI_MUSL_ROOT").map_err(|_| {
        LaneError::Prerequisite("PI_TUI_MUSL_ROOT required with PI_TUI_MUSL_ROW".to_owned())
    })?);
    let pi = root.join("pi");
    let host_compiled = root.join("pi-extension-host");
    let bun = root.join("bun");
    let host_js = root.join("pi-extension-host.js");
    for path in [&pi, &host_compiled, &bun, &host_js] {
        assert!(
            path.exists(),
            "musl lane artifact missing: {}",
            path.display()
        );
    }
    let qemu_label = std::env::var("PI_TUI_MUSL_QEMU")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .map_or_else(
            || "host-native-execution:qemu-not-used".to_owned(),
            |v| format!("qemu-contingency:{v}"),
        );

    let row = RunnerRow {
        tier: RowTier::Local,
        id: host_row_id(),
        runner_image: None,
    };

    // Axis 2a: static link — the musl `pi` binary must have no PT_INTERP.
    let pi_bytes = fs::read(&pi)?;
    assert_eq!(
        elf64_is_static(&pi_bytes),
        Some(true),
        "musl pi binary must be statically linked"
    );

    // Axis 2b: unpack/integrity.
    let integrity =
        unpack_integrity_axis(&root, std::env::var("PI_TUI_MUSL_ARCHIVE").ok().as_deref())?;

    // Axis 1: host-native (or QEMU-contingent) artifact execution.
    let exec_digest = run_axis_k(
        &row_label,
        &row,
        &AxisSpec {
            axis: "pi-execution",
            argv: vec![pi.to_string_lossy().into_owned(), "--version".to_owned()],
            stdin: None,
            settle_needle: b"0.",
            ack_check: None,
        },
    )?;

    // Axis 3: compiled-host JSONL hello protocol smoke. A dynamically
    // linked musl sidecar cannot execute on a host without its musl loader —
    // recorded as a named limitation, never a false claim.
    let host_bytes = fs::read(&host_compiled)?;
    let compiled_verdict: serde_json::Value = match elf64_interp(&host_bytes) {
        Some(loader) if !Path::new(&loader).exists() => serde_json::json!({
            "verdict": "limitation",
            "detail": format!("musl loader {loader} absent on this host; compiled-host protocol smoke pending musl userland (REL-T3)")
        }),
        _ => serde_json::json!({"verdict": "pass", "digest": run_axis_k(
            &row_label,
            &row,
            &AxisSpec {
                axis: "host-compiled-hello",
                argv: vec![host_compiled.to_string_lossy().into_owned()],
                stdin: Some(hello_request_line()),
                settle_needle: b"hello",
                ack_check: Some(is_hello_ack),
            },
        )?}),
    };

    // Axis 4: bundled-Bun-fallback JSONL hello protocol smoke.
    let fallback_digest = run_axis_k(
        &row_label,
        &row,
        &AxisSpec {
            axis: "host-fallback-hello",
            argv: vec![
                bun.to_string_lossy().into_owned(),
                host_js.to_string_lossy().into_owned(),
            ],
            stdin: Some(hello_request_line()),
            settle_needle: b"hello",
            ack_check: Some(is_hello_ack),
        },
    )?;

    // Verdict record with the verbatim absence line and named limitations.
    let verdict = serde_json::json!({
        "stableId": "TUI-V1",
        "corpus": "musl-smoke",
        "row": row_label,
        "executionMode": qemu_label,
        "mode": "contingency",
        "absenceLine": ABSENCE_LINE,
        "k": K,
        "axes": {
            "artifact-execution": {"verdict": "pass", "digest": exec_digest},
            "static-link": "pass",
            "unpack-integrity": integrity,
            "compiled-host-protocol": compiled_verdict,
            "bundled-bun-fallback-protocol": {"verdict": "pass", "digest": fallback_digest},
        },
        "limitations": [
            "limitation:release-archive-unpack-integrity-requires-PI_TUI_MUSL_ARCHIVE",
        ],
    });
    let verdict_path = target_root()
        .join("verification/tui-transcripts")
        .join(&row_label)
        .join("musl-smoke")
        .join("verdict.json");
    let parent = verdict_path
        .parent()
        .ok_or_else(|| LaneError::Io("verdict path has no parent".to_owned()))?;
    fs::create_dir_all(parent)?;
    let body = serde_json::to_vec_pretty(&verdict)
        .map_err(|error| LaneError::Io(format!("serialize verdict: {error}")))?;
    fs::write(&verdict_path, body)?;
    Ok(())
}
