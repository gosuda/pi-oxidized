//! Typed lane/session snapshots and the deterministic event reducer.
//!
//! Snapshots are immutable-at-the-boundary records for remote and watcher
//! consumers.  [`reduce_lane_snapshot`] is deliberately a plain state fold:
//! it does not inspect a registry or perform I/O, and navigation completion is
//! the one event that asks the caller to capture a fresh snapshot.

use pi_ai::{AssistantMessage, DeferredHandle, Message};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::message::AgentMessage;
use crate::session::{
    Entry, EntryId, EntryType, InboxItemKind, LaneConfiguration, LaneName,
    OperationKind, OperationId, OperationResultRecord, SessionStats,
};
use crate::tool::AgentToolResult;

use super::event::{ConfigUpdateChange, HarnessEvent, HarnessEventPayload};
use super::result::{LaneInfo, OperationStatus};

/// A queued lane item in the exact order in which it will be drained.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", rename_all_fields = "camelCase")]
pub enum LaneQueuedItem {
    /// An LLM-compatible queued message.
    Message {
        /// Reserved durable entry id.
        #[serde(rename = "entryId")]
        entry_id: EntryId,
        /// Queue destination.
        kind: InboxItemKind,
        /// Message payload.
        message: AgentMessage,
    },
    /// A custom queued entry.
    Custom {
        /// Reserved durable entry id.
        #[serde(rename = "entryId")]
        entry_id: EntryId,
        /// Queue destination.  Custom items are write entries in the source
        /// contract, but retaining the field keeps the record self-describing.
        kind: InboxItemKind,
        /// Custom message role.
        #[serde(rename = "customType")]
        custom_type: String,
        /// Optional custom payload.
        #[serde(skip_serializing_if = "Option::is_none")]
        data: Option<Value>,
    },
}

/// A tool currently represented in a lane operation.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case", rename_all_fields = "camelCase")]
pub enum LaneSnapshotTool {
    /// A tool whose effect is still running.
    Running {
        /// Tool-call identifier.
        #[serde(rename = "toolCallId")]
        tool_call_id: String,
        /// Registered tool name.
        #[serde(rename = "toolName")]
        tool_name: String,
        /// Effective arguments.
        args: Map<String, Value>,
        /// Latest complete progress result, when one exists.
        #[serde(skip_serializing_if = "Option::is_none")]
        result: Option<AgentToolResult>,
    },
    /// A tool whose effect has settled and whose result is not yet represented
    /// by a committed tool-result entry.
    Settled {
        /// Tool-call identifier.
        #[serde(rename = "toolCallId")]
        tool_call_id: String,
        /// Registered tool name.
        #[serde(rename = "toolName")]
        tool_name: String,
        /// Effective arguments.
        args: Map<String, Value>,
        /// Final result.
        result: AgentToolResult,
        /// Whether the result is an error.
        #[serde(rename = "isError")]
        is_error: bool,
    },
}

impl LaneSnapshotTool {
    /// Returns this tool's invocation identifier.
    #[must_use]
    pub fn tool_call_id(&self) -> &str {
        match self {
            Self::Running { tool_call_id, .. } | Self::Settled { tool_call_id, .. } => tool_call_id,
        }
    }
}

/// A retry state shown while a lane operation waits to run again.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct LaneSnapshotRetry {
    /// One-based current attempt.
    pub attempt: u64,
    /// Maximum total attempts.
    #[serde(rename = "maxAttempts")]
    pub max_attempts: u64,
    /// Earliest next-attempt timestamp in milliseconds.
    #[serde(rename = "nextAttemptAt")]
    pub next_attempt_at: i64,
}

/// A deferred response descriptor shown for a suspended run.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct LaneSnapshotDeferred {
    /// Provider-owned response handle.
    pub handle: DeferredHandle,
    /// Number of polls already performed.
    pub poll: u32,
}

/// The open operation portion of a [`LaneSnapshot`].
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct LaneSnapshotOperation {
    /// Operation identifier.
    pub id: OperationId,
    /// Operation family.
    pub kind: OperationKind,
    /// Unix timestamp in milliseconds when the operation started.
    #[serde(rename = "startedAt")]
    pub started_at: i64,
    /// Tip before operation admission.
    #[serde(rename = "fromTipId")]
    pub from_tip_id: Option<EntryId>,
    /// Current operation status.
    pub status: OperationStatus,
    /// Retry state, when waiting for another attempt.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry: Option<LaneSnapshotRetry>,
    /// Deferred state, when suspended.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deferred: Option<LaneSnapshotDeferred>,
    /// Latest assistant stream snapshot, while one is active.
    #[serde(rename = "streamingMessage", skip_serializing_if = "Option::is_none")]
    pub streaming_message: Option<AssistantMessage>,
    /// Tools whose effects have not yet moved into committed transcript entries.
    #[serde(rename = "runningTools")]
    pub running_tools: Vec<LaneSnapshotTool>,
}

/// Coherent lane state published to watch and remote consumers.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct LaneSnapshot {
    /// Lane name.
    pub lane: LaneName,
    /// Current branch transcript in oldest-to-newest order.
    pub transcript: Vec<Entry>,
    /// Current branch tip.
    #[serde(rename = "tipId")]
    pub tip_id: Option<EntryId>,
    /// Most recently settled operation.
    #[serde(rename = "lastResult", skip_serializing_if = "Option::is_none")]
    pub last_result: Option<OperationResultRecord>,
    /// Configuration used for future operations.
    pub configuration: LaneConfiguration,
    /// Committed usage and message-count totals.
    pub stats: SessionStats,
    /// Current open operation, if any.
    pub operation: Option<LaneSnapshotOperation>,
    /// Complete ordered queue contents.
    pub queues: Vec<LaneQueuedItem>,
    /// Whether a fault has made this lane unsafe to continue.
    pub faulted: bool,
}

/// Session-wide listing state.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct SessionSnapshot {
    /// Lanes in stable listing order.
    pub lanes: Vec<LaneInfo>,
    /// Whether a session-level fault has occurred.
    pub faulted: bool,
}

/// Result of folding one event into a lane snapshot.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReduceOutcome {
    /// The snapshot changed.
    Updated,
    /// The event was valid but not relevant to this snapshot.
    Ignored,
    /// A fresh snapshot is required before applying more events.
    NeedsResnapshot,
}

/// Events for another lane are ignored.  Usage events carry their lane inside
/// the payload because the outer envelope is reserved for lane-scoped events;
/// only the matching lane's totals are updated.  A navigation completion
/// cannot be represented as a local mutation because it may move the tip to a
/// branch not present in the current transcript; it returns
/// [`ReduceOutcome::NeedsResnapshot`].
#[must_use]
pub fn reduce_lane_snapshot(snapshot: &mut LaneSnapshot, event: &HarnessEvent) -> ReduceOutcome {
    if !event_targets_lane(snapshot, event) {
        return ReduceOutcome::Ignored;
    }

    match &event.payload {
        payload @ (HarnessEventPayload::RunStart { .. }
            | HarnessEventPayload::CompactionStart { .. }
            | HarnessEventPayload::NavigationStart { .. }) => {
            reduce_operation_start(snapshot, payload)
        }
        payload @ (HarnessEventPayload::OperationAbort { .. }
            | HarnessEventPayload::RunResume { .. }
            | HarnessEventPayload::RunSuspend { .. }
            | HarnessEventPayload::RetryScheduled { .. }
            | HarnessEventPayload::RetryStart { .. }
            | HarnessEventPayload::RetryEnd { .. }) => {
            reduce_operation_control(snapshot, payload)
        }
        payload @ (HarnessEventPayload::MessageStart { .. }
            | HarnessEventPayload::MessageUpdate { .. }
            | HarnessEventPayload::MessageEnd { .. }) => reduce_message(snapshot, payload),
        payload @ (HarnessEventPayload::ToolStart { .. }
            | HarnessEventPayload::ToolUpdate { .. }
            | HarnessEventPayload::ToolEnd { .. }) => reduce_tool(snapshot, payload),
        payload @ (HarnessEventPayload::EntryAdded { .. }
            | HarnessEventPayload::QueueUpdate { .. }
            | HarnessEventPayload::Usage { .. }
            | HarnessEventPayload::ConfigUpdate { .. }
            | HarnessEventPayload::Fault { .. }) => {
            reduce_entry_queue_usage_config(snapshot, event, payload)
        }
        payload @ (HarnessEventPayload::RunEnd { .. }
            | HarnessEventPayload::CompactionEnd { .. }
            | HarnessEventPayload::NavigationEnd { .. }) => reduce_terminal(snapshot, payload),
        HarnessEventPayload::HandlerError { .. }
        | HarnessEventPayload::TurnStart { .. }
        | HarnessEventPayload::TurnEnd { .. }
        | HarnessEventPayload::ValueUpdate { .. }
        | HarnessEventPayload::LaneCreated { .. } => ReduceOutcome::Ignored,
    }
}

fn event_targets_lane(snapshot: &LaneSnapshot, event: &HarnessEvent) -> bool {
    if event
        .lane
        .as_ref()
        .is_some_and(|lane| lane != &snapshot.lane)
    {
        return false;
    }
    if let HarnessEventPayload::Usage { lane, .. } = &event.payload
        && lane != &snapshot.lane
    {
        return false;
    }
    if event.payload.requires_lane() && event.lane.as_ref() != Some(&snapshot.lane) {
        return false;
    }
    true
}

fn reduce_operation_start(
    snapshot: &mut LaneSnapshot,
    payload: &HarnessEventPayload,
) -> ReduceOutcome {
    match payload {
        HarnessEventPayload::RunStart { run_id, started_at } => {
            snapshot.operation = Some(open_operation(
                run_id.clone(),
                OperationKind::Run,
                *started_at,
                snapshot.tip_id.clone(),
            ));
            ReduceOutcome::Updated
        }
        HarnessEventPayload::CompactionStart { run_id, started_at, .. } => {
            // In-run compaction is a segment bracket and must not replace the
            // open run operation.  A standalone compaction has no operation.
            if snapshot.operation.is_some() {
                return ReduceOutcome::Ignored;
            }
            snapshot.operation = Some(open_operation(
                run_id.clone(),
                OperationKind::Compaction,
                *started_at,
                snapshot.tip_id.clone(),
            ));
            ReduceOutcome::Updated
        }
        HarnessEventPayload::NavigationStart { run_id, started_at, .. } => {
            if snapshot.operation.is_some() {
                return ReduceOutcome::Ignored;
            }
            snapshot.operation = Some(open_operation(
                run_id.clone(),
                OperationKind::Navigation,
                *started_at,
                snapshot.tip_id.clone(),
            ));
            ReduceOutcome::Updated
        }
        _ => ReduceOutcome::Ignored,
    }
}

fn reduce_operation_control(
    snapshot: &mut LaneSnapshot,
    payload: &HarnessEventPayload,
) -> ReduceOutcome {
    match payload {
        HarnessEventPayload::OperationAbort { operation_id, .. } => {
            let Some(operation) = matching_operation_mut(snapshot, operation_id) else {
                return ReduceOutcome::Ignored;
            };
            operation.status = OperationStatus::Aborting;
            ReduceOutcome::Updated
        }
        HarnessEventPayload::RunResume { run_id } => {
            let Some(operation) = matching_operation_mut(snapshot, run_id) else {
                return ReduceOutcome::Ignored;
            };
            if operation.kind != OperationKind::Run {
                return ReduceOutcome::Ignored;
            }
            operation.deferred = None;
            ReduceOutcome::Updated
        }
        HarnessEventPayload::RunSuspend {
            run_id,
            deferred,
            poll,
            ..
        } => {
            let Some(operation) = matching_operation_mut(snapshot, run_id) else {
                return ReduceOutcome::Ignored;
            };
            if operation.kind != OperationKind::Run {
                return ReduceOutcome::Ignored;
            }
            operation.streaming_message = None;
            operation.deferred = Some(LaneSnapshotDeferred {
                handle: deferred.clone(),
                poll: *poll,
            });
            ReduceOutcome::Updated
        }
        HarnessEventPayload::RetryScheduled {
            run_id,
            attempt,
            max_attempts,
            not_before,
            ..
        } => {
            let Some(operation) = matching_operation_mut(snapshot, run_id) else {
                return ReduceOutcome::Ignored;
            };
            operation.retry = Some(LaneSnapshotRetry {
                attempt: *attempt,
                max_attempts: *max_attempts,
                next_attempt_at: *not_before,
            });
            ReduceOutcome::Updated
        }
        HarnessEventPayload::RetryStart { run_id, .. }
        | HarnessEventPayload::RetryEnd { run_id, .. } => {
            let Some(operation) = matching_operation_mut(snapshot, run_id) else {
                return ReduceOutcome::Ignored;
            };
            operation.retry = None;
            ReduceOutcome::Updated
        }
        _ => ReduceOutcome::Ignored,
    }
}

fn reduce_message(snapshot: &mut LaneSnapshot, payload: &HarnessEventPayload) -> ReduceOutcome {
    match payload {
        HarnessEventPayload::MessageStart { run_id, message } => {
            let Some(run_id) = run_id else {
                return ReduceOutcome::Ignored;
            };
            let Some(message) = assistant_message(message) else {
                return ReduceOutcome::Ignored;
            };
            if message.stop_reason != pi_ai::StopReason::Pending {
                return ReduceOutcome::Ignored;
            }
            let Some(operation) = matching_operation_mut(snapshot, run_id) else {
                return ReduceOutcome::Ignored;
            };
            operation.streaming_message = Some(message);
            ReduceOutcome::Updated
        }
        HarnessEventPayload::MessageUpdate { run_id, message, .. } => {
            let Some(message) = assistant_message(message) else {
                return ReduceOutcome::Ignored;
            };
            let Some(operation) = matching_operation_mut(snapshot, run_id) else {
                return ReduceOutcome::Ignored;
            };
            operation.streaming_message = Some(message);
            ReduceOutcome::Updated
        }
        HarnessEventPayload::MessageEnd { run_id, .. } => {
            let Some(run_id) = run_id else {
                return ReduceOutcome::Ignored;
            };
            let Some(operation) = matching_operation_mut(snapshot, run_id) else {
                return ReduceOutcome::Ignored;
            };
            operation.streaming_message = None;
            ReduceOutcome::Updated
        }
        _ => ReduceOutcome::Ignored,
    }
}

fn reduce_tool(snapshot: &mut LaneSnapshot, payload: &HarnessEventPayload) -> ReduceOutcome {
    match payload {
        HarnessEventPayload::ToolStart {
            run_id,
            tool_call_id,
            tool_name,
            args,
            ..
        } => {
            let Some(operation) = matching_operation_mut(snapshot, run_id) else {
                return ReduceOutcome::Ignored;
            };
            upsert_tool(
                operation,
                LaneSnapshotTool::Running {
                    tool_call_id: tool_call_id.clone(),
                    tool_name: tool_name.clone(),
                    args: args.clone(),
                    result: None,
                },
            );
            ReduceOutcome::Updated
        }
        HarnessEventPayload::ToolUpdate {
            run_id,
            tool_call_id,
            partial_result,
            ..
        } => {
            let Some(operation) = matching_operation_mut(snapshot, run_id) else {
                return ReduceOutcome::Ignored;
            };
            let Some(tool) = operation
                .running_tools
                .iter_mut()
                .find(|tool| tool.tool_call_id() == tool_call_id)
            else {
                return ReduceOutcome::Ignored;
            };
            if let LaneSnapshotTool::Running { result, .. } = tool {
                *result = Some(partial_result.clone());
                return ReduceOutcome::Updated;
            }
            ReduceOutcome::Ignored
        }
        HarnessEventPayload::ToolEnd {
            run_id,
            tool_call_id,
            tool_name,
            result,
            is_error,
            ..
        } => {
            let Some(operation) = matching_operation_mut(snapshot, run_id) else {
                return ReduceOutcome::Ignored;
            };
            let Some(index) = operation
                .running_tools
                .iter()
                .position(|tool| tool.tool_call_id() == tool_call_id)
            else {
                return ReduceOutcome::Ignored;
            };
            let args = match &operation.running_tools[index] {
                LaneSnapshotTool::Running { args, .. } | LaneSnapshotTool::Settled { args, .. } => {
                    args.clone()
                }
            };
            operation.running_tools[index] = LaneSnapshotTool::Settled {
                tool_call_id: tool_call_id.clone(),
                tool_name: tool_name.clone(),
                args,
                result: result.clone(),
                is_error: *is_error,
            };
            ReduceOutcome::Updated
        }
        _ => ReduceOutcome::Ignored,
    }
}

fn reduce_entry_queue_usage_config(
    snapshot: &mut LaneSnapshot,
    event: &HarnessEvent,
    payload: &HarnessEventPayload,
) -> ReduceOutcome {
    match payload {
        HarnessEventPayload::EntryAdded { entry } => {
            if entry.entry_type() == EntryType::Message
                && let Some(message) = entry.message()
                && let Some(tool_call_id) = tool_result_call_id(message)
                && let Some(operation) = snapshot.operation.as_mut()
            {
                operation
                    .running_tools
                    .retain(|tool| tool.tool_call_id() != tool_call_id);
            }
            if entry.entry_type() == EntryType::Compaction {
                snapshot.transcript.clear();
            }
            snapshot.transcript.push(entry.clone());
            snapshot.tip_id = Some(entry.id().clone());
            if entry.entry_type() == EntryType::Message {
                snapshot.stats.message_count = snapshot.stats.message_count.saturating_add(1);
            }
            ReduceOutcome::Updated
        }
        HarnessEventPayload::QueueUpdate { queues } => {
            snapshot.queues.clone_from(queues);
            ReduceOutcome::Updated
        }
        HarnessEventPayload::Usage { totals, .. } => {
            snapshot.stats.usage = totals.clone();
            ReduceOutcome::Updated
        }
        HarnessEventPayload::ConfigUpdate { change } => {
            let is_lane_configuration = matches!(
                change,
                ConfigUpdateChange::Model { .. }
                    | ConfigUpdateChange::ThinkingLevel { .. }
                    | ConfigUpdateChange::ActiveTools { .. }
            );
            if is_lane_configuration && event.lane.as_ref() != Some(&snapshot.lane) {
                return ReduceOutcome::Ignored;
            }
            match change {
                ConfigUpdateChange::Model { value, .. } => {
                    snapshot.configuration.model = value.clone();
                    ReduceOutcome::Updated
                }
                ConfigUpdateChange::ThinkingLevel { value, .. } => {
                    snapshot.configuration.thinking_level = *value;
                    ReduceOutcome::Updated
                }
                ConfigUpdateChange::ActiveTools { value, .. } => {
                    snapshot.configuration.active_tool_names.clone_from(value);
                    ReduceOutcome::Updated
                }
                ConfigUpdateChange::Tools
                | ConfigUpdateChange::Resources
                | ConfigUpdateChange::StreamOptions { .. }
                | ConfigUpdateChange::RetryPolicy { .. }
                | ConfigUpdateChange::CompactionSettings { .. }
                | ConfigUpdateChange::SteeringMode { .. }
                | ConfigUpdateChange::FollowUpMode { .. } => ReduceOutcome::Ignored,
            }
        }
        HarnessEventPayload::Fault { .. } => {
            snapshot.faulted = true;
            ReduceOutcome::Updated
        }
        _ => ReduceOutcome::Ignored,
    }
}

fn reduce_terminal(snapshot: &mut LaneSnapshot, payload: &HarnessEventPayload) -> ReduceOutcome {
    match payload {
        HarnessEventPayload::RunEnd {
            run_id,
            status,
            from_tip_id,
            tip_id,
            ended_at,
            error,
        } => {
            let Some(operation) = matching_operation(snapshot, run_id) else {
                return ReduceOutcome::Ignored;
            };
            if operation.kind != OperationKind::Run {
                return ReduceOutcome::Ignored;
            }
            snapshot.last_result = Some(OperationResultRecord {
                operation_id: operation.id.clone(),
                kind: operation.kind,
                status: *status,
                error: error.clone(),
                from_tip_id: from_tip_id.clone(),
                tip_id: tip_id.clone(),
                started_at: operation.started_at,
                ended_at: *ended_at,
            });
            snapshot.operation = None;
            snapshot.tip_id.clone_from(tip_id);
            ReduceOutcome::Updated
        }
        HarnessEventPayload::CompactionEnd {
            run_id,
            status,
            ended_at,
            error,
            ..
        } => {
            let Some(operation) = matching_operation(snapshot, run_id) else {
                return ReduceOutcome::Ignored;
            };
            if operation.kind != OperationKind::Compaction {
                return ReduceOutcome::Ignored;
            }
            snapshot.last_result = Some(OperationResultRecord {
                operation_id: operation.id.clone(),
                kind: operation.kind,
                status: *status,
                error: error.clone(),
                from_tip_id: operation.from_tip_id.clone(),
                tip_id: snapshot.tip_id.clone(),
                started_at: operation.started_at,
                ended_at: *ended_at,
            });
            snapshot.operation = None;
            ReduceOutcome::Updated
        }
        HarnessEventPayload::NavigationEnd { .. } => ReduceOutcome::NeedsResnapshot,
        _ => ReduceOutcome::Ignored,
    }
}

fn open_operation(
    id: OperationId,
    kind: OperationKind,
    started_at: i64,
    from_tip_id: Option<EntryId>,
) -> LaneSnapshotOperation {
    LaneSnapshotOperation {
        id,
        kind,
        started_at,
        from_tip_id,
        status: OperationStatus::Open,
        retry: None,
        deferred: None,
        streaming_message: None,
        running_tools: Vec::new(),
    }
}

fn matching_operation<'a>(
    snapshot: &'a LaneSnapshot,
    id: &OperationId,
) -> Option<&'a LaneSnapshotOperation> {
    snapshot
        .operation
        .as_ref()
        .filter(|operation| &operation.id == id)
}

fn matching_operation_mut<'a>(
    snapshot: &'a mut LaneSnapshot,
    id: &OperationId,
) -> Option<&'a mut LaneSnapshotOperation> {
    snapshot
        .operation
        .as_mut()
        .filter(|operation| &operation.id == id)
}

fn upsert_tool(operation: &mut LaneSnapshotOperation, tool: LaneSnapshotTool) {
    if let Some(existing) = operation
        .running_tools
        .iter_mut()
        .find(|existing| existing.tool_call_id() == tool.tool_call_id())
    {
        *existing = tool;
    } else {
        operation.running_tools.push(tool);
    }
}

fn assistant_message(message: &AgentMessage) -> Option<AssistantMessage> {
    match message.as_llm()? {
        Message::Assistant(message) => Some((**message).clone()),
        Message::User(_) | Message::ToolResult(_) => None,
    }
}

fn tool_result_call_id(message: &AgentMessage) -> Option<&str> {
    match message.as_llm()? {
        Message::ToolResult(tool_result) => Some(&tool_result.tool_call_id),
        Message::User(_) | Message::Assistant(_) => None,
    }
}


