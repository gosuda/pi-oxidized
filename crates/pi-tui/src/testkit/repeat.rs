use std::fmt;

use super::transcript::{TranscriptArtifact, digest_canonical, encode_canonical};
use super::validate::validate_artifact;

/// Failure while repeating a transcript producer.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RepeatError {
    /// `run_k` requires at least three repetitions.
    KTooSmall { k: usize },
    /// Canonical bytes or digests diverged across repetitions.
    Divergence {
        first_divergent_seq: u32,
        left_digest: String,
        right_digest: String,
    },
    /// Producer failed for a specific repetition.
    Producer { iteration: usize, message: String },
    /// Stored digest did not match the recomputed canonical digest.
    DigestMismatch {
        iteration: usize,
        stored: String,
        computed: String,
    },
}

impl fmt::Display for RepeatError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::KTooSmall { k } => write!(formatter, "run_k requires k >= 3, got {k}"),
            Self::Divergence {
                first_divergent_seq,
                left_digest,
                right_digest,
            } => write!(
                formatter,
                "canonical divergence at seq {first_divergent_seq}: {left_digest} != {right_digest}"
            ),
            Self::Producer { iteration, message } => {
                write!(
                    formatter,
                    "producer failed on iteration {iteration}: {message}"
                )
            }
            Self::DigestMismatch {
                iteration,
                stored,
                computed,
            } => write!(
                formatter,
                "stored digest mismatch on iteration {iteration}: stored={stored} computed={computed}"
            ),
        }
    }
}

impl std::error::Error for RepeatError {}

/// Successful k-run comparison result.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepeatReport {
    pub k: usize,
    pub digest: String,
    pub canonical_bytes: Vec<u8>,
}

/// Runs `producer` exactly `k` times (`k >= 3`) and requires byte-identical
/// canonical encodings and equal digests. Returns the first divergent sequence.
pub fn run_k<F, E>(k: usize, mut producer: F) -> Result<RepeatReport, RepeatError>
where
    F: FnMut(usize) -> Result<TranscriptArtifact, E>,
    E: fmt::Display,
{
    if k < 3 {
        return Err(RepeatError::KTooSmall { k });
    }

    let first = produce_checked(0, &mut producer)?;
    let first_bytes = encode_canonical(&first).map_err(|error| RepeatError::Producer {
        iteration: 0,
        message: error.to_string(),
    })?;
    let first_digest = digest_canonical(&first).map_err(|error| RepeatError::Producer {
        iteration: 0,
        message: error.to_string(),
    })?;

    for iteration in 1..k {
        let artifact = produce_checked(iteration, &mut producer)?;
        let bytes = encode_canonical(&artifact).map_err(|error| RepeatError::Producer {
            iteration,
            message: error.to_string(),
        })?;
        let digest = digest_canonical(&artifact).map_err(|error| RepeatError::Producer {
            iteration,
            message: error.to_string(),
        })?;
        if bytes != first_bytes || digest != first_digest {
            return Err(RepeatError::Divergence {
                first_divergent_seq: first_divergent_seq(&first, &artifact),
                left_digest: first_digest,
                right_digest: digest,
            });
        }
    }

    Ok(RepeatReport {
        k,
        digest: first_digest,
        canonical_bytes: first_bytes,
    })
}

fn produce_checked<F, E>(
    iteration: usize,
    producer: &mut F,
) -> Result<TranscriptArtifact, RepeatError>
where
    F: FnMut(usize) -> Result<TranscriptArtifact, E>,
    E: fmt::Display,
{
    let artifact = producer(iteration).map_err(|error| RepeatError::Producer {
        iteration,
        message: error.to_string(),
    })?;
    let computed = digest_canonical(&artifact).map_err(|error| RepeatError::Producer {
        iteration,
        message: error.to_string(),
    })?;
    if computed != artifact.digest {
        return Err(RepeatError::DigestMismatch {
            iteration,
            stored: artifact.digest,
            computed,
        });
    }
    validate_artifact(&artifact).map_err(|error| RepeatError::Producer {
        iteration,
        message: error.to_string(),
    })?;
    Ok(artifact)
}

fn first_divergent_seq(left: &TranscriptArtifact, right: &TranscriptArtifact) -> u32 {
    let left_events = &left.canonical.events;
    let right_events = &right.canonical.events;
    let shared = left_events.len().min(right_events.len());
    for index in 0..shared {
        if left_events[index] != right_events[index] {
            return left_events[index].seq();
        }
    }
    if left_events.len() != right_events.len() {
        return u32::try_from(shared).unwrap_or(u32::MAX);
    }
    if left.canonical.normalizations != right.canonical.normalizations
        || left.schema != right.schema
        || left.scenario != right.scenario
        || left.geometry != right.geometry
        || left.capability_profile != right.capability_profile
        || left.driver != right.driver
        || left.mode != right.mode
        || left.claims != right.claims
    {
        return 0;
    }
    0
}

#[cfg(test)]
mod tests {
    use super::super::transcript::{
        CapabilityProfile, ClaimClass, DriverKind, Geometry, NormalizationContext, RowId, RowTier,
        RunnerRow, Scenario, TimingEnvelope, TranscriptMode, TranscriptRecorder, TranscriptSpec,
    };
    use super::*;

    fn make_artifact(tag: &[u8]) -> Result<TranscriptArtifact, String> {
        let mut recorder = TranscriptRecorder::new(TranscriptSpec {
            scenario: Scenario::FixtureStreamSettle,
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
        recorder
            .spawn(vec!["pi".to_owned()], &NormalizationContext::default())
            .map_err(|error| error.to_string())?;
        recorder
            .output(&[tag], &NormalizationContext::default())
            .map_err(|error| error.to_string())?;
        recorder
            .exit(Some(0), true)
            .map_err(|error| error.to_string())?;
        recorder.finish().map_err(|error| error.to_string())
    }

    #[test]
    fn rejects_k_below_three() -> Result<(), Box<dyn std::error::Error>> {
        let error = run_k(2, |_| make_artifact(b"ok")).err().ok_or("k")?;
        assert_eq!(error, RepeatError::KTooSmall { k: 2 });
        Ok(())
    }

    #[test]
    fn accepts_identical_runs() -> Result<(), Box<dyn std::error::Error>> {
        let report = run_k(3, |_| make_artifact(b"stable"))?;
        assert_eq!(report.k, 3);
        assert!(!report.digest.is_empty());
        assert!(!report.canonical_bytes.is_empty());
        Ok(())
    }

    #[test]
    fn reports_first_divergent_sequence() -> Result<(), Box<dyn std::error::Error>> {
        let error = run_k(3, |iteration| {
            make_artifact(if iteration == 0 { b"same" } else { b"other" })
        })
        .err()
        .ok_or("divergence")?;
        match error {
            RepeatError::Divergence {
                first_divergent_seq,
                ..
            } => assert_eq!(first_divergent_seq, 1),
            other => return Err(format!("unexpected error: {other:?}").into()),
        }
        Ok(())
    }

    #[test]
    fn reports_first_divergent_event_before_length_difference()
    -> Result<(), Box<dyn std::error::Error>> {
        let error = run_k(3, |iteration| {
            if iteration == 0 {
                return make_artifact(b"stable");
            }
            let mut recorder = TranscriptRecorder::new(TranscriptSpec {
                scenario: Scenario::FixtureStreamSettle,
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
            recorder
                .spawn(vec!["pi".to_owned()], &NormalizationContext::default())
                .map_err(|error| error.to_string())?;
            recorder
                .exit(Some(0), true)
                .map_err(|error| error.to_string())?;
            recorder.finish().map_err(|error| error.to_string())
        })
        .err()
        .ok_or("length")?;
        match error {
            RepeatError::Divergence {
                first_divergent_seq,
                ..
            } => assert_eq!(first_divergent_seq, 1),
            other => return Err(format!("unexpected error: {other:?}").into()),
        }
        Ok(())
    }

    #[test]
    fn rejects_forged_equal_stored_digests() -> Result<(), Box<dyn std::error::Error>> {
        let error = run_k(3, |iteration| {
            let mut artifact = make_artifact(if iteration == 0 { b"alpha" } else { b"beta" })?;
            artifact.digest = "sha256:forged-equal-across-runs".to_owned();
            Ok::<_, String>(artifact)
        })
        .err()
        .ok_or("forged")?;
        assert!(matches!(error, RepeatError::DigestMismatch { .. }));
        Ok(())
    }
}
