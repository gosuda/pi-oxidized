use serde::{Deserialize, Serialize};

use super::configuration::{
    CompactionReason, CompactionSettings, HarnessRetryPolicy, HarnessStreamOptions,
    InvalidRetryPolicy,
};
use super::{EntryId, LaneConfiguration, LaneName, OperationId, UsageId};
use crate::queue::QueueMode;
use crate::tool::ToolExecutionMode;

/// Cancellation flag carried by every operation leaf, orthogonal to the leaf
/// itself: a cancel request preempts the dispatcher rather than adding states.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Control {
    /// Tag `"running"` — no cancellation has been requested.
    Running,
    /// Tag `"cancel_requested"` — cancellation is pending and the dispatcher
    /// must route to reconciliation before doing any other work.
    CancelRequested {
        /// When cancellation was requested, in Unix epoch milliseconds.
        #[serde(rename = "requestedAt")]
        requested_at: i64,
    },
}

/// Immutable identity of one durable operation, written once at reservation and
/// never rewritten; the mutable part lives in [`OperationState`].
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct OperationMeta {
    /// Session-unique id of this operation.
    #[serde(rename = "operationId")]
    pub operation_id: OperationId,
    /// Lane whose branch the operation drives.
    pub lane: LaneName,
    /// Branch tip the operation started from. `None` is a normal first
    /// operation on an empty branch, not a lost reference.
    #[serde(rename = "sourceTipId")]
    pub source_tip_id: Option<EntryId>,
    /// Reservation time in Unix epoch milliseconds.
    #[serde(rename = "startedAt")]
    pub started_at: i64,
    /// What the caller asked this operation to do.
    pub intent: OperationIntent,
}

/// Caller-supplied purpose recorded with an operation at reservation.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum OperationIntent {
    /// Tag `"run"` — drive the lane until the transcript settles.
    Run {
        /// Pre-committed prompt entries this run must consume. Empty is
        /// normal when the trigger is an inbox item instead.
        #[serde(rename = "promptEntryIds")]
        prompt_entry_ids: Vec<EntryId>,
    },
    /// Tag `"compaction"` — summarize transcript history on the current branch.
    Compaction {
        /// Caller guidance appended to the summary request; `None` uses the
        /// default instruction set.
        #[serde(rename = "customInstructions")]
        custom_instructions: Option<String>,
    },
    /// Tag `"navigation"` — move the branch tip to an earlier entry.
    Navigation {
        /// Destination entry. `None` means the branch root, which is a normal
        /// request, not an unset target.
        #[serde(rename = "targetId")]
        target_id: Option<EntryId>,
        /// Whether the abandoned side is summarized before the tip moves.
        summarize: bool,
        /// Optional label recorded with the navigation; absence stores no label.
        label: Option<String>,
        /// Caller guidance for the summary request, when summarizing.
        custom_instructions: Option<String>,
    },
}

/// Operation family, recorded on the terminal result for classification.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationKind {
    /// Tag `"run"`.
    Run,
    /// Tag `"compaction"`.
    Compaction,
    /// Tag `"navigation"`.
    Navigation,
}

/// Terminal outcome of a driven operation.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminalStatus {
    /// Tag `"completed"` — reached its success boundary.
    Completed,
    /// Tag `"declined"` — a gate refused the operation before it acted.
    Declined,
    /// Tag `"aborted"` — cancelled by request rather than by failure.
    Aborted,
    /// Tag `"failed"` — ended on an error recorded in the result.
    Failed,
}

/// Error carried by a failed terminal result.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct OperationError {
    /// Machine-readable error code; free-form because it originates in the
    /// driver or a provider, not in this crate's enums.
    pub code: String,
    /// Human-readable detail for transcripts and logs.
    pub message: String,
    /// Optional structured context; absence carries no additional data.
    pub details: Option<serde_json::Value>,
}

/// Immutable lane-lived observation record written by one terminal transaction.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct OperationResultRecord {
    /// Operation this record terminated.
    pub operation_id: OperationId,
    /// Family the operation belonged to.
    pub kind: OperationKind,
    /// Terminal outcome.
    pub status: TerminalStatus,
    /// Failure detail. Normally present only when [`Self::status`] is
    /// [`TerminalStatus::Failed`]; absence is the successful case.
    pub error: Option<OperationError>,
    /// Branch tip when the operation started. `None` for a first operation on
    /// an empty branch.
    #[serde(rename = "fromTipId")]
    pub from_tip_id: Option<EntryId>,
    /// Branch tip after the terminal commit. `None` when the operation left the
    /// branch empty or never advanced it.
    #[serde(rename = "tipId")]
    pub tip_id: Option<EntryId>,
    /// Reservation time in Unix epoch milliseconds; mirrors the meta.
    #[serde(rename = "startedAt")]
    pub started_at: i64,
    /// Terminal commit time in Unix epoch milliseconds.
    #[serde(rename = "endedAt")]
    pub ended_at: i64,
}

/// Uniform scope carried by EVERY leaf. Successor construction copies exactly this.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct OperationScope {
    /// Cancellation state, checked before each drive step.
    pub control: Control,
    /// Run configuration captured when the operation was reserved. Later lane
    /// or harness changes do not rewrite it, so a resumed operation drives
    /// under the settings it started with.
    pub settings: RunSettings,
    /// Most recent assistant entry this operation has committed, or `None`
    /// before the first one exists. Used to decide whether a final assistant
    /// turn is available, not to locate the branch tip.
    #[serde(rename = "latestAssistantEntryId")]
    pub latest_assistant_entry_id: Option<EntryId>,
}

/// Per-run behaviour switches frozen into the operation scope at reservation.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct RunSettings {
    /// Thresholds governing when the driver may compact transcript history.
    pub compaction: CompactionSettings,
    /// How many queued steer messages are injected at a drain point.
    #[serde(rename = "steeringMode")]
    pub steering_mode: QueueMode,
    /// How many queued follow-up messages are injected when nothing else
    /// triggers a turn.
    #[serde(rename = "followUpMode")]
    pub follow_up_mode: QueueMode,
    /// Whether one assistant message's tool calls run one-by-one or
    /// concurrently.
    #[serde(rename = "toolExecution")]
    pub tool_execution: ToolExecutionMode,
}

/// Flat durable operation state: exactly 13 family-neutral dispatcher leaves.
/// Tag field is `at`; every literal is frozen and dotted.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "at")]
pub enum OperationState {
    /// Tag `"starting"` — reserved but nothing consumed yet; the driver has
    /// not yet claimed trigger entries.
    #[serde(rename = "starting")]
    Starting {
        /// Uniform cancellation, settings, and latest-assistant scope.
        #[serde(flatten)]
        scope: OperationScope,
    },
    /// Tag `"checkpoint"` — a resumable boundary is recorded; the next step
    /// decides whether another assistant turn is needed.
    #[serde(rename = "checkpoint")]
    Checkpoint {
        /// Uniform cancellation, settings, and latest-assistant scope.
        #[serde(flatten)]
        scope: OperationScope,
        /// Continuation decision and the entry that triggered it.
        #[serde(flatten)]
        data: CheckpointData,
    },
    /// Tag `"assistant.ready"` — a provider request may be issued for this
    /// step.
    #[serde(rename = "assistant.ready")]
    AssistantReady {
        /// Uniform cancellation, settings, and latest-assistant scope.
        #[serde(flatten)]
        scope: OperationScope,
        /// Model, configuration, and retry parameters captured for this step.
        #[serde(rename = "generationContext")]
        generation_context: GenerationContext,
        /// One-based attempt number the next request will be.
        #[serde(rename = "nextAttempt")]
        next_attempt: u64,
    },
    /// Tag `"assistant.effect_pending"` — a response settled but its durable
    /// effects (entry and usage writes) are not yet confirmed, so recovery must
    /// reconcile rather than re-issue the request.
    #[serde(rename = "assistant.effect_pending")]
    AssistantEffectPending {
        /// Uniform cancellation, settings, and latest-assistant scope.
        #[serde(flatten)]
        scope: OperationScope,
        /// Model, configuration, and retry parameters captured for this step.
        #[serde(rename = "generationContext")]
        generation_context: GenerationContext,
        /// One-based attempt whose outcome is being settled.
        attempt: u64,
        /// Reserved entry id the assistant response will be written under.
        #[serde(rename = "responseEntryId")]
        response_entry_id: EntryId,
        /// Reserved usage-row id recording this attempt's token accounting.
        #[serde(rename = "usageId")]
        usage_id: UsageId,
        /// Output-token limit the model was asked for, captured so an
        /// end-of-turn length check can tell a real overflow from a shorter
        /// configured cap.
        #[serde(rename = "intendedOutputLimit")]
        intended_output_limit: u32,
        /// Model context window in tokens, captured for overflow detection.
        #[serde(rename = "contextWindow")]
        context_window: u32,
    },
    /// Tag `"assistant.retry_wait"` — the attempt failed and the next one is
    /// deferred until the backoff deadline.
    #[serde(rename = "assistant.retry_wait")]
    AssistantRetryWait {
        /// Uniform cancellation, settings, and latest-assistant scope.
        #[serde(flatten)]
        scope: OperationScope,
        /// Model, configuration, and retry parameters captured for this step.
        #[serde(rename = "generationContext")]
        generation_context: GenerationContext,
        /// Persisted backoff deadline and failure message.
        #[serde(flatten)]
        retry: RetryWait,
    },
    /// Tag `"tools"` — tool calls from the settled assistant message are being
    /// executed and placed.
    #[serde(rename = "tools")]
    Tools {
        /// Uniform cancellation, settings, and latest-assistant scope.
        #[serde(flatten)]
        scope: OperationScope,
        /// Nested per-call state machine for this assistant message.
        batch: ToolBatch,
    },
    /// Tag `"deferred.suspended"` — the request produced a deferred handle and
    /// the operation is waiting for a later poll instead of holding a thread.
    #[serde(rename = "deferred.suspended")]
    DeferredSuspended {
        /// Step, source entry, poll counter, and captured request parameters.
        #[serde(flatten)]
        deferred: DeferredScope,
    },
    /// Tag `"deferred.effect_pending"` — a polled deferred response settled and
    /// its durable writes are not yet confirmed.
    #[serde(rename = "deferred.effect_pending")]
    DeferredEffectPending {
        /// Step, source entry, poll counter, and captured request parameters.
        #[serde(flatten)]
        deferred: DeferredScope,
        /// Reserved entry id the settled deferred response will use.
        #[serde(rename = "responseEntryId")]
        response_entry_id: EntryId,
        /// Reserved usage-row id for the settled deferred response.
        #[serde(rename = "usageId")]
        usage_id: UsageId,
    },
    /// Tag `"summary.deciding"` — a summary task is claimed and the driver is
    /// deciding what to summarize.
    #[serde(rename = "summary.deciding")]
    SummaryDeciding {
        /// Uniform cancellation, settings, and latest-assistant scope.
        #[serde(flatten)]
        scope: OperationScope,
        /// Summary work item, including its result boundary.
        task: SummaryTask,
    },
    /// Tag `"summary.ready"` — preparation is durable and a summary request may
    /// be issued.
    #[serde(rename = "summary.ready")]
    SummaryReady {
        /// Uniform cancellation, settings, and latest-assistant scope.
        #[serde(flatten)]
        scope: OperationScope,
        /// Task plus the captured summary-generation parameters.
        #[serde(flatten)]
        generation: SummaryGenerationScope,
        /// One-based attempt number the next summary request will be.
        #[serde(rename = "nextAttempt")]
        next_attempt: u64,
    },
    /// Tag `"summary.effect_pending"` — a summary response settled and its
    /// writes (entry, usage rows, preparation cleanup) are being applied.
    #[serde(rename = "summary.effect_pending")]
    SummaryEffectPending {
        /// Uniform cancellation, settings, and latest-assistant scope.
        #[serde(flatten)]
        scope: OperationScope,
        /// Task plus the captured summary-generation parameters.
        #[serde(flatten)]
        generation: SummaryGenerationScope,
        /// One-based attempt whose outcome is being settled.
        attempt: u64,
        /// In-flight request reference when a request was admitted, or `None`
        /// before the first one — the normal state right after preparation.
        request: Option<SummaryRequestRef>,
        /// Usage-row ids recorded by this attempt, in the order they were
        /// written. Empty before any usage is accounted.
        #[serde(rename = "usageIds")]
        usage_ids: Vec<UsageId>,
    },
    /// Tag `"summary.retry_wait"` — the summary attempt failed and its next
    /// attempt is deferred until the backoff deadline.
    #[serde(rename = "summary.retry_wait")]
    SummaryRetryWait {
        /// Uniform cancellation, settings, and latest-assistant scope.
        #[serde(flatten)]
        scope: OperationScope,
        /// Task plus the captured summary-generation parameters.
        #[serde(flatten)]
        generation: SummaryGenerationScope,
        /// Persisted backoff deadline and failure message.
        #[serde(flatten)]
        retry: RetryWait,
    },
    /// Tag `"navigation.ready_to_commit"` — everything needed for the tip move
    /// is durable; only the committing transaction remains.
    #[serde(rename = "navigation.ready_to_commit")]
    NavigationReadyToCommit {
        /// Uniform cancellation, settings, and latest-assistant scope.
        #[serde(flatten)]
        scope: OperationScope,
        /// Unsummarized navigation may target the branch root (`None`).
        #[serde(rename = "targetId")]
        target_id: Option<EntryId>,
        /// Optional label recorded with the navigation; absence stores none.
        label: Option<String>,
    },
}

impl OperationState {
    /// Borrows the uniform scope every leaf carries (candidate
    /// `operationScopeOf`).
    #[must_use]
    pub fn scope(&self) -> &OperationScope {
        match self {
            Self::Starting { scope }
            | Self::Checkpoint { scope, .. }
            | Self::AssistantReady { scope, .. }
            | Self::AssistantEffectPending { scope, .. }
            | Self::AssistantRetryWait { scope, .. }
            | Self::Tools { scope, .. }
            | Self::SummaryDeciding { scope, .. }
            | Self::SummaryReady { scope, .. }
            | Self::SummaryEffectPending { scope, .. }
            | Self::SummaryRetryWait { scope, .. }
            | Self::NavigationReadyToCommit { scope, .. } => scope,
            Self::DeferredSuspended { deferred } | Self::DeferredEffectPending { deferred, .. } => {
                &deferred.scope
            }
        }
    }
    /// Mutably borrows the uniform scope, including through the deferred
    /// leaves that embed it in [`DeferredScope`].
    #[must_use]
    pub fn scope_mut(&mut self) -> &mut OperationScope {
        match self {
            Self::Starting { scope }
            | Self::Checkpoint { scope, .. }
            | Self::AssistantReady { scope, .. }
            | Self::AssistantEffectPending { scope, .. }
            | Self::AssistantRetryWait { scope, .. }
            | Self::Tools { scope, .. }
            | Self::SummaryDeciding { scope, .. }
            | Self::SummaryReady { scope, .. }
            | Self::SummaryEffectPending { scope, .. }
            | Self::SummaryRetryWait { scope, .. }
            | Self::NavigationReadyToCommit { scope, .. } => scope,
            Self::DeferredSuspended { deferred } | Self::DeferredEffectPending { deferred, .. } => {
                &mut deferred.scope
            }
        }
    }
    /// Returns the dispatcher discriminant for this leaf.
    #[must_use]
    pub fn at(&self) -> OperationAt {
        match self {
            Self::Starting { .. } => OperationAt::Starting,
            Self::Checkpoint { .. } => OperationAt::Checkpoint,
            Self::AssistantReady { .. } => OperationAt::AssistantReady,
            Self::AssistantEffectPending { .. } => OperationAt::AssistantEffectPending,
            Self::AssistantRetryWait { .. } => OperationAt::AssistantRetryWait,
            Self::Tools { .. } => OperationAt::Tools,
            Self::DeferredSuspended { .. } => OperationAt::DeferredSuspended,
            Self::DeferredEffectPending { .. } => OperationAt::DeferredEffectPending,
            Self::SummaryDeciding { .. } => OperationAt::SummaryDeciding,
            Self::SummaryReady { .. } => OperationAt::SummaryReady,
            Self::SummaryEffectPending { .. } => OperationAt::SummaryEffectPending,
            Self::SummaryRetryWait { .. } => OperationAt::SummaryRetryWait,
            Self::NavigationReadyToCommit { .. } => OperationAt::NavigationReadyToCommit,
        }
    }
}

/// Dispatcher discriminant, decoupled from payloads.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OperationAt {
    /// Leaf `at: "starting"`.
    Starting,
    /// Leaf `at: "checkpoint"`.
    Checkpoint,
    /// Leaf `at: "assistant.ready"`.
    AssistantReady,
    /// Leaf `at: "assistant.effect_pending"`.
    AssistantEffectPending,
    /// Leaf `at: "assistant.retry_wait"`.
    AssistantRetryWait,
    /// Leaf `at: "tools"`.
    Tools,
    /// Leaf `at: "deferred.suspended"`.
    DeferredSuspended,
    /// Leaf `at: "deferred.effect_pending"`.
    DeferredEffectPending,
    /// Leaf `at: "summary.deciding"`.
    SummaryDeciding,
    /// Leaf `at: "summary.ready"`.
    SummaryReady,
    /// Leaf `at: "summary.effect_pending"`.
    SummaryEffectPending,
    /// Leaf `at: "summary.retry_wait"`.
    SummaryRetryWait,
    /// Leaf `at: "navigation.ready_to_commit"`.
    NavigationReadyToCommit,
}

/// A reserved operation: immutable identity plus the current durable leaf.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct Operation {
    /// Immutable reservation record.
    pub meta: OperationMeta,
    /// Current dispatcher leaf.
    pub state: OperationState,
}

/// Checkpoint payload; the flat leaf literal replaces the old nested phase tag.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct CheckpointData {
    /// What may happen next from this checkpoint.
    pub continuation: Continuation,
    /// Entry whose arrival triggered this checkpoint.
    #[serde(rename = "triggerEntryId")]
    pub trigger_entry_id: EntryId,
}

/// Continuation decision recorded at a checkpoint.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Continuation {
    /// Tag `"need_assistant"` — another assistant turn must be requested.
    NeedAssistant {
        /// Whether the one allowed overflow-recovery attempt has already been
        /// spent, so a further overflow cannot be retried.
        #[serde(rename = "overflowRecoveryUsed")]
        overflow_recovery_used: bool,
    },
    /// Tag `"may_finish"` — the operation may end here.
    MayFinish {
        /// Whether a final assistant message is still expected before finishing.
        #[serde(rename = "includeFinalAssistant")]
        include_final_assistant: bool,
    },
}

/// Everything a generation step needs that must not be re-read from live lane
/// configuration: the step is replayed after a crash with exactly these values.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct GenerationContext {
    /// Caller-chosen id for the step being generated, used to namespace the
    /// step's tool-argument and memo addresses.
    #[serde(rename = "stepId")]
    pub step_id: String,
    /// Entry whose arrival triggered this generation.
    #[serde(rename = "triggerEntryId")]
    pub trigger_entry_id: EntryId,
    /// Model, thinking level, and tool set captured for the step.
    pub configuration: LaneConfiguration,
    /// Stream options captured for the step, including provider retry settings.
    #[serde(rename = "streamOptions")]
    pub stream_options: HarnessStreamOptions,
    /// Whole-request retry budget normalized when the step was prepared.
    #[serde(rename = "retryPolicy")]
    pub retry_policy: NormalizedRetryPolicy,
    /// Whether this step has already consumed its overflow-recovery attempt.
    #[serde(rename = "overflowRecoveryUsed")]
    pub overflow_recovery_used: bool,
}

/// Whole-request retry budget after validation and normalization, so a resumed
/// step never re-derives it from a policy that may have changed since.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct NormalizedRetryPolicy {
    /// Total attempts allowed, i.e. one more than the configured retries;
    /// always at least 1.
    #[serde(rename = "maxAttempts")]
    pub max_attempts: u64,
    /// Base backoff between attempts, in milliseconds.
    #[serde(rename = "baseDelayMs")]
    pub base_delay_ms: u64,
}

/// Retry state persisted while a whole-request retry is waiting.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct RetryWait {
    /// One-based number of the attempt to issue once the deadline passes.
    #[serde(rename = "nextAttempt")]
    pub next_attempt: u64,
    /// Earliest wall-clock time the next attempt may start, in Unix epoch
    /// milliseconds.
    #[serde(rename = "notBefore")]
    pub not_before: i64,
    /// Failure text from the attempt that triggered this wait, kept for
    /// reporting; the empty string means no message was available.
    #[serde(rename = "errorMessage")]
    pub error_message: String,
}

impl TryFrom<HarnessRetryPolicy> for NormalizedRetryPolicy {
    type Error = InvalidRetryPolicy;

    /// Validates `policy` and converts it to the persisted form. A disabled
    /// policy yields exactly one attempt, and an enabled one yields
    /// `max_retries + 1`; overflow of that addition is rejected.
    fn try_from(policy: HarnessRetryPolicy) -> Result<Self, Self::Error> {
        policy.validate()?;
        let max_attempts = if policy.enabled {
            policy
                .max_retries
                .checked_add(1)
                .ok_or(InvalidRetryPolicy)?
        } else {
            1
        };
        Ok(Self {
            max_attempts,
            base_delay_ms: policy.base_delay_ms,
        })
    }
}

/// Nested per-call state machine for the tool calls of one assistant message.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ToolBatch {
    /// Assistant entry whose tool calls this batch tracks.
    #[serde(rename = "assistantEntryId")]
    pub assistant_entry_id: EntryId,
    /// Lane configuration captured when the batch was created, so tool
    /// execution does not follow a later tool-set change.
    pub configuration: LaneConfiguration,
    /// Caller-chosen turn id grouping the batch's result entries.
    #[serde(rename = "turnId")]
    pub turn_id: String,
    /// One entry per tool call, in assistant source order. Empty is normal for
    /// a message with no calls.
    pub calls: Vec<ToolCall>,
}

/// Durable state of a single tool call.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ToolCall {
    /// Zero-based index in the assistant message's COMPLETE content array,
    /// not a filtered tool-call ordinal.
    #[serde(rename = "sourceIndex")]
    pub source_index: u32,
    /// Reserved entry id the tool result will be written under; also serves as
    /// the call's invocation id.
    #[serde(rename = "resultEntryId")]
    pub result_entry_id: EntryId,
    /// Lifecycle stage of this call.
    pub status: ToolCallStatus,
}

/// Lifecycle stage of one tool call.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ToolCallStatus {
    /// Tag `"planned"` — reserved but not yet executed.
    Planned,
    /// Tag `"effect_pending"` — execution started and its outcome is unknown,
    /// so recovery must decide whether replaying is acceptable.
    EffectPending {
        /// Whether this call may be re-issued after a crash.
        replay: ReplayPolicy,
    },
    /// Tag `"outcome_ready"` — the result exists but has not been committed.
    OutcomeReady {
        /// Whether this result ends the run instead of requesting another
        /// assistant turn.
        terminate: bool,
    },
    /// Tag `"completed"` — the result is committed.
    Completed {
        /// Termination flag carried into the committed result entry.
        terminate: bool,
    },
}

/// Whether a tool call with an unknown outcome may be re-issued during recovery.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReplayPolicy {
    /// Tag `"never"` — side effects may already have happened; never re-run.
    Never,
    /// Tag `"safe"` — re-running is safe, so recovery may re-issue the call.
    Safe,
}

/// Where a summary task's result must land, decided when the task is created.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ResultBoundary {
    /// Tag `"resume_checkpoint"` — continue the operation from this checkpoint.
    ResumeCheckpoint {
        /// Checkpoint to resume after.
        #[serde(rename = "resumeAfter")]
        resume_after: CheckpointData,
    },
    /// Tag `"finish"` — the operation ends when the summary commits.
    Finish,
    /// Tag `"commit_navigation"` — the summary is followed by a branch-tip move.
    CommitNavigation {
        /// Entry the tip must move to. Unlike the navigation leaf's target,
        /// this is always a concrete entry.
        #[serde(rename = "targetId")]
        target_id: EntryId,
        /// Optional label for the navigation; absence stores none.
        label: Option<String>,
    },
}

/// Summary work item claimed by a summary leaf.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct SummaryTask {
    /// Caller-chosen id distinguishing concurrent summary tasks.
    #[serde(rename = "taskId")]
    pub task_id: String,
    /// Why the summary was requested. `None` is a caller-initiated summary with
    /// no automatic trigger to report.
    pub reason: Option<CompactionReason>,
    /// Caller guidance appended to the summary request; `None` uses defaults.
    #[serde(rename = "customInstructions")]
    pub custom_instructions: Option<String>,
    /// Where the committed summary must hand control afterwards.
    pub boundary: ResultBoundary,
}

/// Captured parameters for one summary generation, replayed unchanged on
/// recovery.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct SummaryContext {
    /// Reserved entry id the summary result will be written under.
    #[serde(rename = "resultEntryId")]
    pub result_entry_id: EntryId,
    /// Model, thinking level, and tool set captured for the summary request.
    pub configuration: LaneConfiguration,
    /// Stream options captured for the summary request.
    #[serde(rename = "streamOptions")]
    pub stream_options: HarnessStreamOptions,
    /// Retry budget normalized when the summary was prepared.
    #[serde(rename = "retryPolicy")]
    pub retry_policy: NormalizedRetryPolicy,
}

/// Task and generation parameters carried together by every summary leaf.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct SummaryGenerationScope {
    /// Summary work item being advanced.
    pub task: SummaryTask,
    /// Captured summary-generation parameters.
    #[serde(rename = "summaryContext")]
    pub summary_context: SummaryContext,
}

/// Identifies an admitted summary request and the usage row it must settle
/// against.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct SummaryRequestRef {
    /// Zero-based index of the request within the attempt sequence.
    pub index: u32,
    /// Usage-row id reserved for this request's accounting.
    #[serde(rename = "usageId")]
    pub usage_id: UsageId,
}

/// Scope carried by both deferred leaves: the uniform operation scope plus the
/// step and handle-recovery data needed to resume by handle rather than by
/// resubmitting the original prompt.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct DeferredScope {
    /// Uniform cancellation, settings, and latest-assistant scope.
    #[serde(flatten)]
    pub scope: OperationScope,
    /// Step whose request produced the deferred handle.
    #[serde(rename = "stepId")]
    pub step_id: String,
    /// Entry holding the deferred handle to resume from.
    #[serde(rename = "sourceEntryId")]
    pub source_entry_id: EntryId,
    /// Number of polls already made for this step, used to build a unique turn
    /// id per poll. Zero is the normal state before the first poll.
    pub poll: u32,
    /// Lane configuration captured when the request was issued.
    pub configuration: LaneConfiguration,
    /// Stream options captured when the request was issued.
    #[serde(rename = "streamOptions")]
    pub stream_options: HarnessStreamOptions,
}
