use std::fmt;

use super::transcript::{TranscriptArtifact, encode_canonical};

/// Failure while repeating a transcript producer.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RepeatError {
    /// `run_k` requires at least three repetitions.
    KTooSmall {
        k: usize,
    },
    /// Canonical bytes or digests diverged across repetitions.
    Divergence {
        first_divergent_seq: u32,
        left_digest: String,
        right_digest: String,
    },
    /// Producer failed for a specific repetition.
    Producer {
        iteration: usize,
        message: String,
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
                write!(formatter, "producer failed on iteration {iteration}: {message}")
            }
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

    let first = producer(0).map_err(|error| RepeatError::Producer {
        iteration: 0,
        message: error.to_string(),
    })?;
    let first_bytes = encode_canonical(&first).map_err(|error| RepeatError::Producer {
        iteration: 0,
        message: error.to_string(),
    })?;
    let first_digest = first.digest.clone();

    for iteration in 1..k {
        let artifact = producer(iteration).map_err(|error| RepeatError::Producer {
            iteration,
            message: error.to_string(),
        })?;
        let bytes = encode_canonical(&artifact).map_err(|error| RepeatError::Producer {
            iteration,
            message: error.to_string(),
        })?;
        if bytes != first_bytes || artifact.digest != first_digest {
            return Err(RepeatError::Divergence {
                first_divergent_seq: first_divergent_seq(&first, &artifact),
                left_digest: first_digest,
                right_digest: artifact.digest,
            });
        }
    }

    Ok(RepeatReport {
        k,
        digest: first_digest,
        canonical_bytes: first_bytes,
    })
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
    use super::*;
    use super::super::transcript::{
        CapabilityProfile, ClaimClass, DriverKind, Geometry, NormalizationContext, RowId, RowTier,
        RunnerRow, Scenario, TimingEnvelope, TranscriptMode, TranscriptRecorder,
    };

    fn make_artifact(tag: &[u8]) -> TranscriptArtifact {
        let mut recorder = TranscriptRecorder::new(
            Scenario::FixtureStreamSettle,
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
            .output(&[tag], &NormalizationContext::default())
            .expect("output");
        recorder.exit(Some(0), true).expect("exit");
        recorder.finish().expect("finish")
    }

    #[test]
    fn rejects_k_below_three() {
        let error = run_k(2, |_| Ok::<_, &'static str>(make_artifact(b"ok"))).expect_err("k");
        assert_eq!(error, RepeatError::KTooSmall { k: 2 });
    }

    #[test]
    fn accepts_identical_runs() {
        let report = run_k(3, |_| Ok::<_, &'static str>(make_artifact(b"stable"))).expect("ok");
        assert_eq!(report.k, 3);
        assert!(!report.digest.is_empty());
        assert!(!report.canonical_bytes.is_empty());
    }

    #[test]
    fn reports_first_divergent_sequence() {
        let error = run_k(3, |iteration| {
            Ok::<_, &'static str>(make_artifact(if iteration == 0 { b"same" } else { b"other" }))
        })
        .expect_err("divergence");
        match error {
            RepeatError::Divergence {
                first_divergent_seq, ..
            } => assert_eq!(first_divergent_seq, 1),
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn reports_length_divergence_at_shared_len() {
        let error = run_k(3, |iteration| {
            if iteration == 0 {
                Ok(make_artifact(b"stable"))
            } else {
                let mut recorder = TranscriptRecorder::new(
                    Scenario::FixtureStreamSettle,
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
                recorder.exit(Some(0), true).expect("exit");
                Ok(recorder.finish().expect("finish"))
            }
        })
        .expect_err("length");
        match error {
            RepeatError::Divergence {
                first_divergent_seq, ..
            } => assert_eq!(first_divergent_seq, 2),
            other => panic!("unexpected error: {other:?}"),
        }
    }
}
