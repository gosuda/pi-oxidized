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
    Render,
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

/// Non-canonical timing and audit data.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct TimingEnvelope {
    pub wall_ms: u64,
    pub chunk_log: Vec<ChunkTiming>,
    pub settle_windows_ms: Vec<u64>,
    pub abort_ceiling: Option<AbortCeiling>,
    pub raw_log_b64: String,
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
    let bytes = encode_canonical(artifact)?;
    Ok(format!("sha256:{:x}", Sha256::digest(bytes)))
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

    replace_context(&mut bytes, context.home.as_deref(), b"<HOME>", NormalizationKind::PathHome, &mut applied);
    replace_context(&mut bytes, context.cwd.as_deref(), b"<CWD>", NormalizationKind::PathCwd, &mut applied);
    normalize_tokens(&mut bytes, &mut applied);

    NormalizedBytes {
        bytes,
        applied: applied.into_iter().map(|kind| NormalizationEntry { kind }).collect(),
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
    haystack.windows(needle.len()).position(|window| window == needle)
}

fn normalize_tokens(bytes: &mut Vec<u8>, applied: &mut BTreeSet<NormalizationKind>) {
    let text = String::from_utf8_lossy(bytes);
    let mut output = String::with_capacity(text.len());
    let chars: Vec<char> = text.chars().collect();
    let mut index = 0;
    while index < chars.len() {
        if let Some((len, kind, replacement)) = match_volatile_at(&chars, index) {
            output.push_str(replacement);
            applied.insert(kind);
            index += len;
            continue;
        }
        output.push(chars[index]);
        index += 1;
    }
    *bytes = output.into_bytes();
}

pub(crate) fn detected_volatile_kinds(raw: &[u8]) -> BTreeSet<NormalizationKind> {
    let text = String::from_utf8_lossy(raw);
    let chars: Vec<char> = text.chars().collect();
    let mut detected = BTreeSet::new();
    let mut index = 0;
    while index < chars.len() {
        if let Some((len, kind, _)) = match_volatile_at(&chars, index) {
            detected.insert(kind);
            index += len;
            continue;
        }
        index += 1;
    }
    if text.contains("/home/") || text.contains("/Users/") {
        detected.insert(NormalizationKind::PathHome);
    }
    detected
}

fn match_volatile_at(chars: &[char], index: usize) -> Option<(usize, NormalizationKind, &'static str)> {
    if let Some(len) = match_uuid_at(chars, index) {
        return Some((len, NormalizationKind::IdSession, "<SESSION>"));
    }
    if let Some(len) = match_iso8601_at(chars, index) {
        return Some((len, NormalizationKind::TimeIso8601, "<TS>"));
    }
    if let Some(len) = match_relative_time_at(chars, index) {
        return Some((len, NormalizationKind::TimeRelative, "<AGO>"));
    }
    None
}

fn match_uuid_at(chars: &[char], index: usize) -> Option<usize> {
    if index + 36 > chars.len() {
        return None;
    }
    let slice: String = chars[index..index + 36].iter().collect();
    is_uuid(&slice).then_some(36)
}

fn match_iso8601_at(chars: &[char], index: usize) -> Option<usize> {
    if index + 20 > chars.len() || chars[index + 4] != '-' || chars[index + 7] != '-' || chars[index + 10] != 'T'
    {
        return None;
    }
    let mut end = index + 20;
    while end < chars.len() && !chars[end].is_ascii_whitespace() {
        end += 1;
    }
    let slice: String = chars[index..end].iter().collect();
    is_iso8601(&slice).then_some(end - index)
}

fn match_relative_time_at(chars: &[char], index: usize) -> Option<usize> {
    let mut cursor = index;
    if cursor >= chars.len() || !chars[cursor].is_ascii_digit() {
        return None;
    }
    while cursor < chars.len() && chars[cursor].is_ascii_digit() {
        cursor += 1;
    }
    let unit_len = if matches!(chars.get(cursor..cursor + 2), Some(['m', 's'])) {
        2
    } else if matches!(chars.get(cursor), Some('s' | 'm' | 'h')) {
        1
    } else {
        return None;
    };
    cursor += unit_len;
    if !matches!(chars.get(cursor..cursor + 4), Some([' ', 'a', 'g', 'o'])) {
        return None;
    }
    cursor += 4;
    Some(cursor - index)
}

fn is_uuid(token: &str) -> bool {
    let bytes = token.as_bytes();
    bytes.len() == 36
        && [8, 13, 18, 23].into_iter().all(|index| bytes[index] == b'-')
        && bytes.iter().enumerate().all(|(index, byte)| {
            [8, 13, 18, 23].contains(&index) || byte.is_ascii_hexdigit()
        })
}

fn is_iso8601(token: &str) -> bool {
    let bytes = token.as_bytes();
    bytes.len() >= 20
        && bytes.get(4) == Some(&b'-')
        && bytes.get(7) == Some(&b'-')
        && bytes.get(10) == Some(&b'T')
        && token.ends_with('Z')
}

/// Builds events in sequence and enforces driver-specific evidence restrictions.
pub struct TranscriptRecorder {
    artifact: TranscriptArtifact,
    next_seq: u32,
    applied: BTreeSet<NormalizationEntry>,
    raw_log: Vec<u8>,
}

impl TranscriptRecorder {
    /// Creates an empty recorder. QEMU mode and claims are forcibly narrowed.
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        scenario: Scenario,
        row: RunnerRow,
        geometry: Geometry,
        capability_profile: CapabilityProfile,
        driver_kind: DriverKind,
        mode: TranscriptMode,
        claims: Vec<ClaimClass>,
        timing: TimingEnvelope,
    ) -> Self {
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
                canonical: CanonicalDoc { events: Vec::new(), normalizations: Vec::new() },
                digest: String::new(),
                timing,
            },
            next_seq: 0,
            applied: BTreeSet::new(),
            raw_log: Vec::new(),
        }
    }

    pub fn spawn(&mut self, argv: Vec<String>) -> Result<(), TranscriptError> {
        let seq = self.take_seq()?;
        self.artifact.canonical.events.push(CanonicalEvent::Spawn { seq, argv });
        Ok(())
    }

    pub fn input(&mut self, bytes: &[u8]) -> Result<(), TranscriptError> {
        let seq = self.take_seq()?;
        self.artifact.canonical.events.push(CanonicalEvent::Input { seq, bytes_b64: BASE64.encode(bytes) });
        Ok(())
    }

    /// Normalizes and merges output since the preceding input boundary into one event.
    pub fn output(&mut self, chunks: &[&[u8]], context: &NormalizationContext) -> Result<(), TranscriptError> {
        let raw_len = chunks.iter().map(|chunk| chunk.len()).sum();
        let mut raw = Vec::with_capacity(raw_len);
        for chunk in chunks {
            raw.extend_from_slice(chunk);
        }
        self.raw_log.extend_from_slice(&raw);
        let normalized = normalize_raw_bytes(&raw, context);
        self.applied.extend(normalized.applied);
        let seq = self.take_seq()?;
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
        mut lines: Vec<String>,
    ) -> Result<bool, TranscriptError> {
        if self.artifact.driver.kind == DriverKind::QemuUserSmoke {
            return Ok(false);
        }
        let mut trimmed = false;
        for line in &mut lines {
            let len = line.len();
            *line = line.trim_end_matches(' ').to_owned();
            trimmed |= len != line.len();
        }
        if trimmed {
            self.applied.insert(NormalizationEntry { kind: NormalizationKind::SnapshotTrailingSpaceTrim });
        }
        let seq = self.take_seq()?;
        self.artifact.canonical.events.push(CanonicalEvent::Snapshot { seq, cols, rows, cursor, lines });
        Ok(true)
    }

    pub fn resize(&mut self, cols: u16, rows: u16) -> Result<bool, TranscriptError> {
        if self.artifact.driver.kind == DriverKind::QemuUserSmoke {
            return Ok(false);
        }
        let seq = self.take_seq()?;
        self.artifact.canonical.events.push(CanonicalEvent::Resize { seq, cols, rows });
        Ok(true)
    }

    pub fn resize_storm(&mut self, sizes: Vec<Geometry>) -> Result<bool, TranscriptError> {
        if self.artifact.driver.kind == DriverKind::QemuUserSmoke {
            return Ok(false);
        }
        let collapsed = sizes.last().copied().into_iter().collect::<Vec<_>>();
        if sizes.len() > collapsed.len() {
            self.applied.insert(NormalizationEntry { kind: NormalizationKind::ResizeCollapse });
        }
        let seq = self.take_seq()?;
        self.artifact.canonical.events.push(CanonicalEvent::ResizeStorm { seq, sizes: collapsed });
        Ok(true)
    }

    pub fn exit(&mut self, code: Option<i32>, success: bool) -> Result<(), TranscriptError> {
        let seq = self.take_seq()?;
        self.artifact.canonical.events.push(CanonicalEvent::Exit { seq, code, success });
        Ok(())
    }

    /// Finalizes normalization metadata, raw audit bytes, and digest.
    pub fn finish(mut self) -> Result<TranscriptArtifact, TranscriptError> {
        self.artifact.canonical.normalizations = self.applied.into_iter().collect();
        self.artifact.timing.raw_log_b64 = BASE64.encode(self.raw_log);
        self.artifact.digest = digest_canonical(&self.artifact)?;
        Ok(self.artifact)
    }

    fn take_seq(&mut self) -> Result<u32, TranscriptError> {
        let seq = self.next_seq;
        self.next_seq = self.next_seq.checked_add(1).ok_or(TranscriptError::SequenceOverflow)?;
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
        claims.into_iter().filter(|claim| matches!(claim, ClaimClass::Execution | ClaimClass::Protocol)).collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn recorder(driver: DriverKind) -> TranscriptRecorder {
        TranscriptRecorder::new(
            Scenario::ColdStart,
            RunnerRow { tier: RowTier::Local, id: RowId::GnuX64, runner_image: None },
            Geometry { cols: 80, rows: 24 },
            CapabilityProfile::Xterm256Color,
            driver,
            TranscriptMode::Standard,
            vec![ClaimClass::Execution, ClaimClass::Render],
            TimingEnvelope::default(),
        )
    }

    #[test]
    fn unknown_fields_are_rejected() -> Result<(), Box<dyn std::error::Error>> {
        let json = r#"{"cols":80,"rows":24,"extra":true}"#;
        let error = serde_json::from_str::<Geometry>(json).err().ok_or("unknown field unexpectedly accepted")?;
        assert!(error.to_string().contains("unknown field"));
        Ok(())
    }

    #[test]
    fn timing_and_runner_identity_do_not_change_canonical_bytes() -> Result<(), Box<dyn std::error::Error>> {
        let mut first = complete(recorder(DriverKind::PosixPty))?;
        let before = encode_canonical(&first)?;
        first.row.runner_image = Some("other-image".to_owned());
        first.timing.wall_ms = 99_999;
        first.timing.raw_log_b64 = BASE64.encode(b"different audit bytes");
        assert_eq!(before, encode_canonical(&first)?);
        Ok(())
    }

    #[test]
    fn normalization_happens_before_output_event_and_digest() -> Result<(), Box<dyn std::error::Error>> {
        let mut value = recorder(DriverKind::PosixPty);
        value.spawn(vec!["pi".to_owned()])?;
        value.output(&[b"/home/alice/project 550e8400-e29b-41d4-a716-446655440000"], &NormalizationContext {
            home: Some(b"/home/alice".to_vec()),
            cwd: None,
        })?;
        value.exit(Some(0), true)?;
        let artifact = value.finish()?;
        let output = artifact.canonical.events.get(1).ok_or("missing output")?;
        let CanonicalEvent::Output { bytes_b64, .. } = output else { return Err("wrong event".into()); };
        let decoded = BASE64.decode(bytes_b64)?;
        assert_eq!(decoded, b"<HOME>/project <SESSION>");
        assert!(artifact.canonical.normalizations.iter().any(|entry| entry.kind == NormalizationKind::PathHome));
        assert!(artifact.canonical.normalizations.iter().any(|entry| entry.kind == NormalizationKind::IdSession));
        assert_eq!(artifact.digest, digest_canonical(&artifact)?);
        Ok(())
    }

    #[test]
    fn unchanged_bytes_enumerate_no_normalizations() {
        let normalized = normalize_raw_bytes(b"stable output", &NormalizationContext::default());
        assert_eq!(normalized.bytes, b"stable output");
        assert!(normalized.applied.is_empty());
    }

    #[test]
    fn qemu_builder_forces_contingency_and_non_render_claims() -> Result<(), Box<dyn std::error::Error>> {
        let mut value = recorder(DriverKind::QemuUserSmoke);
        value.spawn(vec!["qemu".to_owned()])?;
        assert!(!value.snapshot(80, 24, [0, 0], vec!["frame".to_owned()])?);
        assert!(!value.resize(40, 12)?);
        value.exit(Some(0), true)?;
        let artifact = value.finish()?;
        assert_eq!(artifact.mode, TranscriptMode::Contingency);
        assert_eq!(artifact.claims, vec![ClaimClass::Execution]);
        assert!(!artifact.canonical.events.iter().any(|event| event.kind() == EventKind::Snapshot));
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
        value.spawn(vec!["pi".to_owned()])?;
        value.exit(Some(0), true)?;
        value.finish()
    }
}
