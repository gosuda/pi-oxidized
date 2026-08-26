use std::collections::{BTreeMap, BTreeSet};

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use serde_json::Value;
use thiserror::Error;

use super::transcript::{
    ClaimClass, CanonicalEvent, DriverKind, EventKind, NORMALIZATION_TABLE_V1, NormalizationAuditContext,
    NormalizationContext, NormalizationKind, RowId, RowTier, SCHEMA_ID, TranscriptArtifact, TranscriptMode,
    detected_volatile_kinds, digest_canonical, normalize_raw_bytes,
};

const TIMING_LIKE_FIELDS: &[&str] = &[
    "wallMs",
    "chunkLog",
    "settleWindowsMs",
    "abortCeiling",
    "rawLogB64",
    "outputAudits",
    "rawBytesB64",
    "homeB64",
    "cwdB64",
    "deltaMs",
    "byteLen",
    "eventSeq",
    "ceilingMs",
    "observedMs",
];

const CANONICAL_LIKE_FIELDS: &[&str] = &[
    "events",
    "normalizations",
    "seq",
    "kind",
    "bytesB64",
    "argv",
    "cursor",
    "lines",
    "sizes",
    "code",
    "success",
    "schema",
    "scenario",
    "claims",
    "digest",
];

/// Validation failure for a schema-v1 transcript artifact.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum ValidatorError {
    #[error("JSON parse failed: {0}")]
    Parse(String),
    #[error("unknown field: {0}")]
    UnknownField(String),
    #[error("wrong schema id: {0}")]
    WrongSchema(String),
    #[error("event sequence gap or disorder at index {0}")]
    SequenceGapOrOrder(usize),
    #[error("missing spawn or exit boundary events")]
    MissingSpawnOrExit,
    #[error("digest mismatch")]
    DigestMismatch,
    #[error("timing-like field present in canonical content: {0}")]
    TimingLikeCanonicalField(String),
    #[error("canonical-like field present in timing envelope: {0}")]
    CanonicalLikeTimingField(String),
    #[error("detected volatile data without enumerated normalization: {0:?}")]
    UnenumeratedVolatile(NormalizationKind),
    #[error("tier-n artifact is missing runnerImage")]
    TierNMissingRunnerImage,
    #[error("qemu-user-smoke artifacts must use contingency mode")]
    QemuNonContingencyMode,
    #[error("qemu-user-smoke claim outside Execution/Protocol: {0:?}")]
    QemuClaimOutsideAllowed(ClaimClass),
    #[error("qemu-user-smoke artifact must not include snapshot or render evidence")]
    QemuSnapshotOrRenderClaim,
    #[error("normalization entry is outside NORMALIZATION_TABLE_V1: {0:?}")]
    UnknownNormalization(NormalizationKind),
    #[error("missing output audit for seq {0}")]
    MissingOutputAudit(u32),
    #[error("duplicate output audit for seq {0}")]
    DuplicateOutputAudit(u32),
    #[error("extra output audit for seq {0}")]
    ExtraOutputAudit(u32),
    #[error("output audit mismatch for seq {0}")]
    OutputAuditMismatch(u32),
    #[error("geometry has zero cols or rows: {cols}x{rows}")]
    ZeroGeometry { cols: u16, rows: u16 },
    #[error("qemu-user-smoke artifacts cannot use tier-n rows")]
    QemuTierN,
    #[error("driver/row pairing mismatch: row={row:?} driver={driver:?}")]
    DriverRowMismatch { row: RowId, driver: DriverKind },
}

/// Validates a typed schema-v1 artifact.
pub fn validate_artifact(artifact: &TranscriptArtifact) -> Result<(), ValidatorError> {
    validate_rules(artifact)
}

/// Parses JSON bytes and validates the resulting artifact.
pub fn validate_bytes(bytes: &[u8]) -> Result<TranscriptArtifact, ValidatorError> {
    let value: Value = serde_json::from_slice(bytes).map_err(|error| {
        let message = error.to_string();
        if message.contains("unknown field") {
            ValidatorError::UnknownField(message)
        } else {
            ValidatorError::Parse(message)
        }
    })?;
    validate_value(&value)
}

/// Validates a JSON value, including cross-contamination field checks.
pub fn validate_value(value: &Value) -> Result<TranscriptArtifact, ValidatorError> {
    reject_cross_contamination(value)?;
    let artifact: TranscriptArtifact = serde_json::from_value(value.clone()).map_err(|error| {
        let message = error.to_string();
        if message.contains("unknown field") {
            ValidatorError::UnknownField(message)
        } else {
            ValidatorError::Parse(message)
        }
    })?;
    validate_rules(&artifact)?;
    Ok(artifact)
}

fn reject_cross_contamination(value: &Value) -> Result<(), ValidatorError> {
    if let Some(canonical) = value.get("canonical") {
        reject_named_fields(canonical, TIMING_LIKE_FIELDS, |name| {
            ValidatorError::TimingLikeCanonicalField(name.to_owned())
        })?;
        if let Some(Value::Array(events)) = canonical.get("events") {
            for event in events {
                reject_named_fields(event, TIMING_LIKE_FIELDS, |name| {
                    ValidatorError::TimingLikeCanonicalField(name.to_owned())
                })?;
            }
        }
    }
    if let Some(timing) = value.get("timing") {
        reject_named_fields(timing, CANONICAL_LIKE_FIELDS, |name| {
            ValidatorError::CanonicalLikeTimingField(name.to_owned())
        })?;
        if let Some(Value::Array(chunks)) = timing.get("chunkLog") {
            for chunk in chunks {
                reject_named_fields(chunk, CANONICAL_LIKE_FIELDS, |name| {
                    ValidatorError::CanonicalLikeTimingField(name.to_owned())
                })?;
            }
        }
    }
    Ok(())
}

fn reject_named_fields(
    value: &Value,
    banned: &[&str],
    error: impl Fn(&str) -> ValidatorError,
) -> Result<(), ValidatorError> {
    let Some(object) = value.as_object() else {
        return Ok(());
    };
    for name in banned {
        if object.contains_key(*name) {
            return Err(error(name));
        }
    }
    Ok(())
}

fn validate_rules(artifact: &TranscriptArtifact) -> Result<(), ValidatorError> {
    if artifact.schema != SCHEMA_ID {
        return Err(ValidatorError::WrongSchema(artifact.schema.clone()));
    }

    reject_zero_geometry(artifact.geometry.cols, artifact.geometry.rows)?;
    if artifact.driver.kind == DriverKind::QemuUserSmoke && artifact.row.tier == RowTier::TierN {
        return Err(ValidatorError::QemuTierN);
    }
    validate_driver_row_pairing(artifact)?;

    let events = &artifact.canonical.events;
    if events.is_empty()
        || !matches!(events.first(), Some(CanonicalEvent::Spawn { .. }))
        || !matches!(events.last(), Some(CanonicalEvent::Exit { .. }))
    {
        return Err(ValidatorError::MissingSpawnOrExit);
    }

    for (index, event) in events.iter().enumerate() {
        let expected = u32::try_from(index).map_err(|_| ValidatorError::SequenceGapOrOrder(index))?;
        if event.seq() != expected {
            return Err(ValidatorError::SequenceGapOrOrder(index));
        }
        match event {
            CanonicalEvent::Snapshot { cols, rows, .. } | CanonicalEvent::Resize { cols, rows, .. } => {
                reject_zero_geometry(*cols, *rows)?;
            }
            CanonicalEvent::ResizeStorm { sizes, .. } => {
                for size in sizes {
                    reject_zero_geometry(size.cols, size.rows)?;
                }
            }
            _ => {}
        }
    }

    let allowed: BTreeSet<_> = NORMALIZATION_TABLE_V1.iter().copied().collect();
    for entry in &artifact.canonical.normalizations {
        if !allowed.contains(&entry.kind) {
            return Err(ValidatorError::UnknownNormalization(entry.kind));
        }
    }

    validate_output_audits(artifact)?;

    let raw = BASE64
        .decode(artifact.timing.raw_log_b64.as_bytes())
        .map_err(|error| ValidatorError::Parse(error.to_string()))?;
    let enumerated: BTreeSet<_> = artifact
        .canonical
        .normalizations
        .iter()
        .map(|entry| entry.kind)
        .collect();
    for kind in detected_volatile_kinds(&raw) {
        if !enumerated.contains(&kind) {
            return Err(ValidatorError::UnenumeratedVolatile(kind));
        }
    }

    let digest = digest_canonical(artifact).map_err(|error| ValidatorError::Parse(error.to_string()))?;
    if digest != artifact.digest {
        return Err(ValidatorError::DigestMismatch);
    }

    if artifact.row.tier == RowTier::TierN {
        match artifact.row.runner_image.as_deref() {
            Some(image) if !image.is_empty() => {}
            _ => return Err(ValidatorError::TierNMissingRunnerImage),
        }
    }

    if artifact.driver.kind == DriverKind::QemuUserSmoke {
        if artifact.mode != TranscriptMode::Contingency {
            return Err(ValidatorError::QemuNonContingencyMode);
        }
        for claim in &artifact.claims {
            if !matches!(claim, ClaimClass::Execution | ClaimClass::Protocol) {
                return Err(ValidatorError::QemuClaimOutsideAllowed(*claim));
            }
        }
        if events.iter().any(|event| {
            matches!(
                event.kind(),
                EventKind::Snapshot | EventKind::Resize | EventKind::ResizeStorm
            )
        }) {
            return Err(ValidatorError::QemuSnapshotOrRenderClaim);
        }
    }

    Ok(())
}

fn reject_zero_geometry(cols: u16, rows: u16) -> Result<(), ValidatorError> {
    if cols == 0 || rows == 0 {
        return Err(ValidatorError::ZeroGeometry { cols, rows });
    }
    Ok(())
}

fn validate_driver_row_pairing(artifact: &TranscriptArtifact) -> Result<(), ValidatorError> {
    let row = artifact.row.id;
    let driver = artifact.driver.kind;
    let mismatch = ValidatorError::DriverRowMismatch { row, driver };

    if row == RowId::WindowsX64 && driver != DriverKind::ConPty {
        return Err(mismatch);
    }

    if artifact.row.tier == RowTier::TierN {
        match row {
            RowId::GnuX64 | RowId::GnuArm64 | RowId::DarwinX64 | RowId::DarwinArm64 => {
                if driver != DriverKind::PosixPty {
                    return Err(mismatch);
                }
            }
            RowId::WindowsX64 => {
                if driver != DriverKind::ConPty {
                    return Err(mismatch);
                }
            }
        }
    }

    Ok(())
}

fn validate_output_audits(artifact: &TranscriptArtifact) -> Result<(), ValidatorError> {
    let mut by_seq = BTreeMap::new();
    for audit in &artifact.timing.output_audits {
        if by_seq.insert(audit.event_seq, audit).is_some() {
            return Err(ValidatorError::DuplicateOutputAudit(audit.event_seq));
        }
    }

    let mut consumed = BTreeSet::new();
    for event in &artifact.canonical.events {
        let CanonicalEvent::Output { seq, bytes_b64 } = event else {
            continue;
        };
        let Some(audit) = by_seq.get(seq) else {
            return Err(ValidatorError::MissingOutputAudit(*seq));
        };
        if !consumed.insert(*seq) {
            return Err(ValidatorError::DuplicateOutputAudit(*seq));
        }

        let raw = BASE64
            .decode(audit.raw_bytes_b64.as_bytes())
            .map_err(|error| ValidatorError::Parse(error.to_string()))?;
        let context = context_from_audit(&audit.context)?;
        let normalized = normalize_raw_bytes(&raw, &context);
        if normalized.applied != audit.applied {
            return Err(ValidatorError::OutputAuditMismatch(*seq));
        }
        let recomputed = BASE64.encode(normalized.bytes);
        if &recomputed != bytes_b64 {
            return Err(ValidatorError::OutputAuditMismatch(*seq));
        }
    }

    for audit in &artifact.timing.output_audits {
        if !consumed.contains(&audit.event_seq) {
            return Err(ValidatorError::ExtraOutputAudit(audit.event_seq));
        }
    }
    Ok(())
}

fn context_from_audit(audit: &NormalizationAuditContext) -> Result<NormalizationContext, ValidatorError> {
    Ok(NormalizationContext {
        home: decode_optional_b64(audit.home_b64.as_deref())?,
        cwd: decode_optional_b64(audit.cwd_b64.as_deref())?,
    })
}

fn decode_optional_b64(value: Option<&str>) -> Result<Option<Vec<u8>>, ValidatorError> {
    value
        .map(|encoded| {
            BASE64
                .decode(encoded.as_bytes())
                .map_err(|error| ValidatorError::Parse(error.to_string()))
        })
        .transpose()
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::transcript::{
        CapabilityProfile, DriverDescriptor, Geometry, NormalizationContext, NormalizationEntry,
        OutputAudit, NormalizationAuditContext, RowId, RunnerRow, Scenario, TimingEnvelope,
        TranscriptRecorder, TranscriptSpec,
    };

    fn valid_posix() -> Result<TranscriptArtifact, Box<dyn std::error::Error>> {
        let mut recorder = TranscriptRecorder::new(TranscriptSpec {
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
            claims: vec![ClaimClass::Execution, ClaimClass::Render],
            timing: TimingEnvelope::default(),
        });
        recorder.spawn(vec!["pi".to_owned()], &NormalizationContext::default())?;
        recorder.output(&[b"ready"], &NormalizationContext::default())?;
        recorder.exit(Some(0), true)?;
        Ok(recorder.finish()?)
    }

    fn as_value(artifact: &TranscriptArtifact) -> Result<Value, Box<dyn std::error::Error>> {
        Ok(serde_json::to_value(artifact)?)
    }

    #[test]
    fn accepts_valid_artifact() -> Result<(), Box<dyn std::error::Error>> {
        let artifact = valid_posix()?;
        validate_artifact(&artifact)?;
        validate_value(&as_value(&artifact)?)?;
        Ok(())
    }

    #[test]
    fn rejects_unknown_fields() -> Result<(), Box<dyn std::error::Error>> {
        let mut value = as_value(&valid_posix()?)?;
        value
            .as_object_mut()
            .ok_or("object")?
            .insert("extra".to_owned(), Value::Bool(true));
        let error = validate_value(&value).err().ok_or("unknown field")?;
        assert!(matches!(error, ValidatorError::UnknownField(_)));
        Ok(())
    }

    #[test]
    fn rejects_wrong_schema() -> Result<(), Box<dyn std::error::Error>> {
        let mut artifact = valid_posix()?;
        artifact.schema = "pi-tui-transcript/0".to_owned();
        artifact.digest = digest_canonical(&artifact)?;
        let error = validate_artifact(&artifact).err().ok_or("wrong schema")?;
        assert_eq!(error, ValidatorError::WrongSchema("pi-tui-transcript/0".to_owned()));
        Ok(())
    }

    #[test]
    fn rejects_sequence_gap_or_order() -> Result<(), Box<dyn std::error::Error>> {
        let mut artifact = valid_posix()?;
        if let CanonicalEvent::Exit { seq, .. } = &mut artifact.canonical.events[2] {
            *seq = 9;
        }
        artifact.digest = digest_canonical(&artifact)?;
        let error = validate_artifact(&artifact).err().ok_or("gap")?;
        assert_eq!(error, ValidatorError::SequenceGapOrOrder(2));
        Ok(())
    }

    #[test]
    fn rejects_missing_spawn_or_exit() -> Result<(), Box<dyn std::error::Error>> {
        let mut artifact = valid_posix()?;
        artifact.canonical.events.pop();
        artifact.digest = digest_canonical(&artifact)?;
        let error = validate_artifact(&artifact).err().ok_or("missing exit")?;
        assert_eq!(error, ValidatorError::MissingSpawnOrExit);
        Ok(())
    }

    #[test]
    fn rejects_digest_mismatch() -> Result<(), Box<dyn std::error::Error>> {
        let mut artifact = valid_posix()?;
        artifact.digest = "sha256:deadbeef".to_owned();
        let error = validate_artifact(&artifact).err().ok_or("digest")?;
        assert_eq!(error, ValidatorError::DigestMismatch);
        Ok(())
    }

    #[test]
    fn rejects_timing_like_canonical_fields() -> Result<(), Box<dyn std::error::Error>> {
        let mut value = as_value(&valid_posix()?)?;
        value
            .pointer_mut("/canonical")
            .and_then(Value::as_object_mut)
            .ok_or("canonical")?
            .insert("wallMs".to_owned(), Value::from(12));
        let error = validate_value(&value).err().ok_or("timing field")?;
        assert_eq!(error, ValidatorError::TimingLikeCanonicalField("wallMs".to_owned()));
        Ok(())
    }

    #[test]
    fn rejects_canonical_like_timing_fields() -> Result<(), Box<dyn std::error::Error>> {
        let mut value = as_value(&valid_posix()?)?;
        value
            .pointer_mut("/timing")
            .and_then(Value::as_object_mut)
            .ok_or("timing")?
            .insert("events".to_owned(), Value::Array(Vec::new()));
        let error = validate_value(&value).err().ok_or("canonical field")?;
        assert_eq!(error, ValidatorError::CanonicalLikeTimingField("events".to_owned()));
        Ok(())
    }

    #[test]
    fn rejects_unenumerated_detected_volatile_data() -> Result<(), Box<dyn std::error::Error>> {
        let mut artifact = valid_posix()?;
        artifact.timing.raw_log_b64 = BASE64.encode(
            b"session 550e8400-e29b-41d4-a716-446655440000 at 2024-01-02T03:04:05Z",
        );
        artifact.canonical.normalizations.clear();
        artifact.digest = digest_canonical(&artifact)?;
        let error = validate_artifact(&artifact).err().ok_or("volatile")?;
        assert!(matches!(error, ValidatorError::UnenumeratedVolatile(_)));
        Ok(())
    }

    #[test]
    fn rejects_tier_n_missing_runner_image() -> Result<(), Box<dyn std::error::Error>> {
        let mut artifact = valid_posix()?;
        artifact.row.tier = RowTier::TierN;
        artifact.row.runner_image = None;
        artifact.digest = digest_canonical(&artifact)?;
        let error = validate_artifact(&artifact).err().ok_or("tier-n")?;
        assert_eq!(error, ValidatorError::TierNMissingRunnerImage);
        Ok(())
    }

    #[test]
    fn rejects_qemu_non_contingency_mode() -> Result<(), Box<dyn std::error::Error>> {
        let mut artifact = valid_posix()?;
        artifact.driver = DriverDescriptor {
            kind: DriverKind::QemuUserSmoke,
        };
        artifact.mode = TranscriptMode::Standard;
        artifact.claims = vec![ClaimClass::Execution];
        artifact.digest = digest_canonical(&artifact)?;
        let error = validate_artifact(&artifact).err().ok_or("mode")?;
        assert_eq!(error, ValidatorError::QemuNonContingencyMode);
        Ok(())
    }

    #[test]
    fn rejects_qemu_claims_outside_execution_protocol() -> Result<(), Box<dyn std::error::Error>> {
        let mut artifact = valid_posix()?;
        artifact.driver = DriverDescriptor {
            kind: DriverKind::QemuUserSmoke,
        };
        artifact.mode = TranscriptMode::Contingency;
        artifact.claims = vec![ClaimClass::Execution, ClaimClass::Render];
        artifact.digest = digest_canonical(&artifact)?;
        let error = validate_artifact(&artifact).err().ok_or("claim")?;
        assert_eq!(error, ValidatorError::QemuClaimOutsideAllowed(ClaimClass::Render));
        Ok(())
    }

    #[test]
    fn rejects_qemu_snapshot_event() -> Result<(), Box<dyn std::error::Error>> {
        let mut recorder = TranscriptRecorder::new(TranscriptSpec {
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
            claims: vec![ClaimClass::Execution],
            timing: TimingEnvelope::default(),
        });
        recorder.spawn(vec!["pi".to_owned()], &NormalizationContext::default())?;
        recorder.snapshot(
            80,
            24,
            [0, 0],
            vec!["frame".to_owned()],
            &NormalizationContext::default(),
        )?;
        recorder.exit(Some(0), true)?;
        let mut artifact = recorder.finish()?;
        artifact.driver = DriverDescriptor {
            kind: DriverKind::QemuUserSmoke,
        };
        artifact.mode = TranscriptMode::Contingency;
        artifact.claims = vec![ClaimClass::Execution];
        artifact.digest = digest_canonical(&artifact)?;
        let error = validate_artifact(&artifact).err().ok_or("snapshot")?;
        assert_eq!(error, ValidatorError::QemuSnapshotOrRenderClaim);
        Ok(())
    }

    #[test]
    fn rejects_qemu_snapshot_claim() -> Result<(), Box<dyn std::error::Error>> {
        let mut artifact = valid_posix()?;
        artifact.driver = DriverDescriptor {
            kind: DriverKind::QemuUserSmoke,
        };
        artifact.mode = TranscriptMode::Contingency;
        artifact.claims = vec![ClaimClass::Snapshot];
        artifact.canonical.events.retain(|event| {
            !matches!(
                event.kind(),
                EventKind::Snapshot | EventKind::Resize | EventKind::ResizeStorm
            )
        });
        for (index, event) in artifact.canonical.events.iter_mut().enumerate() {
            let seq = u32::try_from(index).map_err(|_| "seq")?;
            match event {
                CanonicalEvent::Spawn { seq: value, .. }
                | CanonicalEvent::Input { seq: value, .. }
                | CanonicalEvent::Output { seq: value, .. }
                | CanonicalEvent::Snapshot { seq: value, .. }
                | CanonicalEvent::Resize { seq: value, .. }
                | CanonicalEvent::ResizeStorm { seq: value, .. }
                | CanonicalEvent::Exit { seq: value, .. } => *value = seq,
            }
        }
        artifact.digest = digest_canonical(&artifact)?;
        let error = validate_artifact(&artifact).err().ok_or("snapshot claim")?;
        assert_eq!(error, ValidatorError::QemuClaimOutsideAllowed(ClaimClass::Snapshot));
        Ok(())
    }

    #[test]
    fn qemu_builder_restrictions_survive_validation() -> Result<(), Box<dyn std::error::Error>> {
        let mut recorder = TranscriptRecorder::new(TranscriptSpec {
            scenario: Scenario::ColdStart,
            row: RunnerRow {
                tier: RowTier::Local,
                id: RowId::GnuX64,
                runner_image: None,
            },
            geometry: Geometry { cols: 80, rows: 24 },
            capability_profile: CapabilityProfile::Dumb,
            driver_kind: DriverKind::QemuUserSmoke,
            mode: TranscriptMode::Standard,
            claims: vec![
                ClaimClass::Execution,
                ClaimClass::Protocol,
                ClaimClass::Render,
                ClaimClass::Snapshot,
            ],
            timing: TimingEnvelope::default(),
        });
        recorder.spawn(vec!["qemu".to_owned()], &NormalizationContext::default())?;
        assert!(!recorder.snapshot(
            80,
            24,
            [0, 0],
            vec!["nope".to_owned()],
            &NormalizationContext::default(),
        )?);
        recorder.exit(Some(0), true)?;
        let artifact = recorder.finish()?;
        validate_artifact(&artifact)?;
        assert_eq!(artifact.mode, TranscriptMode::Contingency);
        assert!(artifact
            .claims
            .iter()
            .all(|claim| matches!(claim, ClaimClass::Execution | ClaimClass::Protocol)));
        Ok(())
    }

    #[test]
    fn enumerated_volatile_data_is_accepted() -> Result<(), Box<dyn std::error::Error>> {
        let mut artifact = valid_posix()?;
        artifact.timing.raw_log_b64 =
            BASE64.encode(b"550e8400-e29b-41d4-a716-446655440000 /home/alice");
        artifact.canonical.normalizations = vec![
            NormalizationEntry {
                kind: NormalizationKind::IdSession,
            },
            NormalizationEntry {
                kind: NormalizationKind::PathHome,
            },
        ];
        artifact.digest = digest_canonical(&artifact)?;
        validate_artifact(&artifact)?;
        Ok(())
    }

    #[test]
    fn rejects_missing_output_audit() -> Result<(), Box<dyn std::error::Error>> {
        let mut artifact = valid_posix()?;
        artifact.timing.output_audits.clear();
        artifact.digest = digest_canonical(&artifact)?;
        let error = validate_artifact(&artifact).err().ok_or("missing audit")?;
        assert_eq!(error, ValidatorError::MissingOutputAudit(1));
        Ok(())
    }

    #[test]
    fn rejects_extra_output_audit() -> Result<(), Box<dyn std::error::Error>> {
        let mut artifact = valid_posix()?;
        artifact.timing.output_audits.push(OutputAudit {
            event_seq: 99,
            raw_bytes_b64: BASE64.encode(b"extra"),
            context: NormalizationAuditContext::default(),
            applied: Vec::new(),
        });
        artifact.digest = digest_canonical(&artifact)?;
        let error = validate_artifact(&artifact).err().ok_or("extra audit")?;
        assert_eq!(error, ValidatorError::ExtraOutputAudit(99));
        Ok(())
    }

    #[test]
    fn rejects_duplicate_output_audit() -> Result<(), Box<dyn std::error::Error>> {
        let mut artifact = valid_posix()?;
        let duplicate = artifact
            .timing
            .output_audits
            .first()
            .cloned()
            .ok_or("audit")?;
        artifact.timing.output_audits.push(duplicate);
        artifact.digest = digest_canonical(&artifact)?;
        let error = validate_artifact(&artifact).err().ok_or("duplicate audit")?;
        assert_eq!(error, ValidatorError::DuplicateOutputAudit(1));
        Ok(())
    }

    #[test]
    fn rejects_raw_output_audit_mismatch() -> Result<(), Box<dyn std::error::Error>> {
        let mut artifact = valid_posix()?;
        let audit = artifact
            .timing
            .output_audits
            .first_mut()
            .ok_or("audit")?;
        audit.raw_bytes_b64 = BASE64.encode(b"tampered-raw");
        artifact.digest = digest_canonical(&artifact)?;
        let error = validate_artifact(&artifact).err().ok_or("raw mismatch")?;
        assert_eq!(error, ValidatorError::OutputAuditMismatch(1));
        Ok(())
    }

    fn renumber(artifact: &mut TranscriptArtifact) -> Result<(), Box<dyn std::error::Error>> {
        for (index, event) in artifact.canonical.events.iter_mut().enumerate() {
            let seq = u32::try_from(index).map_err(|_| "seq")?;
            match event {
                CanonicalEvent::Spawn { seq: value, .. }
                | CanonicalEvent::Input { seq: value, .. }
                | CanonicalEvent::Output { seq: value, .. }
                | CanonicalEvent::Snapshot { seq: value, .. }
                | CanonicalEvent::Resize { seq: value, .. }
                | CanonicalEvent::ResizeStorm { seq: value, .. }
                | CanonicalEvent::Exit { seq: value, .. } => *value = seq,
            }
        }
        let mut next_output = 0u32;
        for event in &artifact.canonical.events {
            if let CanonicalEvent::Output { seq, .. } = event {
                if let Some(audit) = artifact.timing.output_audits.get_mut(next_output as usize) {
                    audit.event_seq = *seq;
                }
                next_output += 1;
            }
        }
        Ok(())
    }

    #[test]
    fn rejects_zero_geometry_on_artifact_and_events() -> Result<(), Box<dyn std::error::Error>> {
        let mut artifact = valid_posix()?;
        artifact.geometry.cols = 0;
        artifact.digest = digest_canonical(&artifact)?;
        let error = validate_artifact(&artifact).err().ok_or("artifact geometry")?;
        assert_eq!(error, ValidatorError::ZeroGeometry { cols: 0, rows: 24 });

        let mut artifact = valid_posix()?;
        artifact.canonical.events.insert(
            1,
            CanonicalEvent::Snapshot {
                seq: 1,
                cols: 0,
                rows: 24,
                cursor: [0, 0],
                lines: vec!["x".to_owned()],
            },
        );
        renumber(&mut artifact)?;
        artifact.digest = digest_canonical(&artifact)?;
        let error = validate_artifact(&artifact).err().ok_or("snapshot geometry")?;
        assert_eq!(error, ValidatorError::ZeroGeometry { cols: 0, rows: 24 });

        let mut artifact = valid_posix()?;
        artifact.canonical.events.insert(
            1,
            CanonicalEvent::Resize {
                seq: 1,
                cols: 0,
                rows: 12,
            },
        );
        renumber(&mut artifact)?;
        artifact.digest = digest_canonical(&artifact)?;
        let error = validate_artifact(&artifact).err().ok_or("resize geometry")?;
        assert_eq!(error, ValidatorError::ZeroGeometry { cols: 0, rows: 12 });

        let mut artifact = valid_posix()?;
        artifact.canonical.events.insert(
            1,
            CanonicalEvent::ResizeStorm {
                seq: 1,
                sizes: vec![Geometry { cols: 80, rows: 0 }],
            },
        );
        renumber(&mut artifact)?;
        artifact.digest = digest_canonical(&artifact)?;
        let error = validate_artifact(&artifact).err().ok_or("storm geometry")?;
        assert_eq!(error, ValidatorError::ZeroGeometry { cols: 80, rows: 0 });
        Ok(())
    }

    #[test]
    fn rejects_qemu_tier_n() -> Result<(), Box<dyn std::error::Error>> {
        let mut artifact = valid_posix()?;
        artifact.driver = DriverDescriptor {
            kind: DriverKind::QemuUserSmoke,
        };
        artifact.mode = TranscriptMode::Contingency;
        artifact.claims = vec![ClaimClass::Execution];
        artifact.row.tier = RowTier::TierN;
        artifact.row.runner_image = Some("image".to_owned());
        artifact.digest = digest_canonical(&artifact)?;
        let error = validate_artifact(&artifact).err().ok_or("qemu tier-n")?;
        assert_eq!(error, ValidatorError::QemuTierN);
        Ok(())
    }

    #[test]
    fn rejects_driver_row_mismatches() -> Result<(), Box<dyn std::error::Error>> {
        let mut artifact = valid_posix()?;
        artifact.row.id = RowId::WindowsX64;
        artifact.driver = DriverDescriptor {
            kind: DriverKind::PosixPty,
        };
        artifact.digest = digest_canonical(&artifact)?;
        let error = validate_artifact(&artifact).err().ok_or("windows")?;
        assert_eq!(
            error,
            ValidatorError::DriverRowMismatch {
                row: RowId::WindowsX64,
                driver: DriverKind::PosixPty,
            }
        );

        let mut artifact = valid_posix()?;
        artifact.row.tier = RowTier::TierN;
        artifact.row.id = RowId::DarwinArm64;
        artifact.row.runner_image = Some("image".to_owned());
        artifact.driver = DriverDescriptor {
            kind: DriverKind::ConPty,
        };
        artifact.digest = digest_canonical(&artifact)?;
        let error = validate_artifact(&artifact).err().ok_or("darwin")?;
        assert_eq!(
            error,
            ValidatorError::DriverRowMismatch {
                row: RowId::DarwinArm64,
                driver: DriverKind::ConPty,
            }
        );
        Ok(())
    }
}
