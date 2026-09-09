//! Typed outcomes and expected failures for the harness boundary.
//!
//! Expected operation failures are represented by [`HarnessError`] values. The
//! separate [`HarnessFault`] type is reserved for infrastructure failures that
//! abort a lane and are reported as fault events by the runtime.

use std::error::Error;

use serde::{Deserialize, Serialize};
use std::sync::Arc;

use crate::message::AgentMessage;
use crate::session::{
    EntryId, LaneName, ModelIdentity, OperationId, OperationKind, OperationResultRecord,
};

/// Shared ownership of a sealing fault that stays downcast-visible.
///
/// `Arc<HarnessFault>` cannot serve as a `#[source]` field directly: the
/// standard library implements `Error` for the `Arc` itself, so the chain
/// link would be the wrapper rather than the fault. This wrapper derefs to
/// the fault without implementing `Error`, steering the derived `source`
/// to the shared fault object.
#[derive(Clone, Debug)]
pub struct SharedFault(pub(crate) Arc<HarnessFault>);

impl std::ops::Deref for SharedFault {
    type Target = HarnessFault;
    fn deref(&self) -> &HarnessFault {
        &self.0
    }
}

impl PartialEq for SharedFault {
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}
impl serde::Serialize for SharedFault {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.0.serialize(serializer)
    }
}

impl<'de> serde::Deserialize<'de> for SharedFault {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        HarnessFault::deserialize(deserializer).map(|fault| SharedFault(Arc::new(fault)))
    }
}

/// One tagged error for each expected harness failure class.
///
/// On the wire the variant is selected by `_tag`, whose value is the exact
/// variant name (for example `"LaneBusy"`), and struct-variant fields are
/// serialized in camelCase (`operationId`); the Rust field names below are
/// `snake_case`.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize, thiserror::Error)]
#[serde(tag = "_tag", rename_all_fields = "camelCase")]
pub enum HarnessError {
    /// A lane already owns an operation.
    #[error("{message}")]
    LaneBusy {
        /// Lane holding the conflicting operation.
        lane: LaneName,
        /// Durable id of the operation the lane already owns.
        operation_id: OperationId,
        /// Operation family of that owning operation.
        operation_kind: OperationKind,
        /// Display text describing the conflict.
        message: String,
    },
    /// A drive or abort request names an operation that is not current.
    #[error("{message}")]
    OperationMismatch {
        /// Lane the request targeted.
        lane: LaneName,
        /// Operation the caller named in the request.
        expected_operation_id: OperationId,
        /// Operation currently open on the lane, if any.
        current_operation_id: Option<OperationId>,
        /// Most recently settled operation, if any; lets callers tell a stale
        /// target from an operation that already finished.
        last_operation_id: Option<OperationId>,
        /// Display text describing the mismatch.
        message: String,
    },
    /// No run is active on the lane.
    #[error("{message}")]
    NoActiveRun {
        /// Lane that was queried.
        lane: LaneName,
        /// Display text describing the failure.
        message: String,
    },
    /// No operation is active on the lane.
    #[error("{message}")]
    NoActiveOperation {
        /// Lane that was queried.
        lane: LaneName,
        /// Display text describing the failure.
        message: String,
    },
    /// The lane has no suspended operation to resume.
    #[error("{message}")]
    NothingToResume {
        /// Lane that was asked to resume.
        lane: LaneName,
        /// Display text describing the failure.
        message: String,
    },
    /// The lane has no eligible work to compact.
    #[error("{message}")]
    NothingToCompact {
        /// Lane that was asked to compact.
        lane: LaneName,
        /// Display text describing the failure.
        message: String,
    },
    /// A message or queue input is invalid for the lane.
    #[error("{message}")]
    InvalidMessage {
        /// Lane the input was submitted to.
        lane: LaneName,
        /// Short validation reason, separate from the display text.
        reason: String,
        /// Display text describing the failure.
        message: String,
    },
    /// A navigation request is invalid for the lane.
    #[error("{message}")]
    InvalidNavigation {
        /// Lane the navigation targeted.
        lane: LaneName,
        /// Short validation reason, separate from the display text.
        reason: String,
        /// Display text describing the failure.
        message: String,
    },
    /// A requested skill is not configured.
    #[error("{message}")]
    UnknownSkill {
        /// Requested skill name.
        name: String,
        /// Display text describing the failure.
        message: String,
    },
    /// A requested prompt template is not configured.
    #[error("{message}")]
    UnknownTemplate {
        /// Requested template name.
        name: String,
        /// Display text describing the failure.
        message: String,
    },
    /// A requested tree target does not exist.
    #[error("{message}")]
    UnknownTarget {
        /// Entry id that was not found.
        target_id: EntryId,
        /// Display text describing the failure.
        message: String,
    },
    /// A lane name or lane configuration is invalid.
    #[error("{message}")]
    InvalidLane {
        /// Lane that was rejected.
        lane: LaneName,
        /// Short validation reason, separate from the display text.
        reason: String,
        /// Display text describing the failure.
        message: String,
    },
    /// A retry policy contains an unsafe or otherwise invalid numeric value.
    #[error("{message}")]
    InvalidRetryPolicy {
        /// Rejected retry count as supplied.
        max_retries: u64,
        /// Rejected base delay in milliseconds as supplied.
        base_delay_ms: u64,
        /// Display text describing the failure.
        message: String,
    },
    /// The harness is sealed by an infrastructure fault; retains the fault
    /// so every rejection chain stays inspectable to the sealing cause.
    #[error("{message}")]
    FaultSealed {
        /// Display text carried from the sealing fault.
        message: String,
        /// The sealing fault, shared with every rejection it causes.
        #[source]
        fault: SharedFault,
    },
    /// The harness has been closed.
    #[error("{message}")]
    Closed {
        /// Display text carried from the close fault.
        message: String,
    },
}

/// Unexpected infrastructure failure.
///
/// This is not part of any public method result alias. The runtime reports it
/// as a `fault` event and aborts the affected lane.
#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct HarnessFault {
    /// Human-readable fault description.
    pub message: String,
    /// Underlying infrastructure failure.
    #[source]
    pub cause: Box<dyn Error + Send + Sync>,
}

impl PartialEq for HarnessFault {
    fn eq(&self, other: &Self) -> bool {
        self.message == other.message && self.cause.to_string() == other.cause.to_string()
    }
}

impl serde::Serialize for HarnessFault {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut fault = serializer.serialize_struct("HarnessFault", 2)?;
        fault.serialize_field("message", &self.message)?;
        fault.serialize_field("cause", &self.cause.to_string())?;
        fault.end()
    }
}

impl<'de> serde::Deserialize<'de> for HarnessFault {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(serde::Deserialize)]
        struct FaultWire {
            message: String,
            cause: String,
        }
        let wire = FaultWire::deserialize(deserializer)?;
        // Wire form preserves the cause text only; typed causes never cross
        // the boundary through this shape.
        Ok(HarnessFault {
            message: wire.message,
            cause: Box::new(std::io::Error::other(wire.cause)),
        })
    }
}

/// A run that completed normally or was suspended by deferred generation.
pub type RunResult = Result<RunOutcome, HarnessError>;
/// A compaction result and the run result, when compaction resumed a run.
pub type CompactionResult = Result<CompactionOutcome, HarnessError>;
/// A navigation result and the run result, when navigation resumed a run.
pub type NavigationResult = Result<NavigationOutcome, HarnessError>;
/// A resumed run result.
pub type ResumeResult = Result<RunOutcome, HarnessError>;
/// The reserved entry id for an enqueued input.
pub type QueueResult = Result<EntryId, HarnessError>;
/// The result of cancelling a queued item.
pub type CancelQueuedResult = Result<CancelQueuedKind, HarnessError>;
/// The result of aborting the current operation.
pub type AbortResult = Result<AbortOutcome, HarnessError>;
/// The usage id assigned to a recorded usage row.
pub type RecordUsageResult = Result<crate::session::UsageId, HarnessError>;
/// The result of one low-level drive step.
pub type DriveResult = Result<DriveOutcome, HarnessError>;
/// The result of requesting an abort for a specific operation.
pub type AbortRequestResult = Result<AbortRequestOutcome, HarnessError>;
/// The result of admitting an operation before driving it.
pub type OperationAdmissionResult = Result<OperationAdmission, HarnessError>;

/// High-level run outcome.
#[derive(Clone, Debug)]
pub enum RunOutcome {
    /// The operation reached a durable terminal record.
    Settled(OperationResultRecord),
    /// The operation is waiting for a deferred provider response.
    Suspended(SuspendedRun),
}

/// A run suspended while a provider-owned deferred response is pending.
#[derive(Clone, Debug)]
pub struct SuspendedRun {
    /// Durable operation identifier.
    pub operation_id: OperationId,
    /// Provider handle used to poll the deferred response.
    pub deferred: pi_ai::DeferredHandle,
}

/// Compaction result plus a resumed run, when applicable.
#[derive(Clone, Debug)]
pub struct CompactionOutcome {
    /// Durable compaction result.
    pub compaction: OperationResultRecord,
    /// Run resumed after compaction, if the operation boundary requested it.
    pub run: Option<RunOutcome>,
}

/// Navigation result plus a resumed run, when applicable.
#[derive(Clone, Debug)]
pub struct NavigationOutcome {
    /// Durable navigation result.
    pub navigation: OperationResultRecord,
    /// Run resumed after navigation, if the operation boundary requested it.
    pub run: Option<RunOutcome>,
}

/// Outcome of aborting the active operation.
#[derive(Clone, Debug)]
pub struct AbortOutcome {
    /// Aborted operation identifier.
    pub operation_id: OperationId,
    /// Steering messages consumed by the abort.
    pub steer: Vec<AgentMessage>,
    /// Follow-up messages consumed by the abort.
    pub follow_up: Vec<AgentMessage>,
}

/// Outcome of requesting an abort without necessarily driving it to completion.
#[derive(Clone, Debug)]
pub struct AbortRequestOutcome {
    /// Operation requested for abort.
    pub operation_id: OperationId,
    /// Whether this request changed the durable control state.
    pub newly_requested: bool,
    /// Steering messages consumed by the abort request.
    pub steer: Vec<AgentMessage>,
    /// Follow-up messages consumed by the abort request.
    pub follow_up: Vec<AgentMessage>,
}

/// Result of cancelling a queued entry.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CancelQueuedKind {
    /// The entry was cancelled before consumption.
    Cancelled,
    /// The entry was already consumed.
    AlreadyConsumed,
    /// The entry was not found.
    NotFound,
}

/// Outcome of a low-level drive invocation.
#[derive(Clone, Debug)]
pub enum DriveOutcome {
    /// The operation reached a durable terminal record.
    Settled(OperationResultRecord),
    /// The operation is waiting until a retry timestamp.
    WaitingRetry {
        /// Durable operation identifier.
        operation_id: OperationId,
        /// Unix timestamp in milliseconds before the next attempt.
        not_before: i64,
    },
    /// The operation is waiting for a deferred provider response.
    WaitingDeferred {
        /// Durable operation identifier.
        operation_id: OperationId,
        /// Provider handle used to poll the deferred response.
        deferred: pi_ai::DeferredHandle,
    },
}

/// Admission record returned before a drive begins.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OperationAdmission {
    /// Durable operation identifier.
    pub operation_id: OperationId,
    /// Operation family admitted.
    pub kind: OperationKind,
    /// Unix timestamp in milliseconds when admission occurred.
    pub started_at: i64,
}

/// Current operation lifecycle status.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationStatus {
    /// An operation is actively executing.
    Running,
    /// An operation is open but not currently executing.
    Open,
    /// An abort has been requested.
    Aborting,
}

/// Current operation details exposed by a lane.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CurrentOperationInfo {
    /// Durable operation identifier.
    pub id: OperationId,
    /// Operation family.
    pub kind: OperationKind,
    /// Unix timestamp in milliseconds when the operation started.
    pub started_at: i64,
    /// Current lifecycle status.
    pub status: OperationStatus,
    /// Model captured when the operation was admitted, if available.
    pub captured_model: Option<ModelIdentity>,
}

/// Lane execution state used by low-level workers and remote consumers.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LaneExecutionInfo {
    /// Lane name.
    pub lane: LaneName,
    /// Current durable tip, if any.
    pub tip_id: Option<EntryId>,
    /// Model configured for future operations.
    pub configured_model: ModelIdentity,
    /// Current operation, if one is open.
    pub current: Option<CurrentOperationInfo>,
    /// Most recently settled operation, if any.
    pub last_operation_id: Option<OperationId>,
}

/// Summary information for one lane in a harness listing.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LaneInfo {
    /// Lane name.
    pub name: LaneName,
    /// Current durable tip, if any.
    pub tip_id: Option<EntryId>,
    /// Current operation, if one is open.
    pub operation: Option<CurrentOperationInfo>,
}

/// Operation recovered as open while attaching a harness.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenOperation {
    /// Lane containing the open operation.
    pub lane: LaneName,
    /// Durable operation identifier.
    pub operation_id: OperationId,
    /// Operation family.
    pub kind: OperationKind,
    /// Unix timestamp in milliseconds when the operation started.
    pub started_at: i64,
    /// Whether an abort was already requested.
    pub aborting: bool,
}
