use std::collections::BTreeSet;

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use serde_json::Value;
use thiserror::Error;

use super::transcript::{
    ClaimClass, CanonicalEvent, DriverKind, EventKind, NORMALIZATION_TABLE_V1, NormalizationKind,
    RowTier, SCHEMA_ID, TranscriptArtifact, TranscriptMode, detected_volatile_kinds,
    digest_canonical,
};

const TIMING_LIKE_FIELDS: &[&str] = &[
    "wallMs",
    "chunkLog",
    "settleWindowsMs",
    "abortCeiling",
    "rawLogB64",
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
    }

    let allowed: BTreeSet<_> = NORMALIZATION_TABLE_V1.iter().copied().collect();
    for entry in &artifact.canonical.normalizations {
        if !allowed.contains(&entry.kind) {
            return Err(ValidatorError::UnknownNormalization(entry.kind));
        }
    }

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

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::transcript::{
        CapabilityProfile, DriverDescriptor, Geometry, NormalizationContext, NormalizationEntry,
        RowId, RunnerRow, Scenario, TimingEnvelope, TranscriptRecorder,
    };

    fn valid_posix() -> TranscriptArtifact {
        let mut recorder = TranscriptRecorder::new(
            Scenario::ColdStart,
            RunnerRow {
                tier: RowTier::Local,
                id: RowId::GnuX64,
                runner_image: None,
            },
            Geometry { cols: 80, rows: 24 },
            CapabilityProfile::Xterm256Color,
            DriverKind::PosixPty,
            TranscriptMode::Standard,
            vec![ClaimClass::Execution, ClaimClass::Render],
            TimingEnvelope::default(),
        );
        recorder.spawn(vec!["pi".to_owned()]).expect("spawn");
        recorder
            .output(
                &[b"ready"],
                &NormalizationContext::default(),
            )
            .expect("output");
        recorder.exit(Some(0), true).expect("exit");
        recorder.finish().expect("finish")
    }

    fn as_value(artifact: &TranscriptArtifact) -> Value {
        serde_json::to_value(artifact).expect("serialize")
    }

    #[test]
    fn accepts_valid_artifact() {
        let artifact = valid_posix();
        validate_artifact(&artifact).expect("valid");
        validate_value(&as_value(&artifact)).expect("valid json");
    }

    #[test]
    fn rejects_unknown_fields() {
        let mut value = as_value(&valid_posix());
        value
            .as_object_mut()
            .expect("object")
            .insert("extra".to_owned(), Value::Bool(true));
        let error = validate_value(&value).expect_err("unknown field");
        assert!(matches!(error, ValidatorError::UnknownField(_)));
    }

    #[test]
    fn rejects_wrong_schema() {
        let mut artifact = valid_posix();
        artifact.schema = "pi-tui-transcript/0".to_owned();
        artifact.digest = digest_canonical(&artifact).expect("digest");
        let error = validate_artifact(&artifact).expect_err("wrong schema");
        assert_eq!(error, ValidatorError::WrongSchema("pi-tui-transcript/0".to_owned()));
    }

    #[test]
    fn rejects_sequence_gap_or_order() {
        let mut artifact = valid_posix();
        if let CanonicalEvent::Exit { seq, .. } = &mut artifact.canonical.events[2] {
            *seq = 9;
        }
        artifact.digest = digest_canonical(&artifact).expect("digest");
        let error = validate_artifact(&artifact).expect_err("gap");
        assert_eq!(error, ValidatorError::SequenceGapOrOrder(2));
    }

    #[test]
    fn rejects_missing_spawn_or_exit() {
        let mut artifact = valid_posix();
        artifact.canonical.events.pop();
        artifact.digest = digest_canonical(&artifact).expect("digest");
        let error = validate_artifact(&artifact).expect_err("missing exit");
        assert_eq!(error, ValidatorError::MissingSpawnOrExit);
    }

    #[test]
    fn rejects_digest_mismatch() {
        let mut artifact = valid_posix();
        artifact.digest = "sha256:deadbeef".to_owned();
        let error = validate_artifact(&artifact).expect_err("digest");
        assert_eq!(error, ValidatorError::DigestMismatch);
    }

    #[test]
    fn rejects_timing_like_canonical_fields() {
        let mut value = as_value(&valid_posix());
        value
            .pointer_mut("/canonical")
            .and_then(Value::as_object_mut)
            .expect("canonical")
            .insert("wallMs".to_owned(), Value::from(12));
        let error = validate_value(&value).expect_err("timing field");
        assert_eq!(error, ValidatorError::TimingLikeCanonicalField("wallMs".to_owned()));
    }

    #[test]
    fn rejects_canonical_like_timing_fields() {
        let mut value = as_value(&valid_posix());
        value
            .pointer_mut("/timing")
            .and_then(Value::as_object_mut)
            .expect("timing")
            .insert("events".to_owned(), Value::Array(Vec::new()));
        let error = validate_value(&value).expect_err("canonical field");
        assert_eq!(error, ValidatorError::CanonicalLikeTimingField("events".to_owned()));
    }

    #[test]
    fn rejects_unenumerated_detected_volatile_data() {
        let mut artifact = valid_posix();
        artifact.timing.raw_log_b64 = BASE64.encode(
            b"session 550e8400-e29b-41d4-a716-446655440000 at 2024-01-02T03:04:05Z",
        );
        artifact.canonical.normalizations.clear();
        artifact.digest = digest_canonical(&artifact).expect("digest");
        let error = validate_artifact(&artifact).expect_err("volatile");
        assert!(matches!(error, ValidatorError::UnenumeratedVolatile(_)));
    }

    #[test]
    fn rejects_tier_n_missing_runner_image() {
        let mut artifact = valid_posix();
        artifact.row.tier = RowTier::TierN;
        artifact.row.runner_image = None;
        artifact.digest = digest_canonical(&artifact).expect("digest");
        let error = validate_artifact(&artifact).expect_err("tier-n");
        assert_eq!(error, ValidatorError::TierNMissingRunnerImage);
    }

    #[test]
    fn rejects_qemu_non_contingency_mode() {
        let mut artifact = valid_posix();
        artifact.driver = DriverDescriptor {
            kind: DriverKind::QemuUserSmoke,
        };
        artifact.mode = TranscriptMode::Standard;
        artifact.claims = vec![ClaimClass::Execution];
        artifact.digest = digest_canonical(&artifact).expect("digest");
        let error = validate_artifact(&artifact).expect_err("mode");
        assert_eq!(error, ValidatorError::QemuNonContingencyMode);
    }

    #[test]
    fn rejects_qemu_claims_outside_execution_protocol() {
        let mut artifact = valid_posix();
        artifact.driver = DriverDescriptor {
            kind: DriverKind::QemuUserSmoke,
        };
        artifact.mode = TranscriptMode::Contingency;
        artifact.claims = vec![ClaimClass::Execution, ClaimClass::Render];
        artifact.digest = digest_canonical(&artifact).expect("digest");
        let error = validate_artifact(&artifact).expect_err("claim");
        assert_eq!(error, ValidatorError::QemuClaimOutsideAllowed(ClaimClass::Render));
    }

    #[test]
    fn rejects_qemu_snapshot_event() {
        let mut recorder = TranscriptRecorder::new(
            Scenario::ColdStart,
            RunnerRow {
                tier: RowTier::Local,
                id: RowId::GnuX64,
                runner_image: None,
            },
            Geometry { cols: 80, rows: 24 },
            CapabilityProfile::Xterm256Color,
            DriverKind::PosixPty,
            TranscriptMode::Standard,
            vec![ClaimClass::Execution],
            TimingEnvelope::default(),
        );
        recorder.spawn(vec!["pi".to_owned()]).expect("spawn");
        recorder
            .snapshot(80, 24, [0, 0], vec!["frame".to_owned()])
            .expect("snapshot");
        recorder.exit(Some(0), true).expect("exit");
        let mut artifact = recorder.finish().expect("finish");
        artifact.driver = DriverDescriptor {
            kind: DriverKind::QemuUserSmoke,
        };
        artifact.mode = TranscriptMode::Contingency;
        artifact.claims = vec![ClaimClass::Execution];
        artifact.digest = digest_canonical(&artifact).expect("digest");
        let error = validate_artifact(&artifact).expect_err("snapshot");
        assert_eq!(error, ValidatorError::QemuSnapshotOrRenderClaim);
    }

    #[test]
    fn rejects_qemu_snapshot_claim() {
        let mut artifact = valid_posix();
        artifact.driver = DriverDescriptor {
            kind: DriverKind::QemuUserSmoke,
        };
        artifact.mode = TranscriptMode::Contingency;
        artifact.claims = vec![ClaimClass::Snapshot];
        // Remove any render-class events so only the claim is under test.
        artifact.canonical.events.retain(|event| {
            !matches!(
                event.kind(),
                EventKind::Snapshot | EventKind::Resize | EventKind::ResizeStorm
            )
        });
        for (index, event) in artifact.canonical.events.iter_mut().enumerate() {
            let seq = u32::try_from(index).expect("seq");
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
        artifact.digest = digest_canonical(&artifact).expect("digest");
        let error = validate_artifact(&artifact).expect_err("snapshot claim");
        assert_eq!(error, ValidatorError::QemuClaimOutsideAllowed(ClaimClass::Snapshot));
    }

    #[test]
    fn qemu_builder_restrictions_survive_validation() {
        let mut recorder = TranscriptRecorder::new(
            Scenario::ColdStart,
            RunnerRow {
                tier: RowTier::Local,
                id: RowId::GnuX64,
                runner_image: None,
            },
            Geometry { cols: 80, rows: 24 },
            CapabilityProfile::Dumb,
            DriverKind::QemuUserSmoke,
            TranscriptMode::Standard,
            vec![
                ClaimClass::Execution,
                ClaimClass::Protocol,
                ClaimClass::Render,
                ClaimClass::Snapshot,
            ],
            TimingEnvelope::default(),
        );
        recorder.spawn(vec!["qemu".to_owned()]).expect("spawn");
        assert!(!recorder
            .snapshot(80, 24, [0, 0], vec!["nope".to_owned()])
            .expect("blocked"));
        recorder.exit(Some(0), true).expect("exit");
        let artifact = recorder.finish().expect("finish");
        validate_artifact(&artifact).expect("builder-constrained qemu artifact");
        assert_eq!(artifact.mode, TranscriptMode::Contingency);
        assert!(artifact
            .claims
            .iter()
            .all(|claim| matches!(claim, ClaimClass::Execution | ClaimClass::Protocol)));
    }

    #[test]
    fn enumerated_volatile_data_is_accepted() {
        let mut artifact = valid_posix();
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
        artifact.digest = digest_canonical(&artifact).expect("digest");
        validate_artifact(&artifact).expect("enumerated");
    }
}
