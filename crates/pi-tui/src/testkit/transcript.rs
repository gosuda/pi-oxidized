use std::collections::BTreeSet;

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

/// The sole accepted transcript schema identifier.
pub const SCHEMA_ID: &str = "pi-tui-transcript/1";

/// Every normalization available to schema v1, in application order.
pub const NORMALIZATION_TABLE_V1: &[NormalizationKind] = &[
    NormalizationKind::PathHome,
    NormalizationKind::PathCwd,
    NormalizationKind::TimeIso8601,
    NormalizationKind::TimeRelative,
    NormalizationKind::IdSession,
    NormalizationKind::SnapshotTrailingSpaceTrim,
    NormalizationKind::ResizeCollapse,
];

/// A scenario represented by a transcript.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Scenario {
    FixtureStreamSettle,
    FixtureResizeLadder,
    FixtureResizeStorm,
    FixturePasteCursor,
    ColdStart,
    Wizard,
    TrustSelector,
    TrustDialog,
    Streaming,
    Selectors,
    Overlays,
    ProductResizeLadder,
    ProductResizeStorm,
}

/// Evidence tier for the runner that produced an artifact.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum RowTier {
    Local,
    TierN,
}

/// Closed runner row identity.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum RowId {
    GnuX64,
    GnuArm64,
    DarwinX64,
    DarwinArm64,
    WindowsX64,
}

/// Driver implementation used to record an artifact.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum DriverKind {
    PosixPty,
    #[serde(rename = "conpty")]
    ConPty,
    QemuUserSmoke,
}

/// Whether an artifact is primary evidence or contingency evidence.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum TranscriptMode {
    Standard,
    Contingency,
}

/// Observable behavior claimed by an artifact.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ClaimClass {
    Execution,
    Protocol,
    Pty,
    Render,
    SynchronizedOutput,
    NoClear,
    Snapshot,
}

/// Pinned terminal capability profile.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum CapabilityProfile {
    Xterm256ColorTruecolor,
    Xterm256Color,
    Dumb,
    TerminalApp,
    Iterm2,
    WindowsTerminalVt,
    ConhostVtDec2026Fallback,
}

/// Closed canonical event discriminants.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum EventKind {
    Spawn,
    Input,
    Output,
    Snapshot,
    Resize,
    ResizeStorm,
    Exit,
}

/// Closed schema-v1 normalization discriminants.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum NormalizationKind {
    PathHome,
    PathCwd,
    TimeIso8601,
    TimeRelative,
    IdSession,
    SnapshotTrailingSpaceTrim,
    ResizeCollapse,
}

/// Runner identity, excluded from canonical encoding.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct RunnerRow {
    pub tier: RowTier,
    pub id: RowId,
    pub runner_image: Option<String>,
}

/// Terminal dimensions.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Geometry {
    pub cols: u16,
    pub rows: u16,
}

/// Driver metadata.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct DriverDescriptor {
    pub kind: DriverKind,
}

/// One normalization actually applied to canonical content.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct NormalizationEntry {
    pub kind: NormalizationKind,
}

/// Runtime values required by path normalizers. This never enters canonical content.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct NormalizationContext {
    pub home: Option<Vec<u8>>,
    pub cwd: Option<Vec<u8>>,
}

/// A closed canonical event. Sequence numbers start at zero.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum CanonicalEvent {
    Spawn {
        seq: u32,
        argv: Vec<String>,
    },
    Input {
        seq: u32,
        bytes_b64: String,
    },
    Output {
        seq: u32,
        bytes_b64: String,
    },
    Snapshot {
        seq: u32,
        cols: u16,
        rows: u16,
        cursor: [u16; 2],
        lines: Vec<String>,
    },
    Resize {
        seq: u32,
        cols: u16,
        rows: u16,
    },
    ResizeStorm {
        seq: u32,
        sizes: Vec<Geometry>,
    },
    Exit {
        seq: u32,
        code: Option<i32>,
        success: bool,
    },
}

impl CanonicalEvent {
    /// Returns this event's logical sequence number.
    #[must_use]
    pub const fn seq(&self) -> u32 {
        match self {
            Self::Spawn { seq, .. }
            | Self::Input { seq, .. }
            | Self::Output { seq, .. }
            | Self::Snapshot { seq, .. }
            | Self::Resize { seq, .. }
            | Self::ResizeStorm { seq, .. }
            | Self::Exit { seq, .. } => *seq,
        }
    }

    /// Returns this event's closed discriminant.
    #[must_use]
    pub const fn kind(&self) -> EventKind {
        match self {
            Self::Spawn { .. } => EventKind::Spawn,
            Self::Input { .. } => EventKind::Input,
            Self::Output { .. } => EventKind::Output,
            Self::Snapshot { .. } => EventKind::Snapshot,
            Self::Resize { .. } => EventKind::Resize,
            Self::ResizeStorm { .. } => EventKind::ResizeStorm,
            Self::Exit { .. } => EventKind::Exit,
        }
    }
}

/// The canonical, digested event document.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct CanonicalDoc {
    pub events: Vec<CanonicalEvent>,
    pub normalizations: Vec<NormalizationEntry>,
}

/// Timing for one observed output chunk.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ChunkTiming {
    pub event_seq: u32,
    pub byte_len: u64,
    pub delta_ms: u64,
}

/// A settle ceiling observation, outside canonical content.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct AbortCeiling {
    pub ceiling_ms: u64,
    pub observed_ms: u64,
}

/// Exact runtime values used to normalize one output event.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct NormalizationAuditContext {
    pub home_b64: Option<String>,
    pub cwd_b64: Option<String>,
}

/// Non-canonical evidence from which one canonical output is re-derived.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct OutputAudit {
    pub event_seq: u32,
    pub raw_bytes_b64: String,
    pub context: NormalizationAuditContext,
    pub applied: Vec<NormalizationEntry>,
}

/// Non-canonical timing and audit data.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct TimingEnvelope {
    pub wall_ms: u64,
    pub chunk_log: Vec<ChunkTiming>,
    pub settle_windows_ms: Vec<u64>,
    pub abort_ceiling: Option<AbortCeiling>,
    pub raw_log_b64: String,
    pub output_audits: Vec<OutputAudit>,
}

/// A complete schema-v1 transcript artifact.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct TranscriptArtifact {
    pub schema: String,
    pub scenario: Scenario,
    pub row: RunnerRow,
    pub geometry: Geometry,
    pub capability_profile: CapabilityProfile,
    pub driver: DriverDescriptor,
    pub mode: TranscriptMode,
    pub claims: Vec<ClaimClass>,
    pub canonical: CanonicalDoc,
    pub digest: String,
    pub timing: TimingEnvelope,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CanonicalDigestInput<'a> {
    schema: &'a str,
    scenario: Scenario,
    geometry: Geometry,
    capability_profile: CapabilityProfile,
    driver_kind: DriverKind,
    mode: TranscriptMode,
    claims: &'a [ClaimClass],
    events: &'a [CanonicalEvent],
    applied_normalizations: &'a [NormalizationEntry],
}

/// Serialization or construction failure.
#[derive(Debug, thiserror::Error)]
pub enum TranscriptError {
    #[error("canonical JSON serialization failed: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("event sequence overflowed u32")]
    SequenceOverflow,
}

/// Produces compact deterministic JSON for exactly the digested fields.
pub fn encode_canonical(artifact: &TranscriptArtifact) -> Result<Vec<u8>, TranscriptError> {
    let input = CanonicalDigestInput {
        schema: &artifact.schema,
        scenario: artifact.scenario,
        geometry: artifact.geometry,
        capability_profile: artifact.capability_profile,
        driver_kind: artifact.driver.kind,
        mode: artifact.mode,
        claims: &artifact.claims,
        events: &artifact.canonical.events,
        applied_normalizations: &artifact.canonical.normalizations,
    };
    Ok(serde_json::to_vec(&input)?)
}

/// Computes the schema-v1 SHA-256 digest over canonical encoding.
pub fn digest_canonical(artifact: &TranscriptArtifact) -> Result<String, TranscriptError> {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let digest = Sha256::digest(encode_canonical(artifact)?);
    let mut encoded = String::with_capacity("sha256:".len() + digest.len() * 2);
    encoded.push_str("sha256:");
    for byte in digest {
        encoded.push(HEX[(byte >> 4) as usize] as char);
        encoded.push(HEX[(byte & 0x0f) as usize] as char);
    }
    Ok(encoded)
}

/// Result of applying byte-level schema-v1 normalization.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NormalizedBytes {
    pub bytes: Vec<u8>,
    pub applied: Vec<NormalizationEntry>,
}

/// Applies pinned byte-level normalizations before an output event is constructed.
#[must_use]
pub fn normalize_raw_bytes(raw: &[u8], context: &NormalizationContext) -> NormalizedBytes {
    let mut bytes = raw.to_vec();
    let mut applied = BTreeSet::new();

    replace_context(
        &mut bytes,
        context.home.as_deref(),
        b"<HOME>",
        NormalizationKind::PathHome,
        &mut applied,
    );
    replace_context(
        &mut bytes,
        context.cwd.as_deref(),
        b"<CWD>",
        NormalizationKind::PathCwd,
        &mut applied,
    );
    normalize_tokens(&mut bytes, &mut applied);

    NormalizedBytes {
        bytes,
        applied: applied
            .into_iter()
            .map(|kind| NormalizationEntry { kind })
            .collect(),
    }
}

fn replace_context(
    bytes: &mut Vec<u8>,
    needle: Option<&[u8]>,
    replacement: &[u8],
    kind: NormalizationKind,
    applied: &mut BTreeSet<NormalizationKind>,
) {
    if let Some(needle) = needle.filter(|needle| !needle.is_empty())
        && replace_all(bytes, needle, replacement)
    {
        applied.insert(kind);
    }
}

fn replace_all(bytes: &mut Vec<u8>, needle: &[u8], replacement: &[u8]) -> bool {
    let mut output = Vec::with_capacity(bytes.len());
    let mut offset = 0;
    let mut changed = false;
    while let Some(relative) = find_subslice(&bytes[offset..], needle) {
        let index = offset + relative;
        output.extend_from_slice(&bytes[offset..index]);
        output.extend_from_slice(replacement);
        offset = index + needle.len();
        changed = true;
    }
    if changed {
        output.extend_from_slice(&bytes[offset..]);
        *bytes = output;
    }
    changed
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn normalize_tokens(bytes: &mut Vec<u8>, applied: &mut BTreeSet<NormalizationKind>) {
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if let Some((len, kind, replacement)) = match_volatile_at(bytes, index) {
            output.extend_from_slice(replacement);
            applied.insert(kind);
            index += len;
            continue;
        }
        output.push(bytes[index]);
        index += 1;
    }
    *bytes = output;
}

pub(crate) fn detected_volatile_kinds(raw: &[u8]) -> BTreeSet<NormalizationKind> {
    let mut detected = BTreeSet::new();
    let mut index = 0;
    while index < raw.len() {
        if let Some((len, kind, _)) = match_volatile_at(raw, index) {
            detected.insert(kind);
            index += len;
            continue;
        }
        index += 1;
    }
    if has_user_home_path(raw) {
        detected.insert(NormalizationKind::PathHome);
    }
    detected
}

fn has_user_home_path(raw: &[u8]) -> bool {
    find_subslice(raw, b"/home/").is_some()
        || find_subslice(raw, b"/Users/").is_some()
        || find_subslice(raw, br#"\Users\"#).is_some()
}

fn normalize_text(text: &str, context: &NormalizationContext) -> (String, Vec<NormalizationEntry>) {
    let normalized = normalize_raw_bytes(text.as_bytes(), context);
    match String::from_utf8(normalized.bytes) {
        Ok(value) => (value, normalized.applied),
        Err(_) => (text.to_owned(), Vec::new()),
    }
}

fn match_volatile_at(bytes: &[u8], index: usize) -> Option<(usize, NormalizationKind, &'static [u8])> {
    if let Some(len) = match_uuid_at(bytes, index) {
        return Some((len, NormalizationKind::IdSession, b"<SESSION>"));
    }
    if let Some(len) = match_iso8601_at(bytes, index) {
        return Some((len, NormalizationKind::TimeIso8601, b"<TS>"));
    }
    if let Some(len) = match_relative_time_at(bytes, index) {
        return Some((len, NormalizationKind::TimeRelative, b"<AGO>"));
    }
    None
}

fn match_uuid_at(bytes: &[u8], index: usize) -> Option<usize> {
    if index + 36 > bytes.len() {
        return None;
    }
    is_uuid(&bytes[index..index + 36]).then_some(36)
}

fn match_iso8601_at(bytes: &[u8], index: usize) -> Option<usize> {
    if index + 20 > bytes.len()
        || bytes[index + 4] != b'-'
        || bytes[index + 7] != b'-'
        || bytes[index + 10] != b'T'
    {
        return None;
    }
    let mut end = index + 20;
    while end < bytes.len() && !bytes[end].is_ascii_whitespace() {
        end += 1;
    }
    is_iso8601(&bytes[index..end]).then_some(end - index)
}

fn match_relative_time_at(bytes: &[u8], index: usize) -> Option<usize> {
    let mut cursor = index;
    if cursor >= bytes.len() || !bytes[cursor].is_ascii_digit() {
        return None;
    }
    while cursor < bytes.len() && bytes[cursor].is_ascii_digit() {
        cursor += 1;
    }
    let unit_len = if bytes.get(cursor..cursor + 2) == Some(b"ms".as_slice()) {
        2
    } else if matches!(bytes.get(cursor), Some(b's' | b'm' | b'h')) {
        1
    } else {
        return None;
    };
    cursor += unit_len;
    if bytes.get(cursor..cursor + 4) != Some(b" ago".as_slice()) {
        return None;
    }
    cursor += 4;
    Some(cursor - index)
}

fn is_uuid(token: &[u8]) -> bool {
    token.len() == 36
        && [8, 13, 18, 23].into_iter().all(|index| token[index] == b'-')
        && token.iter().enumerate().all(|(index, byte)| {
            [8, 13, 18, 23].contains(&index) || byte.is_ascii_hexdigit()
        })
}

fn is_iso8601(token: &[u8]) -> bool {
    token.len() >= 20
        && token.get(4) == Some(&b'-')
        && token.get(7) == Some(&b'-')
        && token.get(10) == Some(&b'T')
        && token.last() == Some(&b'Z')
}

/// Builds events in sequence and enforces driver-specific evidence restrictions.
pub struct TranscriptRecorder {
    artifact: TranscriptArtifact,
    next_seq: u32,
    applied: BTreeSet<NormalizationEntry>,
    raw_log: Vec<u8>,
    output_audits: Vec<OutputAudit>,
}

/// Named construction inputs for [`TranscriptRecorder`].
#[derive(Clone, Debug)]
pub struct TranscriptSpec {
    pub scenario: Scenario,
    pub row: RunnerRow,
    pub geometry: Geometry,
    pub capability_profile: CapabilityProfile,
    pub driver_kind: DriverKind,
    pub mode: TranscriptMode,
    pub claims: Vec<ClaimClass>,
    pub timing: TimingEnvelope,
}

impl TranscriptRecorder {
    /// Creates an empty recorder. QEMU mode and claims are forcibly narrowed.
    #[must_use]
    pub fn new(spec: TranscriptSpec) -> Self {
        let TranscriptSpec {
            scenario,
            row,
            geometry,
            capability_profile,
            driver_kind,
            mode,
            claims,
            timing,
        } = spec;
        let (mode, claims) = constrain_driver(driver_kind, mode, claims);
        Self {
            artifact: TranscriptArtifact {
                schema: SCHEMA_ID.to_owned(),
                scenario,
                row,
                geometry,
                capability_profile,
                driver: DriverDescriptor { kind: driver_kind },
                mode,
                claims,
                canonical: CanonicalDoc {
                    events: Vec::new(),
                    normalizations: Vec::new(),
                },
                digest: String::new(),
                timing,
            },
            next_seq: 0,
            applied: BTreeSet::new(),
            raw_log: Vec::new(),
            output_audits: Vec::new(),
        }
    }

    pub fn spawn(
        &mut self,
        argv: Vec<String>,
        context: &NormalizationContext,
    ) -> Result<(), TranscriptError> {
        let mut normalized_argv = Vec::with_capacity(argv.len());
        for arg in argv {
            let (value, applied) = normalize_text(&arg, context);
            self.applied.extend(applied);
            normalized_argv.push(value);
        }
        let seq = self.take_seq()?;
        self.artifact.canonical.events.push(CanonicalEvent::Spawn {
            seq,
            argv: normalized_argv,
        });
        Ok(())
    }

    pub fn input(&mut self, bytes: &[u8]) -> Result<(), TranscriptError> {
        let seq = self.take_seq()?;
        self.artifact.canonical.events.push(CanonicalEvent::Input {
            seq,
            bytes_b64: BASE64.encode(bytes),
        });
        Ok(())
    }

    /// Normalizes and merges output since the preceding input boundary into one event.
    pub fn output(
        &mut self,
        chunks: &[&[u8]],
        context: &NormalizationContext,
    ) -> Result<(), TranscriptError> {
        let raw_len = chunks.iter().map(|chunk| chunk.len()).sum();
        let mut raw = Vec::with_capacity(raw_len);
        for chunk in chunks {
            raw.extend_from_slice(chunk);
        }
        self.raw_log.extend_from_slice(&raw);
        let normalized = normalize_raw_bytes(&raw, context);
        self.applied.extend(normalized.applied.iter().copied());
        let seq = self.take_seq()?;
        self.output_audits.push(OutputAudit {
            event_seq: seq,
            raw_bytes_b64: BASE64.encode(&raw),
            context: NormalizationAuditContext {
                home_b64: context.home.as_ref().map(|value| BASE64.encode(value)),
                cwd_b64: context.cwd.as_ref().map(|value| BASE64.encode(value)),
            },
            applied: normalized.applied.clone(),
        });
        self.artifact.canonical.events.push(CanonicalEvent::Output {
            seq,
            bytes_b64: BASE64.encode(normalized.bytes),
        });
        Ok(())
    }

    /// Records a settled snapshot. QEMU recorders cannot add render events.
    pub fn snapshot(
        &mut self,
        cols: u16,
        rows: u16,
        cursor: [u16; 2],
        lines: Vec<String>,
        context: &NormalizationContext,
    ) -> Result<bool, TranscriptError> {
        if self.artifact.driver.kind == DriverKind::QemuUserSmoke {
            return Ok(false);
        }
        let mut trimmed = false;
        let mut normalized_lines = Vec::with_capacity(lines.len());
        for line in lines {
            let (mut value, applied) = normalize_text(&line, context);
            self.applied.extend(applied);
            let len = value.len();
            value = value.trim_end_matches(' ').to_owned();
            trimmed |= len != value.len();
            normalized_lines.push(value);
        }
        if trimmed {
            self.applied.insert(NormalizationEntry {
                kind: NormalizationKind::SnapshotTrailingSpaceTrim,
            });
        }
        let seq = self.take_seq()?;
        self.artifact.canonical.events.push(CanonicalEvent::Snapshot {
            seq,
            cols,
            rows,
            cursor,
            lines: normalized_lines,
        });
        Ok(true)
    }

    pub fn resize(&mut self, cols: u16, rows: u16) -> Result<bool, TranscriptError> {
        if self.artifact.driver.kind == DriverKind::QemuUserSmoke {
            return Ok(false);
        }
        let seq = self.take_seq()?;
        self.artifact
            .canonical
            .events
            .push(CanonicalEvent::Resize { seq, cols, rows });
        Ok(true)
    }

    pub fn resize_storm(&mut self, sizes: Vec<Geometry>) -> Result<bool, TranscriptError> {
        if self.artifact.driver.kind == DriverKind::QemuUserSmoke {
            return Ok(false);
        }
        let collapsed = sizes.last().copied().into_iter().collect::<Vec<_>>();
        if sizes.len() > collapsed.len() {
            self.applied.insert(NormalizationEntry {
                kind: NormalizationKind::ResizeCollapse,
            });
        }
        let seq = self.take_seq()?;
        self.artifact
            .canonical
            .events
            .push(CanonicalEvent::ResizeStorm {
                seq,
                sizes: collapsed,
            });
        Ok(true)
    }

    pub fn exit(&mut self, code: Option<i32>, success: bool) -> Result<(), TranscriptError> {
        let seq = self.take_seq()?;
        self.artifact
            .canonical
            .events
            .push(CanonicalEvent::Exit { seq, code, success });
        Ok(())
    }

    /// Finalizes normalization metadata, raw audit bytes, and digest.
    pub fn finish(mut self) -> Result<TranscriptArtifact, TranscriptError> {
        self.artifact.claims = {
            let claims: BTreeSet<_> = self.artifact.claims.into_iter().collect();
            claims.into_iter().collect()
        };
        self.artifact.canonical.normalizations = self.applied.into_iter().collect();
        self.artifact.timing.raw_log_b64 = BASE64.encode(self.raw_log);
        self.artifact.timing.output_audits = self.output_audits;
        self.artifact.digest = digest_canonical(&self.artifact)?;
        Ok(self.artifact)
    }

    fn take_seq(&mut self) -> Result<u32, TranscriptError> {
        let seq = self.next_seq;
        self.next_seq = self
            .next_seq
            .checked_add(1)
            .ok_or(TranscriptError::SequenceOverflow)?;
        Ok(seq)
    }
}

fn constrain_driver(
    driver: DriverKind,
    mode: TranscriptMode,
    claims: Vec<ClaimClass>,
) -> (TranscriptMode, Vec<ClaimClass>) {
    if driver != DriverKind::QemuUserSmoke {
        return (mode, claims);
    }
    (
        TranscriptMode::Contingency,
        claims
            .into_iter()
            .filter(|claim| matches!(claim, ClaimClass::Execution | ClaimClass::Protocol))
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn recorder(driver: DriverKind) -> TranscriptRecorder {
        TranscriptRecorder::new(TranscriptSpec {
            scenario: Scenario::ColdStart,
            row: RunnerRow {
                tier: RowTier::Local,
                id: RowId::GnuX64,
                runner_image: None,
            },
            geometry: Geometry { cols: 80, rows: 24 },
            capability_profile: CapabilityProfile::Xterm256Color,
            driver_kind: driver,
            mode: TranscriptMode::Standard,
            claims: vec![ClaimClass::Execution, ClaimClass::Render],
            timing: TimingEnvelope::default(),
        })
    }

    #[test]
    fn unknown_fields_are_rejected() -> Result<(), Box<dyn std::error::Error>> {
        let json = r#"{"cols":80,"rows":24,"extra":true}"#;
        let error = serde_json::from_str::<Geometry>(json)
            .err()
            .ok_or("unknown field unexpectedly accepted")?;
        assert!(error.to_string().contains("unknown field"));
        Ok(())
    }

    #[test]
    fn trust_dialog_scenario_is_distinct_from_trust_selector()
    -> Result<(), Box<dyn std::error::Error>> {
        assert_ne!(Scenario::TrustDialog, Scenario::TrustSelector);
        let encoded = serde_json::to_string(&Scenario::TrustDialog)?;
        assert_eq!(encoded, "\"trust-dialog\"");
        Ok(())
    }

    #[test]
    fn timing_and_runner_identity_do_not_change_canonical_bytes()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut first = complete(recorder(DriverKind::PosixPty))?;
        let before = encode_canonical(&first)?;
        first.row.runner_image = Some("other-image".to_owned());
        first.timing.wall_ms = 99_999;
        first.timing.raw_log_b64 = BASE64.encode(b"different audit bytes");
        first.timing.output_audits.clear();
        assert_eq!(before, encode_canonical(&first)?);
        Ok(())
    }

    #[test]
    fn normalization_happens_before_output_event_and_digest()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut value = recorder(DriverKind::PosixPty);
        value.spawn(vec!["pi".to_owned()], &NormalizationContext::default())?;
        value.output(
            &[b"/home/alice/project 550e8400-e29b-41d4-a716-446655440000"],
            &NormalizationContext {
                home: Some(b"/home/alice".to_vec()),
                cwd: None,
            },
        )?;
        value.exit(Some(0), true)?;
        let artifact = value.finish()?;
        let output = artifact
            .canonical
            .events
            .get(1)
            .ok_or("missing output")?;
        let CanonicalEvent::Output { bytes_b64, .. } = output else {
            return Err("wrong event".into());
        };
        let decoded = BASE64.decode(bytes_b64)?;
        assert_eq!(decoded, b"<HOME>/project <SESSION>");
        assert!(artifact
            .canonical
            .normalizations
            .iter()
            .any(|entry| entry.kind == NormalizationKind::PathHome));
        assert!(artifact
            .canonical
            .normalizations
            .iter()
            .any(|entry| entry.kind == NormalizationKind::IdSession));
        assert_eq!(artifact.digest, digest_canonical(&artifact)?);
        assert_eq!(artifact.timing.output_audits.len(), 1);
        Ok(())
    }

    #[test]
    fn spawn_argv_and_snapshot_lines_are_normalized() -> Result<(), Box<dyn std::error::Error>> {
        let context = NormalizationContext {
            home: Some(b"/home/alice".to_vec()),
            cwd: Some(b"/home/alice/project".to_vec()),
        };
        let mut value = recorder(DriverKind::PosixPty);
        value.spawn(
            vec!["/home/alice/.cargo/bin/pi".to_owned(), "--cwd".to_owned()],
            &context,
        )?;
        value.snapshot(
            80,
            24,
            [0, 0],
            vec!["cwd=/home/alice/project  ".to_owned()],
            &context,
        )?;
        value.exit(Some(0), true)?;
        let artifact = value.finish()?;
        let CanonicalEvent::Spawn { argv, .. } = &artifact.canonical.events[0] else {
            return Err("missing spawn".into());
        };
        assert_eq!(argv[0], "<HOME>/.cargo/bin/pi");
        let CanonicalEvent::Snapshot { lines, .. } = &artifact.canonical.events[1] else {
            return Err("missing snapshot".into());
        };
        assert_eq!(lines[0], "cwd=<CWD>");
        assert!(artifact
            .canonical
            .normalizations
            .iter()
            .any(|entry| entry.kind == NormalizationKind::PathHome));
        assert!(artifact
            .canonical
            .normalizations
            .iter()
            .any(|entry| entry.kind == NormalizationKind::PathCwd));
        assert!(artifact
            .canonical
            .normalizations
            .iter()
            .any(|entry| entry.kind == NormalizationKind::SnapshotTrailingSpaceTrim));
        Ok(())
    }

    #[test]
    fn windows_user_paths_are_detected_as_home_volatile() {
        let kinds = detected_volatile_kinds(br"C:\Users\alice\project");
        assert!(kinds.contains(&NormalizationKind::PathHome));
    }

    #[test]
    fn unchanged_bytes_enumerate_no_normalizations() {
        let normalized = normalize_raw_bytes(b"stable output", &NormalizationContext::default());
        assert_eq!(normalized.bytes, b"stable output");
        assert!(normalized.applied.is_empty());
    }

    #[test]
    fn invalid_utf8_bytes_are_preserved_through_normalization() {
        let raw = b"prefix\xff/home/alice\xfe suffix";
        let normalized = normalize_raw_bytes(
            raw,
            &NormalizationContext {
                home: Some(b"/home/alice".to_vec()),
                cwd: None,
            },
        );
        assert_eq!(normalized.bytes, b"prefix\xff<HOME>\xfe suffix");
        assert!(normalized
            .applied
            .iter()
            .any(|entry| entry.kind == NormalizationKind::PathHome));
    }

    #[test]
    fn claims_are_sorted_and_deduplicated_before_digest()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut value = TranscriptRecorder::new(TranscriptSpec {
            scenario: Scenario::ColdStart,
            row: RunnerRow {
                tier: RowTier::Local,
                id: RowId::GnuX64,
                runner_image: None,
            },
            geometry: Geometry { cols: 80, rows: 24 },
            capability_profile: CapabilityProfile::Xterm256Color,
            driver_kind: DriverKind::PosixPty,
            mode: TranscriptMode::Standard,
            claims: vec![
                ClaimClass::Snapshot,
                ClaimClass::Execution,
                ClaimClass::Pty,
                ClaimClass::Execution,
                ClaimClass::Render,
            ],
            timing: TimingEnvelope::default(),
        });
        value.spawn(vec!["pi".to_owned()], &NormalizationContext::default())?;
        value.exit(Some(0), true)?;
        let artifact = value.finish()?;
        assert_eq!(
            artifact.claims,
            vec![
                ClaimClass::Execution,
                ClaimClass::Pty,
                ClaimClass::Render,
                ClaimClass::Snapshot,
            ]
        );
        let mut scrambled = artifact.clone();
        scrambled.claims = vec![
            ClaimClass::Snapshot,
            ClaimClass::Render,
            ClaimClass::Pty,
            ClaimClass::Execution,
            ClaimClass::Execution,
        ];
        assert_ne!(encode_canonical(&artifact)?, encode_canonical(&scrambled)?);
        scrambled.claims = artifact.claims.clone();
        assert_eq!(artifact.digest, digest_canonical(&scrambled)?);
        Ok(())
    }

    #[test]
    fn qemu_builder_forces_contingency_and_non_render_claims()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut value = recorder(DriverKind::QemuUserSmoke);
        value.spawn(vec!["qemu".to_owned()], &NormalizationContext::default())?;
        assert!(!value.snapshot(
            80,
            24,
            [0, 0],
            vec!["frame".to_owned()],
            &NormalizationContext::default(),
        )?);
        assert!(!value.resize(40, 12)?);
        value.exit(Some(0), true)?;
        let artifact = value.finish()?;
        assert_eq!(artifact.mode, TranscriptMode::Contingency);
        assert_eq!(artifact.claims, vec![ClaimClass::Execution]);
        let has_snapshot = artifact
            .canonical
            .events
            .iter()
            .any(|event| event.kind() == EventKind::Snapshot);
        assert!(!has_snapshot);
        Ok(())
    }

    #[test]
    fn relative_time_phrase_is_normalized() {
        let normalized = normalize_raw_bytes(b"done 5ms ago", &NormalizationContext::default());
        assert_eq!(normalized.bytes, b"done <AGO>");
        assert!(normalized
            .applied
            .iter()
            .any(|entry| entry.kind == NormalizationKind::TimeRelative));
    }

    fn complete(mut value: TranscriptRecorder) -> Result<TranscriptArtifact, TranscriptError> {
        value.spawn(vec!["pi".to_owned()], &NormalizationContext::default())?;
        value.exit(Some(0), true)?;
        value.finish()
    }
}
