//! Typed, wire-compatible harness lifecycle events.
//!
//! Harness events are passive observations of committed state and lifecycle
//! transitions.  The envelope is flattened on the wire so the JSON shape is
//! the same as the TypeScript harness event union.

use std::fmt;
use std::sync::Arc;

use pi_ai::{AssistantMessageEvent, AssistantMessageFrame, DeferredHandle, ToolResultMessage};
use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value};

use crate::message::AgentMessage;
use crate::queue::QueueMode;
use crate::session::{
    CompactionReason, CompactionSettings, Entry, EntryId, HarnessRetryPolicy, LaneName,
    ModelIdentity, OperationError, OperationId, TerminalStatus, UsageRow, HarnessStreamOptions,
};
use crate::tool::AgentToolResult;

/// One of the event names accepted by [`HarnessEventBus`](super::bus::HarnessEventBus).
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HarnessEventType {
    /// A run operation started.
    RunStart,
    /// A deferred run resumed.
    RunResume,
    /// A run suspended while a deferred response is pending.
    RunSuspend,
    /// Cancellation was durably requested for an operation.
    OperationAbort,
    /// A run operation reached a terminal state.
    RunEnd,
    /// An infrastructure fault was observed.
    Fault,
    /// A listener or hook failed in isolation.
    HandlerError,
    /// A conversational turn started.
    TurnStart,
    /// A conversational turn ended.
    TurnEnd,
    /// A whole-request retry was scheduled.
    RetryScheduled,
    /// A retry attempt started.
    RetryStart,
    /// A retry attempt ended.
    RetryEnd,
    /// A transcript message started.
    MessageStart,
    /// A transcript message changed while streaming.
    MessageUpdate,
    /// A transcript message ended.
    MessageEnd,
    /// A tool effect was admitted.
    ToolStart,
    /// A tool effect produced a progress update.
    ToolUpdate,
    /// A tool effect ended.
    ToolEnd,
    /// An immutable session entry became queryable.
    EntryAdded,
    /// The complete lane queue changed.
    QueueUpdate,
    /// A session metadata value changed.
    ValueUpdate,
    /// A configuration value changed.
    ConfigUpdate,
    /// A structural compaction operation started or an in-run segment began.
    CompactionStart,
    /// A structural compaction operation or in-run segment ended.
    CompactionEnd,
    /// A navigation operation started.
    NavigationStart,
    /// A navigation operation ended.
    NavigationEnd,
    /// A lane was created.
    LaneCreated,
    /// Authoritative usage totals changed.
    Usage,
}

/// A configuration property carried by a [`ConfigUpdate`](HarnessEventPayload::ConfigUpdate).
///
/// The enum is flattened into the event payload.  This keeps known records
/// typed while retaining the candidate wire shape (`property`, `value`, and
/// `previous` are sibling fields).
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "property", rename_all = "camelCase")]
pub enum ConfigUpdateChange {
    /// Lane model identity changed.
    Model {
        /// New model identity.
        value: ModelIdentity,
        /// Previous model identity.
        previous: ModelIdentity,
    },
    /// Lane thinking level changed.
    ThinkingLevel {
        /// New thinking level.
        value: pi_ai::ModelThinkingLevel,
        /// Previous thinking level.
        previous: pi_ai::ModelThinkingLevel,
    },
    /// Lane active-tool names changed.
    ActiveTools {
        /// New active tool names.
        value: Vec<String>,
        /// Previous active tool names.
        previous: Vec<String>,
    },
    /// Host tool registry changed.  Registries are not replicated.
    Tools,
    /// Host resource registry changed.  Registries are not replicated.
    Resources,
    /// Stream options changed globally.
    StreamOptions {
        /// New stream options.
        value: HarnessStreamOptions,
        /// Previous stream options.
        previous: HarnessStreamOptions,
    },
    /// Whole-request retry policy changed globally.
    RetryPolicy {
        /// New retry policy.
        value: HarnessRetryPolicy,
        /// Previous retry policy.
        previous: HarnessRetryPolicy,
    },
    /// Compaction settings changed globally.
    CompactionSettings {
        /// New compaction settings.
        value: CompactionSettings,
        /// Previous compaction settings.
        previous: CompactionSettings,
    },
    /// Steering queue mode changed globally.
    SteeringMode {
        /// New queue mode.
        value: QueueMode,
        /// Previous queue mode.
        previous: QueueMode,
    },
    /// Follow-up queue mode changed globally.
    FollowUpMode {
        /// New queue mode.
        value: QueueMode,
        /// Previous queue mode.
        previous: QueueMode,
    },
}

/// A value update's known value address.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "value", rename_all = "snake_case")]
pub enum ValueUpdateChange {
    /// The session display name changed.
    SessionName {
        /// New display name, or `None` when deleted.
        #[serde(skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },
    /// An entry label changed.
    EntryLabel {
        /// Labelled entry.
        #[serde(rename = "targetId")]
        target_id: EntryId,
        /// New label, or `None` when deleted.
        #[serde(skip_serializing_if = "Option::is_none")]
        label: Option<String>,
    },
}

/// The source of an isolated handler failure.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HandlerErrorKind {
    /// A hook handler failed.
    Hook {
        /// Hook name.
        hook: String,
    },
    /// An event listener failed.
    Event {
        /// Event name whose listener failed.
        event: String,
    },
}

/// Harness event payload, with the frozen `type` literals.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HarnessEventPayload {
    /// A run operation started.
    RunStart {
        /// Operation/run identifier.
        #[serde(rename = "runId")]
        run_id: OperationId,
        /// Unix timestamp in milliseconds.
        #[serde(rename = "startedAt")]
        started_at: i64,
    },
    /// A deferred run resumed.
    RunResume {
        /// Operation/run identifier.
        #[serde(rename = "runId")]
        run_id: OperationId,
    },
    /// A run suspended for deferred generation.
    RunSuspend {
        /// Operation/run identifier.
        #[serde(rename = "runId")]
        run_id: OperationId,
        /// Suspension reason.  The only supported reason is `deferred`.
        reason: SuspendReason,
        /// Provider-owned deferred response handle.
        deferred: DeferredHandle,
        /// Provider poll count.
        poll: u32,
    },
    /// Cancellation was durably requested for an operation.
    OperationAbort {
        /// Operation identifier.
        #[serde(rename = "operationId")]
        operation_id: OperationId,
        /// Drained steering messages.
        steer: Vec<AgentMessage>,
        /// Drained follow-up messages.
        #[serde(rename = "followUp")]
        follow_up: Vec<AgentMessage>,
    },
    /// A run operation reached a terminal state.
    RunEnd {
        /// Operation/run identifier.
        #[serde(rename = "runId")]
        run_id: OperationId,
        /// Terminal status.
        status: TerminalStatus,
        /// Tip before the operation.
        #[serde(rename = "fromTipId")]
        from_tip_id: Option<EntryId>,
        /// Tip after the operation.
        #[serde(rename = "tipId")]
        tip_id: Option<EntryId>,
        /// Unix timestamp in milliseconds.
        #[serde(rename = "endedAt")]
        ended_at: i64,
        /// Failure details when status is `failed`.
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<OperationError>,
    },
    /// An infrastructure fault.
    Fault {
        /// Stable fault code.
        code: String,
        /// Human-readable fault message.
        message: String,
    },
    /// An isolated hook or event-listener error.
    HandlerError {
        /// Handler source and name.
        #[serde(flatten)]
        kind: HandlerErrorKind,
        /// Human-readable error message.
        error: String,
        /// Optional diagnostic stack.
        #[serde(skip_serializing_if = "Option::is_none")]
        stack: Option<String>,
    },
    /// A conversational turn started.
    TurnStart {
        /// Operation/run identifier.
        #[serde(rename = "runId")]
        run_id: OperationId,
        /// Turn identifier.
        #[serde(rename = "turnId")]
        turn_id: String,
    },
    /// A conversational turn ended.
    TurnEnd {
        /// Operation/run identifier.
        #[serde(rename = "runId")]
        run_id: OperationId,
        /// Turn identifier.
        #[serde(rename = "turnId")]
        turn_id: String,
        /// Assistant message for the turn.
        message: AgentMessage,
        /// Tool results in assistant source order.
        #[serde(rename = "toolResults")]
        tool_results: Vec<ToolResultMessage>,
    },
    /// A whole-request retry was scheduled.
    RetryScheduled {
        /// Operation/run identifier.
        #[serde(rename = "runId")]
        run_id: OperationId,
        /// Durable step identifier.
        step: String,
        /// One-based attempt number.
        attempt: u64,
        /// Maximum total attempts.
        #[serde(rename = "maxAttempts")]
        max_attempts: u64,
        /// Delay in milliseconds.
        #[serde(rename = "delayMs")]
        delay_ms: u64,
        /// Earliest next-attempt timestamp in milliseconds.
        #[serde(rename = "notBefore")]
        not_before: i64,
        /// Error that caused the retry.
        #[serde(rename = "errorMessage")]
        error_message: String,
    },
    /// A retry attempt started.
    RetryStart {
        /// Operation/run identifier.
        #[serde(rename = "runId")]
        run_id: OperationId,
        /// Durable step identifier.
        step: String,
        /// One-based attempt number.
        attempt: u64,
    },
    /// A retry attempt ended.
    RetryEnd {
        /// Operation/run identifier.
        #[serde(rename = "runId")]
        run_id: OperationId,
        /// Durable step identifier.
        step: String,
        /// One-based attempt number.
        attempt: u64,
        /// Whether this attempt succeeded.
        success: bool,
        /// Final error after exhausted retries, when any.
        #[serde(rename = "finalError", skip_serializing_if = "Option::is_none")]
        final_error: Option<String>,
    },
    /// A transcript message started.
    MessageStart {
        /// Operation/run identifier, when associated with a run.
        #[serde(rename = "runId", skip_serializing_if = "Option::is_none")]
        run_id: Option<OperationId>,
        /// Message snapshot at start.
        message: AgentMessage,
    },
    /// A transcript message changed while streaming.
    MessageUpdate {
        /// Operation/run identifier.
        #[serde(rename = "runId")]
        run_id: OperationId,
        /// Latest message snapshot.
        message: AgentMessage,
        /// Provider stream event that produced the snapshot.
        #[serde(rename = "event")]
        event: Box<AssistantMessageEvent>,
        /// Optional persisted provider frame.
        #[serde(skip_serializing_if = "Option::is_none")]
        frame: Option<AssistantMessageFrame>,
    },
    /// A transcript message ended.
    MessageEnd {
        /// Operation/run identifier, when associated with a run.
        #[serde(rename = "runId", skip_serializing_if = "Option::is_none")]
        run_id: Option<OperationId>,
        /// Final message snapshot.
        message: AgentMessage,
        /// Immutable transcript entry id when committed.
        #[serde(rename = "entryId", skip_serializing_if = "Option::is_none")]
        entry_id: Option<EntryId>,
    },
    /// A tool effect was admitted.
    ToolStart {
        /// Operation/run identifier.
        #[serde(rename = "runId")]
        run_id: OperationId,
        /// Turn identifier.
        #[serde(rename = "turnId")]
        turn_id: String,
        /// Tool-call identifier.
        #[serde(rename = "toolCallId")]
        tool_call_id: String,
        /// Tool name.
        #[serde(rename = "toolName")]
        tool_name: String,
        /// Effective validated arguments.
        args: Map<String, Value>,
    },
    /// A tool effect produced a progress update.
    ToolUpdate {
        /// Operation/run identifier.
        #[serde(rename = "runId")]
        run_id: OperationId,
        /// Turn identifier.
        #[serde(rename = "turnId")]
        turn_id: String,
        /// Tool-call identifier.
        #[serde(rename = "toolCallId")]
        tool_call_id: String,
        /// Tool name.
        #[serde(rename = "toolName")]
        tool_name: String,
        /// Latest complete partial result.
        #[serde(rename = "partialResult")]
        partial_result: AgentToolResult,
    },
    /// A tool effect ended.
    ToolEnd {
        /// Operation/run identifier.
        #[serde(rename = "runId")]
        run_id: OperationId,
        /// Turn identifier.
        #[serde(rename = "turnId")]
        turn_id: String,
        /// Tool-call identifier.
        #[serde(rename = "toolCallId")]
        tool_call_id: String,
        /// Tool name.
        #[serde(rename = "toolName")]
        tool_name: String,
        /// Finalized result.
        result: AgentToolResult,
        /// Whether the result is an error.
        #[serde(rename = "isError")]
        is_error: bool,
        /// Whether the tool requests turn termination.
        terminate: bool,
    },
    /// An immutable session entry became queryable.
    EntryAdded {
        /// Newly committed entry.
        entry: Entry,
    },
    /// The complete ordered lane queue changed.
    QueueUpdate {
        /// Current queue contents.
        queues: Vec<super::snapshot::LaneQueuedItem>,
    },
    /// A session metadata value changed.
    ValueUpdate {
        /// Updated value address and payload.
        #[serde(flatten)]
        change: ValueUpdateChange,
    },
    /// A configuration value changed.
    ConfigUpdate {
        /// Updated property and typed values.
        #[serde(flatten)]
        change: ConfigUpdateChange,
    },
    /// A structural compaction operation started or an in-run segment began.
    CompactionStart {
        /// Operation/run identifier.
        #[serde(rename = "runId")]
        run_id: OperationId,
        /// Compaction trigger.
        reason: CompactionReason,
        /// Unix timestamp in milliseconds.
        #[serde(rename = "startedAt")]
        started_at: i64,
    },
    /// A structural compaction operation or segment ended.
    CompactionEnd {
        /// Operation/run identifier.
        #[serde(rename = "runId")]
        run_id: OperationId,
        /// Compaction trigger.
        reason: CompactionReason,
        /// Terminal status.
        status: TerminalStatus,
        /// New compaction entry for a completed operation.
        #[serde(rename = "entryId", skip_serializing_if = "Option::is_none")]
        entry_id: Option<EntryId>,
        /// Failure details when status is `failed`.
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<OperationError>,
        /// Unix timestamp in milliseconds.
        #[serde(rename = "endedAt")]
        ended_at: i64,
    },
    /// A navigation operation started.
    NavigationStart {
        /// Operation/run identifier.
        #[serde(rename = "runId")]
        run_id: OperationId,
        /// Target entry, or the current tree tip when absent.
        #[serde(rename = "targetId")]
        target_id: Option<EntryId>,
        /// Unix timestamp in milliseconds.
        #[serde(rename = "startedAt")]
        started_at: i64,
    },
    /// A navigation operation ended.
    NavigationEnd {
        /// Operation/run identifier.
        #[serde(rename = "runId")]
        run_id: OperationId,
        /// Terminal status.
        status: TerminalStatus,
        /// Tip before navigation.
        #[serde(rename = "fromTipId")]
        from_tip_id: Option<EntryId>,
        /// Tip after navigation.
        #[serde(rename = "tipId")]
        tip_id: Option<EntryId>,
        /// Failure details when status is `failed`.
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<OperationError>,
        /// Unix timestamp in milliseconds.
        #[serde(rename = "endedAt")]
        ended_at: i64,
    },
    /// A lane was created at the supplied parent tip.
    LaneCreated {
        /// Parent tip at creation, if any.
        at: Option<EntryId>,
    },
    /// Authoritative usage totals changed for one lane.
    Usage {
        /// Lane whose committed totals changed.  This is an inner field:
        /// usage events deliberately have no outer envelope lane.
        lane: LaneName,
        /// Committed usage row.
        row: UsageRow,
        /// Totals after applying the row.
        totals: pi_ai::Usage,
    },
}

/// The only supported run suspension reason.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SuspendReason {
    /// The provider accepted the request for deferred completion.
    Deferred,
}

impl HarnessEventPayload {
    /// Returns the stable event type for this payload.
    #[must_use]
    pub const fn event_type(&self) -> HarnessEventType {
        match self {
            Self::RunStart { .. } => HarnessEventType::RunStart,
            Self::RunResume { .. } => HarnessEventType::RunResume,
            Self::RunSuspend { .. } => HarnessEventType::RunSuspend,
            Self::OperationAbort { .. } => HarnessEventType::OperationAbort,
            Self::RunEnd { .. } => HarnessEventType::RunEnd,
            Self::Fault { .. } => HarnessEventType::Fault,
            Self::HandlerError { .. } => HarnessEventType::HandlerError,
            Self::TurnStart { .. } => HarnessEventType::TurnStart,
            Self::TurnEnd { .. } => HarnessEventType::TurnEnd,
            Self::RetryScheduled { .. } => HarnessEventType::RetryScheduled,
            Self::RetryStart { .. } => HarnessEventType::RetryStart,
            Self::RetryEnd { .. } => HarnessEventType::RetryEnd,
            Self::MessageStart { .. } => HarnessEventType::MessageStart,
            Self::MessageUpdate { .. } => HarnessEventType::MessageUpdate,
            Self::MessageEnd { .. } => HarnessEventType::MessageEnd,
            Self::ToolStart { .. } => HarnessEventType::ToolStart,
            Self::ToolUpdate { .. } => HarnessEventType::ToolUpdate,
            Self::ToolEnd { .. } => HarnessEventType::ToolEnd,
            Self::EntryAdded { .. } => HarnessEventType::EntryAdded,
            Self::QueueUpdate { .. } => HarnessEventType::QueueUpdate,
            Self::ValueUpdate { .. } => HarnessEventType::ValueUpdate,
            Self::ConfigUpdate { .. } => HarnessEventType::ConfigUpdate,
            Self::CompactionStart { .. } => HarnessEventType::CompactionStart,
            Self::CompactionEnd { .. } => HarnessEventType::CompactionEnd,
            Self::NavigationStart { .. } => HarnessEventType::NavigationStart,
            Self::NavigationEnd { .. } => HarnessEventType::NavigationEnd,
            Self::LaneCreated { .. } => HarnessEventType::LaneCreated,
            Self::Usage { .. } => HarnessEventType::Usage,
        }
    }

    /// Returns whether this payload is associated with a lane by envelope rule.
    #[must_use]
    pub const fn requires_lane(&self) -> bool {
        match self {
            Self::Fault { .. }
            | Self::HandlerError { .. }
            | Self::ValueUpdate { .. }
            | Self::Usage { .. }
            | Self::ConfigUpdate {
                change:
                    ConfigUpdateChange::Tools
                    | ConfigUpdateChange::Resources
                    | ConfigUpdateChange::StreamOptions { .. }
                    | ConfigUpdateChange::RetryPolicy { .. }
                    | ConfigUpdateChange::CompactionSettings { .. }
                    | ConfigUpdateChange::SteeringMode { .. }
                    | ConfigUpdateChange::FollowUpMode { .. },
            } => false,
            Self::ConfigUpdate {
                change:
                    ConfigUpdateChange::Model { .. }
                    | ConfigUpdateChange::ThinkingLevel { .. }
                    | ConfigUpdateChange::ActiveTools { .. },
            }
            | Self::RunStart { .. }
            | Self::RunResume { .. }
            | Self::RunSuspend { .. }
            | Self::OperationAbort { .. }
            | Self::RunEnd { .. }
            | Self::TurnStart { .. }
            | Self::TurnEnd { .. }
            | Self::RetryScheduled { .. }
            | Self::RetryStart { .. }
            | Self::RetryEnd { .. }
            | Self::MessageStart { .. }
            | Self::MessageUpdate { .. }
            | Self::MessageEnd { .. }
            | Self::ToolStart { .. }
            | Self::ToolUpdate { .. }
            | Self::ToolEnd { .. }
            | Self::EntryAdded { .. }
            | Self::QueueUpdate { .. }
            | Self::CompactionStart { .. }
            | Self::CompactionEnd { .. }
            | Self::NavigationStart { .. }
            | Self::NavigationEnd { .. }
            | Self::LaneCreated { .. } => true,
        }
    }

    /// Returns whether `recovery: true` is meaningful for this payload.
    #[must_use]
    pub const fn allows_recovery(&self) -> bool {
        !matches!(self, Self::Fault { .. } | Self::ValueUpdate { .. } | Self::Usage { .. })
    }
}

/// A flattened harness event envelope.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct HarnessEvent {
    /// Lane associated with a lane-scoped event.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lane: Option<LaneName>,
    /// Whether this event was replayed during recovery.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub recovery: bool,
    /// Typed event payload.
    #[serde(flatten)]
    pub payload: HarnessEventPayload,
}

impl<'de> Deserialize<'de> for HarnessEvent {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        // Usage carries its lane inside the payload while every other
        // lane-scoped event carries it in the flattened envelope.  Reading the
        // object first is the only way to disambiguate that shared wire key
        // without weakening either typed record.
        let mut object = Map::<String, Value>::deserialize(deserializer)?;
        let event_type = object
            .get("type")
            .and_then(Value::as_str)
            .ok_or_else(|| D::Error::custom("harness event is missing string type"))?;
        let lane = if event_type == HarnessEventType::Usage.as_str() {
            None
        } else {
            object
                .remove("lane")
                .map(serde_json::from_value)
                .transpose()
                .map_err(D::Error::custom)?
        };
        let recovery = object
            .remove("recovery")
            .map(serde_json::from_value)
            .transpose()
            .map_err(D::Error::custom)?
            .unwrap_or(false);
        let payload =
            serde_json::from_value(Value::Object(object)).map_err(D::Error::custom)?;
        let event = Self {
            lane,
            recovery,
            payload,
        };
        event.validate().map_err(D::Error::custom)?;
        Ok(event)
    }
}

impl HarnessEvent {
    /// Constructs and validates an event envelope.
    ///
    /// # Errors
    ///
    /// Returns [`EventEnvelopeError`] when the lane and recovery fields
    /// violate the payload's envelope rules.
    pub fn new(
        lane: Option<LaneName>,
        recovery: bool,
        payload: HarnessEventPayload,
    ) -> Result<Self, EventEnvelopeError> {
        let event = Self {
            lane,
            recovery,
            payload,
        };
        event.validate()?;
        Ok(event)
    }

    /// Constructs a lane-scoped event.
    pub fn lane(lane: impl Into<LaneName>, payload: HarnessEventPayload) -> Self {
        Self {
            lane: Some(lane.into()),
            recovery: false,
            payload,
        }
    }

    /// Constructs a global event.
    #[must_use]
    pub fn global(payload: HarnessEventPayload) -> Self {
        Self {
            lane: None,
            recovery: false,
            payload,
        }
    }

    /// Constructs a recovery event for a lane.
    pub fn recovery(lane: impl Into<LaneName>, payload: HarnessEventPayload) -> Self {
        Self {
            lane: Some(lane.into()),
            recovery: true,
            payload,
        }
    }

    /// Returns this event's stable type.
    #[must_use]
    pub const fn event_type(&self) -> HarnessEventType {
        self.payload.event_type()
    }

    /// Validates lane and recovery envelope rules.
    ///
    /// # Errors
    ///
    /// Returns [`EventEnvelopeError`] when a lane-required payload has no
    /// lane, a lane-forbidden payload carries one, or a recovery event lacks
    /// a lane or a replayable payload.
    pub fn validate(&self) -> Result<(), EventEnvelopeError> {
        if self.payload.requires_lane() && self.lane.is_none() {
            return Err(EventEnvelopeError::new(format!(
                "{} events require a lane",
                self.event_type().as_str()
            )));
        }
        if !self.payload.requires_lane()
            && !matches!(self.payload, HarnessEventPayload::Fault { .. } | HarnessEventPayload::ValueUpdate { .. } | HarnessEventPayload::HandlerError { .. })
            && self.lane.is_some()
        {
            return Err(EventEnvelopeError::new(format!(
                "{} events cannot carry a lane",
                self.event_type().as_str()
            )));
        }
        if self.recovery && (!self.payload.allows_recovery() || self.lane.is_none()) {
            return Err(EventEnvelopeError::new(
                "recovery events require a lane and a replayable payload",
            ));
        }
        Ok(())
    }
}

impl HarnessEventType {
    /// Returns the wire literal.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::RunStart => "run_start",
            Self::RunResume => "run_resume",
            Self::RunSuspend => "run_suspend",
            Self::OperationAbort => "operation_abort",
            Self::RunEnd => "run_end",
            Self::Fault => "fault",
            Self::HandlerError => "handler_error",
            Self::TurnStart => "turn_start",
            Self::TurnEnd => "turn_end",
            Self::RetryScheduled => "retry_scheduled",
            Self::RetryStart => "retry_start",
            Self::RetryEnd => "retry_end",
            Self::MessageStart => "message_start",
            Self::MessageUpdate => "message_update",
            Self::MessageEnd => "message_end",
            Self::ToolStart => "tool_start",
            Self::ToolUpdate => "tool_update",
            Self::ToolEnd => "tool_end",
            Self::EntryAdded => "entry_added",
            Self::QueueUpdate => "queue_update",
            Self::ValueUpdate => "value_update",
            Self::ConfigUpdate => "config_update",
            Self::CompactionStart => "compaction_start",
            Self::CompactionEnd => "compaction_end",
            Self::NavigationStart => "navigation_start",
            Self::NavigationEnd => "navigation_end",
            Self::LaneCreated => "lane_created",
            Self::Usage => "usage",
        }
    }
}

impl fmt::Display for HarnessEventType {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Invalid lane/recovery envelope combination.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
#[error("{message}")]
pub struct EventEnvelopeError {
    message: String,
}

impl EventEnvelopeError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    /// Returns the validation message.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

/// Exactly-once marker supplied to a resnapshot capture.
///
/// The callback is backed by the watcher state and is intentionally shared:
/// a second call returns an error rather than silently creating a second
/// boundary.  Its representation is private so callers cannot forge a mark.
pub struct MarkBoundary(Arc<dyn Fn() -> Result<(), super::result::HarnessError> + Send + Sync>);

impl MarkBoundary {
    /// Marks the capture boundary exactly once.
    ///
    /// # Errors
    ///
    /// Returns [`super::result::HarnessError`] when the boundary was already
    /// marked or the mark callback is unavailable.
    pub fn mark(&self) -> Result<(), super::result::HarnessError> {
        (self.0)()
    }

    pub(crate) fn from_callback(
        callback: Arc<dyn Fn() -> Result<(), super::result::HarnessError> + Send + Sync>,
    ) -> Self {
        Self(callback)
    }
}

// Keep the event module's public API independent of the bus implementation's
// storage details while allowing downstream code to name the listener type.
/// An asynchronously invoked, passive event listener.
pub type EventListener =
    Arc<dyn Fn(HarnessEvent, crate::context::Context) -> futures::future::BoxFuture<'static, ()> + Send + Sync>;

/// A filter used by a watcher to select events.
pub type EventFilter = Arc<dyn Fn(&HarnessEvent) -> bool + Send + Sync>;
